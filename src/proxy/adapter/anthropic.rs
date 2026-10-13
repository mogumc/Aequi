//! Anthropic 格式。
//!
//! 与 OpenAI 的结构差异集中在三处：system 不在 messages 里而要单独提取；
//! 工具结果要包成 user 消息里的 `tool_result` 块；`max_tokens` 是必填的。

use serde_json::{Map, Value, json};

use crate::error::{Error, Result};
use crate::model::ApiFormat;

use super::openai::text_at;
use super::{
    Adapter, AuthStyle, ProbeOutcome, UpstreamRequest, insert_if_present, parse_arguments,
};
use crate::proxy::scheme::{
    ChatRequest, ChatResponse, ContentPart, FinishReason, Message, Role, StreamEvent, ToolCall,
    ToolCallDelta, ToolDefinition, Usage,
};

/// 对话接口。
const CHAT_PATH: &str = "/v1/messages";
/// 列模型接口，探测密钥用。
const MODELS_PATH: &str = "/v1/models";
/// Anthropic 要求带版本头。
const API_VERSION: &str = "2023-06-01";
/// 未指定 max_tokens 时的兜底值——这个字段在 Anthropic 是必填的。
const DEFAULT_MAX_TOKENS: u32 = 4096;

pub struct AnthropicAdapter;

impl Adapter for AnthropicAdapter {
    fn format(&self) -> ApiFormat {
        ApiFormat::Anthropic
    }

    fn auth_style(&self) -> AuthStyle {
        AuthStyle::XApiKey
    }

    fn extra_headers(&self) -> Vec<(&'static str, &'static str)> {
        vec![("anthropic-version", API_VERSION)]
    }

    fn build_chat(&self, request: &ChatRequest, model: &str) -> UpstreamRequest {
        let mut body = Map::new();
        body.insert("model".into(), json!(model));
        body.insert(
            "max_tokens".into(),
            json!(request.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS)),
        );

        if let Some(system) = request.system_prompt() {
            insert_if_present(&mut body, "system", json!(system));
        }

        let messages = build_messages(request);
        if !messages.is_empty() {
            body.insert("messages".into(), Value::Array(messages));
        }

        if let Some(temperature) = request.temperature {
            insert_if_present(&mut body, "temperature", json!(temperature));
        }
        if let Some(top_p) = request.top_p {
            insert_if_present(&mut body, "top_p", json!(top_p));
        }
        if !request.stop.is_empty() {
            // Anthropic 管这个叫 stop_sequences。
            insert_if_present(&mut body, "stop_sequences", json!(request.stop));
        }
        if !request.tools.is_empty() {
            insert_if_present(
                &mut body,
                "tools",
                Value::Array(request.tools.iter().map(build_tool).collect()),
            );
        }
        if let Some(choice) = request.tool_choice.as_ref().and_then(convert_tool_choice) {
            body.insert("tool_choice".into(), choice);
        }

