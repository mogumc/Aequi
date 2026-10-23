//! 数据层：SQLite 连接、建表与各表的读写入口。
//!
//! 上层只通过本模块访问数据库，不直接持有 SQL。
//!
//! # 分区
//!
//! 连接池按数据归属分成三个域，**默认落在两个不同的数据库文件上**：
//!
//! | 域 | 内容 | 默认文件 |
//! | --- | --- | --- |
//! | [`Store::control`] | 网关自身：账号、会话、设置 | `aequi.db` |
//! | [`Store::data`] | 业务数据：密钥、积分、用量、模型倍率 | `aequi-data.db` |
//! | [`Store::logs`] | 请求日志 | 暂随数据面 |
//!
//! 控制面与数据面分开存放，是为了让**管理员体系与密钥体系在文件层面就不共享读写**：
//! 密钥侧的任何查询都碰不到管理员数据。两个路径相同时会退化成单文件模式。
//!
//! # 时间
//!
//! 所有时间字段都是秒级 Unix 时间戳（INTEGER），一律 UTC，
//! 不做本地时区换算——混用会直接导致过期判断错乱。

pub mod account;
pub mod block;
pub mod keys;
pub mod proxy_log;
pub mod session;
pub mod settings;
pub mod usage;

use std::path::Path;

use anyhow::Context;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};

/// 三个域的连接池。
#[derive(Clone)]
pub struct Store {
    /// 控制面：账号、会话、设置等网关自身的数据。
    pub control: SqlitePool,
    /// 数据面：密钥、积分、用量等业务数据。
    pub data: SqlitePool,
    /// 日志面：请求日志。
    pub logs: SqlitePool,
}

impl Store {
    /// 打开存储。两个库各自独立；路径相同时共用同一个连接池。
    pub async fn connect(control_path: &Path, data_path: &Path) -> anyhow::Result<Self> {
        let control = open_pool(control_path).await?;

        let data = if data_path == control_path {
            control.clone()
        } else {
            open_pool(data_path).await?
        };

        Ok(Self {
            control,
            data: data.clone(),
            logs: data,
        })
    }

    /// 建表。幂等，每次启动都可安全执行。
    pub async fn migrate(&self) -> anyhow::Result<()> {
        migrate_control(&self.control).await?;
        migrate_data(&self.data).await?;
        Ok(())
    }
}

/// 建控制面表结构。
///
/// 拆成独立函数，是为了让只关心控制面的调用方（例如命令行重置密码）
/// 能只建这一个库，不必为了拿账号数据顺带把数据面也碰一遍。
pub async fn migrate_control(pool: &SqlitePool) -> anyhow::Result<()> {
    for (name, ddl) in [
        ("accounts", ACCOUNTS_DDL),
        ("sessions", SESSIONS_DDL),
        ("settings", SETTINGS_DDL),
        ("idx_sessions_account", SESSIONS_ACCOUNT_INDEX),
    ] {
        sqlx::query(ddl)
            .execute(pool)
            .await
            .with_context(|| format!("执行控制面建表语句失败: {name}"))?;
    }

    Ok(())
}

/// 上游块。一个块只使用一种 API 格式，密钥的调度以块为范围。
///
/// `max_concurrency` 是该块同时发往上游的请求数上限：0 = 不限制。
/// 上限按块配置——不同块的出口与配额彼此独立，挤在一起没有意义。
const UPSTREAM_BLOCKS_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS upstream_blocks (
    id           TEXT    PRIMARY KEY,
    name         TEXT    NOT NULL DEFAULT '',
    api_format   TEXT    NOT NULL,
    weight       INTEGER NOT NULL DEFAULT 1,
    max_concurrency INTEGER NOT NULL DEFAULT 0,
    base_url     TEXT    NOT NULL,
    proxy        TEXT,
    models       TEXT    NOT NULL DEFAULT '[]',
    headers      TEXT    NOT NULL DEFAULT '[]',
    enabled      INTEGER NOT NULL DEFAULT 1,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL
)
"#;

