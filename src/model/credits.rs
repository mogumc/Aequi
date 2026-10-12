//! 积分的定点表示与换算。
//!
//! 积分保留四位小数，全程用整数存储与运算，不引入浮点：
//! **1 积分 = 10000 个最小单位**。
//!
//! 计费口径：1 积分抵扣 10000 token；模型倍率用万分比整数表示
//! （`10000` = 1.0 倍）。
//!
//! 两个 10000 是同一把尺子，于是 **1 token 恰好对应 1 个最小单位**，
//! 换算过程中不需要任何比例因子，也不会产生截断误差。

/// 1 积分可抵扣的 token 数。
pub const TOKENS_PER_CREDIT: i64 = 10_000;

/// 积分的最小单位刻度：1 积分 = 10000 单位，即四位小数。
pub const CREDIT_SCALE: i64 = 10_000;

/// 倍率刻度：10000 表示 1.0 倍。
pub const MULTIPLIER_SCALE: i64 = 10_000;

/// 模型未配置倍率时的取值（1.0 倍）。
pub const DEFAULT_MULTIPLIER_BP: i64 = MULTIPLIER_SCALE;

/// 把最小单位格式化成四位小数，例如 `15000` → `"1.5000"`。
pub fn format_credits(scaled: i64) -> String {
    let magnitude = scaled.unsigned_abs();
    let whole = magnitude / CREDIT_SCALE as u64;
    let fraction = magnitude % CREDIT_SCALE as u64;

    format!(
        "{}{}.{:04}",
        if scaled < 0 { "-" } else { "" },
        whole,
        fraction
    )
}

/// 解析积分字符串，例如 `"1.5"` → `15000`。
///
/// 超过四位小数会超出最小单位，直接拒绝而不是悄悄截断。
pub fn parse_credits(text: &str) -> Option<i64> {
    let text = text.trim();
    let (negative, body) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };

    // 至少要有一位数字。""、"-"、"." 这类输入必须被拒绝，
    // 否则会被静默当成 0 积分——那是最危险的一种「成功」。
    if !body.bytes().any(|byte| byte.is_ascii_digit()) {
        return None;
    }

    let (whole, fraction) = match body.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (body, ""),
    };

    if !fraction.bytes().all(|byte| byte.is_ascii_digit()) || fraction.len() > 4 {
        return None;
    }

    let whole = if whole.is_empty() {
        0
    } else if whole.bytes().all(|byte| byte.is_ascii_digit()) {
        whole.parse::<i64>().ok()?
    } else {
        return None;
    };

    let mut digits = fraction.to_string();
    while digits.len() < 4 {
        digits.push('0');
    }
    let fraction = digits.parse::<i64>().ok()?;

    let value = whole.checked_mul(CREDIT_SCALE)?.checked_add(fraction)?;
    Some(if negative { -value } else { value })
}

/// 把万分比格式化成两位小数（0.01 精度），例如 `15000` → `"1.50"`。
///
/// 倍率精度是 0.01（见 [`parse_multiplier`]），历史数据里更细的位数
/// 会被就地取整到两位——展示不需要比约定更精确。
pub fn format_multiplier(bp: i64) -> String {
    let whole = bp / MULTIPLIER_SCALE;
    let fraction = ((bp % MULTIPLIER_SCALE).abs() as f64 / 100.0).round() as i64;

    format!("{whole}.{fraction:02}")
}

/// 解析倍率字符串为万分比整数。
///
/// 倍率精度为 **0.01**：只接受两位以内小数，即万分比必须是 100 的倍数。
/// 更细的输入（如 `1.005`）直接拒绝——精度是刻意收窄的约定，不是巧合。
/// 倍率不允许为负，非法输入返回 `None`。
pub fn parse_multiplier(text: &str) -> Option<i64> {
    let scaled = parse_credits(text)?;
    (scaled >= 0 && scaled % 100 == 0).then_some(scaled)
}

