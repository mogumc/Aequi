//! HTTP 接口层。

pub mod auth;
pub mod device;
pub mod middleware;
pub mod otp;
pub mod settings;
mod router;

use serde::Serialize;

pub use router::router;

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
