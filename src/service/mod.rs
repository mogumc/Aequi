//! 业务逻辑层。
//!
//! 每个子模块负责一类用例，向上只暴露语义化函数，
//! 不感知 HTTP，也不直接拼 SQL。
//!
//! 管理员体系（account / session / otp / settings）与密钥体系（keys / billing）
//! 各自独立：前者只碰控制面库，后者只碰数据面库。

pub mod auth;
pub mod billing;
pub mod device;
pub mod keys;
pub mod maintenance;
pub mod otp;
pub mod password;
pub mod provider;
pub mod settings;
pub mod token;
pub mod totp;
pub mod upstream;
