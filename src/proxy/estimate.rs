//! 本地估算 token 用量。
//!
//! 部分上游（尤其是中转与故障响应）不返回 usage。直接按 0 记账等于白送，
//! 所以用启发式补齐：CJK 字符按每字一个 token，其余按每四个字符一个，
//! 图片按每张一千 token。这是 newapi 一类网关通用的近似口径——
//! 不追求逐字精确，只保证量级正确、计费不被钻空子。
//!
//! 估算结果会打上 `estimated` 标记，管理员日志里看得到哪些数字是估的。

use crate::proxy::scheme::{ChatRequest, ContentPart, Usage};

/// 非 CJK 字符平均多少个折一个 token。
const ASCII_PER_TOKEN: f64 = 4.0;
/// 一张图片的估算 token 数（近似一张高清图的量级）。
const IMAGE_TOKENS: i64 = 1000;
/// 每条消息的角色与结构开销，业界惯例按 4 token 计。
const MESSAGE_OVERHEAD: i64 = 4;

/// 估算一段文本的 token 数。
pub fn estimate_text(text: &str) -> i64 {
    let mut cjk = 0.0_f64;
    let mut other = 0.0_f64;

    for ch in text.chars() {
        if is_cjk(ch) {
            cjk += 1.0;
        } else {
            other += 1.0;
        }
    }

    (cjk + other / ASCII_PER_TOKEN).ceil() as i64
}

/// 估算一次请求的输入 token。
pub fn estimate_prompt(request: &ChatRequest) -> i64 {
    let mut total = 0;

    for message in &request.messages {
        for part in &message.content {
            match part {
                ContentPart::Text { text } => total += estimate_text(text),
                ContentPart::Image { .. } => total += IMAGE_TOKENS,
            }
        }
        total += MESSAGE_OVERHEAD;
    }

    // 工具定义也是输入的一部分：名字、描述与 JSON schema 全算上。
    for tool in &request.tools {
        total += estimate_text(&tool.name);
        if let Some(description) = &tool.description {
            total += estimate_text(description);
        }
        total += estimate_text(&tool.parameters.to_string());
    }

    total
}

/// 估算一次响应的输出 token。
pub fn estimate_completion(text: &str) -> i64 {
    estimate_text(text) + MESSAGE_OVERHEAD
}

/// 上游没报用量时的兜底：整套数字都是估的。
pub fn estimate_usage(request: &ChatRequest, response_text: &str) -> Usage {
    let prompt_tokens = estimate_prompt(request);
    let completion_tokens = estimate_completion(response_text);
    Usage::estimated(prompt_tokens, completion_tokens)
}

/// 是否为 CJK 字符（含中文、日文假名、韩文与全角形式）。
fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x4E00..=0x9FFF    // CJK 统一表意文字
        | 0x3400..=0x4DBF  // 扩展 A
        | 0x3000..=0x303F  // CJK 符号与标点
        | 0x3040..=0x30FF  // 日文假名
        | 0xAC00..=0xD7AF  // 韩文音节
        | 0xFF00..=0xFFEF  // 全角形式
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_text_is_about_four_chars_per_token() {
        // 40 个字母 → 约 10 token
        assert_eq!(estimate_text(&"a".repeat(40)), 10);
    }

    #[test]
    fn cjk_text_is_about_one_token_per_char() {
        assert_eq!(estimate_text("你好世界"), 4);
    }

    #[test]
    fn mixed_text_combines_both_rules() {
        // 2 个中文 + 8 个字母 → 2 + 2 = 4
        assert_eq!(estimate_text("你好abcdefgh"), 4);
    }

    #[test]
    fn empty_text_costs_nothing() {
        assert_eq!(estimate_text(""), 0);
    }
}
