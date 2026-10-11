//! 数据层：SQLite 连接、建表与各表的读写入口。
//!
//! 这是所有业务模块的底层。上层只通过本模块访问数据库，不直接持有 SQL。
//!
//! # 分区
//!
//! 连接池按数据归属分成三个域：
//!
//! | 域 | 内容 | 状态 |
//! | --- | --- | --- |
//! | [`Store::control`] | 网关自身：账号、会话、设置 | 已实现 |
//! | [`Store::data`] | 业务数据：下游用户与密钥、上游配置 | 待对应模块落地 |
//! | [`Store::logs`] | 请求日志 | 待对应模块落地 |
//!
//! 三个域**默认共用同一个文件与连接池**。等业务表落地、写竞争变得明显时，
//! 让某个域指向独立文件即可物理拆开——业务代码只认域，不认文件，届时无需改动
//! 上层逻辑。这里刻意不预先设计业务表结构，避免在没有真实需求时猜错。
//!
//! # 时间
//!
//! 所有时间字段都是秒级 Unix 时间戳（INTEGER），一律 UTC，
//! 不做本地时区换算——混用会直接导致过期判断错乱。

pub mod account;
pub mod session;
pub mod settings;

use std::path::Path;

use anyhow::Context;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::SqlitePool;

/// 三个域的连接池。
///
/// 各域相互独立，可以指向不同文件，也可以（默认）共用同一个。
#[derive(Clone)]
pub struct Store {
    /// 控制面：账号、会话、设置等网关自身的数据。
    pub control: SqlitePool,
    /// 数据面：下游用户与密钥、上游配置等业务数据。
    pub data: SqlitePool,
    /// 日志面：请求日志。
    pub logs: SqlitePool,
}

impl Store {
    /// 打开单文件存储：三个域共用同一个连接池。
    ///
    /// 需要拆分时，对目标域单独 [`open_pool`] 开一个池替换即可。
    pub async fn connect(path: &Path) -> anyhow::Result<Self> {
        let pool = open_pool(path).await?;

        Ok(Self {
            control: pool.clone(),
            data: pool.clone(),
            logs: pool,
        })
    }

    /// 建表。幂等，每次启动都可安全执行。
    pub async fn migrate(&self) -> anyhow::Result<()> {
        self.migrate_control().await?;
        // 数据面与日志面的表结构在对应模块落地时补充：
        // self.migrate_data().await?;
        // self.migrate_logs().await?;
        Ok(())
    }

    /// 控制面表结构。
    async fn migrate_control(&self) -> anyhow::Result<()> {
        for (name, ddl) in [
            ("accounts", ACCOUNTS_DDL),
            ("sessions", SESSIONS_DDL),
            ("settings", SETTINGS_DDL),
            ("idx_sessions_account", SESSIONS_ACCOUNT_INDEX),
            ("drop idx_sessions_token", DROP_REDUNDANT_TOKEN_INDEX),
        ] {
            sqlx::query(ddl)
                .execute(&self.control)
                .await
                .with_context(|| format!("执行建表语句失败: {name}"))?;
        }

        Ok(())
    }
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

/// 早期版本建的显式索引。`token_hash` 上的唯一约束已经提供了索引，
/// 这个属于重复，清掉以免白占写入成本。
const DROP_REDUNDANT_TOKEN_INDEX: &str = "DROP INDEX IF EXISTS idx_sessions_token";
