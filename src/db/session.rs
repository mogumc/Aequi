//! `sessions` 表的读写。
//!
//! 一条记录对应一次登录，也就是一台登录设备。会话之间互不干扰。
//!
//! # 契约
//!
//! 表里**只留仍然有效的会话**——退出即删除，过期由后台任务回收，
//! 不保留退役记录。因此这里的查询都带上 `expires_at > now`。
//!
//! [`find_active_by_token`] 供鉴权使用，只认散列；
//! `token_prefix` 只是给后台辨认设备用的展示字段，不参与任何校验。

use sqlx::sqlite::SqliteRow;
use sqlx::{Executor, Row, Sqlite, SqlitePool};

use crate::model::{NewSession, Session};

/// 按令牌散列查找仍然有效的会话。
const SQL_FIND_ACTIVE_BY_TOKEN: &str = "\
SELECT id, account_id, token_hash, token_prefix, login_type, ip, user_agent, \
       created_at, last_seen_at, expires_at \
FROM sessions WHERE token_hash = ? AND expires_at > ?";

/// 按会话 ID 查找。
const SQL_FIND_BY_ID: &str = "\
SELECT id, account_id, token_hash, token_prefix, login_type, ip, user_agent, \
       created_at, last_seen_at, expires_at \
FROM sessions WHERE id = ?";

/// 列出某账号仍然有效的会话，最近登录的排在前面。
const SQL_LIST_BY_ACCOUNT: &str = "\
SELECT id, account_id, token_hash, token_prefix, login_type, ip, user_agent, \
       created_at, last_seen_at, expires_at \
FROM sessions WHERE account_id = ? AND expires_at > ? ORDER BY created_at DESC";

/// 刷新最近使用时间，仅限仍然有效的会话。
const SQL_TOUCH: &str = "\
UPDATE sessions SET last_seen_at = ? WHERE id = ? AND expires_at > ?";

fn map_row(row: &SqliteRow) -> Session {
    Session {
        id: row.get("id"),
        account_id: row.get("account_id"),
        token_hash: row.get("token_hash"),
        token_prefix: row.get("token_prefix"),
        login_type: row.get("login_type"),
        ip: row.get("ip"),
        user_agent: row.get("user_agent"),
        created_at: row.get("created_at"),
        last_seen_at: row.get("last_seen_at"),
        expires_at: row.get("expires_at"),
    }
}

/// 写入一条新会话。
///
/// `id` 与 `token_hash` 都是随机值且有唯一约束。碰撞概率极低，但真撞上时
/// 会返回唯一约束冲突，调用方可用 [`crate::db::is_unique_violation`] 识别并重试，
/// 不要把它当成普通故障。
pub async fn insert(pool: &SqlitePool, session: &NewSession<'_>) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO sessions \
         (id, account_id, token_hash, token_prefix, login_type, ip, user_agent, created_at, last_seen_at, expires_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(session.id)
    .bind(session.account_id)
    .bind(session.token_hash)
    .bind(session.token_prefix)
    .bind(session.login_type)
    .bind(session.ip)
    .bind(session.user_agent)
    .bind(session.created_at)
    .bind(session.created_at)
    .bind(session.expires_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// 按令牌散列查找**仍然有效**的会话。
///
/// 已过期的会话查不出来，直接返回 `None`——这样上层不会因为漏了一次
/// 状态判断就把失效会话放行。
pub async fn find_active_by_token(
    pool: &SqlitePool,
    token_hash: &str,
    now: i64,
) -> Result<Option<Session>, sqlx::Error> {
    let row = sqlx::query(SQL_FIND_ACTIVE_BY_TOKEN)
        .bind(token_hash)
        .bind(now)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(map_row))
}

/// 按会话 ID 查找。
pub async fn find_by_id(pool: &SqlitePool, id: &str) -> Result<Option<Session>, sqlx::Error> {
    let row = sqlx::query(SQL_FIND_BY_ID)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(map_row))
}

/// 列出某账号**仍然有效**的会话。
pub async fn list_by_account(
    pool: &SqlitePool,
    account_id: i64,
    now: i64,
) -> Result<Vec<Session>, sqlx::Error> {
    let rows = sqlx::query(SQL_LIST_BY_ACCOUNT)
        .bind(account_id)
        .bind(now)
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(map_row).collect())
}

/// 刷新最近使用时间。
///
/// 已过期的会话不会被改写，此时返回 `false`，
/// 调用方应当把这个请求当作未认证处理。
pub async fn touch(pool: &SqlitePool, id: &str, now: i64) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(SQL_TOUCH)
        .bind(now)
        .bind(id)
        .bind(now)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// 撤销指定会话——直接删除，不留退役记录。
///
/// 返回是否确实删掉了一行。重复撤销返回 `false`，由调用方决定算不算失败。
pub async fn delete<'e, E>(
    executor: E,
    id: &str,
    account_id: i64,
) -> Result<bool, sqlx::Error>
where
    E: Executor<'e, Database = Sqlite>,
{
    let result = sqlx::query("DELETE FROM sessions WHERE id = ? AND account_id = ?")
        .bind(id)
        .bind(account_id)
        .execute(executor)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// 清掉除指定会话外的全部会话，用于「退出其他设备」和改密码后的收尾。
pub async fn delete_others<'e, E>(
    executor: E,
    account_id: i64,
    keep_id: &str,
) -> Result<u64, sqlx::Error>
where
    E: Executor<'e, Database = Sqlite>,
{
    let result = sqlx::query("DELETE FROM sessions WHERE account_id = ? AND id <> ?")
        .bind(account_id)
        .bind(keep_id)
        .execute(executor)
        .await?;
    Ok(result.rows_affected())
}

/// 回收已过期的会话，返回删除的行数。
///
/// 清理要拿写锁，由低频后台任务调用，不要放进请求路径。
pub async fn purge(pool: &SqlitePool, now: i64) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM sessions WHERE expires_at <= ?")
        .bind(now)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}
