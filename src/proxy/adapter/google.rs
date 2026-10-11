//! Google（Gemini）格式 —— v1beta Interactions API。
//!
//! 为什么不是 `generateContent`：新版 Interactions API 的请求形态完全是另一套
//! 「事件流」模型（`input` 数组 + `steps` 输出），而且对字段极其严格——
//! `generateContent` 的 body 里多一个 `stream` 字段都会被 400 拒掉
//! （「Unknown name "stream": Cannot find field」）。流式与非流式共用
//! 同一个端点，区别只在 body 里的 `stream` 布尔。
//!
//! 工具调用在这个 API 下也格外严格：`function_call` / `function_result`
//! 都不能出现在输入里，所以多轮工具对话要把「assistant 的调用」折叠掉、
//! 把「工具结果」伪装成一段 `user_input` 文本补回去。

use std::collections::HashMap;

use serde_json::{json, Map, Value};

use crate::error::{Error, Result};
use crate::model::ApiFormat;

use super::{Adapter, AuthStyle, ProbeOutcome, UpstreamRequest};
use crate::proxy::scheme::{
    ChatRequest, ChatResponse, ContentPart, FinishReason, Role, StreamEvent, ToolCall,
    ToolCallDelta, Usage,
};

/// 列模型接口，探测密钥用（Interactions API 下它没有变）。
const MODELS_PATH: &str = "/v1beta/models";

/// Interactions API 端点。流式与非流式同一个，区别在 body 的 `stream` 字段。
const INTERACTIONS_PATH: &str = "/v1beta/interactions";

pub struct GoogleAdapter;

impl Adapter for GoogleAdapter {
    fn format(&self) -> ApiFormat {
        ApiFormat::Google
    }

    fn auth_style(&self) -> AuthStyle {
        AuthStyle::GoogleApiKey
    }

    /// Interactions API 要求带版本头，缺了会被拒。
    fn extra_headers(&self) -> Vec<(&'static str, &'static str)> {
        vec![("api-revision", "2026-05-20")]
    }

    fn build_chat(&self, request: &ChatRequest, model: &str) -> UpstreamRequest {
        UpstreamRequest {
            path: INTERACTIONS_PATH.to_string(),
            body: Some(build_body(request, model, false)),
        }
    }

    /// 覆写默认实现：这里不能往 body 里塞一个 `stream` 就完事——
    /// 端点本身倒是同一个，但 body 形状要整体重建，交给 `build_body`。
    fn build_chat_stream(&self, request: &ChatRequest, model: &str) -> UpstreamRequest {
        UpstreamRequest {
            path: INTERACTIONS_PATH.to_string(),
            body: Some(build_body(request, model, true)),
        }
    }

