//! 转换过程的调整留痕。
//!
//! 网关有些地方会**替用户做决定**：档位取不到就降级或封顶、非标字段不认就丢掉。
//! 这些都发生在用户看不见的地方，但不留痕就查不出来——用户看到的是
//! 「设了 `high` 却只有 `medium` 的效果」，而日志里什么都没有。
//!
//! 所以统一成结构化形状（机器可读的 `code` + 字段路径 + 人话消息），
//! 而不是把消息拼成一行自由文本：`code` 与 `path` 要能按值检索，
//! 「为什么我的设置没生效」才能一眼筛出来。
//! 形状与同类网关（new-api 的 `ConversionDiagnostic`）对齐，将来若要回给前端，
//! 这一层不用重做。

/// 一次「替用户做的决定」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adjustment {
    /// 机器可读的短码，同类调整共用一个码，便于按码聚合。
    pub code: &'static str,
    /// 出问题的字段路径，例如 `generationConfig.thinkingConfig`。
    pub path: &'static str,
    /// 给运维看的人话：说清**原来是什么、变成了什么、为什么**。
    pub message: String,
}

impl Adjustment {
    pub fn new(code: &'static str, path: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            path,
            message: message.into(),
        }
    }
}

/// 档位被就近取到别的档（Anthropic 没有 `minimal`）。
pub const EFFORT_ADJUSTED: &str = "effort_adjusted";
/// 档位超出上游值域被封顶（Gemini 没有 `xhigh`）。
pub const EFFORT_CAPPED: &str = "effort_capped";
/// 上游没有「关闭思考」的能力，只能降到最省档。
pub const DISABLE_UNSUPPORTED: &str = "thinking_disable_unsupported";
/// 入口不认识该字段，已忽略。
pub const FIELD_IGNORED: &str = "reasoning_field_ignored";

/// 落一次调整。
///
/// 用结构化字段而不是把消息塞进文本：`code` 与 `path` 要能按值过滤。
/// 由调用方带上模型名——调整几乎都是「这个模型不支持那个档」，
/// 不带模型名的日志等于没记。
///
/// 接受任意 `&Adjustment` 迭代器，调用方既能直接传 `&Vec`，
/// 也能传 `Option::as_slice()`（单条调整的常见形态）。
pub(crate) fn log<'a>(model: &str, adjustments: impl IntoIterator<Item = &'a Adjustment>) {
    for adjustment in adjustments {
        tracing::warn!(
            code = adjustment.code,
            path = adjustment.path,
            model = %model,
            "{}",
            adjustment.message
        );
    }
}
