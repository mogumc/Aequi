//! `sessions` 表的读写。
//!
//! 一条记录对应一次登录，也就是一台登录设备。会话之间互不干扰。
//!
//! # 契约
//!
//! - [`find_active_by_token`] 只返回**仍然有效**的会话，有效性条件写在 SQL 里，
//!   不依赖上层记得再校验一遍。
//! - [`find_by_id`] 与 [`list_by_account`] 返回**原始记录**，包含已撤销与已过期项，
//!   供管理接口展示历史设备；调用方需自行按 `is_active()` 判断状态。
//! - [`touch`] 与 [`revoke`] 都带有效性条件，已失效的会话不会被改动。

use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

use crate::model::{NewSession, Session};

/// 按令牌散列查找仍然有效的会话。
const SQL_FIND_ACTIVE_BY_TOKEN: &str = "\
SELECT id, account_id, token_hash, login_type, ip, user_agent, \
       created_at, last_seen_at, expires_at, revoked_at \
FROM sessions \
WHERE token_hash = ? AND revoked_at IS NULL AND expires_at > ?";

/// 按会话 ID 查找（不过滤状态）。
const SQL_FIND_BY_ID: &str = "\
SELECT id, account_id, token_hash, login_type, ip, user_agent, \
       created_at, last_seen_at, expires_at, revoked_at \
FROM sessions WHERE id = ?";

/// 列出某账号的全部会话，最近登录的排在前面。
const SQL_LIST_BY_ACCOUNT: &str = "\
SELECT id, account_id, token_hash, login_type, ip, user_agent, \
       created_at, last_seen_at, expires_at, revoked_at \
FROM sessions WHERE account_id = ? ORDER BY created_at DESC";

/// 刷新最近使用时间，仅限仍然有效的会话。
const SQL_TOUCH: &str = "\
UPDATE sessions SET last_seen_at = ? \
WHERE id = ? AND revoked_at IS NULL AND expires_at > ?";

fn map_row(row: &SqliteRow) -> Session {
    Session {
        id: row.get("id"),
        account_id: row.get("account_id"),
        // token_hash 随行返回，内部使用；对外视图里不包含。
        token_hash: row.get("token_hash"),
        login_type: row.get("login_type"),
        ip: row.get("ip"),
        user_agent: row.get("user_agent"),
        created_at: row.get("created_at"),
        last_seen_at: row.get("last_seen_at"),
        expires_at: row.get("expires_at"),
        revoked_at: row.get("revoked_at"),
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
         (id, account_id, token_hash, login_type, ip, user_agent, created_at, last_seen_at, expires_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(session.id)
    .bind(session.account_id)
    .bind(session.token_hash)
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
/// 已撤销或已过期的会话查不出来，直接返回 `None`——这样上层不会因为漏了
/// 一次状态判断就把失效会话放行。
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

/// 按会话 ID 查找，返回原始记录（可能是已撤销或已过期的）。
pub async fn find_by_id(pool: &SqlitePool, id: &str) -> Result<Option<Session>, sqlx::Error> {
    let row = sqlx::query(SQL_FIND_BY_ID)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(map_row))
}

/// 列出某账号的全部会话，**包含已撤销与已过期**。
///
/// 供管理接口展示登录历史；需要只看在线设备时由调用方按 `is_active()` 过滤。
pub async fn list_by_account(
    pool: &SqlitePool,
    account_id: i64,
) -> Result<Vec<Session>, sqlx::Error> {
    let rows = sqlx::query(SQL_LIST_BY_ACCOUNT)
        .bind(account_id)
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(map_row).collect())
}

/// 刷新最近使用时间。
///
/// 已撤销或已过期的会话不会被改写，此时返回 `false`，
/// 调用方应当把这个请求当作未认证处理——否则拿一个失效会话也能一直「保活」。
pub async fn touch(pool: &SqlitePool, id: &str, now: i64) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(SQL_TOUCH)
        .bind(now)
        .bind(id)
        .bind(now)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// 撤销指定会话，返回是否确实撤销了一条记录。
///
/// `revoked_at IS NULL` 让这次更新自带原子性：并发重复撤销只会有一条生效，
/// 不需要额外的事务或乐观锁。已经撤销过再调用返回 `false`。
pub async fn revoke(
    pool: &SqlitePool,
    id: &str,
    account_id: i64,
    now: i64,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE sessions SET revoked_at = ? \
         WHERE id = ? AND account_id = ? AND revoked_at IS NULL",
    )
    .bind(now)
    .bind(id)
    .bind(account_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// 撤销除当前会话外的全部会话，返回被撤销的数量。
///
/// 同样是单条带条件的 UPDATE，并发调用安全。
pub async fn revoke_others(
    pool: &SqlitePool,
    account_id: i64,
    keep_id: &str,
    now: i64,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE sessions SET revoked_at = ? \
         WHERE account_id = ? AND id <> ? AND revoked_at IS NULL",
    )
    .bind(now)
    .bind(account_id)
    .bind(keep_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// 清理会话表，返回删除的行数。
///
/// 两类记录会被删除：
/// - 已过期的会话；
/// - 已撤销且撤销时间早于 `revoked_before` 的会话——否则手动退出的设备会永远留在表里。
///
/// 清理要拿写锁，由低频后台任务调用，不要放进请求路径。
pub async fn purge(
    pool: &SqlitePool,
    now: i64,
    revoked_before: i64,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "DELETE FROM sessions \
         WHERE expires_at <= ? OR (revoked_at IS NOT NULL AND revoked_at < ?)",
    )
    .bind(now)
    .bind(revoked_before)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}
