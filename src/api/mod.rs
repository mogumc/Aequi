//! HTTP 接口层。

pub mod auth;
pub mod device;
pub mod key_auth;
pub mod key_portal;
pub mod keys;
pub mod middleware;
pub mod otp;
pub mod settings;
mod router;

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
}

fn default_usage_limit() -> i64 {
    100
}

impl UsageQuery {
    /// 收敛到合理区间，避免一次把整张表拉出来。
    pub fn limit(&self) -> i64 {
        self.limit.clamp(1, 500)
    }
}

/// 用量响应：累计汇总 + 明细列表。
///
/// 管理员接口与密钥自助接口共用同一结构，字段完全一致。
#[derive(Debug, Serialize)]
pub struct UsageListResponse {
    pub summary: UsageSummary,
    pub records: Vec<UsageView>,
}

impl UsageListResponse {
    pub fn new(summary: UsageSummary, records: Vec<UsageView>) -> Self {
        Self { summary, records }
    }
}
