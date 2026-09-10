#![forbid(unsafe_code)]

//! Aequi v2 — OpenAI-format API transparent proxy.
//!
//! 模块分层（REFACTOR_PLAN.md §4.1，D7 单 crate + 强边界）：
//! - [`core`]    纯领域：零 IO、零 hyper、零 sqlx，可 100% 单测
//! - [`storage`] SQLite 端口与适配（单写者 actor + keyset 分页 + JSONL logsink）
//! - [`upstream`] 连接层 + 统一输出层（P3）
//! - [`gateway`] 对外 HTTP（P4）
//! - [`runtime`] 状态容器与后台任务（P5）

pub mod core;
pub mod gateway;
pub mod runtime;
pub mod storage;
pub mod upstream;

pub use core as domain;

fn main() {
    // P2 阶段：底层 API 与存储层重构的组装入口随 P4/P5 网关落地时补全。
    // 当前 crate 以库模块 + 测试驱动（cargo test）为主要验证路径。
    println!("aequi v2 — construction in progress (P1 core + P2 storage landed)");
}
