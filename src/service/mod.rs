//! 业务逻辑层。
//!
//! 每个子模块负责一类用例，向上只暴露语义化函数，
//! 不感知 HTTP，也不直接拼 SQL。

pub mod auth;
pub mod device;
pub mod maintenance;
pub mod otp;
pub mod password;
pub mod provider;
pub mod settings;
pub mod token;
pub mod totp;