/// 上游密钥。明文保存（要与上游通信就必须能取回来，散列没有意义）。
///
/// 同一块内密钥唯一：重复粘贴同一把不该变成双份调度。
///
/// `never_disable` = 免于被自动停用：连续失败只记录、不改状态，
/// 手动暂停/恢复不受影响。短期故障（限流、上游 5xx）永久禁用好密钥
/// 是这套生命周期规则里最疼的一种死法，由此留一个显式豁免。
const UPSTREAM_KEYS_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS upstream_keys (
    id             TEXT    PRIMARY KEY,
    block_id       TEXT    NOT NULL REFERENCES upstream_blocks(id) ON DELETE CASCADE,
    secret         TEXT    NOT NULL,
    status         TEXT    NOT NULL DEFAULT 'active',
    never_disable  INTEGER NOT NULL DEFAULT 0,
    weight         INTEGER NOT NULL DEFAULT 1,
    failure_count  INTEGER NOT NULL DEFAULT 0,
    cooldown_until INTEGER,
    last_error     TEXT,
    last_used_at   INTEGER,
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL,
    UNIQUE(block_id, secret)
)
"#;

/// 块内查密钥用。
const UPSTREAM_KEYS_BLOCK_INDEX: &str =
    "CREATE INDEX IF NOT EXISTS idx_upstream_keys_block ON upstream_keys(block_id)";

/// `usage_records` 的列清单，用于判断要不要补列。
///
/// 表名写成字面量：sqlx 0.9 的动态 SQL 检查只认 `&'static str`
/// （`raw_sql` 同样），而且这些语句本来就该能直接审读。
const USAGE_RECORDS_COLUMNS: &str = "SELECT name FROM pragma_table_info('usage_records')";

/// `upstream_blocks` 的列清单，用于判断要不要补列。
const UPSTREAM_BLOCKS_COLUMNS: &str = "SELECT name FROM pragma_table_info('upstream_blocks')";

/// `upstream_keys` 的列清单，用于判断要不要补列。
const UPSTREAM_KEYS_COLUMNS: &str = "SELECT name FROM pragma_table_info('upstream_keys')";

/// 给**已存在**的库补上 `block_id`。
///
/// `CREATE TABLE IF NOT EXISTS` 对已存在的表什么都不做，所以老库必须显式
/// `ALTER TABLE`。这条加的是**可空**列，因而可以安全重入（先查列再改）、
/// 也可以回滚（旧二进制忽略新列）。`headers` 那条加的是 `NOT NULL DEFAULT`，
/// 同为可重入、可回滚，理由见它自己的注释。
const USAGE_RECORDS_ADD_BLOCK_ID: &str =
    "ALTER TABLE usage_records ADD COLUMN block_id TEXT";

/// 给**已存在**的库补上 `upstream_blocks.headers`（自定义请求头）。
///
/// 带 `NOT NULL DEFAULT '[]'` 而不是留空：新库与老库要读成同一个形状，而
/// 「没有自定义头」与「自定义头为空」本就无处可区分（不像 `block_id` 的
/// NULL 还承载「不知道走的哪个上游」）。带常量默认值的加列一样可重入、
/// 可回滚——旧二进制的 INSERT/UPDATE 都不列这一列，落的就是这个默认值。
const UPSTREAM_BLOCKS_ADD_HEADERS: &str =
    "ALTER TABLE upstream_blocks ADD COLUMN headers TEXT NOT NULL DEFAULT '[]'";

/// 给**已存在**的库补上 `upstream_blocks.max_concurrency`（每块并发上限）。
///
/// 缺省 0 = 不限制：老块的行为一字不变，上限要管理员显式配置才生效。
/// 与 `headers` 同款理由——带常量默认值的加列可重入、可回滚。
const UPSTREAM_BLOCKS_ADD_MAX_CONCURRENCY: &str =
    "ALTER TABLE upstream_blocks ADD COLUMN max_concurrency INTEGER NOT NULL DEFAULT 0";

/// 给**已存在**的库补上 `upstream_keys.never_disable`（免自动停用）。
///
/// 缺省 0 = 关闭：现有密钥的生命周期规则不变，豁免要显式开启。
const UPSTREAM_KEYS_ADD_NEVER_DISABLE: &str =
    "ALTER TABLE upstream_keys ADD COLUMN never_disable INTEGER NOT NULL DEFAULT 0";

/// 一次「老库补列」迁移。
struct ColumnMigration {
    table: &'static str,
    column: &'static str,
    /// 查该表现有列的 SQL（字面量常量，见上）。
    columns_sql: &'static str,
    /// 缺列时执行的 ALTER。
    alter_sql: &'static str,
}

