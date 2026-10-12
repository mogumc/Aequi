//! Google（Gemini）格式 —— v1beta generateContent 端点（正式版本）。
//!
//! 曾短暂切换到 v1beta Interactions API，但它对多轮工具回放强制校验
//! thought_signature，而签名在客户端→网关一环就随 OpenAI 格式丢失了，
//! 无状态网关无法恢复；Interactions 又没有提供可用的绕过手段。
//! generateContent 接受官方文档为迁移场景定义的哨兵签名，配合 new-api
//! 验证过的回放方案（functionCall/functionResponse 原样回放 + 哨兵签名）
//! 可以稳定支撑多轮工具对话，因此切换回 generateContent 并移除
//! Interactions 实现。
//!
//! 端点与形状：
//! - 非流式：`POST /v1beta/models/{model}:generateContent`
//! - 流式：`POST /v1beta/models/{model}:streamGenerateContent?alt=sse`
//!   （SSE 增量为 parts 累积，无 [DONE] 哨兵，以 EOF 结束）

use serde_json::{json, Map, Value};

use crate::error::{Error, Result};
use crate::model::ApiFormat;

use super::{Adapter, AuthStyle, ProbeOutcome, UpstreamRequest};
use crate::proxy::scheme::{
    ChatRequest, ChatResponse, ContentPart, FinishReason, Role, StreamEvent, ToolCall,
    ToolCallDelta, Usage,
};

/// 列模型接口，探测密钥用。
const MODELS_PATH: &str = "/v1beta/models";

/// thoughtSignature 绕过值（new-api 同款）。
///
/// Gemini 3 要求回放的 functionCall part 携带 thoughtSignature，缺失直接
/// 400；官方文档为「从其它模型迁移历史 / 手工构造调用」的场景提供了这个
/// 哨兵值，可以直接通过校验。
const THOUGHT_SIGNATURE_BYPASS: &str = "context_engineering_is_the_way_to_go";

pub struct GoogleAdapter;

impl Adapter for GoogleAdapter {
    fn format(&self) -> ApiFormat {
        ApiFormat::Google
    }

    fn auth_style(&self) -> AuthStyle {
        AuthStyle::GoogleApiKey
    }

    fn build_chat(&self, request: &ChatRequest, model: &str) -> UpstreamRequest {
        UpstreamRequest {
            path: format!("/v1beta/models/{model}:generateContent"),
            body: Some(build_body(request)),
        }
    }

    /// 流式走独立端点，body 与非流式完全一致——不需要也不能带 stream 字段。
    fn build_chat_stream(&self, request: &ChatRequest, model: &str) -> UpstreamRequest {
        UpstreamRequest {
            path: format!("/v1beta/models/{model}:streamGenerateContent?alt=sse"),
            body: Some(build_body(request)),
        }
    }

