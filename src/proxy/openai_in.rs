//! 用户端的 OpenAI 格式解析与封装。
//!
//! 对外承诺的协议只有一种：进来看 OpenAI，出去也吐 OpenAI。
//! 上游用哪家格式是内部实现细节，用户不需要知道。

use serde_json::{Map, Value, json};

use crate::error::{Error, Result};

use super::scheme::{
    ChatRequest, ChatResponse, ContentPart, FinishReason, Message, Role, ToolCall, ToolCallDelta,
    ToolDefinition, Usage,
};

/// 解析用户发来的对话请求。
pub fn parse_request(body: &Value) -> Result<ChatRequest> {
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

    // 推理内容按 OpenAI 兼容生态的惯例放 message.reasoning_content，
    // 与正文分开——客户端靠这个字段折叠展示思考过程。
    if !response.reasoning.is_empty() {
        message.insert("reasoning_content".into(), json!(response.reasoning));
    }

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
            // 两种形状都接受：OpenAI 的包装式（定义收在 function 子对象里）
            // 与扁平式（name/description/parameters 直接在条目上——Anthropic
            // 与不少新客户端的写法）。此前只认包装式，扁平形状的工具条目
            // 会被静默丢光，模型收不到任何工具，调用全部退化成正文文本。
            let function = item.get("function").unwrap_or(item);
            let name = function.get("name").and_then(Value::as_str)?;
            Some(ToolDefinition {
                name: name.to_string(),
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

// ---------- 流式输出（SSE） ----------

/// SSE 输出的公共骨架：`object` 固定为 `chat.completion.chunk`。
fn render_stream_base(id: &str, created: i64, model: &str) -> serde_json::Map<String, Value> {
    let mut object = serde_json::Map::new();
    object.insert("id".to_string(), json!(id));
    object.insert("object".to_string(), json!("chat.completion.chunk"));
    object.insert("created".to_string(), json!(created));
    object.insert("model".to_string(), json!(model));
    object
}

/// 首个 chunk：只带角色，客户端靠它确认流已开始。
pub fn render_stream_role(id: &str, created: i64, model: &str) -> Value {
    let mut object = render_stream_base(id, created, model);
    object.insert(
        "choices".to_string(),
        json!([{ "index": 0, "delta": { "role": "assistant" }, "finish_reason": null }]),
    );
    Value::Object(object)
}

/// 内容增量 chunk：一段思考、一段正文和/或工具调用分片。
pub fn render_stream_delta(
    id: &str,
    created: i64,
    model: &str,
    reasoning: &str,
    text: &str,
    tool_calls: &[ToolCallDelta],
) -> Value {
    let mut delta = serde_json::Map::new();
    // 空字段一律不写：OpenAI 的 chunk 约定里 absent 与 empty string
    // 对客户端的含义不同（部分 SDK 会把空串当作一次真实增量）。
    if !reasoning.is_empty() {
        delta.insert("reasoning_content".to_string(), json!(reasoning));
    }
    if !text.is_empty() {
        delta.insert("content".to_string(), json!(text));
    }
    if !tool_calls.is_empty() {
        delta.insert(
            "tool_calls".to_string(),
            Value::Array(
                tool_calls
                    .iter()
                    .map(|call| {
                        let mut function = serde_json::Map::new();
                        if let Some(name) = &call.name {
                            function.insert("name".to_string(), json!(name));
                        }
                        if let Some(fragment) = &call.arguments_fragment {
                            function.insert("arguments".to_string(), json!(fragment));
                        }

                        let mut call_object = serde_json::Map::new();
                        call_object.insert("index".to_string(), json!(call.index));
                        if let Some(id) = &call.id {
                            call_object.insert("id".to_string(), json!(id));
                            call_object.insert("type".to_string(), json!("function"));
                        }
                        call_object.insert("function".to_string(), Value::Object(function));
                        Value::Object(call_object)
                    })
                    .collect(),
            ),
        );
    }

    let mut object = render_stream_base(id, created, model);
    object.insert(
        "choices".to_string(),
        json!([{ "index": 0, "delta": Value::Object(delta), "finish_reason": null }]),
    );
    Value::Object(object)
}

/// 结束 chunk：finish_reason 就位。
pub fn render_stream_finish(id: &str, created: i64, model: &str, reason: FinishReason) -> Value {
    let mut object = render_stream_base(id, created, model);
    object.insert(
        "choices".to_string(),
        json!([{ "index": 0, "delta": {}, "finish_reason": finish_reason_name(reason) }]),
    );
    Value::Object(object)
}

/// 用量 chunk：OpenAI 的 include_usage 约定，choices 为空数组。
pub fn render_stream_usage(id: &str, created: i64, model: &str, usage: Usage) -> Value {
    let mut object = render_stream_base(id, created, model);
    object.insert("choices".to_string(), json!([]));
    object.insert(
        "usage".to_string(),
        json!({
            "prompt_tokens": usage.prompt_tokens,
            "completion_tokens": usage.completion_tokens,
            "total_tokens": usage.total_tokens,
        }),
    );
    Value::Object(object)
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
    fn tools_parse_in_both_wrapped_and_flat_shapes() {
        // OpenAI 包装式（function 子对象）与扁平式（定义直接在条目上）都接受。
        let body = json!({
            "model": "gpt-x",
            "messages": [{ "role": "user", "content": "hi" }],
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "wrapped_tool",
                        "description": "包装式",
                        "parameters": { "type": "object", "properties": {} }
                    }
                },
                {
                    "type": "function",
                    "name": "flat_tool",
                    "description": "扁平式",
                    "parameters": { "type": "object", "properties": {} }
                },
                { "name": "minimal_tool" }
            ]
        });

        let request = parse_request(&body).unwrap();
        assert_eq!(request.tools.len(), 3, "两种形状都不能丢");
        assert_eq!(request.tools[0].name, "wrapped_tool");
        assert_eq!(request.tools[1].name, "flat_tool");
        assert_eq!(request.tools[2].name, "minimal_tool");
        assert_eq!(request.tools[0].description.as_deref(), Some("包装式"));
        assert_eq!(request.tools[1].description.as_deref(), Some("扁平式"));
        assert!(request.tools[1].parameters["type"] == "object");
    }

    #[test]
    fn stream_false_or_absent_is_fine() {
        let absent = json!({
            "model": "gpt-x",
            "messages": [{ "role": "user", "content": "hi" }]
        });
        assert!(parse_request(&absent).is_ok());

        let explicit_false = json!({
            "model": "gpt-x",
            "stream": false,
            "messages": [{ "role": "user", "content": "hi" }]
        });
        assert!(parse_request(&explicit_false).is_ok());
    }

    #[test]
    fn stream_delta_renders_reasoning_and_content_separately() {
        let with_reasoning = render_stream_delta("id", 0, "m", "思考中", "", &[]);
        assert_eq!(
            with_reasoning["choices"][0]["delta"]["reasoning_content"],
            "思考中"
        );
        assert!(
            with_reasoning["choices"][0]["delta"].get("content").is_none(),
            "空字段不写：absent 与空串对客户端含义不同"
        );

        let with_text = render_stream_delta("id", 0, "m", "", "正文", &[]);
        assert_eq!(with_text["choices"][0]["delta"]["content"], "正文");
        assert!(with_text["choices"][0]["delta"]
            .get("reasoning_content")
            .is_none());
    }

    #[test]
    fn response_renders_reasoning_content_in_message() {
        let response = ChatResponse {
            model: "m".into(),
            content: vec![ContentPart::text("答案")],
            reasoning: "推理".into(),
            tool_calls: vec![],
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
        };

        let rendered = render_response(&response, "id", 0);
        assert_eq!(rendered["choices"][0]["message"]["reasoning_content"], "推理");
        assert_eq!(rendered["choices"][0]["message"]["content"], "答案");
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