/// 按倍率折算后的 token 量。
///
/// 零头**向上取整**：计费不因抹零被打磨——截断可被低倍率 + 小请求反复
/// 利用，每次少收不足一个最小单位却可无限累积；进位单次误差 ≤ 1 个
/// 最小单位（0.0001 积分），对用户无感。`usage.total_tokens` 非负
/// （入口已拒绝负数），因此这里的除法只对非负数生效。
///
/// # 保底：用过就至少收一个最小单位
///
/// 低倍率配小请求时，四舍五入会算出 0——上游照样收了钱，账上却查不到。
/// 所以**只要这次真的消耗过 token**（`raw_tokens > 0`）且倍率非零，
/// 结果就保底 1 个最小单位（0.0001 积分）。
///
/// 区分两种「0」：
/// - **真的没用**：`raw_tokens == 0`，没消耗就不该收，保底不适用；
/// - **倍率为 0**：管理员显式把这个模型设成免费（校验只拦负数），
///   那是定价意图，不是计算误差，同样不适用保底。
///
/// 否则「免费模型」会被保底成每次收 0.0001 积分，管理员设的 0 倍形同虚设。
pub fn final_tokens(raw_tokens: i64, multiplier_bp: i64) -> i64 {
    let product = i128::from(raw_tokens) * i128::from(multiplier_bp);
    let rounded_up = (product + i128::from(MULTIPLIER_SCALE) - 1) / i128::from(MULTIPLIER_SCALE);
    let value = clamp(rounded_up);

    // 只有「确实消耗过 + 倍率非零」才保底：见上方说明的两种例外。
    if value == 0 && raw_tokens > 0 && multiplier_bp > 0 {
        1
    } else {
        value
    }
}

/// 本次调用应扣减的积分（最小单位）。
///
/// 与 [`final_tokens`] 数值恒等——1 token 恰好对应 1 个最小单位。
/// 分开命名只为让调用处读起来贴合各自的语义；哪天换算比例变了，两者会在这里分叉。
pub fn cost_scaled(raw_tokens: i64, multiplier_bp: i64) -> i64 {
    final_tokens(raw_tokens, multiplier_bp)
}