/// 补列清单：只加列，不改列、不删列、不动数据。
///
/// 收成一个清单而不是散在若干函数里，「哪个库缺哪一列」一眼可见。
/// 以后再加列，在这里补一项、并补一组字面量常量即可。
const COLUMN_MIGRATIONS: &[ColumnMigration] = &[
    ColumnMigration {
        table: "usage_records",
        column: "block_id",
        columns_sql: USAGE_RECORDS_COLUMNS,
        alter_sql: USAGE_RECORDS_ADD_BLOCK_ID,
    },
    ColumnMigration {
        table: "upstream_blocks",
        column: "headers",
        columns_sql: UPSTREAM_BLOCKS_COLUMNS,
        alter_sql: UPSTREAM_BLOCKS_ADD_HEADERS,
    },
    ColumnMigration {
        table: "upstream_blocks",
        column: "max_concurrency",
        columns_sql: UPSTREAM_BLOCKS_COLUMNS,
        alter_sql: UPSTREAM_BLOCKS_ADD_MAX_CONCURRENCY,
    },
    ColumnMigration {
        table: "upstream_keys",
        column: "never_disable",
        columns_sql: UPSTREAM_KEYS_COLUMNS,
        alter_sql: UPSTREAM_KEYS_ADD_NEVER_DISABLE,
    },
];

async fn add_missing_columns(pool: &SqlitePool) -> anyhow::Result<()> {
    for migration in COLUMN_MIGRATIONS {
        add_column_if_missing(pool, migration).await?;
    }
    Ok(())
}

/// 缺哪列补哪列，已经有了就跳过——可重入。
async fn add_column_if_missing(
    pool: &SqlitePool,
    migration: &ColumnMigration,
) -> anyhow::Result<()> {
    let existing: Vec<String> = sqlx::query_scalar(migration.columns_sql)
        .fetch_all(pool)
        .await
        .with_context(|| format!("读取 {} 的列信息失败", migration.table))?;

    if existing.iter().any(|name| name == migration.column) {
        return Ok(());
    }

    sqlx::query(migration.alter_sql)
        .execute(pool)
        .await
        .with_context(|| format!("给 {} 补列 {} 失败", migration.table, migration.column))?;

    tracing::info!(
        table = migration.table,
        column = migration.column,
        "已为已有的表补上缺失的列"
    );

    Ok(())
}

/// 建数据面表结构。
pub async fn migrate_data(pool: &SqlitePool) -> anyhow::Result<()> {
    for (name, ddl) in [
        ("api_keys", API_KEYS_DDL),
        ("usage_records", USAGE_RECORDS_DDL),
        ("idx_usage_key_time", USAGE_KEY_TIME_INDEX),
        ("idx_usage_key_id", USAGE_KEY_ID_INDEX),
        ("upstream_blocks", UPSTREAM_BLOCKS_DDL),
        ("upstream_keys", UPSTREAM_KEYS_DDL),
        ("idx_upstream_keys_block", UPSTREAM_KEYS_BLOCK_INDEX),
        ("proxy_logs", PROXY_LOGS_DDL),
        ("idx_proxy_logs_time", PROXY_LOGS_TIME_INDEX),
        ("idx_proxy_logs_id", PROXY_LOGS_ID_INDEX),
        ("idx_proxy_logs_status_id", PROXY_LOGS_STATUS_ID_INDEX),
    ] {
        sqlx::query(ddl)
            .execute(pool)
            .await
            .with_context(|| format!("执行数据面建表语句失败: {name}"))?;
    }

    add_missing_columns(pool).await?;

    Ok(())
}

/// 打开（必要时创建）一个 SQLite 连接池。
pub async fn open_pool(path: &Path) -> anyhow::Result<SqlitePool> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建数据库目录失败: {}", parent.display()))?;
        }
    }

    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        // WAL 让读写不互相阻塞，适合「读多写少」的场景。
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true);

    // SQLite 同一时刻只允许一个写事务，连接池开太大只会互相等锁。
    SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await
        .with_context(|| format!("连接数据库失败: {}", path.display()))
}

/// 是否为唯一约束冲突。
///
/// 用于把「随机标识撞车」这类极小概率事件和真实故障区分开：
/// 前者可以换个标识重试，后者应该直接报错。
pub fn is_unique_violation(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(err) if err.is_unique_violation())
}

