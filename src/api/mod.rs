//! HTTP 接口层。

pub mod auth;
pub mod blocks;
pub mod device;
pub mod key_auth;
pub mod key_portal;
pub mod keys;
pub mod logs;
pub mod middleware;
pub mod otp;
pub mod proxy_api;
mod router;
pub mod settings;
pub mod stats;

use serde::{Deserialize, Serialize};

pub use router::router;

use crate::model::{UsageSummary, UsageView};

/// 通用消息响应。
#[derive(Debug, Serialize)]
pub struct MessageResponse {
    pub message: String,
}

impl MessageResponse {
    pub fn ok(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// 用量查询参数。
#[derive(Debug, Deserialize)]
pub struct UsageQuery {
    #[serde(default = "default_usage_limit")]
    pub limit: i64,
    /// 游标分页：上一页最后一条记录的 id；0 或缺省表示从最新开始。
    #[serde(default)]
    pub before_id: i64,
}

fn default_usage_limit() -> i64 {
    100
}

impl UsageQuery {
    /// 收敛到数据层允许的区间，避免一次把整张表拉出来。
    ///
    /// 上限复用数据层的常量，两处不会各写各的。
    pub fn limit(&self) -> i64 {
        self.limit.clamp(1, crate::db::usage::MAX_USAGE_LIMIT)
    }

    /// 负数游标一律按 0 处理（从最新开始）。
    pub fn before_id(&self) -> i64 {
        self.before_id.max(0)
    }
}

/// 用量响应：累计汇总 + 明细列表。
///
/// 管理员接口与密钥自助接口共用同一结构，字段完全一致。
#[derive(Debug, Serialize)]
pub struct UsageListResponse {
    pub summary: UsageSummary,
    pub records: Vec<UsageView>,
    /// 是否还有更早的记录，前端据此决定是否展示「查询更多」。
    pub has_more: bool,
}

impl UsageListResponse {
    pub fn new(summary: UsageSummary, records: Vec<UsageView>, has_more: bool) -> Self {
        Self {
            summary,
            records,
            has_more,
        }
    }
}