/// 中间结果用 i128 计算，溢出时收敛到 i64 边界而不是 panic。
fn clamp(value: i128) -> i64 {
    value.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_credit_equals_ten_thousand_tokens() {
        assert_eq!(
            cost_scaled(TOKENS_PER_CREDIT, DEFAULT_MULTIPLIER_BP),
            CREDIT_SCALE
        );
        assert_eq!(format_credits(cost_scaled(10_000, 10_000)), "1.0000");
    }

    #[test]
    fn single_token_is_exactly_one_unit() {
        assert_eq!(cost_scaled(1, DEFAULT_MULTIPLIER_BP), 1);
        assert_eq!(format_credits(1), "0.0001");
    }

    #[test]
    fn cost_and_final_tokens_agree_by_construction() {
        for tokens in [0, 1, 334, 9_999, 10_000, 123_456] {
            for bp in [5_000, 10_000, 15_000, 20_000] {
                assert_eq!(final_tokens(tokens, bp), cost_scaled(tokens, bp));
            }
        }
    }

    #[test]
    fn multiplier_scales_the_cost() {
        // 0.5 倍：10000 token 只需 0.5 积分
        assert_eq!(format_credits(cost_scaled(10_000, 5_000)), "0.5000");
        // 2.0 倍
        assert_eq!(format_credits(cost_scaled(10_000, 20_000)), "2.0000");
        // 1.5 倍下 334 个 token
        assert_eq!(cost_scaled(334, 15_000), 501);
        assert_eq!(format_credits(501), "0.0501");
    }

    #[test]
    fn final_tokens_are_rounded_up() {
        // 333 × 1.5 = 499.5 → 500：零头进位，计费不因抹零被打磨。
        assert_eq!(final_tokens(333, 15_000), 500);
        assert_eq!(final_tokens(10_000, 10_000), 10_000);
        // 1 token 在 0.01 倍下 = 0.0001 积分，恰好一个最小单位。
        assert_eq!(final_tokens(1, 100), 1);
    }

    #[test]
    fn low_multiplier_small_request_falls_back_to_one_unit() {
        // 低倍率 + 小请求会算出 0，但上游照样收了钱——保底 1 个最小单位。
        // 0.01 倍（100）下 50 token 只有 0.5 单位，进位后仍是 1；
        // 更小的组合才会真的落到 0，这里显式构造一个。
        assert_eq!(final_tokens(1, 1), 1, "最小倍率也至少收一个单位");
        assert_eq!(format_credits(final_tokens(1, 1)), "0.0001");
        assert_eq!(final_tokens(5, 1), 1);
    }

    #[test]
    fn genuine_zero_is_not_charged() {
        // 真的没用：一个 token 都没消耗，不该收钱。
        assert_eq!(final_tokens(0, DEFAULT_MULTIPLIER_BP), 0);
        assert_eq!(final_tokens(0, 100), 0);
        assert_eq!(final_tokens(0, 0), 0);
    }

    #[test]
    fn zero_multiplier_is_a_pricing_intent_not_a_rounding_error() {
        // 管理员显式把模型设成免费（校验只拦负数，0 是合法的）。
        // 这是定价意图，不能被保底抬成 0.0001 积分——否则 0 倍形同虚设。
        assert_eq!(final_tokens(10_000, 0), 0, "0 倍率必须保持免费");
        assert_eq!(final_tokens(1_000_000, 0), 0);
    }

    #[test]
    fn normal_rates_are_untouched_by_the_floor() {
        // 保底只影响「算出 0」那一种情况，常规计费一个都不能变。
        assert_eq!(final_tokens(10_000, DEFAULT_MULTIPLIER_BP), 10_000);
        assert_eq!(final_tokens(333, 15_000), 500);
        assert_eq!(final_tokens(1, 100), 1);
        assert_eq!(final_tokens(1500, 10_000), 1500);
    }

    #[test]
    fn multiplier_precision_is_two_decimals() {
        assert_eq!(parse_multiplier("1.05"), Some(10_500));
        assert_eq!(parse_multiplier("0.01"), Some(100));
        assert_eq!(parse_multiplier("2"), Some(20_000));
        // 超过两位小数直接拒绝——精度是刻意的约定。
        assert_eq!(parse_multiplier("1.005"), None);
        assert_eq!(parse_multiplier("1.0001"), None);
        assert_eq!(parse_multiplier("-1"), None);
    }

    #[test]
    fn multiplier_formatting() {
        assert_eq!(format_multiplier(10_000), "1.00");
        assert_eq!(format_multiplier(15_000), "1.50");
        assert_eq!(format_multiplier(8_000), "0.80");
    }

    #[test]
    fn credits_round_trip_at_four_decimals() {
        for text in ["0.0000", "1.5000", "12.3456", "0.0001", "-3.2000"] {
            let scaled = parse_credits(text).expect("应能解析");
            assert_eq!(format_credits(scaled), text);
        }
    }

    #[test]
    fn parsing_accepts_short_forms_and_rejects_junk() {
        assert_eq!(parse_credits("1"), Some(CREDIT_SCALE));
        assert_eq!(parse_credits("1.5"), Some(15_000));
        assert_eq!(parse_credits(".5"), Some(5_000));
        assert_eq!(parse_credits("  2.25  "), Some(22_500));

        assert_eq!(parse_credits("1.23456"), None, "超过四位小数应拒绝");
        assert_eq!(parse_credits("abc"), None);
        assert_eq!(parse_credits("1.2.3"), None);
        assert_eq!(parse_credits(""), None);
        assert_eq!(parse_credits("   "), None);
        assert_eq!(parse_credits("-"), None, "只有符号不算数字");
        assert_eq!(parse_credits("."), None, "只有小数点不算数字");
    }

    #[test]
    fn huge_values_do_not_panic() {
        assert_eq!(cost_scaled(i64::MAX, i64::MAX), i64::MAX);
    }
}
