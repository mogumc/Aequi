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
    Text { text: String },
    /// 图片。`url` 可以是 http(s) 地址，也可以是 `data:` 形式的 base64。
    Image { url: String },
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
}

impl Usage {
    /// 上游只给了其中两项时，补出总数。
    pub fn new(prompt_tokens: i64, completion_tokens: i64) -> Self {
        Self {
            prompt_tokens,
            completion_tokens,
            total_tokens: prompt_tokens + completion_tokens,
        }
    }
}

/// 标准对话响应。
#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub model: String,
    pub content: Vec<ContentPart>,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: FinishReason,
    pub usage: Usage,
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
}