        UpstreamRequest {
            path: CHAT_PATH.to_string(),
            body: Some(Value::Object(body)),
        }
    }

    fn build_chat_stream(&self, request: &ChatRequest, model: &str) -> UpstreamRequest {
        let mut upstream = self.build_chat(request, model);
        if let Some(body) = &mut upstream.body {
            if let Some(object) = body.as_object_mut() {
                object.insert("stream".to_string(), Value::Bool(true));
            }
        }
        upstream
    }

    /// Anthropic 的流事件：content_block_start / content_block_delta /
    /// message_delta / message_stop。
    ///
    /// 工具调用的 index 直接沿用 Anthropic 的内容块序号——客户端按 index
    /// 聚合分片，序号在同一流内一致即可，不必从 0 连续。
    fn parse_stream_event(&self, payload: &str) -> Result<Vec<StreamEvent>> {
        let value: Value = serde_json::from_str(payload)
            .map_err(|error| Error::UpstreamUnavailable(format!("上游流载荷解析失败: {error}")))?;

        match value.get("type").and_then(Value::as_str) {
            Some("content_block_start") => {
                let block = value.get("content_block").cloned().unwrap_or(Value::Null);
                // 只有 tool_use 需要在这里亮相（id/name 在块头，参数随后才到）。
                // thinking 块不带可转发信息：内容全在后续的 thinking_delta 里，
                // 签名在 signature_delta 里（OpenAI 协议没有放它的位置，跳过）。
                if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                    return Ok(Vec::new());
                }

                let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                Ok(vec![StreamEvent::Delta {
                    reasoning: String::new(),
                    text: String::new(),
                    tool_calls: vec![ToolCallDelta {
                        index,
                        id: Some(text_at(&block, "id")),
                        name: Some(text_at(&block, "name")),
                        arguments_fragment: None,
                    }],
                }])
            }
            Some("content_block_delta") => {
                let delta = value.get("delta").cloned().unwrap_or(Value::Null);
                let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;

                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        let text = delta.get("text").and_then(Value::as_str).unwrap_or("");
                        if text.is_empty() {
                            Ok(Vec::new())
                        } else {
                            Ok(vec![StreamEvent::Delta {
                                reasoning: String::new(),
                                text: text.to_string(),
                                tool_calls: Vec::new(),
                            }])
                        }
                    }
                    // 扩展思考的增量。文本在 delta.thinking 里而不是 delta.text——
                    // 只认 text_delta 会把整个思考过程静默丢掉。
                    Some("thinking_delta") => {
                        let thinking = delta.get("thinking").and_then(Value::as_str).unwrap_or("");
                        if thinking.is_empty() {
                            Ok(Vec::new())
                        } else {
                            Ok(vec![StreamEvent::Delta {
                                reasoning: thinking.to_string(),
                                text: String::new(),
                                tool_calls: Vec::new(),
                            }])
                        }
                    }
                    Some("input_json_delta") => {
                        let fragment = delta
                            .get("partial_json")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if fragment.is_empty() {
                            Ok(Vec::new())
                        } else {
                            Ok(vec![StreamEvent::Delta {
                                reasoning: String::new(),
                                text: String::new(),
                                tool_calls: vec![ToolCallDelta {
                                    index,
                                    id: None,
                                    name: None,
                                    arguments_fragment: Some(fragment.to_string()),
                                }],
                            }])
                        }
                    }
                    // signature_delta 是思考块的校验签名，OpenAI 协议里没有
                    // 对应位置，跳过（redacted_thinking 是密文，同样无可转发）。
                    _ => Ok(Vec::new()),
                }
            }
            // 结束原因与累计的输出用量在同一条 message_delta 里，产出两个事件。
            Some("message_delta") => {
                let mut events = Vec::new();

                if let Some(reason) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    events.push(StreamEvent::Finish {
                        finish_reason: finish_reason(Some(reason)),
                    });
                }

                // 与非流式同一个口径：input_tokens 只是未命中缓存的那段，
                // 三段相加才是完整输入（见 `total_input_tokens`）。
                let prompt_tokens = value
                    .pointer("/usage")
                    .map(total_input_tokens)
                    .unwrap_or(0);
                let completion_tokens = value
                    .pointer("/usage/output_tokens")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                // 新版 API 在 message_delta 里也带累计的缓存字段；老版没有就取 0，
                // 真正的值来自 message_start（见下），merge_usage 按字段取较大值，
                // 两处都读不会重复计。只认**读取命中**，写入不算（见 parse_usage）。
                let cache_tokens = value
                    .pointer("/usage/cache_read_input_tokens")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                if prompt_tokens > 0 || completion_tokens > 0 {
                    events.push(StreamEvent::Usage(Usage {
                        prompt_tokens,
                        completion_tokens,
                        total_tokens: prompt_tokens + completion_tokens,
                        cache_tokens,
                        estimated: false,
                    }));
                }

                Ok(events)
            }
            // 输入用量在流开头就报了，与 message_delta 的输出分开——编排层聚合。
            Some("message_start") => {
                let usage = value.pointer("/message/usage");
                // 同非流式：三段输入相加，缓存命中另外单列。
                let prompt_tokens = usage.map(total_input_tokens).unwrap_or(0);
                // 缓存命中就在这一条里（cache_read_input_tokens）。此前这里恒写 0，
                // 于是**流式请求的缓存列永远是 0**——而 Claude 的输入侧用量
                // 全在 message_start 报，流式又是绝大多数客户端的走法。
                // 只取读取命中：`cache_creation_input_tokens` 是本次正在写入缓存的部分，
                // 本次并未命中任何缓存，不进缓存列（它已被 total_input_tokens 算进输入）。
                let cache_tokens = usage
                    .and_then(|u| u.get("cache_read_input_tokens"))
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                if prompt_tokens > 0 || cache_tokens > 0 {
                    Ok(vec![StreamEvent::Usage(Usage {
                        prompt_tokens,
                        completion_tokens: 0,
                        total_tokens: 0,
                        cache_tokens,
                        estimated: false,
                    })])
                } else {
                    Ok(Vec::new())
                }
            }
            Some("message_stop") => Ok(vec![StreamEvent::End]),
            _ => Ok(Vec::new()),
        }
    }

    fn parse_chat(&self, body: &Value) -> Result<ChatResponse> {
        let blocks = body
            .get("content")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::UpstreamUnavailable("上游响应里没有 content".to_string()))?;

        let mut content = Vec::new();
        let mut reasoning = String::new();
        let mut tool_calls = Vec::new();

        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        if !text.is_empty() {
                            content.push(ContentPart::text(text));
                        }
                    }
                }
                // 扩展思考的完整块：文本在 block.thinking 里而不是 block.text。
                Some("thinking") => {
                    if let Some(text) = block.get("thinking").and_then(Value::as_str) {
                        reasoning.push_str(text);
                    }
                }
                Some("tool_use") => tool_calls.push(ToolCall {
                    id: text_at(block, "id"),
                    name: text_at(block, "name"),
                    // Anthropic 给的是结构化 input，归一回 JSON 字符串。
                    arguments: block
                        .get("input")
                        .map(Value::to_string)
                        .unwrap_or_else(|| "{}".to_string()),
                }),
                _ => {}
            }
        }

        Ok(ChatResponse {
            model: text_at(body, "model"),
            content,
            reasoning,
            tool_calls,
            finish_reason: finish_reason(body.get("stop_reason").and_then(Value::as_str)),
            usage: parse_usage(body.get("usage")),
        })
    }

    fn build_probe(&self) -> UpstreamRequest {
        UpstreamRequest {
            path: MODELS_PATH.to_string(),
            body: None,
        }
    }

    fn parse_probe(&self, status: u16, body: &Value) -> ProbeOutcome {
        ProbeOutcome::from_status(status, || {
            body.pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("密钥被上游拒绝")
                .to_string()
        })
    }
}