const ACCOUNTS_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS accounts (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    username      TEXT    NOT NULL UNIQUE,
    password_hash TEXT    NOT NULL,
    totp_enabled  INTEGER NOT NULL DEFAULT 0,
    totp_secret   TEXT,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
)
"#;

/// `id` 是主键、`token_hash` 有唯一约束，两者都自带唯一索引，
/// 鉴权与按 ID 查找不会退化成全表扫描；`account_id` 需要显式索引。
///
/// `token_prefix` 是令牌的前几位，只用于在后台辨认「这是哪一次登录」，
/// 不参与鉴权——鉴权只认散列。
///
/// 表里只留**仍然有效**的会话：退出即删除，过期由后台任务回收，
/// 不留退役记录。
const SESSIONS_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS sessions (
    id           TEXT    PRIMARY KEY,
    account_id   INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    token_hash   TEXT    NOT NULL UNIQUE,
    token_prefix TEXT    NOT NULL,
    login_type   TEXT    NOT NULL,
    ip           TEXT,
    user_agent   TEXT,
    created_at   INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    expires_at   INTEGER NOT NULL
)
"#;

const SETTINGS_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS settings (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at INTEGER NOT NULL
)
"#;

const SESSIONS_ACCOUNT_INDEX: &str =
    "CREATE INDEX IF NOT EXISTS idx_sessions_account ON sessions(account_id)";

/// 密钥表。积分直接挂在密钥上。
///
/// 密钥**明文保存**（理由见 [`crate::model::ApiKey`]），不存散列：
/// 既然要支持管理员随时查看，再存一份散列只会让同一份凭证在库里躺两遍。
const API_KEYS_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS api_keys (
    id             TEXT    PRIMARY KEY,
    name           TEXT    NOT NULL DEFAULT '',
    secret         TEXT    NOT NULL UNIQUE,
    key_prefix     TEXT    NOT NULL,
    credits_scaled INTEGER NOT NULL,
    -- 无限额度：扣费时跳过余额判断，但用量与消耗照记。
    unlimited      INTEGER NOT NULL DEFAULT 0,
    enabled        INTEGER NOT NULL DEFAULT 1,
    allowed_models TEXT    NOT NULL DEFAULT '[]',
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL,
    last_used_at   INTEGER
)
"#;

/// 用量记录。
///
/// 倍率按记账当时的取值快照下来，事后调整倍率不会改写历史账单。
/// `block_id` 同样是一次**路由快照**：只写模型名（不带块前缀）的请求会在
/// 提供该模型的块之间轮询，事后没法再算出来，所以记账时把实际落到的块记下来。
/// 可空——旧记录、以及「调度前就失败」的请求都拿不到块。
const USAGE_RECORDS_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS usage_records (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    key_id            TEXT    NOT NULL REFERENCES api_keys(id) ON DELETE CASCADE,
    model             TEXT    NOT NULL,
    block_id          TEXT,
    raw_tokens        INTEGER NOT NULL DEFAULT 0,
    prompt_tokens     INTEGER NOT NULL DEFAULT 0,
    completion_tokens INTEGER NOT NULL DEFAULT 0,
    cache_tokens      INTEGER NOT NULL DEFAULT 0,
    estimated         INTEGER NOT NULL DEFAULT 0,
    stream            INTEGER NOT NULL DEFAULT 0,
    multiplier_bp     INTEGER NOT NULL DEFAULT 10000,
    final_tokens      INTEGER NOT NULL DEFAULT 0,
    credits_scaled    INTEGER NOT NULL DEFAULT 0,
    status            TEXT    NOT NULL DEFAULT 'success',
    error             TEXT,
    created_at        INTEGER NOT NULL
)
"#;

/// 代理详细日志。管理员排障用，与密钥侧的用量记录物理分表。
const PROXY_LOGS_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS proxy_logs (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at        INTEGER NOT NULL,
    access_key_id     TEXT    NOT NULL,
    block_id          TEXT,
    upstream_key_id   TEXT,
    model             TEXT    NOT NULL DEFAULT '',
    ip                TEXT,
    user_agent        TEXT,
    status            TEXT    NOT NULL,
    error_detail      TEXT,
    duration_ms       INTEGER NOT NULL DEFAULT 0,
    prompt_tokens     INTEGER NOT NULL DEFAULT 0,
    completion_tokens INTEGER NOT NULL DEFAULT 0,
    cache_tokens      INTEGER NOT NULL DEFAULT 0,
    estimated         INTEGER NOT NULL DEFAULT 0,
    stream            INTEGER NOT NULL DEFAULT 0,
    credits_scaled    INTEGER NOT NULL DEFAULT 0
)
"#;

