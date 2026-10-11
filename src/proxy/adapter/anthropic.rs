//! Anthropic 格式。
//!
//! 与 OpenAI 的结构差异集中在三处：system 不在 messages 里而要单独提取；
//! 工具结果要包成 user 消息里的 `tool_result` 块；`max_tokens` 是必填的。

use serde_json::{json, Map, Value};

use crate::error::{Error, Result};
use crate::model::ApiFormat;

use super::openai::text_at;
use super::{
    insert_if_present, parse_arguments, Adapter, AuthStyle, ProbeOutcome, UpstreamRequest,
};
use crate::proxy::scheme::{
    ChatRequest, ChatResponse, ContentPart, FinishReason, Message, Role, ToolCall, ToolDefinition,
    Usage,
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

    fn parse_chat(&self, body: &Value) -> Result<ChatResponse> {
        let blocks = body
            .get("content")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::UpstreamUnavailable("上游响应里没有 content".to_string()))?;

        let mut content = Vec::new();
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

/// 字段名与 OpenAI 不同：input_tokens / output_tokens。
fn parse_usage(value: Option<&Value>) -> Usage {
    let Some(value) = value else {
        return Usage::default();
    };

    Usage::new(
        value
            .get("input_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        value
            .get("output_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0),
    )
}
