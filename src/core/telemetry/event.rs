//! 遥测事件（§4.4 / §6.6）：`TelemetryEvent` enum + `schema_version` + 错误码目录。
//!
//! - `schema_version = 1`：本轮能力所需维度**一次性加全**（CI 门槛 §6.6）
//! - 关联 id：`RequestId(uuidv7)` 贯穿 span / 事件 / 落盘 / 响应头
//! - 敏感字段：只留 id 与指纹，凭证永不进事件
//! - 类型化替代魔法字符串：`ErrorCode` enum（稳定 `as_str()` + HTTP 状态映射）

use crate::core::account::UsageCounters;
use crate::core::id::{AccessKeyId, BindingId, ProbeOutcome, RequestId, UpstreamId, UpstreamKeyId};
use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u16 = 1;

/// 错误码目录（§3.2 统一错误体）。稳定字符串 + HTTP 状态映射，取代散落裸字符串。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    // 客户端侧
    AccessKeyInvalid,
    AccessKeyDisabled,
    QuotaExceeded,
    ModelNotFound,
    ModelNotAllowedForGroup,
    InvalidRequest,
    LimitTooLarge,
    CursorFilterMismatch,
    // 上游侧
    UpstreamAuthFailed,
    UpstreamRateLimited,
    UpstreamUnavailable,
    UpstreamCircuitOpen,
    NoAvailableKey,
    MaxRetriesExceeded,
    UpstreamError,
    // 系统
    InternalError,
    QuotaThresholdReached,
}

impl ErrorCode {
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::AccessKeyInvalid => "access_key_invalid",
            Self::AccessKeyDisabled => "access_key_disabled",
            Self::QuotaExceeded => "quota_exceeded",
            Self::ModelNotFound => "model_not_found",
            Self::ModelNotAllowedForGroup => "model_not_allowed_for_group",
            Self::InvalidRequest => "invalid_request",
            Self::LimitTooLarge => "limit_too_large",
            Self::CursorFilterMismatch => "cursor_filter_mismatch",
            Self::UpstreamAuthFailed => "upstream_auth_failed",
            Self::UpstreamRateLimited => "upstream_rate_limited",
            Self::UpstreamUnavailable => "upstream_unavailable",
            Self::UpstreamCircuitOpen => "upstream_circuit_open",
            Self::NoAvailableKey => "no_available_key",
            Self::MaxRetriesExceeded => "max_retries_exceeded",
            Self::UpstreamError => "upstream_error",
            Self::InternalError => "internal_error",
            Self::QuotaThresholdReached => "quota_threshold_reached",
        }
    }

    /// HTTP 状态映射（§6.5.6：额度超额 429 且不带 Retry-After、retryable:false）。
    #[must_use]
    pub const fn http_status(&self) -> u16 {
        match self {
            Self::AccessKeyInvalid | Self::AccessKeyDisabled => 401,
            Self::QuotaExceeded => 429,
            Self::ModelNotFound => 404,
            Self::ModelNotAllowedForGroup => 403,
            Self::InvalidRequest => 400,
            Self::LimitTooLarge | Self::CursorFilterMismatch => 400,
            Self::UpstreamAuthFailed => 502,
            Self::UpstreamRateLimited => 429,
            Self::UpstreamUnavailable => 502,
            Self::UpstreamCircuitOpen => 503,
            Self::NoAvailableKey => 503,
            Self::MaxRetriesExceeded => 502,
            Self::UpstreamError => 502,
            Self::InternalError => 500,
            Self::QuotaThresholdReached => 200,
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 用量来源（§2.3 类型化，取代 `token_source: Option<String>`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenSource {
    Upstream,
    Estimated,
}

/// 计时分解（§4.4 timing 重定义；取代旧版 `upstream_ms = total - queue` 的粗口径）。
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct RequestTiming {
    pub queue_ms: u64,
    /// 首字节到达耗时（TTFB）。
    pub upstream_ttfb_ms: u64,
    pub upstream_total_ms: u64,
    /// 真实重试次数（取代旧版恒 0 的 `attempts`）。
    pub retry_count: u32,
    pub total_ms: u64,
}

use std::fmt;

/// 遥测事件（§6.6 维度表，`schema_version=1`）。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TelemetryEvent {
    RequestFinished {
        request_id: RequestId,
        #[serde(skip_serializing_if = "Option::is_none")]
        akey_id: Option<AccessKeyId>,
        #[serde(skip_serializing_if = "Option::is_none")]
        group: Option<String>,
        model: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        binding_id: Option<BindingId>,
        #[serde(skip_serializing_if = "Option::is_none")]
        upstream_id: Option<UpstreamId>,
        #[serde(skip_serializing_if = "Option::is_none")]
        key_id: Option<UpstreamKeyId>,
        status: u16,
        retry_count: u32,
        timing: RequestTiming,
        usage: UsageCounters,
        /// µcredit。
        credits: i64,
        token_source: TokenSource,
        #[serde(skip_serializing_if = "Option::is_none")]
        error_code: Option<ErrorCode>,
    },
    UpstreamSelected {
        request_id: RequestId,
        binding_id: BindingId,
        upstream_id: UpstreamId,
        key_id: UpstreamKeyId,
        selection_reason: String,
        key_inflight: u32,
    },
    KeyCooldownSet {
        key_id: UpstreamKeyId,
        /// 429 / 5xx / network
        reason: String,
        dur_ms: u64,
        streak: u32,
        jittered: bool,
    },
    BreakerStateChanged {
        upstream_id: UpstreamId,
        from: String,
        to: String,
        errors_in_window: u32,
    },
    KeyProbeCompleted {
        key_id: UpstreamKeyId,
        outcome: ProbeOutcome,
        latency_ms: u64,
        /// import / manual / sweeper
        source: String,
    },
    AccessDenied {
        #[serde(skip_serializing_if = "Option::is_none")]
        akey_id: Option<AccessKeyId>,
        model: String,
        reason: String,
    },
    QuotaThresholdReached {
        akey_id: AccessKeyId,
        used: i64,
        cap: i64,
    },
    QuotaExceeded {
        akey_id: AccessKeyId,
        used: i64,
        cap: i64,
    },
    AccountAdjusted {
        akey_id: AccessKeyId,
        delta: i64,
        new_cap: Option<i64>,
        operator: String,
    },
    UpstreamKeyQuotaExhausted {
        ukey_id: UpstreamKeyId,
        upstream_id: UpstreamId,
        used_tokens: u64,
        limit_tokens: u64,
        trigger: QuotaTrigger,
    },
    UpstreamKeyLifecycle {
        ukey_id: UpstreamKeyId,
        action: String,
        operator: String,
    },
}

/// 上游密钥额度触顶来源（§6.5.1）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaTrigger {
    Counter,
    UpstreamReported,
}

