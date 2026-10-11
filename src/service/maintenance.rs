//! 后台维护任务。
//!
//! 清理类操作都要拿 SQLite 的写锁，既不能放在请求路径上，也不能跑得太频繁，
//! 所以统一收在这里，由低频定时任务驱动。

use std::sync::Arc;
use std::time::Duration;

use crate::db;
use crate::utils::time;
use crate::AppState;

/// 清理任务的执行间隔。
const PURGE_INTERVAL: Duration = Duration::from_secs(3600);

/// 已撤销会话的保留时长。
///
/// 不立即删除，是为了让管理后台还能看到「最近退出的设备」；
/// 过了这段时间再清掉，避免表被历史记录无限撑大。
const REVOKED_RETENTION_SECONDS: i64 = 30 * 24 * 3600;

/// 启动后台清理任务。
///
/// 定时器的首次 tick 会立即触发，相当于启动时先清一遍。
pub fn spawn(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(PURGE_INTERVAL);

        loop {
            ticker.tick().await;

            let now = time::now_secs();
            let revoked_before = now - REVOKED_RETENTION_SECONDS;

            match db::session::purge(&state.store.control, now, revoked_before).await {
                Ok(0) => tracing::debug!("会话表无需清理"),
                Ok(removed) => tracing::info!(removed, "已清理失效的会话记录"),
                Err(error) => tracing::warn!(error = %error, "清理会话表失败"),
            }
        }
    });
}
