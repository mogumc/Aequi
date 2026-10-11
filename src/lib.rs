//! Aequi
//!
//! 模块分层（上层依赖下层，反向不依赖）：
//!
//! - [`api`]：HTTP 接口层，路由、请求解析、认证中间件
//! - [`web`]：管理面板的 embed 层，前端产物打进二进制
//! - [`service`]：业务逻辑，登录流程、会话管理、OTP
//! - [`db`]：数据层，SQLite 连接与各表读写
//! - [`model`]：数据模型
//! - [`config`]：配置
//!
//! 横切关注点：[`error`] 统一错误、[`utils`] 通用工具、[`app`] 状态装配、[`server`] 服务启停。

pub mod api;
pub mod app;
pub mod cli;
pub mod config;
pub mod db;
pub mod error;
pub mod model;
pub mod proxy;
pub mod server;
pub mod service;
pub mod utils;
pub mod web;

pub use app::AppState;
pub use error::{Error, Result};
