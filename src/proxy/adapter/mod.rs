//! 上游 API 格式适配。
//!
//! 每个 adapter 负责三件事：把标准请求翻译成该家的请求体、把该家的响应翻译回
//! 标准响应，以及给出一个**不消耗 token** 的探测请求供「测试密钥」使用。

mod anthropic;
mod google;
mod openai;

use serde_json::Value;

use crate::error::Result;
use crate::model::ApiFormat;

use super::scheme::{ChatRequest, ChatResponse, StreamEvent};

/// 发给上游的请求。
///
/// `body` 为 `None` 表示这是一个没有请求体的 GET，探测请求走这条路径。
#[derive(Debug, Clone)]
pub struct UpstreamRequest {
    /// 相对块 `base_url` 的路径。
    pub path: String,
    pub body: Option<Value>,
}

impl UpstreamRequest {
    /// OpenAI 流式注入 `stream_options.include_usage`——上游只有带着这个
    /// 字段才会在流末尾回 usage chunk，没有它就只能估算计费。这是 OpenAI
    /// 官方字段，但个别兼容网关不认识；因此默认注入 + 400 时剥离重试。
    /// 与旧版一致：客户端显式给过 `include_usage: false` 也会被覆盖成 true。
    pub fn inject_stream_usage(&mut self) {
        let Some(body) = &mut self.body else {
            return;
        };
        let Some(object) = body.as_object_mut() else {
            return;
        };
        if object.get("stream").and_then(Value::as_bool) != Some(true) {
            return;
        }
        let options = object
            .entry("stream_options")
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        if let Some(options) = options.as_object_mut() {
            options.insert("include_usage".to_string(), Value::Bool(true));
        }
    }

    /// 剥离注入的 `stream_options`（上游用 400 拒绝该字段时的降级重试）。
    pub fn strip_stream_usage(&mut self) {
        if let Some(body) = &mut self.body {
            if let Some(object) = body.as_object_mut() {
                object.remove("stream_options");
            }
        }
    }
}

/// 密钥在请求里的位置。各家放法不同，由 adapter 自报。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthStyle {
    /// `Authorization: Bearer <key>`
    Bearer,
    /// `x-api-key: <key>`
    XApiKey,
    /// `x-goog-api-key: <key>`
    GoogleApiKey,
}

/// 探测结果。
#[derive(Debug, Clone)]
pub enum ProbeOutcome {
    /// 密钥可用。
    Valid,
    /// 密钥本身有问题（认证失败、被吊销），应当停用它。
    Invalid(String),
    /// 暂时性问题（限流、网络、上游故障），不该牵连密钥。
    Transient(String),
}

impl ProbeOutcome {
    /// 依据 HTTP 状态码归类。
    ///
    /// 401/403 是密钥自己的问题；429 与 5xx 属于临时状况。
    pub fn from_status(status: u16, detail: impl FnOnce() -> String) -> Self {
        match status {
            200..=299 => Self::Valid,
            401 | 403 => Self::Invalid(detail()),
            429 => Self::Transient("被限流".to_string()),
            500..=599 => Self::Transient(format!("上游返回 {status}")),
            _ => Self::Transient(detail()),
        }
    }
}

/// 一种上游 API 格式。
pub trait Adapter: Send + Sync {
    /// 对应的格式标识。
    fn format(&self) -> ApiFormat;

    /// 密钥放在哪里。
    fn auth_style(&self) -> AuthStyle;

    /// 需要额外附加的固定请求头，例如 Anthropic 的版本头。
    fn extra_headers(&self) -> Vec<(&'static str, &'static str)> {
        Vec::new()
    }

    /// 生成对话请求。
    fn build_chat(&self, request: &ChatRequest, model: &str) -> UpstreamRequest;

    /// 生成流式对话请求。
    ///
    /// 默认在非流式请求体上加 `stream: true`；各家有额外开关时自行覆写。
    fn build_chat_stream(&self, request: &ChatRequest, model: &str) -> UpstreamRequest {
        let mut upstream = self.build_chat(request, model);
        if let Some(body) = &mut upstream.body {
            if let Some(object) = body.as_object_mut() {
                object.insert("stream".to_string(), Value::Bool(true));
            }
        }
        upstream
    }

    /// 把一条上游 SSE 载荷翻译成标准流事件。
    ///
    /// 一条载荷可能产出多个事件（如 Anthropic 的 message_delta 同时带来
    /// 结束原因与累计用量）；与流无关的载荷产出空 vec。
    /// 翻译必须无状态——聚合、计费与收尾都在编排层。
    fn parse_stream_event(&self, payload: &str) -> Result<Vec<StreamEvent>>;

