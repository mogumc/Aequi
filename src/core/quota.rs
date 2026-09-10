//! 上游密钥额度（§6.5.1，D13）：极简显式计数 + 反应式快速确认。
//!
//! - 触顶 → `QuotaExhausted`，**摘出轮转池、不随时间恢复**
//! - 恢复仅两条路径：管理员 reset（清 `used_tokens`）/ 调高 `limit_tokens`
//! - 探针**不会**清除此状态（密钥有效，只是没额度）
//! - 不做周期 / 多维 / 每模型额度，不主动查上游余额（明确拒绝的过度设计）

use serde::{Deserialize, Serialize};

/// 上游密钥额度触顶来源（§6.5.1 / §6.6）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaExhaustTrigger {
    /// 本地显式计数触顶（`used_tokens >= limit × safety_margin`）。
    Counter,
    /// 上游明确报配额耗尽（402 / body 命中配额正则，快速确认路径）。
    UpstreamReported,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UpstreamKeyQuota {
    /// `None` = 不限。典型："该平台密钥共 1000 万 token"。
    pub limit_tokens: Option<u64>,
    /// 提前停用余量，默认 0.98（吸收估算误差，避免"正好撞墙时正在服务"）。
    pub safety_margin: f64,
    /// settle 时累加（四维之和）。
    pub used_tokens: u64,
    /// 触顶时刻（ms），供审计；`None` = 未触顶。
    pub exhausted_at_ms: Option<u64>,
}

impl Default for UpstreamKeyQuota {
    fn default() -> Self {
        Self {
            limit_tokens: None,
            safety_margin: 0.98,
            used_tokens: 0,
            exhausted_at_ms: None,
        }
    }
}

impl UpstreamKeyQuota {
    /// 计数判定：`used_tokens >= limit × safety_margin` → 触顶。
    #[must_use]
    pub fn is_exhausted(&self) -> bool {
        match self.limit_tokens {
            Some(limit) => (self.used_tokens as f64) >= (limit as f64) * self.safety_margin,
            None => false,
        }
    }

    /// 结算后累计并判定（纯函数，返回新状态；不 mutate —— 单写者决定是否落库）。
    #[must_use]
    pub fn after_adding(&self, delta_tokens: u64, now_ms: u64) -> Self {
        let mut next = Self {
            used_tokens: self.used_tokens.saturating_add(delta_tokens),
            exhausted_at_ms: self.exhausted_at_ms,
            ..self.clone()
        };
        if next.exhausted_at_ms.is_none() && next.is_exhausted() {
            next.exhausted_at_ms = Some(now_ms);
        }
        next
    }

    /// 管理员 reset：清 `used_tokens` 并解除触顶（§4.5 动作表）。
    #[must_use]
    pub const fn reset(&self) -> Self {
        Self {
            used_tokens: 0,
            exhausted_at_ms: None,
            ..*self
        }
    }

    /// 管理员加额 / 改限。
    #[must_use]
    pub const fn with_limit(&self, limit_tokens: Option<u64>) -> Self {
        Self {
            limit_tokens,
            exhausted_at_ms: None, // 提限额即解除触顶判定；是否恢复由管理员确认
            ..*self
        }
    }
}

/// 快速确认路径的输入（§6.5.1）：上游明确报配额耗尽。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpstreamQuotaSignal {
    pub status: u16,
    /// body 摘要（仅前 4KB，已脱敏）。
    pub body_snippet: String,
}

/// 配额耗尽正文模式（可配置覆盖，此处为默认集）。
pub const DEFAULT_QUOTA_BODY_PATTERNS: &[&str] = &[
    "insufficient_quota",
    "quota exceeded",
    "quota_exceeded",
    "billing_hard_limit_reached",
    "余额不足",
    "欠费",
];

