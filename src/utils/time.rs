//! 时间工具。

use std::time::{SystemTime, UNIX_EPOCH};

/// 当前 Unix 时间戳（秒）。
///
/// 数据库统一以秒级时间戳存储，避免时区与格式转换带来的歧义。
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}
