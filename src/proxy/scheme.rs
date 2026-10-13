//! 标准对话模型（内部统一格式）。
//!
//! 用户端只认 OpenAI 格式，上游却可能是 OpenAI / Google / Anthropic 里任意一家。
//! 若直接写「三家互相转换」就是六条路径，加一家还要再补两条。
//!
//! 所以中间放一层标准模型：入口把用户请求解析成标准模型，出口由对应的 adapter
//! 从标准模型生成上游请求体；响应反着走一遍。新增一种格式只需要多写一个 adapter。

/// 对话角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
    /// 工具调用结果。
    Tool,
}

/// 消息内容块。
///
/// 内容用「块的列表」而不是一个字符串，多模态才有位置可放。
#[derive(Debug, Clone)]
pub enum ContentPart {
    Text {
        text: String,
    },
    /// 图片。`url` 可以是 http(s) 地址，也可以是 `data:` 形式的 base64。
    Image {
        url: String,
    },
}

impl ContentPart {
    /// 纯文本块的便捷构造。
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }
}

/// 一次工具调用。
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// 调用参数。统一用 JSON 字符串表示——OpenAI 格式本就如此，
    /// 其它格式解析后也归一到字符串，回写时再按各家要求转换。
    pub arguments: String,
}

/// 可供模型调用的工具。
#[derive(Debug, Clone)]
pub struct ToolDefinition {
    pub name: String,
    pub description: Option<String>,
    /// JSON Schema，原样透传给上游。
    pub parameters: serde_json::Value,
}

/// 一条对话消息。
#[derive(Debug, Clone)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentPart>,
    /// assistant 消息里发起的工具调用。
    pub tool_calls: Vec<ToolCall>,
    /// role 为 [`Role::Tool`] 时，对应哪次调用。
    pub tool_call_id: Option<String>,
    pub name: Option<String>,
}

impl Message {
    /// 取纯文本内容，用于把 system 消息折叠成单串这类场景。
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text } => Some(text.as_str()),
                ContentPart::Image { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// 标准对话请求。
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    /// 工具选择的约束，原样透传（各家表达不同，adapter 自己决定用不用）。
    pub tool_choice: Option<serde_json::Value>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub max_tokens: Option<u32>,
    pub stop: Vec<String>,
}

impl ChatRequest {
    /// 拼出上游要的 system 提示。
    ///
    /// OpenAI 与 Anthropic 都把 system 单独放，Google 亦如此；
    /// 多条 system 消息按出现顺序拼接。
    pub fn system_prompt(&self) -> Option<String> {
        let parts: Vec<String> = self
            .messages
            .iter()
            .filter(|message| message.role == Role::System)
            .map(|message| message.text())
            .filter(|text| !text.is_empty())
            .collect();

        if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n\n"))
        }
    }

    /// 除去 system 之后的消息，即上游真正按轮次看待的部分。
    pub fn dialogue(&self) -> impl Iterator<Item = &Message> {
        self.messages
            .iter()
            .filter(|message| message.role != Role::System)
    }
}

/// 结束原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishReason {
    /// 正常结束。
    Stop,
    /// 触达长度上限。
    Length,
    /// 模型要求调用工具。
    ToolCalls,
    /// 被内容策略拦下。
    ContentFilter,
    /// 上游给了不认识的值。
    Unknown,
}

/// token 用量。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    /// 命中缓存的输入 token——**读取**命中，才叫命中。
    ///
    /// 各家字段不同，这里归一：
    /// OpenAI `prompt_tokens_details.cached_tokens`、Anthropic
    /// `cache_read_input_tokens`、Google `cachedContentTokenCount`。
    ///
    /// 注意 Anthropic 的 `cache_creation_input_tokens` **不进这一列**：
    /// 它是**这次请求正在写入缓存**的部分（下次请求才可能读到），
    /// 本次并没有「命中」任何东西。它仍然算输入（并入 `prompt_tokens`），
    /// 但不该被记成缓存命中——否则命中率会把「写入量」算进去，越写越高。
    pub cache_tokens: i64,
    /// true 表示这套数字是本地估算的，不是上游报的。
    pub estimated: bool,
}