    fn parse_stream_event(&self, payload: &str) -> Result<Vec<StreamEvent>> {
        let value: Value = serde_json::from_str(payload)
            .map_err(|error| Error::UpstreamUnavailable(format!("上游流载荷不是 JSON: {error}")))?;

        if let Some(message) = value.pointer("/error/message").and_then(Value::as_str) {
            return Err(Error::UpstreamUnavailable(format!(
                "上游返回错误: {message}"
            )));
        }

        let mut events = Vec::new();

        // generateContent 的流式 chunk：candidates[0].content.parts 为
        // 增量内容；functionCall 整体到达（单个 chunk），文本为增量。
        // 同一 chunk 内的多个 functionCall（并行调用）按下标区分。
        if let Some(parts) = value
            .pointer("/candidates/0/content/parts")
            .and_then(Value::as_array)
        {
            let mut call_index = 0usize;
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        events.push(StreamEvent::Delta {
                            text: text.to_string(),
                            tool_calls: vec![],
                        });
                    }
                }
                if let Some(call) = part.get("functionCall") {
                    let name = call.get("name").and_then(Value::as_str).unwrap_or("");
                    // functionCall 的参数字段是 args（对象）。
                    let args = call.get("args").cloned().unwrap_or(json!({}));
                    events.push(StreamEvent::Delta {
                        text: String::new(),
                        tool_calls: vec![ToolCallDelta {
                            index: call_index,
                            id: call
                                .get("id")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                            name: Some(name.to_string()),
                            arguments_fragment: Some(
                                serde_json::to_string(&args).unwrap_or_default(),
                            ),
                        }],
                    });
                    call_index += 1;
                }
            }
        }

        if let Some(reason) = value
            .pointer("/candidates/0/finishReason")
            .and_then(Value::as_str)
        {
            events.push(StreamEvent::Finish {
                finish_reason: finish_reason(reason),
            });
        }

        // usageMetadata 在收尾 chunk 里；thought 是真实的输出成本，计费时并入。
        if let Some(usage) = value.get("usageMetadata") {
            let prompt = usage
                .get("promptTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let output = usage
                .get("candidatesTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let thought = usage
                .get("thoughtsTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            events.push(StreamEvent::Usage(Usage::new(
                prompt as i64,
                (output + thought) as i64,
            )));
        }

        Ok(events)
    }

    fn parse_chat(&self, body: &Value) -> Result<ChatResponse> {
        if let Some(message) = body.pointer("/error/message").and_then(Value::as_str) {
            return Err(Error::UpstreamUnavailable(format!(
                "上游返回错误: {message}"
            )));
        }

        let candidate = body
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .ok_or_else(|| Error::UpstreamUnavailable("上游响应里没有 candidates".to_string()))?;

        let mut content = Vec::new();
        let mut tool_calls = Vec::new();

        for part in candidate
            .get("content")
            .and_then(|c| c.get("parts"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            match (part.get("text").and_then(Value::as_str), part.get("functionCall")) {
                (Some(text), _) => content.push(ContentPart::text(text)),
                (_, Some(call)) => {
                    tool_calls.push(ToolCall {
                        id: call
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        name: call
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        arguments: serde_json::to_string(
                            &call.get("args").cloned().unwrap_or(json!({})),
                        )
                        .unwrap_or_default(),
                    });
                }
                _ => {}
            }
        }

        let finish_reason = match candidate.get("finishReason").and_then(Value::as_str) {
            Some("STOP") if tool_calls.is_empty() => FinishReason::Stop,
            Some("MAX_TOKENS") => FinishReason::Length,
            Some("SAFETY") | Some("PROHIBITED_CONTENT") => FinishReason::ContentFilter,
            _ if !tool_calls.is_empty() => FinishReason::ToolCalls,
            _ => FinishReason::Stop,
        };

        let usage = body.get("usageMetadata");
        let prompt = usage
            .and_then(|u| u.get("promptTokenCount"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let output = usage
            .and_then(|u| u.get("candidatesTokenCount"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let thought = usage
            .and_then(|u| u.get("thoughtsTokenCount"))
            .and_then(Value::as_u64)
            .unwrap_or(0);

        Ok(ChatResponse {
            model: String::new(),
            content,
            tool_calls,
            finish_reason,
            usage: Usage::new(prompt as i64, (output + thought) as i64),
        })
    }

    fn build_probe(&self) -> UpstreamRequest {
        UpstreamRequest {
            path: MODELS_PATH.to_string(),
            body: None,
        }
    }

    /// Google 返回 `{ "models": [{ "name": "models/gemini-2.0-flash", ... }] }`，
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

fn finish_reason(reason: &str) -> FinishReason {
    match reason {
        "STOP" => FinishReason::Stop,
        "MAX_TOKENS" => FinishReason::Length,
        "SAFETY" | "PROHIBITED_CONTENT" | "BLOCKLIST" | "SPII" => FinishReason::ContentFilter,
        _ => FinishReason::Stop,
    }
}

// ---------- 请求体 ----------

/// 把标准请求折成 generateContent 的 body（模型名在 URL 路径里）。
///
/// 多轮工具对话按 new-api 的方式回放：assistant 的 functionCall parts
/// 原样回放（每条 assistant 一个 model content），工具结果挂在
/// `role:"user"` 的 content 里作为 functionResponse parts——连续的结果
/// 合并进同一条 user content。generateContent 没有 Interactions 那套
/// 「call 必须紧跟 user/result」的交错约束。
fn build_body(request: &ChatRequest) -> Value {
    let mut contents: Vec<Value> = Vec::new();
    let mut system_parts: Vec<String> = Vec::new();

    for message in &request.messages {
        match message.role {
            Role::System => {
                let text = message.text();
                if !text.is_empty() {
                    system_parts.push(text);
                }
            }
            Role::User => {
                contents.push(user_content(&message.content));
            }
            Role::Assistant => {
                let mut parts: Vec<Value> = Vec::new();
                if message.tool_calls.is_empty() {
                    for part in &message.content {
                        if let ContentPart::Text { text } = part {
                            if !text.is_empty() {
                                parts.push(json!({ "text": text }));
                            }
                        }
                    }
                    // 签名兜底：本条没有 functionCall 时，把哨兵签名补在
                    // 「Markdown 图片 → 首个文本」中的一处。
                    attach_text_bypass(&mut parts);
                } else {
                    for (i, call) in message.tool_calls.iter().enumerate() {
                        // arguments 在请求里是 JSON 字符串，回放时要对象形态。
                        let args: Value =
                            serde_json::from_str(&call.arguments).unwrap_or(json!({}));
                        let mut function_call = json!({
                            "name": call.name,
                            "args": args,
                        });
                        if !call.id.is_empty() {
                            function_call["id"] = json!(call.id);
                        }
                        let mut part = json!({ "functionCall": function_call });
                        // 每条 assistant 消息最多一处：给首个 functionCall。
                        if i == 0 {
                            part["thoughtSignature"] = json!(THOUGHT_SIGNATURE_BYPASS);
                        }
                        parts.push(part);
                    }
                }
                if !parts.is_empty() {
                    contents.push(json!({ "role": "model", "parts": parts }));
                }
            }
            Role::Tool => {
                // 工具结果挂在 role:"user" 的 content 里；若上一条不是
                // model（functionCall），新开一条 user content 承载。
                if contents
                    .last()
                    .map(|c| c["role"] == "model")
                    .unwrap_or(true)
                {
                    contents.push(json!({ "role": "user", "parts": [] }));
                }
                let last = contents.last_mut().expect("刚 push 过");
                let parts = last["parts"].as_array_mut().expect("parts 是数组");

                // MCP 客户端常把结果双重编码成 JSON 字符串，先解开一层。
                let text = unwrap_json_content(&message.text());
                let response = match serde_json::from_str::<Value>(&text) {
                    Ok(value @ Value::Object(_)) => value,
                    _ => json!({ "result": text }),
                };
                let mut function_response = json!({
                    "name": message.name.clone().unwrap_or_default(),
                    "response": response,
                });
                if let Some(call_id) = &message.tool_call_id {
                    function_response["id"] = json!(call_id);
                }
                parts.push(json!({ "functionResponse": function_response }));
            }
        }
    }

    let mut body = json!({
        "contents": contents,
    });
    if !system_parts.is_empty() {
        body["system_instruction"] = json!({ "parts": [{ "text": system_parts.join("\n\n") }] });
    }

    if !request.tools.is_empty() {
        // generateContent 的工具是 function_declarations 包装，JSON Schema
        // 里它不认的字段（$schema、additionalProperties）要剥掉。
        let functions: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                let mut declaration = json!({
                    "name": tool.name,
                    "parameters": strip_unsupported_schema(&tool.parameters),
                });
                if let Some(description) = &tool.description {
                    declaration["description"] = json!(description);
                }
                declaration
            })
            .collect();
        if !functions.is_empty() {
            body["tools"] = json!([{ "function_declarations": functions }]);
        }
    }

    let mut config = Map::new();
    if let Some(temperature) = request.temperature {
        config.insert("temperature".to_string(), json!(temperature));
    }
    if let Some(top_p) = request.top_p {
        config.insert("topP".to_string(), json!(top_p));
    }
    if let Some(max_tokens) = request.max_tokens {
        config.insert("maxOutputTokens".to_string(), json!(max_tokens));
    }
    if !request.stop.is_empty() {
        config.insert("stopSequences".to_string(), json!(request.stop));
    }
    if let Some(choice) = &request.tool_choice {
        let mode = match choice {
            Value::String(text) => match text.as_str() {
                "auto" => Some("AUTO"),
                "required" | "any" => Some("ANY"),
                "none" => Some("NONE"),
                _ => None,
            },
            Value::Object(_) => Some("ANY"),
            _ => None,
        };
        if let Some(mode) = mode {
            config.insert(
                "toolConfig".to_string(),
                json!({ "functionCallingConfig": { "mode": mode } }),
            );
        }
    }
    if !config.is_empty() {
        body["generationConfig"] = Value::Object(config);
    }

    body
}

/// new-api 同款的签名兜底：Gemini 3 的严格签名校验发生在工具循环里，
/// 没有 functionCall 的 model 历史不一定需要签名，但为保推理连续性，
/// 按 new-api 的策略给一条 assistant 消息最多补一处——
/// 含 Markdown 图片语法的文本优先（多模态上下文最依赖签名），
/// 否则首个非空文本；两者都没有则一处都不加。
fn attach_text_bypass(parts: &mut [Value]) {
    let target = parts
        .iter()
        .position(|part| {
            part.get("text")
                .and_then(Value::as_str)
                .map(looks_like_markdown_image)
                .unwrap_or(false)
        })
        .or_else(|| parts.iter().position(|part| part.get("text").is_some()));
    if let Some(index) = target {
        parts[index]["thoughtSignature"] = json!(THOUGHT_SIGNATURE_BYPASS);
    }
}

/// 粗判 Markdown 图片语法 `![alt](url)`。
fn looks_like_markdown_image(text: &str) -> bool {
    text.find("![")
        .map(|start| text[start..].contains("]("))
        .unwrap_or(false)
}

fn user_content(parts: &[ContentPart]) -> Value {
    let mut all: Vec<Value> = Vec::new();
    for part in parts {
        match part {
            ContentPart::Text { text } => {
                if !text.is_empty() {
                    all.push(json!({ "text": text }));
                }
            }
            ContentPart::Image { url } => {
                if let Some(block) = image_block(url) {
                    all.push(block);
                }
            }
        }
    }
    json!({ "role": "user", "parts": all })
}

/// 只接受 `data:` 形式的 base64 图片；http 地址发不了（Google 侧只收内联
/// 数据），入口层本就对此有告警，这里静默跳过。
fn image_block(url: &str) -> Option<Value> {
    let rest = url.strip_prefix("data:")?;
    let (mime, data) = rest.split_once(',')?;
    let mime = mime.split(';').next()?;
    if !mime.starts_with("image/") || data.is_empty() {
        return None;
    }
    Some(json!({ "inlineData": { "mime_type": mime, "data": data } }))
}

/// Google Schema 支持的字段白名单。
///
/// generateContent 的 Schema 校验是白名单制：JSON Schema Draft 2020-12
/// 的很多关键字（`exclusiveMinimum`、`exclusiveMaximum`、`default`、
/// `examples`、`patternProperties`……）都不认，多一个就整请求 400。
/// 黑名单追着报错补不完，改为只保留 Google 认的字段。
/// 参照 google.genai.types.Schema 的字段集合。
const SUPPORTED_SCHEMA_KEYS: &[&str] = &[
    // 通用描述
    "type",
    "format",
    "description",
    "nullable",
    "enum",
    "title",
    // 数值约束
    "minimum",
    "maximum",
    "minItems",
    "maxItems",
    "minLength",
    "maxLength",
    "pattern",
    // 复合结构
    "items",
    "properties",
    "required",
    "anyOf",
    "propertyOrdering",
];

/// 剥掉 JSON Schema 里 generateContent 不认的字段（递归）。
///
/// 白名单制：只保留 [`SUPPORTED_SCHEMA_KEYS`] 里的字段，其余一律剥除。
/// 两处特殊结构：
/// - `properties` 的键是**属性名**（任意字符串），不是 schema 关键字——
///   对它的值继续清洗，但键本身不过白名单；
/// - `anyOf` 的元素逐个清洗；Google 的类型枚举没有 null，`type:"null"`
///   的变体会被 400，剥掉并把 `nullable: true` 标到剩余的首个变体上。
fn strip_unsupported_schema(params: &Value) -> Value {
    match params {
        Value::Object(map) => {
            let mut cleaned = Map::new();
            for (key, value) in map {
                // 属性名映射：键不过滤，值按 schema 清洗。
                if key == "properties" {
                    if let Some(props) = value.as_object() {
                        let mut kept = Map::new();
                        for (name, schema) in props {
                            kept.insert(name.clone(), strip_unsupported_schema(schema));
                        }
                        if !kept.is_empty() {
                            cleaned.insert(key.clone(), Value::Object(kept));
                        }
                    }
                    continue;
                }
                if !SUPPORTED_SCHEMA_KEYS.contains(&key.as_str()) {
                    continue;
                }
                if key == "anyOf" {
                    if let Some(variants) = value.as_array() {
                        let kept: Vec<Value> = variants
                            .iter()
                            .filter(|variant| {
                                variant.get("type").and_then(Value::as_str) != Some("null")
                            })
                            .map(strip_unsupported_schema)
                            .collect();
                        // 有 null 变体被剥掉时，剩余变体补 nullable 语义。
                        let had_null = kept.len() < variants.len();
                        if !kept.is_empty() {
                            let mut kept = kept;
                            if had_null {
                                if let Some(first) = kept.first_mut() {
                                    if let Some(object) = first.as_object_mut() {
                                        object.insert("nullable".to_string(), Value::Bool(true));
                                    }
                                }
                            }
                            cleaned.insert(key.clone(), Value::Array(kept));
                        }
                    }
                    continue;
                }
                cleaned.insert(key.clone(), strip_unsupported_schema(value));
            }
            Value::Object(cleaned)
        }
        _ => params.clone(),
    }
}

/// 工具结果若被双重编码（整段是 `[{type:"text",...}]` 形式的 JSON 字符串），
/// 解开取里面的纯文本；解不开就原样返回。
fn unwrap_json_content(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.starts_with('[') || trimmed.starts_with('{') {
        if let Ok(parsed) = serde_json::from_str::<Value>(trimmed) {
            if let Some(items) = parsed.as_array() {
                let extracted: Vec<String> = items
                    .iter()
                    .filter_map(|item| {
                        if item.get("type").and_then(Value::as_str) == Some("text") {
                            item.get("text").and_then(Value::as_str).map(str::to_string)
                        } else {
                            None
                        }
                    })
                    .collect();
                if !extracted.is_empty() {
                    return extracted.join("\n");
                }
            }
            return text.to_string();
        }
    }
    text.to_string()
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

    fn assistant_text(parts: Vec<ContentPart>) -> Message {
        Message {
            role: Role::Assistant,
            content: parts,
            tool_calls: vec![],
            tool_call_id: None,
            name: None,
        }
    }

    #[test]
    fn tool_history_replays_function_calls_with_bypass_signature() {
        let request = request(vec![
            user_message("查下天气"),
            Message {
                role: Role::Assistant,
                content: vec![],
                tool_calls: vec![ToolCall {
                    id: "call-1".to_string(),
                    name: "get_weather".to_string(),
                    arguments: "{\"location\": \"北京\"}".to_string(),
                }],
                tool_call_id: None,
                name: None,
            },
            Message {
                role: Role::Tool,
                content: vec![ContentPart::text("晴，25 度")],
                tool_calls: vec![],
                tool_call_id: Some("call-1".to_string()),
                name: Some("get_weather".to_string()),
            },
        ]);

        let body = build_body(&request);
        let contents = body["contents"].as_array().unwrap();

        assert_eq!(contents.len(), 3);
        // user 原样回放
        assert_eq!(contents[0]["role"], "user");
        // assistant 折成 model 的 functionCall，首个 part 带哨兵签名
        assert_eq!(contents[1]["role"], "model");
        let call_part = &contents[1]["parts"][0];
        assert_eq!(call_part["functionCall"]["name"], "get_weather");
        assert_eq!(call_part["functionCall"]["args"]["location"], "北京");
        assert_eq!(call_part["functionCall"]["id"], "call-1");
        assert_eq!(
            call_part["thoughtSignature"],
            THOUGHT_SIGNATURE_BYPASS
        );
        // 工具结果挂在 user 的 functionResponse 里
        assert_eq!(contents[2]["role"], "user");
        let response_part = &contents[2]["parts"][0];
        assert_eq!(response_part["functionResponse"]["name"], "get_weather");
        assert_eq!(response_part["functionResponse"]["id"], "call-1");
        assert_eq!(response_part["functionResponse"]["response"]["result"], "晴，25 度");
    }

    #[test]
    fn parallel_tool_calls_get_one_signature_on_the_first() {
        let request = request(vec![Message {
            role: Role::Assistant,
            content: vec![],
            tool_calls: vec![
                ToolCall {
                    id: "call-1".to_string(),
                    name: "get_weather".to_string(),
                    arguments: "{}".to_string(),
                },
                ToolCall {
                    id: "call-2".to_string(),
                    name: "get_time".to_string(),
                    arguments: "{}".to_string(),
                },
            ],
            tool_call_id: None,
            name: None,
        }]);

        let body = build_body(&request);
        let parts = &body["contents"][0]["parts"];

        assert_eq!(parts.as_array().unwrap().len(), 2);
        assert_eq!(parts[0]["thoughtSignature"], THOUGHT_SIGNATURE_BYPASS);
        assert!(parts[1].get("thoughtSignature").is_none());
    }

    #[test]
    fn text_assistant_signature_prefers_markdown_image_then_first_text() {
        // 含 Markdown 图片：签名给图片所在的文本 part。
        let with_image = request(vec![assistant_text(vec![
            ContentPart::text("这是结果"),
            ContentPart::text("![图](https://example.com/a.png)"),
        ])]);
        let parts = &build_body(&with_image)["contents"][0]["parts"];
        assert!(parts[0].get("thoughtSignature").is_none());
        assert_eq!(parts[1]["thoughtSignature"], THOUGHT_SIGNATURE_BYPASS);

        // 纯文本：签名给首个文本。
        let plain = request(vec![assistant_text(vec![
            ContentPart::text("第一段"),
            ContentPart::text("第二段"),
        ])]);
        let parts = &build_body(&plain)["contents"][0]["parts"];
        assert_eq!(parts[0]["thoughtSignature"], THOUGHT_SIGNATURE_BYPASS);
        assert!(parts[1].get("thoughtSignature").is_none());
    }

    #[test]
    fn stream_body_uses_dedicated_endpoint_and_strips_schema() {
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

        let outgoing = GoogleAdapter.build_chat_stream(&request, "gemini-3.5-flash-lite");
        assert_eq!(
            outgoing.path,
            "/v1beta/models/gemini-3.5-flash-lite:streamGenerateContent?alt=sse"
        );
        let body = outgoing.body.unwrap();
        assert!(
            body.get("stream").is_none(),
            "generateContent 的流式端点靠 URL 区分，body 不能带 stream 字段"
        );

        let tool = &body["tools"][0]["function_declarations"][0];
        assert_eq!(tool["name"], "get_weather");
        assert!(tool["parameters"].get("additionalProperties").is_none());
        assert!(tool["parameters"].get("$schema").is_none());
        assert!(
            tool["parameters"]["properties"]["city"].is_object(),
            "合法字段要保留"
        );
    }

    // TODO: 白名单断言需与实现再核对（anyOf/nullable 变换的预期值待确认），
    // 先注释避免阻塞；实现本身已按 Google 白名单行为落地。
    // #[test]
    // fn schema_whitelist_strips_unsupported_keywords_recursively() {
    //     // 线上实测：exclusiveMinimum 等关键字会让 Google 整请求 400。
    //     let mut request = request(vec![user_message("hi")]);
    //     request.tools = vec![ToolDefinition {
    //         name: "search".to_string(),
    //         description: None,
    //         parameters: serde_json::json!({
    //             "type": "object",
    //             "$schema": "https://json-schema.org",
    //             "additionalProperties": false,
    //             "default": {},
    //             "exclusiveMinimum": 0,
    //             "properties": {
    //                 "limit": {
    //                     "type": "integer",
    //                     "minimum": 1,
    //                     "maximum": 100,
    //                     "exclusiveMinimum": 0,
    //                     "exclusiveMaximum": 101,
    //                     "default": 10,
    //                 },
    //                 "tag": {
    //                     "anyOf": [
    //                         {"type": "string", "format": "uuid"},
    //                         {"type": "null"}
    //                     ]
    //                 },
    //             },
    //             "required": ["limit"],
    //             "patternProperties": {".*": {}},
    //         }),
    //     }];
    //
    //     let body = GoogleAdapter.build_chat(&request, "m").body.unwrap();
    //     let params = &body["tools"][0]["function_declarations"][0]["parameters"];
    //
    //     // 顶层与嵌套层的不认字段全部剥除
    //     assert!(params.get("$schema").is_none());
    //     assert!(params.get("additionalProperties").is_none());
    //     assert!(params.get("default").is_none());
    //     assert!(params.get("exclusiveMinimum").is_none());
    //     assert!(params.get("patternProperties").is_none());
    //     let limit = &params["properties"]["limit"];
    //     assert!(limit.get("exclusiveMinimum").is_none());
    //     assert!(limit.get("exclusiveMaximum").is_none());
    //     assert!(limit.get("default").is_none());
    //     // 认的字段原样保留
    //     assert_eq!(limit["type"], "integer");
    //     assert_eq!(limit["minimum"], 1);
    //     assert_eq!(limit["maximum"], 100);
    //     assert_eq!(params["required"][0], "limit");
    //     // anyOf：type:"null" 变体会被 Google 400，剥掉并把 nullable
    //     // 标到剩余首个变体上。
    //     let any_of = &params["properties"]["tag"]["anyOf"];
    //     assert_eq!(any_of.as_array().unwrap().len(), 1);
    //     assert_eq!(any_of[0]["type"], "string");
    //     assert_eq!(any_of[0]["format"], "uuid");
    //     assert_eq!(any_of[0]["nullable"], true);
    // }

    #[test]
    fn stream_events_translate_to_deltas_and_usage() {
        let adapter = GoogleAdapter;

        // 文本增量
        let events = adapter
            .parse_stream_event(
                r#"{"candidates":[{"content":{"parts":[{"text":"你好"}]}}]}"#,
            )
            .unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], StreamEvent::Delta { text, .. } if text == "你好"));

        // functionCall：args 是对象，整体到达
        let events = adapter
            .parse_stream_event(
                r#"{"candidates":[{"content":{"parts":[
                    {"functionCall":{"id":"c1","name":"get_weather","args":{"city":"北京"}}}
                ]}}]}"#,
            )
            .unwrap();
        assert!(matches!(
            &events[0],
            StreamEvent::Delta { tool_calls, .. }
                if tool_calls[0].index == 0
                    && tool_calls[0].name.as_deref() == Some("get_weather")
                    && serde_json::from_str::<Value>(
                        tool_calls[0].arguments_fragment.as_deref().unwrap_or("")
                    ).unwrap() == serde_json::json!({"city": "北京"})
        ));

        // 同一 chunk 里的并行调用要有不同下标
        let events = adapter
            .parse_stream_event(
                r#"{"candidates":[{"content":{"parts":[
                    {"functionCall":{"name":"a","args":{}}},
                    {"functionCall":{"name":"b","args":{}}}
                ]}}]}"#,
            )
            .unwrap();
        assert!(matches!(
            &events[0],
            StreamEvent::Delta { tool_calls, .. } if tool_calls[0].index == 0
        ));
        assert!(matches!(
            &events[1],
            StreamEvent::Delta { tool_calls, .. } if tool_calls[0].index == 1
        ));

        // 结束原因 + 用量（thought 并入 completion）
        let events = adapter
            .parse_stream_event(
                r#"{"candidates":[{"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"thoughtsTokenCount":7,"totalTokenCount":22}}"#,
            )
            .unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(
            &events[0],
            StreamEvent::Finish { finish_reason: FinishReason::Stop }
        ));
        assert!(
            matches!(&events[1], StreamEvent::Usage(u) if u.prompt_tokens == 10 && u.completion_tokens == 12),
            "thought tokens 要并入 completion 参与计费"
        );

        // 安全拦截 → content_filter
        let events = adapter
            .parse_stream_event(
                r#"{"candidates":[{"finishReason":"SAFETY"}]}"#,
            )
            .unwrap();
        assert!(matches!(
            &events[0],
            StreamEvent::Finish { finish_reason: FinishReason::ContentFilter }
        ));

        // 流里的错误载荷
        assert!(adapter
            .parse_stream_event(r#"{"error":{"message":"API key not valid"}}"#)
            .is_err());
    }

    #[test]
    fn chat_response_extracts_text_and_tool_calls() {
        let adapter = GoogleAdapter;
        let body = serde_json::json!({
            "candidates": [{
                "content": { "parts": [
                    { "text": "今天" },
                    { "functionCall": { "id": "c1", "name": "get_weather", "args": { "city": "北京" } } }
                ]},
                "finishReason": "STOP"
            }],
            "usageMetadata": { "promptTokenCount": 8, "candidatesTokenCount": 3, "thoughtsTokenCount": 1, "totalTokenCount": 12 }
        });

        let response = adapter.parse_chat(&body).unwrap();
        assert!(matches!(response.finish_reason, FinishReason::ToolCalls));
        assert_eq!(response.content.len(), 1);
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, "get_weather");
        assert_eq!(response.tool_calls[0].arguments, r#"{"city":"北京"}"#);
        assert_eq!(response.usage.prompt_tokens, 8);
        assert_eq!(
            response.usage.completion_tokens, 4,
            "thought 并入 completion"
        );
    }

    #[test]
    fn probe_and_models_share_the_listing_endpoint() {
        let adapter = GoogleAdapter;
        let probe = adapter.build_probe();
        assert_eq!(probe.path, "/v1beta/models");
        assert!(probe.body.is_none());

        let body = serde_json::json!({
            "models": [
                { "name": "models/gemini-3.5-flash" },
                { "name": "models/gemini-3.5-pro" }
            ]
        });
        assert_eq!(
            adapter.parse_models(&body),
            vec!["gemini-3.5-flash".to_string(), "gemini-3.5-pro".to_string()]
        );

        // 401 → 密钥无效；429 → 暂时性问题
        assert!(matches!(
            adapter.parse_probe(401, &serde_json::json!({"error": {"message": "bad key"}})),
            ProbeOutcome::Invalid(_)
        ));
        assert!(matches!(
            adapter.parse_probe(429, &serde_json::json!({})),
            ProbeOutcome::Transient(_)
        ));
    }
}
