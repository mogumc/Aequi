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
pub mod keys;
pub mod models;
pub mod session;
pub mod settings;
pub mod usage;

use std::path::Path;

use anyhow::Context;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::SqlitePool;

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

/// 建数据面表结构。
pub async fn migrate_data(pool: &SqlitePool) -> anyhow::Result<()> {
    for (name, ddl) in [
        ("api_keys", API_KEYS_DDL),
        ("usage_records", USAGE_RECORDS_DDL),
        ("models", MODELS_DDL),
        ("idx_usage_key_time", USAGE_KEY_TIME_INDEX),
    ] {
        sqlx::query(ddl)
            .execute(pool)
            .await
            .with_context(|| format!("执行数据面建表语句失败: {name}"))?;
    }

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
const SESSIONS_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS sessions (
    id           TEXT    PRIMARY KEY,
    account_id   INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    token_hash   TEXT    NOT NULL UNIQUE,
    login_type   TEXT    NOT NULL,
    ip           TEXT,
    user_agent   TEXT,
    created_at   INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    expires_at   INTEGER NOT NULL,
    revoked_at   INTEGER
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
const USAGE_RECORDS_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS usage_records (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    key_id         TEXT    NOT NULL REFERENCES api_keys(id) ON DELETE CASCADE,
    model          TEXT    NOT NULL,
    raw_tokens     INTEGER NOT NULL,
    multiplier_bp  INTEGER NOT NULL,
    final_tokens   INTEGER NOT NULL,
    credits_scaled INTEGER NOT NULL,
    created_at     INTEGER NOT NULL
)
"#;

/// 模型配置。
///
/// 本轮只建表预留：倍率参与计费，但模型的增删改查属于模型管理模块，尚未实现。
const MODELS_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS models (
    name          TEXT    PRIMARY KEY,
    multiplier_bp INTEGER NOT NULL DEFAULT 10000,
    enabled       INTEGER NOT NULL DEFAULT 1,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
)
"#;

const USAGE_KEY_TIME_INDEX: &str =
    "CREATE INDEX IF NOT EXISTS idx_usage_key_time ON usage_records(key_id, created_at DESC)";
