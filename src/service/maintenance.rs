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
///
/// 会话在退出时就已经删掉了，这里只负责回收**自然过期**的记录，
/// 所以频率不需要高——每次都要拿写锁。
const PURGE_INTERVAL: Duration = Duration::from_secs(3600);

/// 启动后台清理任务。
///
/// 定时器的首次 tick 会立即触发，相当于启动时先清一遍。
pub fn spawn(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(PURGE_INTERVAL);

        loop {
            ticker.tick().await;

            let now = time::now_secs();

            match db::session::purge(&state.store.control, now).await {
                Ok(0) => tracing::debug!("会话表无需清理"),
                Ok(removed) => tracing::info!(removed, "已回收过期的会话"),
                Err(error) => tracing::warn!(error = %error, "清理会话表失败"),
            }

            // 日志保留期来自数据库设置，改设置立即影响下一轮清理。
            let retention = state.settings().log_retention_days.clamp(1, 365);
            let cutoff = now - retention * 86_400;

            match db::proxy_log::purge_before(&state.store.data, cutoff).await {
                Ok(0) => tracing::debug!("代理日志无需清理"),
                Ok(removed) => tracing::info!(removed, retention, "已回收过期的代理日志"),
                Err(error) => tracing::warn!(error = %error, "清理代理日志失败"),
            }

            match db::usage::purge_before(&state.store.data, cutoff).await {
                Ok(0) => tracing::debug!("用量记录无需清理"),
                Ok(removed) => tracing::info!(removed, retention, "已回收过期的用量记录"),
                Err(error) => tracing::warn!(error = %error, "清理用量记录失败"),
            }
        }
    });
}
