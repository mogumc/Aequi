//! SQLite schema（§5.6）：建表语句 + 覆盖索引。
//!
//! 设计要点（建表时即定死，与数据结构联动）：
//! - `seq INTEGER PRIMARY KEY AUTOINCREMENT` = 分页游标 + 稳定排序（不用随机 id 排序）
//! - 覆盖索引服务 keyset 分页（恒为 O(limit)）
//! - 强类型列，禁定长字节拼包（§八 第 4 条）
//! - 无哨兵值：`limit_tokens NULL = 不限`、`credits_cap NULL = 无限`
//! - 删除用软删或保留 `seq` 空洞（AUTOINCREMENT 本身不复用序号）

pub const SCHEMA_V1: &str = r#"
-- ---------- 上游 ----------
CREATE TABLE IF NOT EXISTS upstream (
    seq           INTEGER PRIMARY KEY AUTOINCREMENT,
    id            TEXT    NOT NULL UNIQUE,          -- Uuid7
    name          TEXT    NOT NULL,                 -- 展示名（可变，不作身份）
    format        TEXT    NOT NULL,                 -- openai | anthropic | gemini
    base_url      TEXT    NOT NULL,
    enabled       INTEGER NOT NULL DEFAULT 1,
    weight        INTEGER NOT NULL DEFAULT 1,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    meta_json     TEXT
);
CREATE INDEX IF NOT EXISTS idx_upstream_seq ON upstream (seq);

-- ---------- 模型绑定（§4.2.1 单一真源）----------
CREATE TABLE IF NOT EXISTS binding (
    seq            INTEGER PRIMARY KEY AUTOINCREMENT,
    id             TEXT    NOT NULL UNIQUE,         -- Uuid7
    display_model  TEXT    NOT NULL,
    upstream_id    TEXT    NOT NULL REFERENCES upstream(id),
    upstream_model TEXT    NOT NULL,
    enabled        INTEGER NOT NULL DEFAULT 1,
    rate_override_json TEXT,                        -- NULL → RateTable 模型级倍率
    created_at_ms  INTEGER NOT NULL,
    updated_at_ms  INTEGER NOT NULL
);
-- 覆盖索引：按 display_model 查候选；按上游反查；两者均可 keyset 翻页
CREATE INDEX IF NOT EXISTS idx_binding_display_seq ON binding (display_model, seq);
CREATE INDEX IF NOT EXISTS idx_binding_upstream_seq ON binding (upstream_id, seq);