/// 把标准消息转成 Anthropic 的 messages。
fn build_messages(request: &ChatRequest) -> Vec<Value> {
    let mut messages = Vec::new();

    for message in request.dialogue() {
        let value = match message.role {
            // 工具结果在 Anthropic 里属于 user 侧。
            Role::Tool => json!({
                "role": "user",
                "content": [build_tool_result(message)],
            }),
            Role::Assistant => {
                let mut blocks = build_blocks(&message.content);
                blocks.extend(message.tool_calls.iter().map(build_tool_use));

                if blocks.is_empty() {
                    continue;
                }
                json!({ "role": "assistant", "content": blocks })
            }
            _ => {
                let blocks = build_blocks(&message.content);
                if blocks.is_empty() {
                    continue;
                }
                json!({ "role": "user", "content": blocks })
            }
        };

        messages.push(value);
    }

    messages
}

fn build_tool_result(message: &Message) -> Value {
    let mut block = Map::new();
    block.insert("type".into(), json!("tool_result"));
    if let Some(id) = &message.tool_call_id {
        block.insert("tool_use_id".into(), json!(id));
    }
    block.insert("content".into(), json!(message.text()));

    Value::Object(block)
}

fn build_tool_use(call: &ToolCall) -> Value {
    json!({
        "type": "tool_use",
        "id": call.id,
        "name": call.name,
        "input": parse_arguments(&call.arguments),
    })
}

/// 内容块转 Anthropic 的写法。
fn build_blocks(parts: &[ContentPart]) -> Vec<Value> {
    parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(json!({ "type": "text", "text": text })),
            ContentPart::Image { url } => build_image(url),
        })
        .collect()
}

