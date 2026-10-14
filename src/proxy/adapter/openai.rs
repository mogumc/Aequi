//! OpenAI 格式。
//!
//! 用户端的出入口也是这个格式，但方向相反：入口把 OpenAI 请求解析成标准模型，
//! 出口把标准响应封装回 OpenAI 结构（那部分在 [`crate::proxy::openai_in`]）。

use serde_json::{Map, Value, json};

use crate::error::{Error, Result};
use crate::model::ApiFormat;

use super::{Adapter, AuthStyle, ProbeOutcome, UpstreamRequest, insert_if_present};
use crate::proxy::scheme::{
    ChatRequest, ChatResponse, ContentPart, FinishReason, Message, Role, StreamEvent, ToolCall,
    ToolCallDelta, ToolDefinition, Usage,
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
            // 按客户端用的字段名回写：o 系与 gpt-5 只认 max_completion_tokens，
            // 一律写成 max_tokens 会被上游判「不支持的参数」，上限等于没设。
            insert_if_present(
                &mut body,
                request.max_tokens_field.name(),
                json!(max_tokens),
            );
        }
        if let Some(reasoning) = &request.reasoning {
            // 本家格式一字不改地转发：网关不换算、也不添上游没要的字段。
            // 档位与模型世代的适配（如 gpt-5-pro 只支持 high）由上游自己报错。
            insert_if_present(&mut body, "reasoning_effort", json!(reasoning.effort));
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

    fn build_chat_stream(&self, request: &ChatRequest, model: &str) -> UpstreamRequest {
        let mut upstream = self.build_chat(request, model);
        if let Some(body) = &mut upstream.body {
            if let Some(object) = body.as_object_mut() {
                object.insert("stream".to_string(), Value::Bool(true));
            }
        }
        upstream
    }

    fn parse_stream_event(&self, payload: &str) -> Result<Vec<StreamEvent>> {
        if payload.trim() == "[DONE]" {
            return Ok(vec![StreamEvent::End]);
        }

        let value: Value = serde_json::from_str(payload)
            .map_err(|error| Error::UpstreamUnavailable(format!("上游流载荷解析失败: {error}")))?;

        // 只含 usage 的收尾 chunk（include_usage 模式）。
        if value
            .get("choices")
            .and_then(Value::as_array)
            .is_none_or(|c| c.is_empty())
        {
            let usage = parse_usage(value.get("usage"));
            if usage.total_tokens > 0 {
                return Ok(vec![StreamEvent::Usage(usage)]);
            }
            return Ok(Vec::new());
        }

        let choice = &value["choices"][0];
        let delta = choice.get("delta").cloned().unwrap_or(Value::Null);

        // 推理内容：DeepSeek / QwQ / GLM / Moonshot 一类用 reasoning_content，
        // 部分网关（如 OpenRouter）用 reasoning。两个字段名都认，前者优先。
        let reasoning = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let text = delta
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let tool_calls: Vec<ToolCallDelta> = delta
            .get("tool_calls")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|call| {
                        let function = call.get("function")?;
                        Some(ToolCallDelta {
                            index: call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize,
                            id: call.get("id").and_then(Value::as_str).map(str::to_string),
                            name: function
                                .get("name")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                            arguments_fragment: function
                                .get("arguments")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        let mut events = Vec::new();
        if !reasoning.is_empty() || !text.is_empty() || !tool_calls.is_empty() {
            events.push(StreamEvent::Delta {
                reasoning,
                text,
                tool_calls,
            });
        }

        // usage 不一定只出现在 choices 为空的收尾 chunk——不少 OpenAI 兼容
        // 网关把它塞在最后一个普通 chunk 里。宽松提取，merge_usage 取较大
        // 值，多次出现不重复计。
        let usage = parse_usage(value.get("usage"));
        if usage.total_tokens > 0 {
            events.push(StreamEvent::Usage(usage));
        }

        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            events.push(StreamEvent::Finish {
                finish_reason: finish_reason(Some(reason)),
            });
        }

        Ok(events)
    }

    fn parse_chat(&self, body: &Value) -> Result<ChatResponse> {
        let choice = body
            .get("choices")
            .and_then(|choices| choices.get(0))
            .ok_or_else(|| Error::UpstreamUnavailable("上游响应里没有 choices".to_string()))?;

        let message = choice.get("message").unwrap_or(&Value::Null);

        // 与流式口径一致：reasoning_content 优先，reasoning 兜底。
        let reasoning = message
            .get("reasoning_content")
            .or_else(|| message.get("reasoning"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
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
            content,
            reasoning,
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

/// 解析用量。缓存命中在 `prompt_tokens_details.cached_tokens` 里。
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
        cache_tokens: value
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        estimated: false,
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
