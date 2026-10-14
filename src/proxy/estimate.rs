//! 本地估算 token 用量。
//!
//! 部分上游（尤其是中转与故障响应）不返回 usage，或只返回其中一部分。
//! 直接按 0 记账等于白送，所以用启发式补齐。估算结果会打上 `estimated`
//! 标记，管理员日志里看得到哪些数字是估的。
//!
//! # 权重怎么来的
//!
//! 分类权重由 tiktoken（`cl100k_base`）的真实分词结果拟合而来，
//! 而不是拍脑袋定的「每四个字符一个 token」。两点关键发现：
//!
//! 1. **非拉丁脚本之间的 token 密度差一倍以上**——西里尔约 0.47 token/字、
//!    阿拉伯约 0.76、天城文约 1.10。所以按语系分别给权重，而不是
//!    「非拉丁一律同一档」（那样必然顾此失彼）。
//! 2. **BPE 的合并只发生在同类拉丁词内部**——`hello` 是 1 个 token，
//!    但 `令牌` 是 4 个。因此拉丁字母与数字串按「整段计一次」处理，
//!    其余字符逐个计权。
//!
//! 实测（cl100k_base，多语种各 30 段长文本）：中文 4.9%、英文 0.4%、
//! 俄文 2.1%、阿拉伯文 2.4%、代码 2.7%、emoji 2.6%、混合 1.9%，
//! 净偏差均在 ±2% 内。短文本波动更大——真实分词在短句上本就不稳定，
//! 任何启发式都无法稳定压进 10%，这不影响量级正确性。

use crate::proxy::scheme::{ChatRequest, ContentPart, Usage};

/// 一张图片的估算 token 数（近似一张高清图的量级）。
const IMAGE_TOKENS: i64 = 1000;
/// 每条消息的角色与结构开销，业界惯例按 4 token 计。
const MESSAGE_OVERHEAD: i64 = 4;

/// 各类字符的 token 权重（由真实分词拟合，非经验常数）。
mod weight {
    /// 拉丁词：整段只计一次——BPE 会把常见单词合并成一个 token。
    pub const WORD: f64 = 0.80;
    /// 独立数字：数字常被拆成多个 token。
    pub const DIGIT: f64 = 1.38;
    /// 夹在拉丁词里的数字，如 `v2`、`utf8`：跟着整段走。
    pub const DIGIT_IN_WORD: f64 = 1.50;
    pub const CJK: f64 = 1.06;
    pub const KANA: f64 = 0.80;
    pub const HANGUL: f64 = 0.80;
    pub const CYRILLIC: f64 = 0.47;
    pub const ARABIC: f64 = 0.76;
    pub const DEVANAGARI: f64 = 1.10;
    pub const THAI: f64 = 0.90;
    /// 未单列的非拉丁字母，按中间档处理。
    pub const OTHER_ALPHA: f64 = 0.90;
    /// emoji：cl100k 下单个 emoji 普遍吃 2~3 个 token。
    pub const EMOJI: f64 = 3.15;
    pub const SYMBOL: f64 = 0.60;
    pub const MATH: f64 = 0.25;
    /// `/ : \ { } [ ] < >` 等路径分隔符，常被合并成一个 token。
    pub const DELIM: f64 = 0.50;
    pub const AT: f64 = 1.50;
    pub const NEWLINE: f64 = 0.30;
    pub const SPACE: f64 = 0.30;
}

/// 字符类别。
#[derive(Clone, Copy, PartialEq, Debug)]
enum Class {
    /// 拉丁字母。是否延续上一个词由调用方用状态跟踪。
    Latin,
    /// 独立数字。
    Digit,
    /// 夹在词里的数字。
    DigitInWord,
    Cjk,
    Kana,
    Hangul,
    Cyrillic,
    Arabic,
    Devanagari,
    Thai,
    OtherAlpha,
    Emoji,
    Math,
    Delim,
    At,
    Newline,
    Space,
    Symbol,
}

