//! 用户端的 OpenAI 格式解析与封装。
//!
//! 对外承诺的协议只有一种：进来看 OpenAI，出去也吐 OpenAI。
//! 上游用哪家格式是内部实现细节，用户不需要知道。

use serde_json::{json, Map, Value};

use crate::error::{Error, Result};

use super::scheme::{
    ChatRequest, ChatResponse, ContentPart, FinishReason, Message, Role, ToolCall, ToolDefinition,
};

/// 解析用户发来的对话请求。
pub fn parse_request(body: &Value) -> Result<ChatRequest> {
    // 流式（SSE）尚未支持：显式拒绝比静默返回非流式更诚实——
    // 期待 SSE 的客户端拿到 JSON 只会解析失败，还以为是网络问题。
    if body.get("stream").and_then(Value::as_bool) == Some(true) {
        return Err(Error::BadRequest(
            "暂不支持流式响应（stream=true），请去掉该字段".to_string(),
        ));
    }

    let model = body
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .ok_or_else(|| Error::BadRequest("请求里缺少 model".to_string()))?
        .to_string();

    let raw_messages = body
        .get("messages")
        .and_then(Value::as_array)
        .filter(|messages| !messages.is_empty())
        .ok_or_else(|| Error::BadRequest("请求里缺少 messages".to_string()))?;

    let mut messages = Vec::with_capacity(raw_messages.len());
    for raw in raw_messages {
        messages.push(parse_message(raw)?);
    }

    Ok(ChatRequest {
        model,
        messages,
        tools: parse_tools(body.get("tools")),
        tool_choice: body.get("tool_choice").cloned(),
        temperature: body.get("temperature").and_then(Value::as_f64),
        top_p: body.get("top_p").and_then(Value::as_f64),
        // 新旧两套字段名都认。
        max_tokens: body
            .get("max_tokens")
            .or_else(|| body.get("max_completion_tokens"))
            .and_then(Value::as_u64)
            .map(|value| value as u32),
        stop: parse_stop(body.get("stop")),
    })
}

/// 把标准响应封装成 OpenAI 的响应体。
pub fn render_response(response: &ChatResponse, id: &str, created: i64) -> Value {
    let mut message = Map::new();
    message.insert("role".into(), json!("assistant"));
    message.insert("content".into(), json!(response.text()));

    if !response.tool_calls.is_empty() {
        message.insert(
            "tool_calls".into(),
            Value::Array(response.tool_calls.iter().map(render_tool_call).collect()),
        );
    }

    json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": response.model,
        "choices": [{
            "index": 0,
            "message": Value::Object(message),
            "finish_reason": finish_reason_name(response.finish_reason),
        }],
        "usage": {
            "prompt_tokens": response.usage.prompt_tokens,
            "completion_tokens": response.usage.completion_tokens,
            "total_tokens": response.usage.total_tokens,
        },
    })
}

/// 解析一条消息。
fn parse_message(raw: &Value) -> Result<Message> {
    let role = raw
        .get("role")
        .and_then(Value::as_str)
        .and_then(parse_role)
        .ok_or_else(|| Error::BadRequest("消息的 role 无法识别".to_string()))?;

    Ok(Message {
        role,
        content: parse_content(raw.get("content")),
        tool_calls: parse_tool_calls(raw.get("tool_calls")),
        tool_call_id: raw
            .get("tool_call_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        name: raw.get("name").and_then(Value::as_str).map(str::to_string),
    })
}

fn parse_role(value: &str) -> Option<Role> {
    match value {
        "system" | "developer" => Some(Role::System),
        "user" => Some(Role::User),
        "assistant" => Some(Role::Assistant),
        "tool" | "function" => Some(Role::Tool),
        _ => None,
    }
}

/// 内容：字符串是常见形态，数组则意味着多模态。
fn parse_content(value: Option<&Value>) -> Vec<ContentPart> {
    match value {
        Some(Value::String(text)) => vec![ContentPart::text(text.clone())],
        Some(Value::Array(items)) => items.iter().filter_map(parse_part).collect(),
        _ => Vec::new(),
    }
}

fn parse_part(item: &Value) -> Option<ContentPart> {
    match item.get("type").and_then(Value::as_str) {
        Some("text") => item
            .get("text")
            .and_then(Value::as_str)
            .map(ContentPart::text),
        Some("image_url") => item
            .pointer("/image_url/url")
            .and_then(Value::as_str)
            .map(|url| ContentPart::Image {
                url: url.to_string(),
            }),
        _ => None,
    }
}

fn parse_tool_calls(value: Option<&Value>) -> Vec<ToolCall> {
    let Some(items) = value.and_then(Value::as_array) else {
        return Vec::new();
    };

    items
        .iter()
        .filter_map(|item| {
            let function = item.get("function")?;
            Some(ToolCall {
                id: item
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                name: function.get("name").and_then(Value::as_str)?.to_string(),
                arguments: function
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("{}")
                    .to_string(),
            })
        })
        .collect()
}

/// 工具定义。只认 `function` 类型，其余类型忽略。
fn parse_tools(value: Option<&Value>) -> Vec<ToolDefinition> {
    let Some(items) = value.and_then(Value::as_array) else {
        return Vec::new();
    };

    items
        .iter()
        .filter_map(|item| {
            let function = item.get("function")?;
            Some(ToolDefinition {
                name: function.get("name").and_then(Value::as_str)?.to_string(),
                description: function
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                parameters: function
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
            })
        })
        .collect()
}

/// stop 既可以是单个字符串，也可以是数组。
fn parse_stop(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(text)) => vec![text.clone()],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

fn render_tool_call(call: &ToolCall) -> Value {
    json!({
        "id": call.id,
        "type": "function",
        "function": {
            "name": call.name,
            "arguments": call.arguments,
        },
    })
}

fn finish_reason_name(reason: FinishReason) -> &'static str {
    match reason {
        FinishReason::Stop => "stop",
        FinishReason::Length => "length",
        FinishReason::ToolCalls => "tool_calls",
        FinishReason::ContentFilter => "content_filter",
        FinishReason::Unknown => "stop",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_request_parses() {
        let body = json!({
            "model": "gpt-x",
            "messages": [{ "role": "user", "content": "你好" }]
        });

        let request = parse_request(&body).unwrap();
        assert_eq!(request.model, "gpt-x");
        assert_eq!(request.messages.len(), 1);
        assert!(request.tools.is_empty());
    }

    #[test]
    fn stream_true_is_rejected_explicitly() {
        let body = json!({
            "model": "gpt-x",
            "stream": true,
            "messages": [{ "role": "user", "content": "hi" }]
        });

        let error = parse_request(&body).unwrap_err();
        assert!(matches!(error, Error::BadRequest(_)), "stream=true 应得到明确的 400 而不是静默忽略");
    }

    #[test]
    fn stream_false_is_fine() {
        let body = json!({
            "model": "gpt-x",
            "stream": false,
            "messages": [{ "role": "user", "content": "hi" }]
        });

        assert!(parse_request(&body).is_ok());
    }

    #[test]
    fn missing_model_or_messages_is_rejected() {
        let no_model = json!({ "messages": [{ "role": "user", "content": "hi" }] });
        assert!(parse_request(&no_model).is_err());

        let no_messages = json!({ "model": "gpt-x" });
        assert!(parse_request(&no_messages).is_err());

        let empty_messages = json!({ "model": "gpt-x", "messages": [] });
        assert!(parse_request(&empty_messages).is_err());
    }
}