-- ---------- 分组（§6.3）----------
CREATE TABLE IF NOT EXISTS "group" (
    seq            INTEGER PRIMARY KEY AUTOINCREMENT,
    id             TEXT    NOT NULL UNIQUE,         -- 语义字符串（"default"/"pro"/"__admin__"）
    name           TEXT    NOT NULL,
    description    TEXT,
    bindings_json  TEXT    NOT NULL DEFAULT '{"type":"all"}',
    created_at_ms  INTEGER NOT NULL,
    updated_at_ms  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_group_seq ON "group" (seq);

-- ---------- 上游密钥（§6.5.7）----------
CREATE TABLE IF NOT EXISTS upstream_key (
    seq           INTEGER PRIMARY KEY AUTOINCREMENT,  -- ★ 分页游标 + 稳定排序
    id            TEXT    NOT NULL UNIQUE,            -- blake3(secret)[..16]（32 hex）
    upstream_id   TEXT    NOT NULL REFERENCES upstream(id),
    prefix        TEXT    NOT NULL,
    health        INTEGER NOT NULL DEFAULT 0,         -- UpstreamHealth as u8
    enabled       INTEGER NOT NULL DEFAULT 1,         -- 人工意图（与 health 正交）
    weight        INTEGER NOT NULL DEFAULT 1,
    max_inflight  INTEGER NOT NULL DEFAULT 0,         -- 0 = 不限
    -- 上游密钥额度（§6.5.1；与访问密钥积分两层不混用 §八 第 14 条）
    limit_tokens  INTEGER,                            -- NULL = 不限
    used_tokens   INTEGER NOT NULL DEFAULT 0,
    safety_margin_micro INTEGER NOT NULL DEFAULT 980000,  -- 0.98 定点
    exhausted_at_ms INTEGER,
    -- 凭证（默认明文；可选 AEQUI_MASTER_KEY 加密，§6.5.7）
    secret_enc    BLOB    NOT NULL,
    -- 探针元数据（§6.1）
    fail_streak       INTEGER NOT NULL DEFAULT 0,
    last_valid_ms     INTEGER,
    next_probe_at_ms  INTEGER,
    created_at_ms INTEGER NOT NULL,
    meta_json     TEXT
);
-- 覆盖索引：列表按 (upstream_id, health?) 过滤、按 seq 排序
CREATE INDEX IF NOT EXISTS idx_ukey_upstream_seq ON upstream_key (upstream_id, seq);
CREATE INDEX IF NOT EXISTS idx_ukey_health_seq   ON upstream_key (upstream_id, health, seq);

-- ---------- 访问密钥（§6.5.4）----------
CREATE TABLE IF NOT EXISTS access_key (
    seq                INTEGER PRIMARY KEY AUTOINCREMENT,
    id                 TEXT    NOT NULL UNIQUE,      -- Uuid7
    hash               BLOB    NOT NULL UNIQUE,      -- blake3(secret)，明文不落库
    prefix             TEXT    NOT NULL,
    note               TEXT,
    -- 访问密钥积分（下游侧；与上游密钥额度两层不混用）
    credits_cap_micro  INTEGER,                      -- NULL = 无限
    credits_used_micro INTEGER NOT NULL DEFAULT 0,
    lifetime_micro     INTEGER NOT NULL DEFAULT 0,   -- 累计，reset 不清零（审计）
    -- 四维用量（§6.5.2 独立存储）
    used_input  INTEGER NOT NULL DEFAULT 0,
    used_output INTEGER NOT NULL DEFAULT 0,
    used_think  INTEGER NOT NULL DEFAULT 0,
    used_cache  INTEGER NOT NULL DEFAULT 0,
    enabled        INTEGER NOT NULL DEFAULT 1,
    created_at_ms  INTEGER NOT NULL,
    last_used_at_ms INTEGER
);
CREATE INDEX IF NOT EXISTS idx_akey_seq ON access_key (seq);

-- ---------- 分组成员关系（§6.3.1）----------
CREATE TABLE IF NOT EXISTS access_key_group (
    key_seq  INTEGER NOT NULL REFERENCES access_key(seq),
    group_id TEXT    NOT NULL,
    PRIMARY KEY (key_seq, group_id)
);
CREATE INDEX IF NOT EXISTS idx_akg_group_seq ON access_key_group (group_id, key_seq);

-- ---------- 请求日志（可选 sqlite sink；默认 JSONL，§5.4）----------
-- 摘要行（不含 body）。默认 jsonl 主通道时不建此表。
CREATE TABLE IF NOT EXISTS req_log (
    seq          INTEGER PRIMARY KEY AUTOINCREMENT,
    day          INTEGER NOT NULL,                   -- UTC YYYYMMDD
    ts_ms        INTEGER NOT NULL,
    request_id   TEXT    NOT NULL,
    akey_id      TEXT,
    model        TEXT,
    upstream_id  TEXT,
    key_id       TEXT,
    status       INTEGER,
    retry_count  INTEGER,
    usage_input  INTEGER,
    usage_output INTEGER,
    usage_think  INTEGER,
    usage_cache  INTEGER,
    credits      INTEGER,
    error_code   TEXT
);
CREATE INDEX IF NOT EXISTS idx_req_log_day_seq ON req_log (day, seq);

-- ---------- schema 版本 ----------
CREATE TABLE IF NOT EXISTS schema_version (
    version    INTEGER PRIMARY KEY,
    applied_ms INTEGER NOT NULL
);
"#;

/// 高优先写命令通道标记（§5.3）：额度/积分扣减立即提交，不攒批。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WritePriority {
    /// 额度/积分扣减 —— 请求路径必须确认。
    High,
    /// 用量累加、统计、last_used —— 攒批提交。
    Low,
}