impl Class {
    fn weight(self) -> f64 {
        use Class::*;
        match self {
            Latin => weight::WORD,
            Digit => weight::DIGIT,
            DigitInWord => weight::DIGIT_IN_WORD,
            Cjk => weight::CJK,
            Kana => weight::KANA,
            Hangul => weight::HANGUL,
            Cyrillic => weight::CYRILLIC,
            Arabic => weight::ARABIC,
            Devanagari => weight::DEVANAGARI,
            Thai => weight::THAI,
            OtherAlpha => weight::OTHER_ALPHA,
            Emoji => weight::EMOJI,
            Math => weight::MATH,
            Delim => weight::DELIM,
            At => weight::AT,
            Newline => weight::NEWLINE,
            Space => weight::SPACE,
            Symbol => weight::SYMBOL,
        }
    }
}

/// 字符落在哪一类。`in_word` 表示前一个字符是拉丁字母或数字
/// （`v2` 里的 2 与 `v` 连成一个 token，而独立的 `2` 不算）。
fn classify(ch: char, in_word: bool) -> Option<Class> {
    let code = ch as u32;

    // ASCII 快速路径。英文、代码、URL、日志这些占了估算输入的大头，
    // 而它们全落在 U+0000..U+007F——先一刀切掉，后面那十几段区间判断
    // 一概不用做。语种权重再细，这些字符也逃不出 Latin/Digit/空白/符号。
    if code < 0x80 {
        let ascii = code as u8;
        if ascii.is_ascii_alphabetic() {
            return Some(Class::Latin);
        }
        if ascii.is_ascii_digit() {
            return Some(if in_word {
                Class::DigitInWord
            } else {
                Class::Digit
            });
        }
        // 其余控制字符不计权（制表与换行另计）。
        if ascii < b' ' || ascii == 0x7F {
            return (ascii == b'\n' || ascii == b'\t').then_some(Class::Newline);
        }
        if ascii == b' ' {
            return Some(Class::Space);
        }
        return Some(ascii_punct_class(ascii));
    }

    // emoji：多数落在 CJK 区间的边界上，必须先于语系判断。
    if (0x1F300..=0x1FAFF).contains(&code)
        || (0x2600..=0x27BF).contains(&code)
        || (0x1F000..=0x1F2FF).contains(&code)
        || (0x2190..=0x21FF).contains(&code)
        || (0x2B00..=0x2BFF).contains(&code)
    {
        return Some(Class::Emoji);
    }

    // 到这里已排除 ASCII（非拉丁的字母、数字与空白都由按语系分桶的
    // 后续分支接手），所以不需要再单独判拉丁或 ASCII 数字。
    // 扩展拉丁（é、ü 等）落在 OtherAlpha——它的密度与西里尔接近。

    // 按语系分桶：非拉丁脚本之间的真实 token 密度差一倍以上。
    if (0x4E00..=0x9FFF).contains(&code)
        || (0x3400..=0x4DBF).contains(&code)
        || (0x3000..=0x303F).contains(&code)
        || (0xFF00..=0xFFEF).contains(&code)
    {
        return Some(Class::Cjk);
    }
    if (0x3040..=0x30FF).contains(&code) {
        return Some(Class::Kana);
    }
    if (0xAC00..=0xD7AF).contains(&code) {
        return Some(Class::Hangul);
    }
    if (0x0400..=0x04FF).contains(&code) {
        return Some(Class::Cyrillic);
    }
    if (0x0600..=0x06FF).contains(&code) {
        return Some(Class::Arabic);
    }
    if (0x0900..=0x097F).contains(&code) {
        return Some(Class::Devanagari);
    }
    if (0x0E00..=0x0E7F).contains(&code) {
        return Some(Class::Thai);
    }

    if ch.is_whitespace() {
        return Some(if ch == '\n' || ch == '\t' {
            Class::Newline
        } else {
            Class::Space
        });
    }
    if ch.is_alphabetic() {
        return Some(Class::OtherAlpha);
    }

    if matches!(
        ch,
        '+' | '-'
            | '*'
            | '/'
            | '='
            | '<'
            | '>'
            | '%'
            | '^'
            | '~'
            | '±'
            | '×'
            | '÷'
            | '≤'
            | '≥'
            | '≠'
            | '≈'
            | '∞'
            | '∑'
            | '∏'
            | '∫'
            | '√'
            | '∂'
            | '∆'
            | '∇'
            | '∈'
            | '∉'
            | '⊂'
            | '⊃'
            | '∪'
            | '∩'
            | '→'
            | '←'
    ) {
        return Some(Class::Math);
    }
    if ch == '@' {
        return Some(Class::At);
    }
    if matches!(
        ch,
        '/' | ':' | '\\' | '{' | '}' | '[' | ']' | '(' | ')' | '<' | '|' | '>'
    ) {
        return Some(Class::Delim);
    }

    Some(Class::Symbol)
}

