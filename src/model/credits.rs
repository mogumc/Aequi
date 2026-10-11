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

/// 把万分比格式化成四位小数，例如 `15000` → `"1.5000"`。
pub fn format_multiplier(bp: i64) -> String {
    let whole = bp / MULTIPLIER_SCALE;
    let fraction = (bp % MULTIPLIER_SCALE).abs();

    format!("{whole}.{fraction:04}")
}

/// 解析倍率字符串（四位小数以内）为万分比整数。
///
/// 直接复用积分的解析：1 积分的刻度是 10000，万分比的刻度也是 10000，
/// 两者量纲一致——这不是巧合，是同一套四位小数约定。
/// 倍率不允许为负，非法输入返回 `None`。
pub fn parse_multiplier(text: &str) -> Option<i64> {
    let scaled = parse_credits(text)?;
    (scaled >= 0).then_some(scaled)
}

/// 按倍率折算后的 token 量。
pub fn final_tokens(raw_tokens: i64, multiplier_bp: i64) -> i64 {
    clamp(i128::from(raw_tokens) * i128::from(multiplier_bp) / i128::from(MULTIPLIER_SCALE))
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
    fn final_tokens_are_rounded_down() {
        assert_eq!(final_tokens(333, 15_000), 499);
        assert_eq!(final_tokens(10_000, 10_000), 10_000);
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
    fn multiplier_formatting() {
        assert_eq!(format_multiplier(10_000), "1.0000");
        assert_eq!(format_multiplier(15_000), "1.5000");
        assert_eq!(format_multiplier(8_000), "0.8000");
    }

    #[test]
    fn huge_values_do_not_panic() {
        assert_eq!(cost_scaled(i64::MAX, i64::MAX), i64::MAX);
    }
}
