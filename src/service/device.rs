//! 登录设备（会话）管理。
//!
//! 只涉及仍然有效的会话：退役的记录在退出时就已经删掉了。

use sqlx::SqlitePool;

use crate::db;
use crate::error::{Error, Result};
use crate::model::SessionView;
use crate::utils::time;

/// 列出全部在线设备，当前设备会被标记出来。
pub async fn list(
    pool: &SqlitePool,
    account_id: i64,
    current_session_id: &str,
) -> Result<Vec<SessionView>> {
    let sessions = db::session::list_by_account(pool, account_id, time::now_secs()).await?;

    Ok(sessions
        .iter()
        .map(|session| SessionView::new(session, current_session_id))
        .collect())
}

/// 移出一台设备。
///
/// 记录直接删除而不是打标记，数据库里不留残余。因此重复移出同一台会得到
/// 「不存在」——它确实已经不在了，这个答案是对的。
pub async fn revoke(pool: &SqlitePool, account_id: i64, session_id: &str) -> Result<()> {
    if !db::session::delete(pool, session_id, account_id).await? {
        return Err(Error::NotFound("该登录设备不存在".to_string()));
    }

    Ok(())
}

/// 移出除本机以外的全部设备，返回被移出的数量。
pub async fn revoke_others(
    pool: &SqlitePool,
    account_id: i64,
    current_session_id: &str,
) -> Result<u64> {
    Ok(db::session::delete_others(pool, account_id, current_session_id).await?)
}