impl TelemetryEvent {
    #[must_use]
    pub const fn schema_version(&self) -> u16 {
        SCHEMA_VERSION
    }

    /// 事件名（kind tag），供日志 / 指标标签。
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::RequestFinished { .. } => "request_finished",
            Self::UpstreamSelected { .. } => "upstream_selected",
            Self::KeyCooldownSet { .. } => "key_cooldown_set",
            Self::BreakerStateChanged { .. } => "breaker_state_changed",
            Self::KeyProbeCompleted { .. } => "key_probe_completed",
            Self::AccessDenied { .. } => "access_denied",
            Self::QuotaThresholdReached { .. } => "quota_threshold_reached",
            Self::QuotaExceeded { .. } => "quota_exceeded",
            Self::AccountAdjusted { .. } => "account_adjusted",
            Self::UpstreamKeyQuotaExhausted { .. } => "upstream_key_quota_exhausted",
            Self::UpstreamKeyLifecycle { .. } => "upstream_key_lifecycle",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_code_stability() {
        // 稳定字符串是对外契约 —— 不得无意变更
        assert_eq!(ErrorCode::QuotaExceeded.as_str(), "quota_exceeded");
        assert_eq!(ErrorCode::QuotaExceeded.http_status(), 429);
        assert_eq!(ErrorCode::ModelNotAllowedForGroup.http_status(), 403);
        assert_eq!(ErrorCode::NoAvailableKey.http_status(), 503);
        assert_eq!(ErrorCode::LimitTooLarge.http_status(), 400);
    }

    #[test]
    fn event_serializes_with_schema_tag() {
        let ev = TelemetryEvent::KeyCooldownSet {
            key_id: UpstreamKeyId::from_secret(b"k"),
            reason: "429".into(),
            dur_ms: 1500,
            streak: 2,
            jittered: true,
        };
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["kind"], "key_cooldown_set");
        assert_eq!(ev.schema_version(), SCHEMA_VERSION);
        // 凭证只以 32 位 hex 指纹出现
        assert_eq!(json["key_id"].as_str().unwrap().len(), 32);
    }

    #[test]
    fn timing_has_real_retry_count() {
        // 回归锚点：旧版 RequestTiming.attempts 恒 0；新版由调用方写入真实值
        let t = RequestTiming { retry_count: 3, ..Default::default() };
        let json = serde_json::to_value(&t).unwrap();
        assert_eq!(json["retry_count"], 3);
    }

    #[test]
    fn no_plaintext_secret_in_events() {
        // 所有事件字段均为 id / 指纹 / 计数 —— 编译期由类型保证（UpstreamKeyId 无明文）
        let ev = TelemetryEvent::UpstreamSelected {
            request_id: RequestId::generate(),
            binding_id: BindingId::generate(),
            upstream_id: UpstreamId::generate(),
            key_id: UpstreamKeyId::from_secret(b"sk-anything"),
            selection_reason: "swrr".into(),
            key_inflight: 1,
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(!json.contains("sk-anything"));
    }
}