/// 按时间清理与按时间倒序列都要走到这一列。
const PROXY_LOGS_TIME_INDEX: &str =
    "CREATE INDEX IF NOT EXISTS idx_proxy_logs_time ON proxy_logs(created_at)";

const USAGE_KEY_TIME_INDEX: &str =
    "CREATE INDEX IF NOT EXISTS idx_usage_key_time ON usage_records(key_id, created_at DESC)";

/// 游标分页走 `WHERE key_id = ? AND id < ? ORDER BY id DESC`——
/// 排序与游标条件都落在 id 上，需要 (key_id, id) 才能免排序、免全表扫。
const USAGE_KEY_ID_INDEX: &str =
    "CREATE INDEX IF NOT EXISTS idx_usage_key_id ON usage_records(key_id, id)";

/// 管理端日志的游标查询只有 `id < ?` + 各筛选列，id 序扫描是唯一免排序路径。
const PROXY_LOGS_ID_INDEX: &str =
    "CREATE INDEX IF NOT EXISTS idx_proxy_logs_id ON proxy_logs(id)";

/// 首页「最近几条错误」是 `WHERE status = 'error' ORDER BY id DESC LIMIT 3`。
/// 没有这个索引就得倒序扫全表直到凑够三条——错误稀疏时等于每次刷新全表扫，
/// 正是首页慢的原因之一。
const PROXY_LOGS_STATUS_ID_INDEX: &str =
    "CREATE INDEX IF NOT EXISTS idx_proxy_logs_status_id ON proxy_logs(status, id)";

#[cfg(test)]
mod tests {
    use super::*;

    /// 老版 usage_records 的建表语句：当前 DDL 去掉 block_id 那一行。
    ///
    /// 用字面量是为了过 sqlx 0.9 的动态 SQL 检查；下面另有一致性断言
    /// 把它钉在 `USAGE_RECORDS_DDL` 上，避免抄一份之后各自腐坏
    /// （那样测的就不是「老库能不能被补上」了）。
    const LEGACY_USAGE_RECORDS_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS usage_records (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    key_id            TEXT    NOT NULL REFERENCES api_keys(id) ON DELETE CASCADE,
    model             TEXT    NOT NULL,
    raw_tokens        INTEGER NOT NULL DEFAULT 0,
    prompt_tokens     INTEGER NOT NULL DEFAULT 0,
    completion_tokens INTEGER NOT NULL DEFAULT 0,
    cache_tokens      INTEGER NOT NULL DEFAULT 0,
    estimated         INTEGER NOT NULL DEFAULT 0,
    stream            INTEGER NOT NULL DEFAULT 0,
    multiplier_bp     INTEGER NOT NULL DEFAULT 10000,
    final_tokens      INTEGER NOT NULL DEFAULT 0,
    credits_scaled    INTEGER NOT NULL DEFAULT 0,
    status            TEXT    NOT NULL DEFAULT 'success',
    error             TEXT,
    created_at        INTEGER NOT NULL
)
"#;

    /// 老版 upstream_blocks 的建表语句：当前 DDL 去掉 headers 与 max_concurrency 两行。
    ///
    /// 同样用字面量，并在下面钉住与 `UPSTREAM_BLOCKS_DDL` 的一致性。
    const LEGACY_UPSTREAM_BLOCKS_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS upstream_blocks (
    id           TEXT    PRIMARY KEY,
    name         TEXT    NOT NULL DEFAULT '',
    api_format   TEXT    NOT NULL,
    weight       INTEGER NOT NULL DEFAULT 1,
    base_url     TEXT    NOT NULL,
    proxy        TEXT,
    models       TEXT    NOT NULL DEFAULT '[]',
    enabled      INTEGER NOT NULL DEFAULT 1,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL
)
"#;