    /// 解析对话响应。
    fn parse_chat(&self, body: &Value) -> Result<ChatResponse>;

    /// 生成探测请求。
    fn build_probe(&self) -> UpstreamRequest;

    /// 依据探测响应判断密钥状态。
    fn parse_probe(&self, status: u16, body: &Value) -> ProbeOutcome;

    /// 从列模型响应里提取模型名。
    ///
    /// 探测请求走的就是列模型接口，所以同一份响应顺手就能给出可用模型列表。
    /// 默认按 OpenAI / Anthropic 的形状解析（`data[].id`）；Google 的形状不同，自行覆写。
    fn parse_models(&self, body: &Value) -> Vec<String> {
        body.get("data")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("id").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// 按格式取对应的 adapter。
pub fn adapter_for(format: ApiFormat) -> &'static dyn Adapter {
    match format {
        ApiFormat::Openai => &openai::OpenAiAdapter,
        ApiFormat::Google => &google::GoogleAdapter,
        ApiFormat::Anthropic => &anthropic::AnthropicAdapter,
    }
}

/// 只在有值且非空时才写进请求体。
///
/// 各家对空数组、空字符串、null 的容忍度不一样，一律不送最省事——
/// 「按需插入」比「送过去再被拒」便宜得多。
pub(crate) fn insert_if_present(
    body: &mut serde_json::Map<String, Value>,
    key: &str,
    value: Value,
) {
    if is_empty(&value) {
        return;
    }
    body.insert(key.to_string(), value);
}

/// 判断一个 JSON 值是否等同于「没有内容」。
pub(crate) fn is_empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.is_empty(),
        Value::String(text) => text.is_empty(),
        _ => false,
    }
}

