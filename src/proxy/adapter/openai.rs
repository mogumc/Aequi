//! OpenAI 格式。
//!
//! 用户端的出入口也是这个格式，但方向相反：入口把 OpenAI 请求解析成标准模型，
//! 出口把标准响应封装回 OpenAI 结构（那部分在 [`crate::proxy::openai_in`]）。

use serde_json::{json, Map, Value};

use crate::error::{Error, Result};
use crate::model::ApiFormat;

use super::{insert_if_present, Adapter, AuthStyle, ProbeOutcome, UpstreamRequest};
use crate::proxy::scheme::{
    ChatRequest, ChatResponse, ContentPart, FinishReason, Message, Role, ToolCall, ToolDefinition,
    Usage,
};

/// 对话接口。
const CHAT_PATH: &str = "/v1/chat/completions";
/// 列模型接口，探测密钥用——不产生 token 消耗。
const MODELS_PATH: &str = "/v1/models";

pub struct OpenAiAdapter;

impl Adapter for OpenAiAdapter {
    fn format(&self) -> ApiFormat {
        ApiFormat::Openai
    }

    fn auth_style(&self) -> AuthStyle {
        AuthStyle::Bearer
    }

    fn build_chat(&self, request: &ChatRequest, model: &str) -> UpstreamRequest {
        let mut body = Map::new();
        body.insert("model".into(), json!(model));
        body.insert(
            "messages".into(),
            Value::Array(request.messages.iter().map(build_message).collect()),
        );

        if let Some(temperature) = request.temperature {
            insert_if_present(&mut body, "temperature", json!(temperature));
        }
        if let Some(top_p) = request.top_p {
            insert_if_present(&mut body, "top_p", json!(top_p));
        }
        if let Some(max_tokens) = request.max_tokens {
            insert_if_present(&mut body, "max_tokens", json!(max_tokens));
        }
        if !request.stop.is_empty() {
            insert_if_present(&mut body, "stop", json!(request.stop));
        }
        if !request.tools.is_empty() {
            insert_if_present(
                &mut body,
                "tools",
                Value::Array(request.tools.iter().map(build_tool).collect()),
            );
        }
        if let Some(choice) = &request.tool_choice {
            insert_if_present(&mut body, "tool_choice", choice.clone());
        }

        UpstreamRequest {
            path: CHAT_PATH.to_string(),
            body: Some(Value::Object(body)),
        }
    }

    fn parse_chat(&self, body: &Value) -> Result<ChatResponse> {
        let choice = body
            .get("choices")
            .and_then(|choices| choices.get(0))
            .ok_or_else(|| Error::UpstreamUnavailable("上游响应里没有 choices".to_string()))?;

        let message = choice.get("message").unwrap_or(&Value::Null);

        let content = message
            .get("content")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(|text| vec![ContentPart::text(text)])
            .unwrap_or_default();

        let tool_calls = message
            .get("tool_calls")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(parse_tool_call).collect())
            .unwrap_or_default();

        Ok(ChatResponse {
            model: text_at(body, "model"),
            content,
            tool_calls,
            finish_reason: finish_reason(choice.get("finish_reason").and_then(Value::as_str)),
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

/// 角色名。
pub(crate) fn role_name(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// 把一条标准消息转成 OpenAI 的消息对象。
fn build_message(message: &Message) -> Value {
    let mut object = Map::new();
    object.insert("role".into(), json!(role_name(message.role)));

    // 工具结果只认纯文本。
    if message.role == Role::Tool {
        object.insert("content".into(), json!(message.text()));
        if let Some(id) = &message.tool_call_id {
            object.insert("tool_call_id".into(), json!(id));
        }
    } else {
        // 没有内容就不写这个字段：带 tool_calls 的 assistant 消息本就允许没有正文，
        // 硬塞一个空串反而会被部分上游拒绝。
        if !message.content.is_empty() {
            object.insert("content".into(), build_content(&message.content));
        }
        if !message.tool_calls.is_empty() {
            object.insert(
                "tool_calls".into(),
                Value::Array(message.tool_calls.iter().map(build_tool_call).collect()),
            );
        }
    }

    if let Some(name) = &message.name {
        object.insert("name".into(), json!(name));
    }

    Value::Object(object)
}

/// 内容块：只有单段文本时用字符串，其余用数组。
///
/// 字符串形式兼容性最好，只有确实需要多模态时才升级成数组。
fn build_content(parts: &[ContentPart]) -> Value {
    if let [ContentPart::Text { text }] = parts {
        return json!(text);
    }

    Value::Array(
        parts
            .iter()
            .map(|part| match part {
                ContentPart::Text { text } => json!({ "type": "text", "text": text }),
                ContentPart::Image { url } => json!({
                    "type": "image_url",
                    "image_url": { "url": url },
                }),
            })
            .collect(),
    )
}

fn build_tool(tool: &ToolDefinition) -> Value {
    let mut function = Map::new();
    function.insert("name".into(), json!(tool.name));
    if let Some(description) = &tool.description {
        insert_if_present(&mut function, "description", json!(description));
    }
    function.insert("parameters".into(), tool.parameters.clone());

    json!({ "type": "function", "function": Value::Object(function) })
}

fn build_tool_call(call: &ToolCall) -> Value {
    json!({
        "id": call.id,
        "type": "function",
        "function": {
            "name": call.name,
            "arguments": call.arguments,
        }
    })
}

fn parse_tool_call(value: &Value) -> Option<ToolCall> {
    let function = value.get("function")?;

    Some(ToolCall {
        id: text_at(value, "id"),
        name: text_at(function, "name"),
        arguments: function
            .get("arguments")
            .map(|arguments| match arguments {
                // 有的上游返回的是对象而不是字符串，统一成字符串。
                Value::String(text) => text.clone(),
                other => other.to_string(),
            })
            .unwrap_or_default(),
    })
}

/// 结束原因映射；不认识的取值归入 `Unknown`。
pub(crate) fn finish_reason(value: Option<&str>) -> FinishReason {
    match value {
        Some("stop") => FinishReason::Stop,
        Some("length") => FinishReason::Length,
        Some("tool_calls") | Some("function_call") => FinishReason::ToolCalls,
        Some("content_filter") => FinishReason::ContentFilter,
        _ => FinishReason::Unknown,
    }
}

/// 解析用量。上游给的字段名一致，缺失时按 0 处理。
fn parse_usage(value: Option<&Value>) -> Usage {
    let Some(value) = value else {
        return Usage::default();
    };

    let prompt_tokens = value
        .get("prompt_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let completion_tokens = value
        .get("completion_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);

    Usage {
        prompt_tokens,
        completion_tokens,
        total_tokens: value
            .get("total_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(prompt_tokens + completion_tokens),
    }
}

/// 取字符串字段。
pub(crate) fn text_at(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}
