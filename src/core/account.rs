//! 账户与预留-结算（§6.5.4 / §6.5.5，D6 / D9）。
//!
//! 四维用量独立存储；两层记账（访问密钥积分 + 上游密钥额度）在**同一次结算**内完成，
//! 不新增第二计费路径（§八 第 9 条）。无周期重置 —— 恢复只走管理员手动操作。

use serde::{Deserialize, Serialize};

/// 四维用量计数（§6.5.2）。`think` 计入输出计费但独立存储。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageCounters {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub think_tokens: u64,
    pub cache_tokens: u64,
}

impl UsageCounters {
    /// 四维之和 —— 上游密钥额度 `used_tokens` 的口径（与平台总 token 配额对齐）。
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.think_tokens + self.cache_tokens
    }

    pub const fn add(&mut self, o: &Self) {
        self.input_tokens += o.input_tokens;
        self.output_tokens += o.output_tokens;
        self.think_tokens += o.think_tokens;
        self.cache_tokens += o.cache_tokens;
    }
}

/// 积分（µcredit 定点，1 credit = 10⁶ µcredit）。强类型避免与 token 数混淆。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Credits(pub i64);

impl Credits {
    pub const ZERO: Credits = Credits(0);
    pub const RESERVE_MIN: Credits = Credits(1);

    #[must_use]
    pub const fn as_micro(&self) -> i64 {
        self.0
    }

    /// 展示为浮点 credit（仅用于 API/管理端展示，不参与数学）。
    #[must_use]
    pub fn to_credit_f64(self) -> f64 {
        self.0 as f64 / 1_000_000.0
    }

    #[must_use]
    pub const fn saturating_add(self, o: Credits) -> Credits {
        Credits(self.0.saturating_add(o.0))
    }

    #[must_use]
    pub const fn saturating_sub(self, o: Credits) -> Credits {
        Credits(self.0.saturating_sub(o.0))
    }
}

/// 预留结果（§6.5.5）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reservation {
    /// 已预留 `RESERVE_MIN`（1 µcredit 轻闸）。
    Reserved,
    /// 余额不足（`credits_cap` 有限且 used + min > cap）。
    QuotaExceeded,
    /// 密钥被停用。
    KeyDisabled,
}

impl Reservation {
    #[must_use]
    pub const fn is_ok(&self) -> bool {
        matches!(self, Self::Reserved)
    }
}

/// 结算明细（§6.5.5）—— 一次 settle 的全部账务结果，供上层一次性落库。
#[derive(Clone, Copy, Debug)]
pub struct Settlement {
    /// 访问密钥侧：本次实际费用（µcredit）。
    pub credits_delta: Credits,
    /// 结算后的 `credits_used`。
    pub credits_used_after: Credits,
    /// 结算后的 `lifetime_credits_used`（审计用，reset 不清零）。
    pub lifetime_after: Credits,
    /// 上游密钥侧：`used_tokens` 增量 = 四维之和。
    pub ukey_used_delta: u64,
    /// 上游密钥额度是否在本次结算触顶（→ `QuotaExhausted`，由存储层置状态）。
    pub ukey_quota_exhausted: bool,
}

/// 纯函数：访问密钥 reserve 判定（§6.5.5）。
///
/// 无 IO；真实落库由存储层单写者执行。
#[must_use]
pub fn reserve(
    credits_cap: Option<Credits>,
    credits_used: Credits,
    enabled: bool,
) -> Reservation {
    if !enabled {
        return Reservation::KeyDisabled;
    }
    match credits_cap {
        Some(cap) if credits_used.saturating_add(Credits::RESERVE_MIN).as_micro() > cap.as_micro() => {
            Reservation::QuotaExceeded
        }
        _ => Reservation::Reserved,
    }
}

/// 纯函数：结算计算（§6.5.5）。
///
/// `ukey` 侧：`used_tokens += actual.total()`；若 `used >= limit × safety_margin` → 触顶。
#[must_use]
pub fn settle(
    credits_cap: Option<Credits>,
    credits_used_before: Credits,
    reserved_min: Credits,
    actual_cost: Credits,
    actual_usage: &UsageCounters,
    ukey_used_tokens_before: u64,
    ukey_limit_tokens: Option<u64>,
    ukey_safety_margin: f64,
) -> Settlement {
    // ① 访问密钥侧：used += actual_cost − reserved_min（差额结算，失败归还由 release 完成）
    let net = actual_cost.saturating_sub(reserved_min);
    let credits_used_after = credits_used_before.saturating_add(net);
    // cap 有限时不得超扣：clamp 到 cap
    let credits_used_after = match credits_cap {
        Some(cap) if credits_used_after.as_micro() > cap.as_micro() => cap,
        _ => credits_used_after,
    };
    let lifetime_after = credits_used_before.saturating_add(net);

    // ② 上游密钥侧
    let ukey_used_delta = actual_usage.total();
    let ukey_used_after = ukey_used_tokens_before.saturating_add(ukey_used_delta);
    let ukey_quota_exhausted = match ukey_limit_tokens {
        Some(limit) => (ukey_used_after as f64) >= (limit as f64) * ukey_safety_margin,
        None => false,
    };

    Settlement {
        credits_delta: net,
        credits_used_after,
        lifetime_after,
        ukey_used_delta,
        ukey_quota_exhausted,
    }
}