/// 把 JSON 字符串解析成值；解析不了就退回空对象。
///
/// 工具调用的参数在上游之间往返时只是字符串，各家的结构要求不同，
/// 这里统一按「能解析就转结构，不能就包一层」处理。
pub(crate) fn parse_arguments(arguments: &str) -> Value {
    if arguments.trim().is_empty() {
        return Value::Object(Default::default());
    }

    serde_json::from_str(arguments).unwrap_or_else(|_| Value::String(arguments.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::proxy::scheme::{ChatRequest, ContentPart, FinishReason, Message, Role};
    use crate::service::upstream::UpstreamState;

    /// 一个带 system、user、可选参数的标准请求。
    fn sample_request() -> ChatRequest {
        ChatRequest {
            model: "gpt-test".into(),
            messages: vec![
                Message {
                    role: Role::System,
                    content: vec![ContentPart::text("你是助手")],
                    tool_calls: vec![],
                    tool_call_id: None,
                    name: None,
                },
                Message {
                    role: Role::User,
                    content: vec![ContentPart::text("你好")],
                    tool_calls: vec![],
                    tool_call_id: None,
                    name: None,
                },
            ],
            tools: vec![],
            tool_choice: None,
            temperature: Some(0.7),
            top_p: None,
            max_tokens: Some(1024),
            stop: vec![],
        }
    }

    #[test]
    fn openai_keeps_system_in_messages_and_drops_empty_fields() {
        let outgoing = adapter_for(ApiFormat::Openai).build_chat(&sample_request(), "gpt-real");
        let body = outgoing.body.as_ref().unwrap();

        assert_eq!(outgoing.path, "/v1/chat/completions");
        assert_eq!(body["model"], "gpt-real");
        assert_eq!(body["temperature"], 0.7);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "你是助手");

        // 按需插入：没给的参数不出现在请求体里。
        assert!(body.get("tools").is_none());
        assert!(body.get("stop").is_none());
        assert!(body.get("top_p").is_none());
    }

    #[test]
    fn openai_single_text_content_stays_a_string() {
        let outgoing = adapter_for(ApiFormat::Openai).build_chat(&sample_request(), "gpt");
        let content = &outgoing.body.as_ref().unwrap()["messages"][1]["content"];
        assert!(
            content.is_string(),
            "纯文本消息应为字符串形式以获得最好兼容性"
        );
    }

    #[test]
    fn openai_response_parses_text_and_usage() {
        let body = json!({
            "model": "gpt-real",
            "choices": [{
                "message": { "role": "assistant", "content": "你好" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
        });

        let response = adapter_for(ApiFormat::Openai).parse_chat(&body).unwrap();
        assert_eq!(response.text(), "你好");
        assert_eq!(response.usage.total_tokens, 15);
        assert_eq!(response.finish_reason, FinishReason::Stop);
    }

    #[test]
    fn anthropic_moves_system_out_of_messages() {
        let outgoing = adapter_for(ApiFormat::Anthropic).build_chat(&sample_request(), "claude");
        let body = outgoing.body.as_ref().unwrap();

        assert_eq!(outgoing.path, "/v1/messages");
        assert_eq!(body["system"], "你是助手");
        assert_eq!(body["max_tokens"], 1024, "max_tokens 在 Anthropic 是必填的");

        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1, "system 不应出现在 messages 里");
        assert_eq!(messages[0]["role"], "user");
    }

    #[test]
    fn anthropic_wraps_tool_result_into_user_side_block() {
        let mut request = sample_request();
        request.messages.push(Message {
            role: Role::Tool,
            content: vec![ContentPart::text("工具结果")],
            tool_calls: vec![],
            tool_call_id: Some("call_1".into()),
            name: None,
        });

        let outgoing = adapter_for(ApiFormat::Anthropic).build_chat(&request, "claude");
        let messages = outgoing.body.as_ref().unwrap()["messages"]
            .as_array()
            .unwrap();
        let last = messages.last().unwrap();

        assert_eq!(last["role"], "user");
        assert_eq!(last["content"][0]["type"], "tool_result");
        assert_eq!(last["content"][0]["tool_use_id"], "call_1");
    }

    #[test]
    fn anthropic_usage_fields_are_renamed() {
        let body = json!({
            "model": "claude",
            "content": [{ "type": "text", "text": "好的" }],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 8, "output_tokens": 3 }
        });

        let response = adapter_for(ApiFormat::Anthropic).parse_chat(&body).unwrap();
        assert_eq!(response.usage.prompt_tokens, 8);
        assert_eq!(response.usage.completion_tokens, 3);
        assert_eq!(response.usage.total_tokens, 11, "总数由两项相补");
        assert_eq!(response.finish_reason, FinishReason::Stop);
    }

    #[test]
    fn google_uses_generate_content_endpoint_with_system_instruction() {
        let outgoing = adapter_for(ApiFormat::Google).build_chat(&sample_request(), "gemini-3.5");
        let body = outgoing.body.as_ref().unwrap();

        // generateContent：模型在 URL 路径里，system 折成 system_instruction。
        assert_eq!(outgoing.path, "/v1beta/models/gemini-3.5:generateContent");
        assert_eq!(
            body["system_instruction"]["parts"][0]["text"],
            "你是助手"
        );

        let contents = body["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[0]["parts"][0]["text"], "你好");
    }

    #[test]
    fn google_usage_fields_are_renamed() {
        let body = json!({
            "candidates": [{
                "content": { "parts": [{ "text": "嗯" }] },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 6,
                "candidatesTokenCount": 2,
                "totalTokenCount": 8
            }
        });

        let response = adapter_for(ApiFormat::Google).parse_chat(&body).unwrap();
        assert_eq!(response.usage.total_tokens, 8, "总数由两项相补");
        assert_eq!(response.finish_reason, FinishReason::Stop);
    }

    #[test]
    fn scheduler_rotates_even_weights_in_order() {
        let state = UpstreamState::default();
        let candidates = vec![("a".to_string(), 1), ("b".to_string(), 1)];

        let mut picks = Vec::new();
        for _ in 0..4 {
            picks.push(state.pick(&candidates).unwrap());
        }

        assert_eq!(picks, vec!["a", "b", "a", "b"], "等权重应当轮流");
    }

    #[test]
    fn scheduler_gives_heavy_candidates_their_share() {
        let state = UpstreamState::default();
        let candidates = vec![("light".to_string(), 1), ("heavy".to_string(), 3)];

        let mut heavy = 0;
        for _ in 0..8 {
            if state.pick(&candidates).unwrap() == "heavy" {
                heavy += 1;
            }
        }

        assert_eq!(heavy, 6, "8 次里权重 3:1 应分到 6:2");
    }

    #[test]
    fn scheduler_returns_none_without_candidates() {
        let state = UpstreamState::default();
        assert!(state.pick(&[]).is_none());
    }

    #[test]
    fn probe_classifies_auth_failures_as_invalid() {
        assert!(matches!(
            ProbeOutcome::from_status(401, || "bad key".to_string()),
            ProbeOutcome::Invalid(_)
        ));
        assert!(matches!(
            ProbeOutcome::from_status(429, || "slow down".to_string()),
            ProbeOutcome::Transient(_)
        ));
        assert!(matches!(
            ProbeOutcome::from_status(200, || String::new()),
            ProbeOutcome::Valid
        ));
    }
}
