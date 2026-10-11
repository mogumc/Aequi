//! Google（Gemini）格式。
//!
//! 结构差异比 Anthropic 还大：模型名写在**路径**里而不是请求体里；轮次的角色只有
//! `user` / `model` 两种；工具结果靠**函数名**关联（它不返回调用 id）；
//! 采样参数统一收在 `generationConfig` 下。

use serde_json::{json, Map, Value};

use crate::error::{Error, Result};
use crate::model::ApiFormat;

use super::openai::text_at;
use super::{
    insert_if_present, parse_arguments, Adapter, AuthStyle, ProbeOutcome, UpstreamRequest,
};
use crate::proxy::scheme::{
    ChatRequest, ChatResponse, ContentPart, FinishReason, Message, Role, StreamEvent, ToolCall,
    ToolCallDelta, ToolDefinition, Usage,
};

/// 列模型接口，探测密钥用。
const MODELS_PATH: &str = "/v1beta/models";

pub struct GoogleAdapter;

impl Adapter for GoogleAdapter {
    fn format(&self) -> ApiFormat {
        ApiFormat::Google
    }

    fn auth_style(&self) -> AuthStyle {
        AuthStyle::GoogleApiKey
    }

    fn build_chat(&self, request: &ChatRequest, model: &str) -> UpstreamRequest {
        let mut body = Map::new();

        let contents = build_contents(request);
        if !contents.is_empty() {
            body.insert("contents".into(), Value::Array(contents));
        }

        if let Some(system) = request.system_prompt() {
            body.insert(
                "systemInstruction".into(),
                json!({ "parts": [{ "text": system }] }),
            );
        }

        if !request.tools.is_empty() {
            body.insert(
                "tools".into(),
                json!([{
                    "functionDeclarations": request.tools.iter().map(build_tool).collect::<Vec<_>>(),
                }]),
            );
        }

        let mut config = Map::new();
        if let Some(temperature) = request.temperature {
            insert_if_present(&mut config, "temperature", json!(temperature));
        }
        if let Some(top_p) = request.top_p {
            insert_if_present(&mut config, "topP", json!(top_p));
        }
        if let Some(max_tokens) = request.max_tokens {
            insert_if_present(&mut config, "maxOutputTokens", json!(max_tokens));
        }
        if !request.stop.is_empty() {
            insert_if_present(&mut config, "stopSequences", json!(request.stop));
        }
        if !config.is_empty() {
            body.insert("generationConfig".into(), Value::Object(config));
        }

        UpstreamRequest {
            // Google 把模型名放在路径里。
            path: format!("/v1beta/models/{model}:generateContent"),
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
        // alt=sse 让 streamGenerateContent 以 SSE 而不是 JSON 数组流返回。
        upstream.path = format!("{}?alt=sse", upstream.path);
        upstream
    }

    /// Google 的流 chunk：candidates[].content.parts 里的文本与完整
    /// functionCall，收尾 chunk 带 finishReason 与 usageMetadata。
    ///
    /// 限制：functionCall 不分片且不带序号，index 恒为 0——同一流内
    /// 多个工具调用会被客户端聚合到同一槽位，真实场景里极少发生。
    fn parse_stream_event(&self, payload: &str) -> Result<Vec<StreamEvent>> {
        let value: Value = serde_json::from_str(payload).map_err(|error| {
            Error::UpstreamUnavailable(format!("上游流载荷解析失败: {error}"))
        })?;

        let candidate = value.get("candidates").and_then(|c| c.get(0));
        let mut events = Vec::new();

        if let Some(parts) = candidate.and_then(|c| c.pointer("/content/parts")).and_then(Value::as_array) {
            let mut text = String::new();
            let mut tool_calls = Vec::new();

            for part in parts {
                if let Some(fragment) = part.get("text").and_then(Value::as_str) {
                    text.push_str(fragment);
                }
                if let Some(call) = part.get("functionCall") {
                    let name = text_at(call, "name");
                    tool_calls.push(ToolCallDelta {
                        index: 0,
                        id: Some(format!("call_{name}")),
                        name: Some(name),
                        arguments_fragment: call
                            .get("args")
                            .map(Value::to_string),
                    });
                }
            }

            if !text.is_empty() || !tool_calls.is_empty() {
                events.push(StreamEvent::Delta { text, tool_calls });
            }
        }

        if let Some(reason) = candidate
            .and_then(|c| c.get("finishReason"))
            .and_then(Value::as_str)
        {
            events.push(StreamEvent::Finish {
                finish_reason: finish_reason(Some(reason)),
            });
        }

        if let Some(usage) = value.get("usageMetadata") {
            let prompt_tokens = usage.get("promptTokenCount").and_then(Value::as_i64).unwrap_or(0);
            let completion_tokens = usage
                .get("candidatesTokenCount")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let cache_tokens = usage
                .get("cachedContentTokenCount")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            if prompt_tokens > 0 || completion_tokens > 0 {
                events.push(StreamEvent::Usage(Usage {
                    prompt_tokens,
                    completion_tokens,
                    total_tokens: usage
                        .get("totalTokenCount")
                        .and_then(Value::as_i64)
                        .unwrap_or(prompt_tokens + completion_tokens),
                    cache_tokens,
                    estimated: false,
                }));
            }
        }

        Ok(events)
    }

    fn parse_chat(&self, body: &Value) -> Result<ChatResponse> {
        let candidate = body
            .get("candidates")
            .and_then(|candidates| candidates.get(0))
            .ok_or_else(|| Error::UpstreamUnavailable("上游响应里没有 candidates".to_string()))?;

        let parts = candidate
            .pointer("/content/parts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let mut content = Vec::new();
        let mut tool_calls = Vec::new();

        for part in &parts {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                if !text.is_empty() {
                    content.push(ContentPart::text(text));
                }
            }

            if let Some(call) = part.get("functionCall") {
                let name = text_at(call, "name");
                tool_calls.push(ToolCall {
                    // Google 不返回调用 id：用函数名凑一个稳定标识，
                    // 回传结果时它也只认名字，不认 id。
                    id: format!("call_{name}"),
                    name,
                    arguments: call
                        .get("args")
                        .map(Value::to_string)
                        .unwrap_or_else(|| "{}".to_string()),
                });
            }
        }

        Ok(ChatResponse {
            model: text_at(body, "modelVersion"),
            content,
            tool_calls,
            finish_reason: finish_reason(candidate.get("finishReason").and_then(Value::as_str)),
            usage: parse_usage(body.get("usageMetadata")),
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

/// 轮次：角色只有 user / model，工具结果属于 user 侧的 part。
fn build_contents(request: &ChatRequest) -> Vec<Value> {
    let mut contents = Vec::new();

    for message in request.dialogue() {
        let parts = match message.role {
            Role::Assistant => {
                let mut parts = build_parts(&message.content);
                parts.extend(message.tool_calls.iter().map(build_function_call));
                parts
            }
            Role::Tool => vec![build_function_response(message)],
            _ => build_parts(&message.content),
        };

        if parts.is_empty() {
            continue;
        }

        contents.push(json!({
            "role": if message.role == Role::Assistant { "model" } else { "user" },
            "parts": parts,
        }));
    }

    contents
}

fn build_parts(parts: &[ContentPart]) -> Vec<Value> {
    parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(json!({ "text": text })),
            ContentPart::Image { url } => build_image(url),
        })
        .collect()
}

/// Google 的图片同样走 base64。
fn build_image(url: &str) -> Option<Value> {
    let Some(rest) = url.strip_prefix("data:") else {
        tracing::warn!("Google 只接受 base64 图片，已跳过一张 http 图片");
        return None;
    };

    let (meta, data) = rest.split_once(',')?;
    let mime_type = meta.split(';').next()?;

    Some(json!({ "inlineData": { "mimeType": mime_type, "data": data } }))
}

fn build_function_call(call: &ToolCall) -> Value {
    json!({
        "functionCall": {
            "name": call.name,
            "args": parse_arguments(&call.arguments),
        }
    })
}

/// 工具结果。Google 用函数名关联，所以要求调用方在消息里带上名字。
fn build_function_response(message: &Message) -> Value {
    json!({
        "functionResponse": {
            "name": message.name.clone().unwrap_or_default(),
            "response": { "content": message.text() },
        }
    })
}

/// 工具定义：没有 `function` 包装层，schema 字段就叫 `parameters`。
fn build_tool(tool: &ToolDefinition) -> Value {
    let mut object = Map::new();
    object.insert("name".into(), json!(tool.name));
    if let Some(description) = &tool.description {
        insert_if_present(&mut object, "description", json!(description));
    }
    object.insert("parameters".into(), tool.parameters.clone());

    Value::Object(object)
}

/// 结束原因：Google 用大写下划线风格。
fn finish_reason(value: Option<&str>) -> FinishReason {
    match value {
        Some("STOP") => FinishReason::Stop,
        Some("MAX_TOKENS") => FinishReason::Length,
        Some("SAFETY") | Some("RECITATION") | Some("BLOCKLIST") => FinishReason::ContentFilter,
        _ => FinishReason::Unknown,
    }
}

/// 用量字段名与 OpenAI 完全不同，缓存命中在 cachedContentTokenCount。
fn parse_usage(value: Option<&Value>) -> Usage {
    let Some(value) = value else {
        return Usage::default();
    };

    let prompt_tokens = value
        .get("promptTokenCount")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let completion_tokens = value
        .get("candidatesTokenCount")
        .and_then(Value::as_i64)
        .unwrap_or(0);

    Usage {
        prompt_tokens,
        completion_tokens,
        total_tokens: value
            .get("totalTokenCount")
            .and_then(Value::as_i64)
            .unwrap_or(prompt_tokens + completion_tokens),
        cache_tokens: value
            .get("cachedContentTokenCount")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        estimated: false,
    }
}