    /// 老版 upstream_keys 的建表语句：当前 DDL 去掉 never_disable 那一行。
    const LEGACY_UPSTREAM_KEYS_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS upstream_keys (
    id             TEXT    PRIMARY KEY,
    block_id       TEXT    NOT NULL REFERENCES upstream_blocks(id) ON DELETE CASCADE,
    secret         TEXT    NOT NULL,
    status         TEXT    NOT NULL DEFAULT 'active',
    weight         INTEGER NOT NULL DEFAULT 1,
    failure_count  INTEGER NOT NULL DEFAULT 0,
    cooldown_until INTEGER,
    last_error     TEXT,
    last_used_at   INTEGER,
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL,
    UNIQUE(block_id, secret)
)
"#;

    /// 造一张「老版」数据面的库：usage_records 没有 block_id，
    /// upstream_blocks 没有 headers 与 max_concurrency，upstream_keys 没有 never_disable。
    ///
    /// 三张表都要建——补列是照着真实表名跑的，老库里本来就都有它们；
    /// 只建一张测的就不是「老库能不能被补上」，而是「表不存在时会怎样」。
    async fn legacy_pool() -> SqlitePool {
        assert_eq!(
            LEGACY_USAGE_RECORDS_DDL,
            USAGE_RECORDS_DDL.replace("    block_id          TEXT,\n", ""),
            "老 DDL 与「当前 DDL 去掉 block_id」已经不一致，测试夹具该更新了"
        );
        assert_eq!(
            LEGACY_UPSTREAM_BLOCKS_DDL,
            UPSTREAM_BLOCKS_DDL
                .replace("    headers      TEXT    NOT NULL DEFAULT '[]',\n", "")
                .replace("    max_concurrency INTEGER NOT NULL DEFAULT 0,\n", ""),
            "老 DDL 与「当前 DDL 去掉 headers 与 max_concurrency」已经不一致，测试夹具该更新了"
        );
        assert_eq!(
            LEGACY_UPSTREAM_KEYS_DDL,
            UPSTREAM_KEYS_DDL.replace("    never_disable  INTEGER NOT NULL DEFAULT 0,\n", ""),
            "老 DDL 与「当前 DDL 去掉 never_disable」已经不一致，测试夹具该更新了"
        );

        let pool = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("打开内存库");
        for ddl in [
            LEGACY_USAGE_RECORDS_DDL,
            LEGACY_UPSTREAM_BLOCKS_DDL,
            LEGACY_UPSTREAM_KEYS_DDL,
        ] {
            sqlx::raw_sql(ddl).execute(&pool).await.expect("建老表");
        }
        pool
    }

    async fn columns_of(pool: &SqlitePool) -> Vec<String> {
        sqlx::query_scalar(USAGE_RECORDS_COLUMNS)
            .fetch_all(pool)
            .await
            .expect("读列信息")
    }

    #[tokio::test]
    async fn adds_block_id_to_a_legacy_usage_table() {
        let pool = legacy_pool().await;
        assert!(
            !columns_of(&pool).await.iter().any(|c| c == "block_id"),
            "老表本来就不该有 block_id"
        );

        add_missing_columns(&pool).await.expect("补列");
        assert!(
            columns_of(&pool).await.iter().any(|c| c == "block_id"),
            "补列之后应当有 block_id"
        );

        // 可重入：再跑一遍直接跳过，不该报错。
        add_missing_columns(&pool).await.expect("第二次应当跳过");
    }

    #[tokio::test]
    async fn adds_headers_to_a_legacy_block_table() {
        let pool = legacy_pool().await;
        let columns: Vec<String> = sqlx::query_scalar(UPSTREAM_BLOCKS_COLUMNS)
            .fetch_all(&pool)
            .await
            .expect("读列信息");
        assert!(
            !columns.iter().any(|c| c == "headers"),
            "老表本来就不该有 headers"
        );

        // 老库里先有一行块——补列之后它必须读到「没有自定义头」，而不是报错或 NULL。
        sqlx::query(
            "INSERT INTO upstream_blocks (id, api_format, base_url, created_at, updated_at) \
             VALUES ('block-a', 'openai', 'https://demo-upstream.invalid', 1, 1)",
        )
        .execute(&pool)
        .await
        .expect("写入老块");

        add_missing_columns(&pool).await.expect("补列");

        let headers: String = sqlx::query_scalar("SELECT headers FROM upstream_blocks")
            .fetch_one(&pool)
            .await
            .expect("新列可读");
        assert_eq!(headers, "[]", "老块的自定义头应当补成空数组，而不是 NULL");

        // 可重入。
        add_missing_columns(&pool).await.expect("第二次应当跳过");
    }