/// 快速确认判定（§6.5.1）：`402` 或（`403`/`429` 且 body 命中配额正则）。
///
/// ★ 普通 429 **不**触发本路径（避免把"限流"误判为"配额耗尽"），仅 body 明确命中时置终态。
#[must_use]
pub fn fast_confirm(signal: &UpstreamQuotaSignal, patterns: &[&str]) -> Option<QuotaExhaustTrigger> {
    if signal.status == 402 {
        return Some(QuotaExhaustTrigger::UpstreamReported);
    }
    if signal.status == 403 || signal.status == 429 {
        let body = signal.body_snippet.to_lowercase();
        if patterns.iter().any(|p| body.contains(&p.to_lowercase())) {
            return Some(QuotaExhaustTrigger::UpstreamReported);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_trigger_at_margin() {
        let q = UpstreamKeyQuota { limit_tokens: Some(1000), safety_margin: 0.98, ..Default::default() };
        assert!(!q.is_exhausted());
        let q2 = q.after_adding(979, 100);
        assert!(!q2.is_exhausted(), "979 < 980");
        let q3 = q.after_adding(980, 100);
        assert!(q3.is_exhausted(), "980 >= 980 → 触顶");
        assert_eq!(q3.exhausted_at_ms, Some(100));
    }

    #[test]
    fn no_limit_never_exhausts() {
        let q = UpstreamKeyQuota { limit_tokens: None, ..Default::default() };
        let q2 = q.after_adding(u64::MAX / 2, 1).after_adding(u64::MAX / 2, 2);
        assert!(!q2.is_exhausted());
    }

    #[test]
    fn exhausted_does_not_resurrect_by_time() {
        // 不随时间恢复 —— exhausted_at_ms 一旦置位不再清除（除非 reset/加额）
        let q = UpstreamKeyQuota { limit_tokens: Some(10), safety_margin: 0.98, ..Default::default() };
        let q2 = q.after_adding(10, 100).after_adding(0, 999_999);
        assert!(q2.is_exhausted());
        assert_eq!(q2.exhausted_at_ms, Some(100));
    }

    #[test]
    fn reset_clears_and_restores() {
        let q = UpstreamKeyQuota { limit_tokens: Some(10), safety_margin: 0.98, ..Default::default() };
        let exhausted = q.after_adding(10, 100);
        assert!(exhausted.is_exhausted());
        let reset = exhausted.reset();
        assert_eq!(reset.used_tokens, 0);
        assert!(!reset.is_exhausted());
        assert_eq!(reset.exhausted_at_ms, None);
    }

    #[test]
    fn raise_limit_restores() {
        let q = UpstreamKeyQuota { limit_tokens: Some(10), ..Default::default() };
        let exhausted = q.after_adding(10, 1);
        let raised = exhausted.with_limit(Some(1_000_000));
        assert!(!raised.is_exhausted());
        assert_eq!(raised.used_tokens, 10, "加额不清计数，仅解除触顶");
    }

    #[test]
    fn fast_confirm_402() {
        let s = UpstreamQuotaSignal { status: 402, body_snippet: "Payment Required".into() };
        assert_eq!(fast_confirm(&s, DEFAULT_QUOTA_BODY_PATTERNS), Some(QuotaExhaustTrigger::UpstreamReported));
    }

    #[test]
    fn fast_confirm_body_pattern_on_429() {
        let s = UpstreamQuotaSignal { status: 429, body_snippet: r#"{"error":"insufficient_quota"}"#.into() };
        assert_eq!(fast_confirm(&s, DEFAULT_QUOTA_BODY_PATTERNS), Some(QuotaExhaustTrigger::UpstreamReported));
    }

    #[test]
    fn plain_429_never_triggers() {
        // ★ 硬性：普通限流不得误判为配额耗尽
        let s = UpstreamQuotaSignal { status: 429, body_snippet: r#"{"error":"rate limit exceeded, retry later"}"#.into() };
        assert_eq!(fast_confirm(&s, DEFAULT_QUOTA_BODY_PATTERNS), None);
    }

    #[test]
    fn plain_403_auth_error_never_triggers() {
        let s = UpstreamQuotaSignal { status: 403, body_snippet: r#"{"error":"invalid api key"}"#.into() };
        assert_eq!(fast_confirm(&s, DEFAULT_QUOTA_BODY_PATTERNS), None);
    }
}