/// Anthropic 的图片只接受 base64。
///
/// `data:` 形式可以直接拆开；http 地址它接不了，只能跳过并留下警告——
/// 静默丢内容比报错更难排查。
fn build_image(url: &str) -> Option<Value> {
    let Some(rest) = url.strip_prefix("data:") else {
        tracing::warn!("Anthropic 只接受 base64 图片，已跳过一张 http 图片");
        return None;
    };

    let (meta, data) = rest.split_once(',')?;
    let media_type = meta.split(';').next()?;

    Some(json!({
        "type": "image",
        "source": { "type": "base64", "media_type": media_type, "data": data },
    }))
}

/// Anthropic 的工具定义没有 `function` 这一层包装，schema 字段叫 `input_schema`。
fn build_tool(tool: &ToolDefinition) -> Value {
    let mut object = Map::new();
    object.insert("name".into(), json!(tool.name));
    if let Some(description) = &tool.description {
        insert_if_present(&mut object, "description", json!(description));
    }
    object.insert("input_schema".into(), tool.parameters.clone());

    Value::Object(object)
}

/// OpenAI 的 tool_choice 写法与 Anthropic 不同，需要转换。
fn convert_tool_choice(choice: &Value) -> Option<Value> {
    match choice {
        Value::String(text) => match text.as_str() {
            "auto" => Some(json!({ "type": "auto" })),
            "required" | "any" => Some(json!({ "type": "any" })),
            // `none` 与未知取值都不约束，Anthropic 也不需要这个字段。
            _ => None,
        },
        Value::Object(_) => choice
            .pointer("/function/name")
            .and_then(Value::as_str)
            .map(|name| json!({ "type": "tool", "name": name })),
        _ => None,
    }
}

/// 结束原因映射。
fn finish_reason(value: Option<&str>) -> FinishReason {
    match value {
        Some("end_turn") | Some("stop_sequence") => FinishReason::Stop,
        Some("max_tokens") => FinishReason::Length,
        Some("tool_use") => FinishReason::ToolCalls,
        _ => FinishReason::Unknown,
    }
}

/// 把 Anthropic 的三段输入合成 `prompt_tokens`。
///
/// Claude 与另两家**口径相反**：它的 `input_tokens` **不含**缓存部分，
/// 官方定义是「最后一个缓存断点之后的 token」，并要求
/// `total_input = input_tokens + cache_read_input_tokens + cache_creation_input_tokens`。
/// 而 OpenAI 的 `prompt_tokens`、Google 的 `promptTokenCount` 都把缓存算在里面。
///
/// 出口要统一，这里就按后者的口径相加：输入列显示的是**完整输入**，
/// 缓存列另外单独记「命中」的那部分（`cached ⊆ prompt` 成立，缓存率才是可读的
/// ≤100% 的比值）。相加后缓存 token 也随输入进入计费总数——与另两家一致。
///
/// `cache_creation_input_tokens` 同样相加：它是**这次请求正在写入缓存**的部分，
/// 属于本次输入（上游也按输入计费），只是还不属于「读取命中」——
/// 见 `Usage::cache_tokens` 的说明，那个字段只认命中。
fn total_input_tokens(usage: &Value) -> i64 {
    let field = |key: &str| usage.get(key).and_then(Value::as_i64).unwrap_or(0);
    field("input_tokens") + field("cache_read_input_tokens") + field("cache_creation_input_tokens")
}

/// 字段名与 OpenAI 不同：input_tokens / output_tokens；
/// 缓存命中只看**读取**（`cache_read_input_tokens`）——
/// 写入（`cache_creation_input_tokens`）算输入但不算命中，不进 `cache_tokens`。
fn parse_usage(value: Option<&Value>) -> Usage {
    let Some(value) = value else {
        return Usage::default();
    };

    let prompt_tokens = total_input_tokens(value);
    let completion_tokens = value
        .get("output_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let cache_tokens = value
        .get("cache_read_input_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);

    Usage {
        prompt_tokens,
        completion_tokens,
        total_tokens: prompt_tokens + completion_tokens,
        cache_tokens,
        estimated: false,
    }
}
