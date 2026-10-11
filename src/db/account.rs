//! `accounts` 表的读写。

use sqlx::sqlite::SqliteRow;
use sqlx::{Executor, Row, Sqlite, SqlitePool};

use crate::model::Account;

/// 按用户名查找。仅返回一行。
const SQL_FIND_BY_USERNAME: &str = "\
SELECT id, username, password_hash, totp_enabled, totp_secret, created_at, updated_at \
FROM accounts WHERE username = ?";

/// 按主键查找。
const SQL_FIND_BY_ID: &str = "\
SELECT id, username, password_hash, totp_enabled, totp_secret, created_at, updated_at \
FROM accounts WHERE id = ?";

/// 取第一个账号。系统只维护一个账号。
const SQL_FIND_FIRST: &str = "\
SELECT id, username, password_hash, totp_enabled, totp_secret, created_at, updated_at \
FROM accounts ORDER BY id LIMIT 1";

/// 创建账号并回读整行。
const SQL_INSERT: &str = "\
INSERT INTO accounts (username, password_hash, totp_enabled, created_at, updated_at) \
VALUES (?, ?, 0, ?, ?) \
RETURNING id, username, password_hash, totp_enabled, totp_secret, created_at, updated_at";

fn map_row(row: &SqliteRow) -> Account {
    Account {
        id: row.get("id"),
        username: row.get("username"),
        password_hash: row.get("password_hash"),
        totp_enabled: row.get::<i64, _>("totp_enabled") != 0,
        totp_secret: row.get("totp_secret"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

/// 账号总数，用于判断是否需要做首次初始化。
pub async fn count(pool: &SqlitePool) -> Result<i64, sqlx::Error> {
    let row = sqlx::query("SELECT COUNT(*) AS n FROM accounts")
        .fetch_one(pool)
        .await?;
    row.try_get("n")
}

/// 按用户名查找。
pub async fn find_by_username(
    pool: &SqlitePool,
    username: &str,
) -> Result<Option<Account>, sqlx::Error> {
    let row = sqlx::query(SQL_FIND_BY_USERNAME)
        .bind(username)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(map_row))
}

/// 按主键查找。
pub async fn find_by_id(pool: &SqlitePool, id: i64) -> Result<Option<Account>, sqlx::Error> {
    let row = sqlx::query(SQL_FIND_BY_ID)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(map_row))
}

/// 取第一个账号。系统只维护一个账号，用于 CLI 这类拿不到账号 ID 的场景。
pub async fn first(pool: &SqlitePool) -> Result<Option<Account>, sqlx::Error> {
    let row = sqlx::query(SQL_FIND_FIRST).fetch_optional(pool).await?;
    Ok(row.as_ref().map(map_row))
}

/// 创建账号。
pub async fn insert(
    pool: &SqlitePool,
    username: &str,
    password_hash: &str,
    now: i64,
) -> Result<Account, sqlx::Error> {
    let row = sqlx::query(SQL_INSERT)
        .bind(username)
        .bind(password_hash)
        .bind(now)
        .bind(now)
        .fetch_one(pool)
        .await?;
    Ok(map_row(&row))
}

/// 更新用户名。
///
/// 形状与 [`update_password`] 保持一致，需要时可以和别的写操作组成事务。
pub async fn update_username<'e, E>(
    executor: E,
    id: i64,
    username: &str,
    now: i64,
) -> Result<(), sqlx::Error>
where
    E: Executor<'e, Database = Sqlite>,
{
    sqlx::query("UPDATE accounts SET username = ?, updated_at = ? WHERE id = ?")
        .bind(username)
        .bind(now)
        .bind(id)
        .execute(executor)
        .await?;
    Ok(())
}

/// 更新密码哈希。
///
/// 接受任意 Executor，便于和「撤销其它会话」放进同一个事务：
/// 改密码跟踢设备必须一起生效，否则用户以为踢干净了其实没有。
pub async fn update_password<'e, E>(
    executor: E,
    id: i64,
    password_hash: &str,
    now: i64,
) -> Result<(), sqlx::Error>
where
    E: Executor<'e, Database = Sqlite>,
{
    sqlx::query("UPDATE accounts SET password_hash = ?, updated_at = ? WHERE id = ?")
        .bind(password_hash)
        .bind(now)
        .bind(id)
        .execute(executor)
        .await?;
    Ok(())
}

/// 更新 TOTP 状态。
///
/// `enabled = false` 且 `secret = None` 表示彻底关闭并清除密钥。
pub async fn set_totp(
    pool: &SqlitePool,
    id: i64,
    enabled: bool,
    secret: Option<&str>,
    now: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE accounts SET totp_enabled = ?, totp_secret = ?, updated_at = ? WHERE id = ?")
        .bind(i64::from(enabled))
        .bind(secret)
        .bind(now)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}