    fn parse_stream_event(&self, payload: &str) -> Result<Vec<StreamEvent>> {
        let value: Value = serde_json::from_str(payload)
            .map_err(|error| Error::UpstreamUnavailable(format!("上游流载荷不是 JSON: {error}")))?;

        let mut events = Vec::new();

        match value.get("event_type").and_then(Value::as_str).unwrap_or("") {
            // function_call 的开始：先发出调用身份（id 与名字），
            // 参数随后以 arguments_delta 分片到达。
            "step.start" => {
                if let Some(step) = value.get("step") {
                    if step.get("type").and_then(Value::as_str) == Some("function_call") {
                        events.push(StreamEvent::Delta {
                            text: String::new(),
                            tool_calls: vec![ToolCallDelta {
                                index: 0,
                                id: step.get("id").and_then(Value::as_str).map(str::to_string),
                                name: step.get("name").and_then(Value::as_str).map(str::to_string),
                                arguments_fragment: Some(String::new()),
                            }],
                        });
                    }
                }
            }
            "step.delta" => {
                if let Some(delta) = value.get("delta") {
                    match delta.get("type").and_then(Value::as_str).unwrap_or("") {
                        "text" => {
                            let text = delta.get("text").and_then(Value::as_str).unwrap_or("");
                            if !text.is_empty() {
                                events.push(StreamEvent::Delta {
                                    text: text.to_string(),
                                    tool_calls: vec![],
                                });
                            }
                        }
                        "arguments_delta" => {
                            let args = delta.get("arguments").and_then(Value::as_str).unwrap_or("");
                            events.push(StreamEvent::Delta {
                                text: String::new(),
                                tool_calls: vec![ToolCallDelta {
                                    index: 0,
                                    id: None,
                                    name: None,
                                    arguments_fragment: Some(args.to_string()),
                                }],
                            });
                        }
                        // thought / image 等增量与对话输出无关，跳过。
                        _ => {}
                    }
                }
            }
            "step.stop" => {}
            "interaction.completed" => {
                let interaction = value.get("interaction");
                let has_calls = interaction
                    .and_then(|i| i.get("steps"))
                    .and_then(Value::as_array)
                    .map(|steps| {
                        steps
                            .iter()
                            .any(|s| s.get("type").and_then(Value::as_str) == Some("function_call"))
                    })
                    .unwrap_or(false);
                let status = interaction
                    .and_then(|i| i.get("status"))
                    .and_then(Value::as_str)
                    .unwrap_or("completed");

                let finish = if has_calls {
                    FinishReason::ToolCalls
                } else if status == "completed" {
                    FinishReason::Stop
                } else {
                    // 安全拦截、取消等非正常状态。
                    FinishReason::ContentFilter
                };
                events.push(StreamEvent::Finish {
                    finish_reason: finish,
                });

                // 用量嵌在 interaction.usage 里；thought 是真实的输出成本，并入计费。
                let usage = interaction.and_then(|i| i.get("usage"));
                let input = usage
                    .and_then(|u| u.get("total_input_tokens"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let output = usage
                    .and_then(|u| u.get("total_output_tokens"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let thought = usage
                    .and_then(|u| u.get("total_thought_tokens"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                events.push(StreamEvent::Usage(Usage::new(
                    input as i64,
                    (output + thought) as i64,
                )));
            }
            // interaction.created 与其它未知事件不含对话内容。
            _ => {}
        }

        Ok(events)
    }

    fn parse_chat(&self, body: &Value) -> Result<ChatResponse> {
        if let Some(message) = body.pointer("/error/message").and_then(Value::as_str) {
            return Err(Error::UpstreamUnavailable(format!(
                "上游返回错误: {message}"
            )));
        }

        let status = body
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("completed");
        let mut content = Vec::new();
        let mut tool_calls = Vec::new();

        for step in body
            .get("steps")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            match step.get("type").and_then(Value::as_str).unwrap_or("") {
                "model_output" => {
                    for block in step
                        .get("content")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        if block.get("type").and_then(Value::as_str) == Some("text") {
                            if let Some(text) = block.get("text").and_then(Value::as_str) {
                                content.push(ContentPart::text(text));
                            }
                        }
                    }
                }
                "function_call" => {
                    tool_calls.push(ToolCall {
                        id: step.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
                        name: step.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                        arguments: serde_json::to_string(
                            &step.get("arguments").cloned().unwrap_or(json!({})),
                        )
                        .unwrap_or_default(),
                    });
                }
                _ => {}
            }
        }

        let finish_reason = if !tool_calls.is_empty() {
            FinishReason::ToolCalls
        } else if status == "completed" {
            FinishReason::Stop
        } else {
            FinishReason::ContentFilter
        };

        let usage = body.get("usage");
        let input = usage
            .and_then(|u| u.get("total_input_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let output = usage
            .and_then(|u| u.get("total_output_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let thought = usage
            .and_then(|u| u.get("total_thought_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);

        Ok(ChatResponse {
            model: String::new(),
            content,
            tool_calls,
            finish_reason,
            usage: Usage::new(input as i64, (output + thought) as i64),
        })
    }

    fn build_probe(&self) -> UpstreamRequest {
        UpstreamRequest {
            path: MODELS_PATH.to_string(),
            body: None,
        }
    }

    /// Google 返回的是 `{ "models": [{ "name": "models/gemini-2.0-flash", ... }] }`，
    /// 名字带 `models/` 前缀，要剥掉才是调用时用的模型名。
    fn parse_models(&self, body: &Value) -> Vec<String> {
        body.get("models")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("name").and_then(Value::as_str))
                    .map(|name| name.strip_prefix("models/").unwrap_or(name).to_string())
                    .collect()
            })
            .unwrap_or_default()
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

// ---------- 请求体 ----------

/// 把标准请求折成 Interactions API 的 body。
///
/// 多轮工具对话的难点在 `input`：这个 API 拒绝 `function_call` 与
/// `function_result` 形态的输入，所以 assistant 的调用只留下 id→名字的映射，
/// 工具结果伪装成一段 `[Tool {name} result]` 的 `user_input` 文本补回去。
fn build_body(request: &ChatRequest, model: &str, stream: bool) -> Value {
    let mut input = Vec::new();
    let mut system_parts: Vec<String> = Vec::new();
    let mut call_names: HashMap<String, String> = HashMap::new();

    for message in &request.messages {
        match message.role {
            Role::System => {
                let text = message.text();
                if !text.is_empty() {
                    system_parts.push(text);
                }
            }
            Role::User => push_input(&mut input, "user_input", &message.content),
            Role::Assistant => {
                if message.tool_calls.is_empty() {
                    push_input(&mut input, "model_output", &message.content);
                } else {
                    for call in &message.tool_calls {
                        call_names.insert(call.id.clone(), call.name.clone());
                    }
                }
            }
            Role::Tool => {
                let name = message
                    .name
                    .clone()
                    .or_else(|| {
                        message
                            .tool_call_id
                            .as_ref()
                            .and_then(|id| call_names.get(id).cloned())
                    })
                    .unwrap_or_default();
                let text = message.text();
                // 空结果不发——content 为空的条目会被整个拒掉。
                if text.is_empty() {
                    continue;
                }
                let text = if name.is_empty() {
                    text
                } else {
                    format!("[Tool {name} result]\n{text}")
                };
                input.push(json!({
                    "type": "user_input",
                    "content": [{"type": "text", "text": text}],
                }));
            }
        }
    }

    let mut body = json!({
        "model": model,
        "input": input,
        // 不存储交互历史，每次请求都自带完整上下文。
        "store": false,
    });
    if stream {
        body["stream"] = json!(true);
    }
    if !system_parts.is_empty() {
        body["system_instruction"] = json!(system_parts.join("\n\n"));
    }

    if !request.tools.is_empty() {
        // 工具是扁平的 {type:"function", name, description?, parameters}，
        // JSON Schema 里它不认的字段（$schema、additionalProperties）要剥掉。
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                let mut item = json!({
                    "type": "function",
                    "name": tool.name,
                    "parameters": strip_unsupported_schema(&tool.parameters),
                });
                if let Some(description) = &tool.description {
                    item["description"] = json!(description);
                }
                item
            })
            .collect();
        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }
    }

    let mut config = Map::new();
    if let Some(choice) = &request.tool_choice {
        let mapped = match choice {
            Value::String(text) => match text.as_str() {
                "auto" => Some("auto"),
                "required" | "any" => Some("any"),
                "none" => Some("none"),
                _ => None,
            },
            // 结构化的指定（具体到某个函数）映射成「必须调用」。
            Value::Object(_) => Some("any"),
            _ => None,
        };
        if let Some(mapped) = mapped {
            config.insert("tool_choice".to_string(), json!(mapped));
        }
    }
    if let Some(max_tokens) = request.max_tokens {
        config.insert("max_output_tokens".to_string(), json!(max_tokens));
    }
    if let Some(temperature) = request.temperature {
        config.insert("temperature".to_string(), json!(temperature));
    }
    if let Some(top_p) = request.top_p {
        config.insert("top_p".to_string(), json!(top_p));
    }
    if !request.stop.is_empty() {
        config.insert("stop_sequences".to_string(), json!(request.stop));
    }
    if !config.is_empty() {
        body["generation_config"] = Value::Object(config);
    }

    body
}

fn push_input(input: &mut Vec<Value>, kind: &str, parts: &[ContentPart]) {
    let content = content_blocks(parts);
    // content 为空的条目会被整个拒掉，宁可不发。
    if content.is_empty() {
        return;
    }
    input.push(json!({ "type": kind, "content": content }));
}

fn content_blocks(parts: &[ContentPart]) -> Vec<Value> {
    parts.iter().filter_map(content_block).collect()
}

fn content_block(part: &ContentPart) -> Option<Value> {
    match part {
        ContentPart::Text { text } => {
            if text.is_empty() {
                None
            } else {
                Some(json!({ "type": "text", "text": text }))
            }
        }
        ContentPart::Image { url } => image_block(url),
    }
}

/// 只接受 `data:` 形式的 base64 图片；http 地址发不了（Google 侧只收内联数据），
/// 入口层本就对此有告警，这里静默跳过。
fn image_block(url: &str) -> Option<Value> {
    let rest = url.strip_prefix("data:")?;
    let (mime, data) = rest.split_once(',')?;
    let mime = mime.split(';').next()?;
    if !mime.starts_with("image/") || data.is_empty() {
        return None;
    }
    Some(json!({ "type": "image", "mime_type": mime, "data": data }))
}

/// 剥掉 JSON Schema 里 Interactions API 不认的字段（递归）。
fn strip_unsupported_schema(params: &Value) -> Value {
    match params {
        Value::Object(map) => {
            let mut cleaned = Map::new();
            for (key, value) in map {
                if matches!(key.as_str(), "$schema" | "additionalProperties") {
                    continue;
                }
                cleaned.insert(key.clone(), strip_unsupported_schema(value));
            }
            Value::Object(cleaned)
        }
        _ => params.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::scheme::{Message, ToolCall, ToolDefinition};

    fn request(messages: Vec<Message>) -> ChatRequest {
        ChatRequest {
            model: "gemini-3.5-flash-lite".to_string(),
            messages,
            tools: vec![],
            tool_choice: None,
            temperature: None,
            top_p: None,
            max_tokens: None,
            stop: vec![],
        }
    }

    fn user_message(text: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentPart::text(text)],
            tool_calls: vec![],
            tool_call_id: None,
            name: None,
        }
    }

    #[test]
    fn tool_result_becomes_user_input_not_function_result() {
        // v1beta Interactions API 拒绝 function_result 输入；
        // 工具结果必须伪装成 user_input 文本，并带上调用方名字。
        let request = request(vec![
            user_message("查下天气"),
            Message {
                role: Role::Assistant,
                content: vec![],
                tool_calls: vec![ToolCall {
                    id: "call-1".to_string(),
                    name: "get_weather".to_string(),
                    arguments: "{}".to_string(),
                }],
                tool_call_id: None,
                name: None,
            },
            Message {
                role: Role::Tool,
                content: vec![ContentPart::text("晴，25 度")],
                tool_calls: vec![],
                tool_call_id: Some("call-1".to_string()),
                name: None,
            },
        ]);

        let body = build_body(&request, "gemini-3.5-flash-lite", false);
        let input = body["input"].as_array().unwrap();

        // assistant 的 function_call 被折叠掉，工具结果以 user_input 出现。
        assert_eq!(input.len(), 2, "input 里不允许出现 function_call/result");
        assert_eq!(input[0]["type"], "user_input");
        assert_eq!(input[1]["type"], "user_input");
        assert_eq!(
            input[1]["content"][0]["text"],
            "[Tool get_weather result]\n晴，25 度"
        );
        // 系统级字段
        assert_eq!(body["store"], false);
        assert!(body.get("stream").is_none(), "非流式 body 不该有 stream 字段");
    }

    #[test]
    fn stream_flag_goes_in_body_and_tool_schema_is_stripped() {
        let mut request = request(vec![user_message("hi")]);
        request.tools = vec![ToolDefinition {
            name: "get_weather".to_string(),
            description: Some("查询天气".to_string()),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "additionalProperties": false,
                "$schema": "https://json-schema.org/draft/2020-12/schema",
            }),
        }];

        let body = build_body(&request, "gemini-3.5-flash-lite", true);
        assert_eq!(body["stream"], true);

        let tool = &body["tools"][0];
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["name"], "get_weather");
        assert!(tool["parameters"].get("additionalProperties").is_none());
        assert!(tool["parameters"].get("$schema").is_none());
        assert!(tool["parameters"]["properties"]["city"].is_object(), "合法字段要保留");
    }

    #[test]
    fn stream_events_translate_to_deltas_and_usage() {
        let adapter = GoogleAdapter;

        // 文本增量
        let events = adapter
            .parse_stream_event(r#"{"event_type":"step.delta","delta":{"type":"text","text":"你好"}}"#)
            .unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], StreamEvent::Delta { text, .. } if text == "你好"));

        // 参数增量
        let events = adapter
            .parse_stream_event(
                r#"{"event_type":"step.delta","delta":{"type":"arguments_delta","arguments":"{\"city\""}}"#,
            )
            .unwrap();
        assert!(matches!(
            &events[0],
            StreamEvent::Delta { tool_calls, .. } if tool_calls[0].arguments_fragment.as_deref() == Some("{\"city\"")
        ));

        // 完成：结束原因 + 用量（thought 并入 completion）
        let events = adapter
            .parse_stream_event(
                r#"{"event_type":"interaction.completed","interaction":{"status":"completed","steps":[{"type":"model_output"}],"usage":{"total_input_tokens":10,"total_output_tokens":5,"total_thought_tokens":7,"total_tokens":22}}}"#,
            )
            .unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], StreamEvent::Finish { finish_reason: FinishReason::Stop }));
        assert!(
            matches!(&events[1], StreamEvent::Usage(u) if u.prompt_tokens == 10 && u.completion_tokens == 12),
            "thought tokens 要并入 completion 参与计费"
        );

        // function_call 存在时结束原因是 tool_calls
        let events = adapter
            .parse_stream_event(
                r#"{"event_type":"interaction.completed","interaction":{"status":"completed","steps":[{"type":"function_call","id":"c1","name":"f","arguments":{}}]}}"#,
            )
            .unwrap();
        assert!(matches!(&events[0], StreamEvent::Finish { finish_reason: FinishReason::ToolCalls }));

        // 非正常状态 → content_filter
        let events = adapter
            .parse_stream_event(
                r#"{"event_type":"interaction.completed","interaction":{"status":"blocked","steps":[]}}"#,
            )
            .unwrap();
        assert!(matches!(&events[0], StreamEvent::Finish { finish_reason: FinishReason::ContentFilter }));
    }

    #[test]
    fn chat_response_extracts_text_and_tool_calls() {
        let adapter = GoogleAdapter;
        let body = serde_json::json!({
            "id": "abc",
            "status": "completed",
            "steps": [
                {"type": "model_output", "content": [{"type": "text", "text": "今天"}]},
                {"type": "model_output", "content": [{"type": "text", "text": "晴朗"}]},
                {"type": "function_call", "id": "c1", "name": "get_weather", "arguments": {"city": "北京"}}
            ],
            "usage": {"total_input_tokens": 8, "total_output_tokens": 3, "total_thought_tokens": 1, "total_tokens": 12}
        });

        let response = adapter.parse_chat(&body).unwrap();
        assert!(matches!(response.finish_reason, FinishReason::ToolCalls));
        assert_eq!(response.content.len(), 2);
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, "get_weather");
        assert_eq!(response.usage.prompt_tokens, 8);
        assert_eq!(response.usage.completion_tokens, 4, "thought 并入 completion");
    }

    #[test]
    fn error_shape_is_recognized() {
        let adapter = GoogleAdapter;
        let body = serde_json::json!({"error": {"message": "API key not valid"}});
        assert!(adapter.parse_chat(&body).is_err());
    }
}