impl Usage {
    /// 上游只给了其中两项时，补出总数。
    pub fn new(prompt_tokens: i64, completion_tokens: i64) -> Self {
        Self {
            prompt_tokens,
            completion_tokens,
            total_tokens: prompt_tokens + completion_tokens,
            cache_tokens: 0,
            estimated: false,
        }
    }

    /// 本地估算出来的用量（见 [`crate::proxy::estimate`]）。
    pub fn estimated(prompt_tokens: i64, completion_tokens: i64) -> Self {
        Self {
            prompt_tokens,
            completion_tokens,
            total_tokens: prompt_tokens + completion_tokens,
            cache_tokens: 0,
            estimated: true,
        }
    }
}

/// 标准对话响应。
#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub model: String,
    pub content: Vec<ContentPart>,
    /// 推理（思考）内容。与正文分开存放——客户端普遍按
    /// `reasoning_content` 区分展示，混进正文会让思考模型无法折叠显示。
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: FinishReason,
    pub usage: Usage,
}

/// 流式输出的一个工具调用增量。
///
/// 与 OpenAI 的分片约定一致：客户端按 `index` 拼接 `arguments_fragment`。
#[derive(Debug, Clone)]
pub struct ToolCallDelta {
    pub index: usize,
    pub id: Option<String>,
    pub name: Option<String>,
    pub arguments_fragment: Option<String>,
}

/// 上游流式事件翻译后的标准形态。
///
/// adapter 只负责把各家的 SSE 载荷翻译成这些事件；
/// 增量聚合、计费与输出收尾由编排层统一处理。
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// 内容增量：一段思考、一段正文和/或工具调用分片。
    ///
    /// `reasoning` 与 `text` 分开：三家上游对思考都有独立通道
    /// （OpenAI 系 `delta.reasoning_content`、Anthropic `thinking_delta`、
    /// Gemini 带 `thought` 标志的 part），客户端也按 `reasoning_content`
    /// 单独渲染。合并成一路就再也拆不开。
    Delta {
        reasoning: String,
        text: String,
        tool_calls: Vec<ToolCallDelta>,
    },
    /// 内容结束，结束原因就位。
    Finish { finish_reason: FinishReason },
    /// 工具调用的权威全集（来自上游的收尾事件，如 Interactions 的
    /// interaction.completed）。
    ///
    /// 分片增量可能因上游实现差异而未被识别；编排层只在**之前没有收到过
    /// 任何分片**时才使用这个全集补发，因此不会与增量重复。
    Snapshot { tool_calls: Vec<ToolCall> },
    /// 用量上报（通常在流末尾出现一次）。
    Usage(Usage),
    /// 上游声明流结束（OpenAI 的 `data: [DONE]`；其它家流自然结束时不发）。
    End,
}

impl ChatResponse {
    /// 把文本内容拼成一串，供日志与简单场景使用。
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text } => Some(text.as_str()),
                ContentPart::Image { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    /// 估算口径的完整输出：推理 + 正文。
    ///
    /// 思考 token 也是输出成本——上游没报用量时不能只算可见正文，
    /// 否则推理模型的思考部分等于免费（旧版的口径同样是全算）。
    /// 注意与 [`ChatResponse::text`] 分开：那个是给用户看的内容，
    /// 不能把推理混进去。
    pub fn output_for_estimate(&self) -> String {
        format!("{}{}", self.reasoning, self.text())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 估算口径必须包含推理；给用户看的 text() 必须不包含。
    /// 两者混用会直接造成计费缺口或正文污染。
    #[test]
    fn estimate_output_includes_reasoning_but_text_does_not() {
        let response = ChatResponse {
            model: "m".into(),
            content: vec![ContentPart::text("答案")],
            reasoning: "推理过程".into(),
            tool_calls: vec![],
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
        };

        assert_eq!(response.text(), "答案");
        assert_eq!(response.output_for_estimate(), "推理过程答案");
    }
}
