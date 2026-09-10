//! `core/` — 纯领域内核（P1）。
//!
//! 硬约束（REFACTOR_PLAN.md §4.1）：
//! 1. 本模块及子模块**禁止**依赖 `hyper` / `tokio` / `sqlx` / 任何存储引擎。
//! 2. 所有身份类型不可变；`name` 仅为可变展示名，不得作主键/URI（§八 第 2 条）。
//! 3. 无哨兵值：`Unlimited` / `Admin` 一律 `Option` / enum 显式建模（§八 第 5 条）。
//! 4. 授权判定全项目唯一入口：[`group::allowed`]（§八 第 6 条）。

pub mod account;
pub mod backoff;
pub mod binding;
pub mod config;
pub mod group;
pub mod id;
pub mod quota;
pub mod rating;
pub mod routing;
pub mod secret;
pub mod telemetry;

pub use account::{Credits, Reservation, Settlement, UsageCounters};
pub use backoff::{BackoffPolicy, BreakerState, JitterMode};
pub use binding::ModelBinding;
pub use id::BindingId;
pub use config::{AppConfig, KeyPolicy, RateTable};
pub use group::{allowed, AccessKeyGroups, Group, GroupId, Policy};
pub use id::{
    AccessKeyId, ProbeOutcome, RequestId, UpstreamHealth, UpstreamId, UpstreamKeyId,
};
pub use quota::{QuotaExhaustTrigger, UpstreamKeyQuota};
pub use rating::{Rate, DEFAULT_RATE_CACHE, DEFAULT_RATE_INPUT, DEFAULT_RATE_OUTPUT, DEFAULT_RATE_THINK};
pub use routing::SwrrSelector;
pub use secret::Secret;
pub use telemetry::{ErrorCode, TelemetryEvent, SCHEMA_VERSION};