    #[tokio::test]
    async fn adds_never_disable_to_a_legacy_key_table() {
        let pool = legacy_pool().await;
        let columns: Vec<String> = sqlx::query_scalar(UPSTREAM_KEYS_COLUMNS)
            .fetch_all(&pool)
            .await
            .expect("读列信息");
        assert!(
            !columns.iter().any(|c| c == "never_disable"),
            "老表本来就不该有 never_disable"
        );

        // 老库里先有块、再有密钥——补列之后它必须读到「未开启豁免」，而不是报错。
        sqlx::query(
            "INSERT INTO upstream_blocks (id, api_format, base_url, created_at, updated_at) \
             VALUES ('block-a', 'openai', 'https://demo-upstream.invalid', 1, 1)",
        )
        .execute(&pool)
        .await
        .expect("写入老块");
        sqlx::query(
            "INSERT INTO upstream_keys (id, block_id, secret, created_at, updated_at) \
             VALUES ('key-1', 'block-a', 'sk-demo', 1, 1)",
        )
        .execute(&pool)
        .await
        .expect("写入老密钥");

        add_missing_columns(&pool).await.expect("补列");

        let flag: i64 = sqlx::query_scalar("SELECT never_disable FROM upstream_keys")
            .fetch_one(&pool)
            .await
            .expect("新列可读");
        assert_eq!(flag, 0, "老密钥的永不暂停开关应当补成关闭");

        // 可重入。
        add_missing_columns(&pool).await.expect("第二次应当跳过");
    }

    #[tokio::test]
    async fn adds_max_concurrency_to_a_legacy_block_table() {
        let pool = legacy_pool().await;
        let columns: Vec<String> = sqlx::query_scalar(UPSTREAM_BLOCKS_COLUMNS)
            .fetch_all(&pool)
            .await
            .expect("读列信息");
        assert!(
            !columns.iter().any(|c| c == "max_concurrency"),
            "老表本来就不该有 max_concurrency"
        );

        sqlx::query(
            "INSERT INTO upstream_blocks (id, api_format, base_url, created_at, updated_at) \
             VALUES ('block-a', 'openai', 'https://demo-upstream.invalid', 1, 1)",
        )
        .execute(&pool)
        .await
        .expect("写入老块");

        add_missing_columns(&pool).await.expect("补列");

        let cap: i64 = sqlx::query_scalar("SELECT max_concurrency FROM upstream_blocks")
            .fetch_one(&pool)
            .await
            .expect("新列可读");
        assert_eq!(cap, 0, "老块的并发上限应当补成 0（不限制）");

        add_missing_columns(&pool).await.expect("第二次应当跳过");
    }

    #[tokio::test]
    async fn migrate_backfills_block_id_and_keeps_existing_rows() {
        let pool = legacy_pool().await;
        // usage_records 上有指向 api_keys 的外键，父表与其行都得先在。
        sqlx::raw_sql(API_KEYS_DDL)
            .execute(&pool)
            .await
            .expect("建父表");
        sqlx::query(
            "INSERT INTO api_keys (id, secret, key_prefix, credits_scaled, created_at, updated_at) \
             VALUES ('k1', 'ak_test', 'ak_test', 0, 1, 1)",
        )
        .execute(&pool)
        .await
        .expect("写入父行");
        sqlx::query("INSERT INTO usage_records (key_id, model, created_at) VALUES ('k1', 'gpt-4o', 1)")
            .execute(&pool)
            .await
            .expect("写入一条老记录");

        migrate_data(&pool).await.expect("全量迁移");
        migrate_data(&pool).await.expect("再跑一遍应当幂等");

        // 老记录原样保留，新列降级为 NULL（「不知道走的哪个上游」）。
        let model: String = sqlx::query_scalar("SELECT model FROM usage_records")
            .fetch_one(&pool)
            .await
            .expect("老记录还在");
        let block: Option<String> = sqlx::query_scalar("SELECT block_id FROM usage_records")
            .fetch_one(&pool)
            .await
            .expect("新列可读");
        assert_eq!(model, "gpt-4o");
        assert_eq!(block, None, "旧记录没有块信息，必须是 NULL 而不是空串");
    }
}
