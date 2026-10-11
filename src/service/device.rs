//! 登录设备（会话）管理。

use sqlx::SqlitePool;

use crate::db;
use crate::error::{Error, Result};
use crate::model::SessionView;
use crate::utils::time;

/// 列出全部登录设备，当前设备会被标记出来。
pub async fn list(
    pool: &SqlitePool,
    account_id: i64,
    current_session_id: &str,
) -> Result<Vec<SessionView>> {
    let now = time::now_secs();
    let sessions = db::session::list_by_account(pool, account_id).await?;

    Ok(sessions
        .iter()
        .map(|session| SessionView::new(session, current_session_id, now))
        .collect())
}

/// 撤销指定设备。
///
/// 撤销自己的会话等同于退出登录，不做特殊限制。
pub async fn revoke(pool: &SqlitePool, account_id: i64, session_id: &str) -> Result<()> {
    let revoked = db::session::revoke(pool, session_id, account_id, time::now_secs()).await?;

    if !revoked {
        // 可能是 ID 不存在，也可能已经撤销过。后者不该算失败。
        let existing = db::session::find_by_id(pool, session_id).await?;
        return match existing {
            Some(session) if session.account_id == account_id => Ok(()),
            _ => Err(Error::NotFound("该登录设备不存在".to_string())),
        };
    }

    Ok(())
}

/// 撤销除当前设备外的全部设备，返回被撤销的数量。
pub async fn revoke_others(
    pool: &SqlitePool,
    account_id: i64,
    current_session_id: &str,
) -> Result<u64> {
    Ok(db::session::revoke_others(pool, account_id, current_session_id, time::now_secs()).await?)
}