/// 纯函数：失败归还（release，§6.5.5）—— 退还预留的 1 µcredit。
#[must_use]
pub const fn release(credits_used: Credits) -> Credits {
    credits_used.saturating_sub(Credits::RESERVE_MIN)
}

/// 软阈值告警判定（§6.5.6）：used / cap ≥ threshold（默认 0.8）→ `QuotaThresholdReached`。
#[must_use]
pub fn threshold_reached(cap: Credits, used: Credits, threshold: f64) -> bool {
    if cap.as_micro() <= 0 {
        return false;
    }
    (used.as_micro() as f64 / cap.as_micro() as f64) >= threshold
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: Credits = Credits(1_000_000); // 1 credit

    #[test]
    fn reserve_rejects_when_exhausted() {
        // cap = 10 credits, used = 10 → 拒绝
        assert_eq!(reserve(Some(Credits(10 * M.0)), Credits(10 * M.0), true), Reservation::QuotaExceeded);
        // cap = None（无限）→ 永远可预留
        assert_eq!(reserve(None, Credits(i64::MAX / 2), true), Reservation::Reserved);
        // disabled
        assert_eq!(reserve(Some(M), Credits::ZERO, false), Reservation::KeyDisabled);
        // 差 1 µcredit 满额 → 允许预留 1 µcredit 轻闸
        assert_eq!(reserve(Some(M), Credits(M.0 - 1), true), Reservation::Reserved);
    }

    #[test]
    fn settle_two_layers_same_call() {
        // 访问密钥：cap 10c，已用 1c（含预留 1µc）；实际费用 2.5c
        // 上游密钥：limit 1000 token @ margin 0.98 → 980 触顶
        let usage = UsageCounters { input_tokens: 500, output_tokens: 500, think_tokens: 0, cache_tokens: 0 };
        let s = settle(
            Some(Credits(10 * M.0)),
            M,
            Credits::RESERVE_MIN,
            Credits(2_500_000),
            &usage,
            900,
            Some(1000),
            0.98,
        );
        // net = 2.5c − 1µc → used ≈ 1c + 2.5c
        assert_eq!(s.credits_used_after.as_micro(), M.0 + 2_500_000 - 1);
        assert_eq!(s.ukey_used_delta, 1000);
        assert!(s.ukey_quota_exhausted, "900+1000 >= 980 → 触顶");
    }

    #[test]
    fn settle_no_limit_never_exhausts() {
        let usage = UsageCounters { input_tokens: 1_000_000, output_tokens: 0, think_tokens: 0, cache_tokens: 0 };
        let s = settle(Some(M), Credits::ZERO, Credits::RESERVE_MIN, M, &usage, 0, None, 0.98);
        assert!(!s.ukey_quota_exhausted);
    }

    #[test]
    fn release_returns_reserve_min() {
        assert_eq!(release(Credits(1)), Credits(0));
        assert_eq!(release(Credits(5)), Credits(4));
    }

    #[test]
    fn soft_threshold() {
        assert!(threshold_reached(Credits(100 * M.0), Credits(80 * M.0), 0.8));
        assert!(!threshold_reached(Credits(100 * M.0), Credits(79 * M.0), 0.8));
        assert!(!threshold_reached(Credits::ZERO, Credits::ZERO, 0.8));
    }

    #[test]
    fn lifetime_accumulates_independently() {
        // lifetime = used_before + net；reset 只清 used 不清 lifetime（由存储层保证）
        let usage = UsageCounters { input_tokens: 1000, output_tokens: 0, think_tokens: 0, cache_tokens: 0 };
        let s = settle(Some(M), M, Credits::RESERVE_MIN, M, &usage, 0, Some(100), 0.98);
        // net = M(实际) − 1µc(预留)；lifetime = used_before + net
        assert_eq!(s.lifetime_after, Credits(M.0 - 1));
        assert_eq!(s.credits_used_after, s.lifetime_after, "同一次 settle 内两者增量一致");
    }
}
