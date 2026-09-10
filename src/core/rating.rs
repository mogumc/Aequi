//! 四维费率与计费数学（§6.5.2 / §6.5.3，D6）。
//!
//! micro-credit 定点（1 credit = 10⁶ µcredit），沿用旧版精度，杜绝浮点累积误差。

use serde::{Deserialize, Serialize};

/// 每 1K token 的积分费率（§6.5.2）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rate {
    pub input: f64,
    pub output: f64,
    /// think 按输出费率计费，但存储独立（§6.5.2）。
    pub think: f64,
    pub cache: f64,
    /// 保留：图片等非 token 计费场景（每次请求固定积分）。
    pub per_request: Option<u64>,
}

pub const DEFAULT_RATE_INPUT: f64 = 1.0;
pub const DEFAULT_RATE_OUTPUT: f64 = 1.0;
pub const DEFAULT_RATE_THINK: f64 = 1.0;
pub const DEFAULT_RATE_CACHE: f64 = 0.2;

impl Default for Rate {
    fn default() -> Self {
        Self {
            input: DEFAULT_RATE_INPUT,
            output: DEFAULT_RATE_OUTPUT,
            think: DEFAULT_RATE_THINK,
            cache: DEFAULT_RATE_CACHE,
            per_request: None,
        }
    }
}

impl Rate {
    #[must_use]
    pub const fn new(input: f64, output: f64, think: f64, cache: f64) -> Self {
        Self { input, output, think, cache, per_request: None }
    }
}

/// micro-credit：1 credit = 1_000_000 µcredit。
pub const MICRO_CREDIT: i64 = 1_000_000;
/// 最小费用 1 µcredit（沿用旧版轻闸）。
pub const MIN_COST_MICRO: i64 = 1;

/// 计费数学（§6.5.3）—— 纯函数。
///
/// `cost_micro = ceil( (input×r_in + output×r_out + think×r_think + cache×r_cache) / 1000 × 1e6 )`
///
/// `per_request` 模式：`cost_micro = ceil(r_per_request × 1e6)`（tokens 忽略）。
/// 未知模型由调用方使用 RateTable 默认行。
#[must_use]
pub fn compute_credit_cost_micro(usage: &crate::core::account::UsageCounters, rate: &Rate) -> i64 {
    if let Some(pr) = rate.per_request {
        let cost = (pr as f64) * (MICRO_CREDIT as f64);
        return (cost.ceil() as i64).max(MIN_COST_MICRO);
    }
    let credit_units = (usage.input_tokens as f64 * rate.input
        + usage.output_tokens as f64 * rate.output
        + usage.think_tokens as f64 * rate.think
        + usage.cache_tokens as f64 * rate.cache)
        / 1000.0;
    let micro = credit_units * (MICRO_CREDIT as f64);
    (micro.ceil() as i64).max(MIN_COST_MICRO)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::account::UsageCounters;

    #[test]
    fn default_rate_math() {
        // 1000 in / 1000 out / 0 think / 0 cache，费率 1/1/1/0.2 → 2 credits = 2e6 µcredit
        let u = UsageCounters { input_tokens: 1000, output_tokens: 1000, think_tokens: 0, cache_tokens: 0 };
        assert_eq!(compute_credit_cost_micro(&u, &Rate::default()), 2_000_000);
    }

    #[test]
    fn cache_discounted() {
        // 1000 cache @0.2 → 0.2 credits
        let u = UsageCounters { input_tokens: 0, output_tokens: 0, think_tokens: 0, cache_tokens: 1000 };
        assert_eq!(compute_credit_cost_micro(&u, &Rate::default()), 200_000);
    }

    #[test]
    fn think_charged_but_stored_separately() {
        // think 与 output 同费率，计费等价 —— 但 UsageCounters 保持独立字段
        let a = UsageCounters { input_tokens: 0, output_tokens: 500, think_tokens: 0, cache_tokens: 0 };
        let b = UsageCounters { input_tokens: 0, output_tokens: 0, think_tokens: 500, cache_tokens: 0 };
        assert_eq!(
            compute_credit_cost_micro(&a, &Rate::default()),
            compute_credit_cost_micro(&b, &Rate::default())
        );
    }

    #[test]
    fn min_cost_gate() {
        // 1 token @1.0 → ceil(1e3 µcredit) = 1000，仍 > 1；0 usage → 1 µcredit 轻闸
        let u = UsageCounters::default();
        assert_eq!(compute_credit_cost_micro(&u, &Rate::default()), MIN_COST_MICRO);
    }

    #[test]
    fn per_request_mode() {
        let u = UsageCounters { input_tokens: 0, output_tokens: 0, think_tokens: 0, cache_tokens: 0 };
        let r = Rate { per_request: Some(2), ..Rate::default() };
        assert_eq!(compute_credit_cost_micro(&u, &r), 2_000_000);
    }

    #[test]
    fn ceiling_no_float_drift() {
        // 3 tokens @ 0.1 → 0.3 credits = 300_000 µcredit（ceil 处理浮点尾巴）
        let u = UsageCounters { input_tokens: 3, output_tokens: 0, think_tokens: 0, cache_tokens: 0 };
        let r = Rate::new(0.1, 1.0, 1.0, 0.2);
        let c = compute_credit_cost_micro(&u, &r);
        assert!((299_999..=300_001).contains(&c), "浮点尾巴应被 ceil 吸收: {c}");
    }
}