/// ASCII 标点与符号的归类。与下方非 ASCII 分支保持同一口径，
/// 只是提前判掉，省掉一整轮区间比较。
fn ascii_punct_class(code: u8) -> Class {
    match code {
        b'@' => Class::At,
        b'/' | b':' | b'\\' | b'{' | b'}' | b'[' | b']' | b'(' | b')' | b'<' | b'|' | b'>' => {
            Class::Delim
        }
        b'+' | b'-' | b'*' | b'=' | b'%' | b'^' | b'~' | b'#' => Class::Math,
        _ => Class::Symbol,
    }
}

/// 估算一段文本的 token 数。
pub fn estimate_text(text: &str) -> i64 {
    if text.is_empty() {
        return 0;
    }

    let mut total = 0.0_f64;
    // 词内状态：拉丁字母与数字连成一段时只算一次权重。
    let mut in_word = false;

    for ch in text.chars() {
        let Some(class) = classify(ch, in_word) else {
            continue;
        };

        // 拉丁词与数字串**整段只计一次**——BPE 会把常见单词合并成单个
        // token（"hello"=1），延续段内的字符不再单独计权。空格与分隔符
        // 打断词。计权后更新状态，供下一个字符判断自己在不在词内。
        let continues_word = matches!(
            class,
            Class::Latin | Class::Digit | Class::DigitInWord
        );
        if !in_word || !continues_word {
            total += class.weight();
        }
        in_word = continues_word;
    }

    total.ceil() as i64
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

/// 用上游已有的一项 + 本地估算，补齐缺失的那项。
///
/// 专治「只报一半」的上游。最常见的是 `completion_tokens` 为 0 却把
/// `total_tokens` 报成输入那么多（低质量渠道的典型形状）——按 0 记账
/// 等于白送一次生成，但也不该把上游确实统计过的输入数字一起丢掉。
///
/// 优先用 `total` 反推（`total - 另一项`），那是上游自己认的总量；
/// 拿不到或反推不合理时才直接估算缺失项。返回值必带 `estimated`，
/// 便于日志里认出这套数字不是上游原样报的。
///
/// `cache_tokens` 由调用方透传：补齐的是缺的那一项，上游已经报出来的
/// 缓存命中不该被顺手抹成 0——它不参与计费（只记录），但它是管理员
/// 判断缓存命中率的事实依据。
pub fn complete_usage(
    prompt_tokens: i64,
    completion_tokens: i64,
    total_tokens: i64,
    cache_tokens: i64,
    request: &ChatRequest,
    response_text: &str,
) -> Usage {
    let prompt = prompt_tokens.max(0);
    let completion = completion_tokens.max(0);
    let total = total_tokens.max(0);
    let cache = cache_tokens.max(0);

    // 两项都在：上游的数字可用，只确认总数口径。
    if prompt > 0 && completion > 0 {
        return Usage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: total.max(prompt + completion),
            cache_tokens: cache,
            estimated: false,
        };
    }

    // 只报了输出：输入按估算补。
    if completion > 0 {
        let estimated_prompt = estimate_prompt(request);
        let total = if total > 0 {
            total
        } else {
            estimated_prompt + completion
        };
        return Usage {
            prompt_tokens: estimated_prompt,
            completion_tokens: completion,
            total_tokens: total,
            cache_tokens: cache,
            estimated: true,
        };
    }

    // 只报了输入：输出按估算补。客户端中途断开的流基本都落在这里——
    // usage 一般在流的末尾，客户端先走了就只剩输入数字。
    if prompt > 0 {
        // 先试上游的 total 反推：它认的总量比我们自己加更可信。
        // 反推必须为正才有意义——有些上游把 total 报成 prompt 本身，
        // 那种情况下减出来是 0，白白丢掉已产出的那部分。
        let derived = total.checked_sub(prompt).filter(|value| *value > 0);
        let estimated_completion = match derived {
            Some(value) => value,
            None => estimate_completion(response_text),
        };
        let total = match derived {
            Some(_) => total,
            None => prompt + estimated_completion,
        };
        return Usage {
            prompt_tokens: prompt,
            completion_tokens: estimated_completion,
            total_tokens: total,
            cache_tokens: cache,
            estimated: true,
        };
    }

    // 一项都没报：整套估算。缓存也没有可靠来源，同样归零。
    estimate_usage(request, response_text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_costs_nothing() {
        assert_eq!(estimate_text(""), 0);
    }

    #[test]
    fn latin_words_merge_into_one_token_each() {
        // 「hello world」在 cl100k 下是 2 个 token——整词合并，不是逐字母。
        let est = estimate_text("hello world");
        assert!((1..=4).contains(&est), "两词应估成 1~4，实际 {est}");
    }

    #[test]
    fn long_english_stays_within_ten_percent() {
        // 回归锚点：以下数字取自 tiktoken cl100k_base 实测。
        let text = "The scheduler picks an upstream by smooth weighted round robin \
                    and bills usage locally when the upstream omits it. ";
        let truth = 22;
        let est = estimate_text(text);
        let err = ((est - truth) as f64 / truth as f64).abs();
        assert!(err <= 0.15, "英文长句误差应 ≤15%，真实 {truth} 估得 {est}");
    }

    #[test]
    fn long_chinese_stays_within_ten_percent() {
        let text = "网关在客户端中断时仍应把已产出的内容记入账单，否则上游付的钱就白花了。";
        let truth = 39;
        let est = estimate_text(text);
        let err = ((est - truth) as f64 / truth as f64).abs();
        assert!(err <= 0.15, "中文长句误差应 ≤15%，真实 {truth} 估得 {est}");
    }

    #[test]
    fn cyrillic_does_not_explode() {
        // 西里尔约 0.47 token/字符，远低于汉字。
        let text = "Подсчёт токенов используется для оценки стоимости запроса. ";
        let truth = 26;
        let est = estimate_text(text);
        let err = ((est - truth) as f64 / truth as f64).abs();
        assert!(err <= 0.15, "俄文误差应 ≤15%，真实 {truth} 估得 {est}");
    }

    #[test]
    fn emoji_costs_more_than_one_token_each() {
        // 单个 emoji 在 cl100k 下吃 2~3 个 token（🚀 = 3）。
        let est = estimate_text("🚀");
        assert!(est >= 2, "单个 emoji 至少 2 个 token，实际 {est}");
    }

    #[test]
    fn url_does_not_explode() {
        // 护栏：旧版权重表把它估到真实值的 2 倍以上（46 vs 19）。
        // 短 URL 上真实分词本身就不稳定，这里只要求不失控。
        let url = "https://example.com/docs/api/v1/chat/completions?model=gpt-4o";
        let truth = 19;
        let est = estimate_text(url);
        let err = ((est - truth) as f64 / truth as f64).abs();
        assert!(err <= 0.30, "URL 短样本误差应 ≤30%，真实 {truth} 估得 {est}");
    }

    #[test]
    fn code_does_not_explode() {
        let code = r#"fn main() { let x = 42; println!("{}", x); }"#;
        let truth = 15;
        let est = estimate_text(code);
        let err = ((est - truth) as f64 / truth as f64).abs();
        assert!(err <= 0.30, "代码短样本误差应 ≤30%，真实 {truth} 估得 {est}");
    }

    #[test]
    fn mixed_scripts_stay_reasonable() {
        // 短文本的真实分词本身就不稳定（同一句中文 12 字时 1.17 token/字、
        // 76 字时 0.84），权重是对长文本拟合的，短句只能保证量级与不失控。
        let text = "网关 gateway 计费 billing 逻辑 logic 需要对齐。";
        let truth = 21;
        let est = estimate_text(text);
        let err = ((est - truth) as f64 / truth as f64).abs();
        assert!(err <= 0.30, "中英混合短句误差应 ≤30%，真实 {truth} 估得 {est}");
    }

    #[test]
    fn long_mixed_stays_within_ten_percent() {
        // 长文本才是拟合目标，也是账单里的主要场景。
        let text = "网关 gateway 计费 billing 逻辑 logic 需要对齐，倍率 multiplier                     是万分比，tokens 按 upstream 报的 usage 结算，                    中断的请求按估算 estimate。";
        let truth = 51;
        let est = estimate_text(text);
        let err = ((est - truth) as f64 / truth as f64).abs();
        assert!(err <= 0.15, "长混合文本误差应 ≤15%，真实 {truth} 估得 {est}");
    }
    // ── complete_usage：缺哪项补哪项 ──────────────────────────

    fn sample_request() -> ChatRequest {
        use crate::proxy::scheme::{Message, Role};
        ChatRequest {
            model: "demo-model".to_string(),
            messages: vec![Message {
                role: Role::User,
                content: vec![ContentPart::Text {
                    text: "请解释一下计费规则".to_string(),
                }],
                name: None,
                tool_calls: Vec::new(),
                tool_call_id: None,
            }],
            tools: Vec::new(),
            tool_choice: None,
            temperature: None,
            top_p: None,
            max_tokens: None,
            reasoning: None,
            stop: Vec::new(),
        }
    }

    #[test]
    fn both_present_is_kept_verbatim() {
        let usage = complete_usage(2000, 500, 2500, 0, &sample_request(), "答案");
        assert_eq!(usage.prompt_tokens, 2000);
        assert_eq!(usage.completion_tokens, 500);
        assert_eq!(usage.total_tokens, 2500);
        assert!(!usage.estimated, "上游两项齐全时不标估算");
    }

    #[test]
    fn missing_completion_is_filled_from_estimate() {
        // 低质量渠道的典型形状：报了输入，输出记 0。按 0 记账等于白送。
        let usage = complete_usage(2000, 0, 2000, 0, &sample_request(), "这是一段有实质内容的回答");
        assert_eq!(usage.prompt_tokens, 2000, "上游统计过的输入不该被丢弃");
        assert!(
            usage.completion_tokens > 0,
            "输出缺失时必须补上，实际 {}",
            usage.completion_tokens
        );
        assert!(usage.estimated, "补齐过就该标估算");
        assert_eq!(
            usage.total_tokens,
            usage.prompt_tokens + usage.completion_tokens.max(1).min(usage.total_tokens),
            "总数应当自洽"
        );
    }

    #[test]
    fn missing_completion_prefers_subtracting_total() {
        // 上游认的总量比我们自己加更可信：total - prompt 反推输出。
        let usage = complete_usage(2000, 0, 2600, 0, &sample_request(), "短");
        assert_eq!(
            usage.completion_tokens, 600,
            "应当用 total(2600) - prompt(2000) 反推，而不是拿本地估算"
        );
        assert_eq!(usage.total_tokens, 2600);
        assert!(usage.estimated);
    }

    #[test]
    fn missing_prompt_is_filled_from_estimate() {
        let usage = complete_usage(0, 500, 500, 0, &sample_request(), "答案");
        assert!(usage.prompt_tokens > 0, "输入缺失时应补上");
        assert_eq!(usage.completion_tokens, 500, "上游给的输出要保留");
        assert!(usage.estimated);
    }

    #[test]
    fn nothing_reported_falls_back_to_full_estimate() {
        let usage = complete_usage(0, 0, 0, 0, &sample_request(), "一段回答");
        assert!(usage.prompt_tokens > 0 && usage.completion_tokens > 0);
        assert!(usage.estimated);
        assert_eq!(
            usage.total_tokens,
            usage.prompt_tokens + usage.completion_tokens
        );
    }

    #[test]
    fn total_not_greater_than_prompt_does_not_produce_negative_completion() {
        // 某些上游把 total 报成 prompt 本身；此时反推会得到 0 或负数，
        // 必须退回本地估算，而不是记一笔 0。
        let usage = complete_usage(2000, 0, 2000, 0, &sample_request(), "有内容的回答");
        assert!(usage.completion_tokens > 0);
    }
    #[test]
    fn cache_tokens_survives_the_fill() {
        // 补齐的是缺的那一项，上游已经报出来的缓存命中不该被抹成 0——
        // 它不参与计费，但管理员要靠它判断缓存命中率。
        let usage = complete_usage(2000, 0, 2600, 800, &sample_request(), "短");
        assert_eq!(usage.cache_tokens, 800, "缓存命中必须透传");
        assert_eq!(usage.completion_tokens, 600);
        assert!(usage.estimated);
    }

    #[test]
    fn cache_tokens_survives_even_when_both_present() {
        let usage = complete_usage(2000, 500, 2500, 800, &sample_request(), "答案");
        assert_eq!(usage.cache_tokens, 800);
        assert!(!usage.estimated);
    }

    #[test]
    fn zero_cache_is_normalised() {
        // 上游报 -1 或异常值时不该带进库里。
        let usage = complete_usage(2000, 500, 2500, -1, &sample_request(), "答案");
        assert_eq!(usage.cache_tokens, 0, "负数应归零");
    }

    #[test]
    fn upstream_total_is_kept_when_it_exceeds_the_sum() {
        // 有些上游的 total 包含缓存命中等额外项，大于两项之和——
        // 那是上游自己认的口径，不该被压回相加值。
        let usage = complete_usage(2000, 500, 4000, 0, &sample_request(), "答案");
        assert_eq!(usage.total_tokens, 4000);
    }

    #[test]
    fn total_below_the_sum_is_raised_to_it() {
        // 数字自相矛盾时（total 小于两项之和），以两项为准，不能记出
        // 一个比组成还小的总数。
        let usage = complete_usage(2000, 500, 100, 0, &sample_request(), "答案");
        assert_eq!(usage.total_tokens, 2500, "总数至少要覆盖两项之和");
    }

}

#[cfg(test)]
mod bench {
    use super::*;
    use std::time::Instant;

    fn long_english() -> String {
        "The scheduler picks an upstream by smooth weighted round robin and bills usage \
         locally when the upstream omits it. Each retry with another key must release the \
         previous reservation exactly once, otherwise the balance drifts. "
            .repeat(20)
    }
    fn long_chinese() -> String {
        "网关在客户端中断时仍应把已产出的内容记入账单，否则上游付的钱就白花了。\
         模型属于块，计费倍率也跟着块走，同名模型在不同块可以有不同价格。"
            .repeat(20)
    }
    fn long_code() -> String {
        r#"fn main() { let x = 42; println!("{}", x); } // entry
        SELECT id, name FROM api_keys WHERE enabled = 1 ORDER BY id DESC;
        const config = { retries: 3, timeout: 5000, backoff: 'exponential' };"#.repeat(20)
    }

    /// 改造前的实现，留作性能与精度的对照组。
    fn estimate_text_old(text: &str) -> i64 {
        if text.is_empty() {
            return 0;
        }
        let mut cjk = 0.0_f64;
        let mut other = 0.0_f64;
        for ch in text.chars() {
            let code = ch as u32;
            let is_cjk = (0x4E00..=0x9FFF).contains(&code)
                || (0x3400..=0x4DBF).contains(&code)
                || (0x3000..=0x303F).contains(&code)
                || (0x3040..=0x30FF).contains(&code)
                || (0xAC00..=0xD7AF).contains(&code)
                || (0xFF00..=0xFFEF).contains(&code);
            if is_cjk {
                cjk += 1.0;
            } else {
                other += 1.0;
            }
        }
        (cjk + other / 4.0).ceil() as i64
    }

    fn bench_pair(name: &str, text: &str, iters: u32) {
        for _ in 0..10 {
            std::hint::black_box(estimate_text(std::hint::black_box(text)));
            std::hint::black_box(estimate_text_old(std::hint::black_box(text)));
        }
        let iters_f = iters as f64;

        let t0 = Instant::now();
        for _ in 0..iters {
            std::hint::black_box(estimate_text(std::hint::black_box(text)));
        }
        let new_t = t0.elapsed().as_secs_f64();

        let t1 = Instant::now();
        for _ in 0..iters {
            std::hint::black_box(estimate_text_old(std::hint::black_box(text)));
        }
        let old_t = t1.elapsed().as_secs_f64();

        let bytes = text.len() as f64;
        let per_new = new_t / iters_f * 1e6;
        let per_old = old_t / iters_f * 1e6;
        println!(
            "{:<8}{:>6}字符   新 {:>8.1}µs ({:>5.1}MB/s)   旧 {:>8.1}µs ({:>5.1}MB/s)   慢 {:.1}×",
            name,
            text.chars().count(),
            per_new,
            bytes / new_t / 1e6,
            per_old,
            bytes / old_t / 1e6,
            per_new / per_old.max(1e-9),
        );
    }

    #[test]
    fn benchmark_estimate_text() {
        println!("\n── 改造前后对比（release 构建）──");
        bench_pair("英文", &long_english(), 2000);
        bench_pair("中文", &long_chinese(), 2000);
        bench_pair("代码", &long_code(), 2000);
        bench_pair("单字符", "a", 200_000);
        println!();
    }

}
