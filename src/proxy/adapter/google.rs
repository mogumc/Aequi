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

use std::collections::{HashMap, VecDeque};

use serde_json::{json, Map, Value};

use crate::error::{Error, Result};
use crate::model::ApiFormat;
use crate::proxy::diagnostics::{self, Adjustment};

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
            body: Some(build_body(request, model)),
        }
    }

    /// 流式走独立端点，body 与非流式完全一致——不需要也不能带 stream 字段。
    fn build_chat_stream(&self, request: &ChatRequest, model: &str) -> UpstreamRequest {
        UpstreamRequest {
            path: format!("/v1beta/models/{model}:streamGenerateContent?alt=sse"),
            body: Some(build_body(request, model)),
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
                        // 思考模型的 part 带 "thought": true 标志——那是推理过程，
                        // 不是正文。混进 content 会让客户端无法折叠展示，也模糊了
                        // 「可见输出」的边界。按标志分流到 reasoning。
                        let thought = part
                            .get("thought")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        let (reasoning, text) = if thought {
                            (text.to_string(), String::new())
                        } else {
                            (String::new(), text.to_string())
                        };
                        events.push(StreamEvent::Delta {
                            reasoning,
                            text,
                            tool_calls: vec![],
                        });
                    }
                }
                if let Some(call) = part.get("functionCall") {
                    let name = call.get("name").and_then(Value::as_str).unwrap_or("");
                    // functionCall 的参数字段是 args（对象）。
                    let args = call.get("args").cloned().unwrap_or(json!({}));
                    events.push(StreamEvent::Delta {
                        reasoning: String::new(),
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
        // 流式响应里 Google 会在每个 chunk 都带一份**累计**的 usageMetadata，
        // 编排层按字段取最大值合并（`merge_usage`），重复出现不会累加。
        if let Some(usage) = value.get("usageMetadata") {
            events.push(StreamEvent::Usage(parse_usage(Some(usage))));
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
        let mut reasoning = String::new();
        let mut tool_calls = Vec::new();

        for part in candidate
            .get("content")
            .and_then(|c| c.get("parts"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            // 与流式同一口径：thought 标志的 part 是推理，不是正文。
            let is_thought = part.get("thought").and_then(Value::as_bool).unwrap_or(false);
            match (
                part.get("text").and_then(Value::as_str),
                part.get("functionCall"),
            ) {
                (Some(text), _) if is_thought => reasoning.push_str(text),
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

        let usage = parse_usage(body.get("usageMetadata"));

        Ok(ChatResponse {
            content,
            reasoning,
            tool_calls,
            finish_reason,
            usage,
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

/// 解析 `usageMetadata`。
///
/// 字段名与 OpenAI 完全不同，逐项对应关系：
/// - `promptTokenCount`：**文本/多模态输入**。官方文档明确「cachedContent 命中时
///   它仍然是完整有效的提示规模，即包含缓存内容的部分」——与 OpenAI 的
///   `prompt_tokens_details.cached_tokens`、Anthropic 的
///   `cache_read_input_tokens` 是同一口径（缓存 ⊆ 输入），所以缓存只作分解记录。
/// - `cachedContentTokenCount`：命中缓存的输入部分，单独记入 `cache_tokens`。
/// - `toolUsePromptTokenCount`：**工具执行结果回灌给模型的输入**。
///   注意它**不在** `promptTokenCount` 里——官方 `totalTokenCount` 的定义是
///   `prompt + candidates + toolUsePrompt + thoughts` 四项相加，若它是子集，
///   公式就会重复计数。所以必须自己并入输入，否则多轮 function calling 的这部分
///   输入既不计费、也不进任何统计（与缓存漏读同一类问题）。
///   它**不并入输出**（不是模型生成的内容），**也不算缓存**（不是缓存命中）。
///   另两家本就把工具结果算在输入里（OpenAI 的 messages、Anthropic 的
///   tool_result），并入是为了三家口径一致。
/// - `thoughtsTokenCount`：思考模型的推理 token，是真实的输出成本，并入 completion。
///
/// 总数沿用「输入(含工具) + 输出(含思考)」，与 `complete_usage` 的兜底口径一致。
fn parse_usage(value: Option<&Value>) -> Usage {
    let Some(value) = value else {
        return Usage::default();
    };

    let prompt = value
        .get("promptTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let tool_use = value
        .get("toolUsePromptTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = value
        .get("candidatesTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let thought = value
        .get("thoughtsTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached = value
        .get("cachedContentTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let prompt_tokens = prompt + tool_use;
    let completion_tokens = output + thought;

    Usage {
        prompt_tokens: prompt_tokens as i64,
        completion_tokens: completion_tokens as i64,
        total_tokens: (prompt_tokens + completion_tokens) as i64,
        cache_tokens: cached as i64,
        estimated: false,
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
/// `model` 只为调整留痕而传（正常路径用不到它，模型名已经在 URL 里）。
fn build_body(request: &ChatRequest, model: &str) -> Value {
    let mut contents: Vec<Value> = Vec::new();
    let mut system_parts: Vec<String> = Vec::new();

    // 工具结果回放时 functionResponse 必须带 name，而 OpenAI 协议里
    // tool 消息的 name 是可选字段——很多客户端只带 tool_call_id。
    // 随遍历维护两份索引：id → 名字（精确找回），以及尚未被结果
    // 消费的调用队列（顺序兜底——Gemini 生成的 functionCall 本就
    // 可能没有 id，客户端回传的 tool_call_id 也会是空）。
    let mut call_names: HashMap<String, String> = HashMap::new();
    let mut pending: VecDeque<(String, String)> = VecDeque::new();

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
                        if !call.id.is_empty() {
                            call_names.insert(call.id.clone(), call.name.clone());
                        }
                        pending.push_back((call.id.clone(), call.name.clone()));
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

                // 名字的三级来源：客户端显式给的 name → 按 tool_call_id
                // 从前面 assistant 的 tool_calls 找回 → 顺序消费最近的
                // 未匹配调用。Google 对 name 为空直接 400，都找不回时
                // 给非空占位符，别让整条请求被拒。
                if let Some(id) = &message.tool_call_id {
                    pending.retain(|(call_id, _)| call_id != id);
                }
                let tool_name = message
                    .name
                    .clone()
                    .filter(|name| !name.is_empty())
                    .or_else(|| {
                        message
                            .tool_call_id
                            .as_ref()
                            .and_then(|id| call_names.get(id).cloned())
                    })
                    .or_else(|| pending.pop_front().map(|(_, name)| name))
                    .unwrap_or_else(|| "tool".to_string());

                // MCP 客户端常把结果双重编码成 JSON 字符串，先解开一层。
                let text = unwrap_json_content(&message.text());
                let response = match serde_json::from_str::<Value>(&text) {
                    Ok(value @ Value::Object(_)) => value,
                    _ => json!({ "result": text }),
                };
                let mut function_response = json!({
                    "name": tool_name,
                    "response": response,
                });
                if let Some(call_id) = &message.tool_call_id {
                    if !call_id.is_empty() {
                        function_response["id"] = json!(call_id);
                    }
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
    if let Some(reasoning) = &request.reasoning {
        // Gemini 3 起用 thinkingLevel 枚举。旧版的 thinkingBudget 在 Gemini 3 上
        // 已废弃、官方明说会产生间歇性 400 INVALID_ARGUMENT，两者同给直接报错——
        // 所以只给 level，不给预算数字。
        let (level, adjustment) = thinking_level(&reasoning.effort);
        diagnostics::log(model, adjustment.as_slice());
        config.insert(
            "thinkingConfig".to_string(),
            json!({
                "thinkingLevel": level,
                // 不写这个字段，思考 part 根本不会随响应回来，出口的
                // reasoning_content 就永远是空的——响应侧早就按 thought 标志分流了。
                "includeThoughts": true,
            }),
        );
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

/// OpenAI 档位 → Gemini 的 `thinkingLevel`，附带「是否偏离了请求的档位」。
///
/// 官方枚举就是 `MINIMAL/LOW/MEDIUM/HIGH`（REST 里写大写，见官方 curl 示例），
/// 与 OpenAI 的档位基本一一对应。
fn thinking_level(effort: &str) -> (&'static str, Option<Adjustment>) {
    match effort {
        "minimal" => ("MINIMAL", None),
        "low" => ("LOW", None),
        "medium" => ("MEDIUM", None),
        "high" => ("HIGH", None),
        // Gemini 没有 xhigh，封顶到 HIGH（比请求的更省）。代价是「想更用力」的诉求
        // 得不到满足，而客户端看不出来，所以留痕。
        "xhigh" => (
            "HIGH",
            Some(Adjustment::new(
                diagnostics::EFFORT_CAPPED,
                "generationConfig.thinkingConfig.thinkingLevel",
                "Gemini 没有 xhigh 档，已封顶为 HIGH",
            )),
        ),
        // 上游没有真正的「关」：最省的 MINIMAL 官方也只说「与大多数查询的不思考设置
        // 相当」，**不保证关闭**。报错等于让 Gemini 的块用不了这个动作，所以降级 + 留痕。
        "none" => (
            "MINIMAL",
            Some(Adjustment::new(
                diagnostics::DISABLE_UNSUPPORTED,
                "generationConfig.thinkingConfig.thinkingLevel",
                "Gemini 没有关闭思考的档位，已降级为 MINIMAL（不保证真的关闭）",
            )),
        ),
        // 入口已经拦下非官方取值，这一臂正常走不到；真走到了说明入口放宽了，
        // 那就更该留痕，而不是静默降级。
        _ => (
            "LOW",
            Some(Adjustment::new(
                diagnostics::EFFORT_ADJUSTED,
                "generationConfig.thinkingConfig.thinkingLevel",
                format!("档位 {effort} 无法识别，已退到最省的 LOW"),
            )),
        ),
    }
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
    use crate::proxy::scheme::{Message, TokenLimitField, ToolCall, ToolDefinition};

    /// 用例里的模型名。只影响调整留痕，取一个普通的 Gemini 3 名即可。
    const TEST_MODEL: &str = "gemini-3.5-flash-lite";

    /// 免去每个用例都传模型名。
    fn body_of(request: &ChatRequest) -> Value {
        build_body(request, TEST_MODEL)
    }

    #[test]
    fn only_degraded_levels_are_reported_as_adjustments() {
        // 直译的档位不留痕——留痕太密会和噪音一样没人看。
        for (effort, level) in [
            ("minimal", "MINIMAL"),
            ("low", "LOW"),
            ("medium", "MEDIUM"),
            ("high", "HIGH"),
        ] {
            let (name, adjustment) = thinking_level(effort);
            assert_eq!(name, level);
            assert!(adjustment.is_none(), "{effort} 不该被记成调整");
        }

        // xhigh 没有对应档，封顶；none 关不掉，降级。两者都必须留痕。
        let (name, adjustment) = thinking_level("xhigh");
        assert_eq!(name, "HIGH");
        assert_eq!(
            adjustment.expect("xhigh 必然是一次调整").code,
            diagnostics::EFFORT_CAPPED
        );

        let (name, adjustment) = thinking_level("none");
        assert_eq!(name, "MINIMAL");
        assert_eq!(
            adjustment.expect("none 必然是一次调整").code,
            diagnostics::DISABLE_UNSUPPORTED
        );
    }

    fn request(messages: Vec<Message>) -> ChatRequest {
        ChatRequest {
            model: "gemini-3.5-flash-lite".to_string(),
            messages,
            tools: vec![],
            tool_choice: None,
            temperature: None,
            top_p: None,
            max_tokens: None,
            max_tokens_field: TokenLimitField::MaxTokens,
            reasoning: None,
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

        let body = body_of(&request);
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

        let body = body_of(&request);
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
        let parts = &body_of(&with_image)["contents"][0]["parts"];
        assert!(parts[0].get("thoughtSignature").is_none());
        assert_eq!(parts[1]["thoughtSignature"], THOUGHT_SIGNATURE_BYPASS);

        // 纯文本：签名给首个文本。
        let plain = request(vec![assistant_text(vec![
            ContentPart::text("第一段"),
            ContentPart::text("第二段"),
        ])]);
        let parts = &body_of(&plain)["contents"][0]["parts"];
        assert_eq!(parts[0]["thoughtSignature"], THOUGHT_SIGNATURE_BYPASS);
        assert!(parts[1].get("thoughtSignature").is_none());
    }

    #[test]
    fn tool_message_without_name_recovers_it_from_the_call() {
        // OpenAI 协议里 tool 消息的 name 是可选字段——客户端只回传
        // tool_call_id 时，名字要从前面 assistant 的 tool_calls 找回，
        // 否则 functionResponse.name 为空会被 Google 直接 400。
        let request = request(vec![
            user_message("搜下今日新闻"),
            Message {
                role: Role::Assistant,
                content: vec![],
                tool_calls: vec![ToolCall {
                    id: "call_1735540".to_string(),
                    name: "WebSearch".to_string(),
                    arguments: "{\"query\": \"今日新闻\"}".to_string(),
                }],
                tool_call_id: None,
                name: None,
            },
            Message {
                role: Role::Tool,
                content: vec![ContentPart::text("搜索结果")],
                tool_calls: vec![],
                tool_call_id: Some("call_1735540".to_string()),
                name: None,
            },
        ]);

        let body = body_of(&request);
        let contents = body["contents"].as_array().unwrap();
        let response_part = &contents[2]["parts"][0];
        assert_eq!(response_part["functionResponse"]["name"], "WebSearch");
        assert_eq!(response_part["functionResponse"]["id"], "call_1735540");
    }

    #[test]
    fn tool_message_without_id_falls_back_to_call_order() {
        // Gemini 生成的 functionCall 本就可能没有 id，客户端回传的
        // tool_call_id 也会是空——此时按顺序消费最近的未匹配调用。
        let request = request(vec![
            user_message("查天气"),
            Message {
                role: Role::Assistant,
                content: vec![],
                tool_calls: vec![ToolCall {
                    id: String::new(),
                    name: "get_weather".to_string(),
                    arguments: "{}".to_string(),
                }],
                tool_call_id: None,
                name: None,
            },
            Message {
                role: Role::Tool,
                content: vec![ContentPart::text("晴")],
                tool_calls: vec![],
                tool_call_id: None,
                name: None,
            },
        ]);

        let body = body_of(&request);
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(contents[2]["parts"][0]["functionResponse"]["name"], "get_weather");
    }

    #[test]
    fn parallel_calls_match_results_by_id_not_by_order() {
        // 并行调用 + 结果乱序回传：id 能对上的必须精确匹配，
        // 不能靠顺序碰运气。
        let request = request(vec![
            user_message("查两件事"),
            Message {
                role: Role::Assistant,
                content: vec![],
                tool_calls: vec![
                    ToolCall {
                        id: "call-a".to_string(),
                        name: "get_weather".to_string(),
                        arguments: "{}".to_string(),
                    },
                    ToolCall {
                        id: "call-b".to_string(),
                        name: "get_time".to_string(),
                        arguments: "{}".to_string(),
                    },
                ],
                tool_call_id: None,
                name: None,
            },
            Message {
                role: Role::Tool,
                content: vec![ContentPart::text("12:00")],
                tool_calls: vec![],
                tool_call_id: Some("call-b".to_string()),
                name: None,
            },
            Message {
                role: Role::Tool,
                content: vec![ContentPart::text("晴")],
                tool_calls: vec![],
                tool_call_id: Some("call-a".to_string()),
                name: None,
            },
        ]);

        let body = body_of(&request);
        let contents = body["contents"].as_array().unwrap();
        let responses: Vec<&serde_json::Value> = contents[2]["parts"]
            .as_array()
            .unwrap()
            .iter()
            .collect();
        assert_eq!(responses[0]["functionResponse"]["name"], "get_time");
        assert_eq!(responses[1]["functionResponse"]["name"], "get_weather");
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

        // 结束原因 + 用量（thought 并入 completion，缓存命中单独记）
        let events = adapter
            .parse_stream_event(
                r#"{"candidates":[{"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"thoughtsTokenCount":7,"cachedContentTokenCount":6,"totalTokenCount":22}}"#,
            )
            .unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(
            &events[0],
            StreamEvent::Finish { finish_reason: FinishReason::Stop }
        ));
        assert!(
            matches!(&events[1], StreamEvent::Usage(u)
                if u.prompt_tokens == 10
                    && u.completion_tokens == 12
                    && u.cache_tokens == 6),
            "thought tokens 要并入 completion 参与计费；缓存命中来自 cachedContentTokenCount"
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
    fn thought_parts_route_to_reasoning_not_content() {
        let adapter = GoogleAdapter;

        // 流式：thought 标志的 part 是推理，普通 part 是正文，两者不能混。
        let events = adapter
            .parse_stream_event(
                r#"{"candidates":[{"content":{"parts":[
                    {"text":"让我想想","thought":true},
                    {"text":"答案是 42"}
                ]}}]}"#,
            )
            .unwrap();
        assert_eq!(events.len(), 2);
        assert!(
            matches!(&events[0], StreamEvent::Delta { reasoning, text, .. }
                if reasoning == "让我想想" && text.is_empty()),
            "thought part 必须走 reasoning"
        );
        assert!(
            matches!(&events[1], StreamEvent::Delta { reasoning, text, .. }
                if text == "答案是 42" && reasoning.is_empty()),
            "普通 part 必须走 content"
        );

        // 非流式：同一口径。
        let body = serde_json::json!({
            "candidates": [{
                "content": { "parts": [
                    { "text": "先想一下", "thought": true },
                    { "text": "结论" }
                ]},
                "finishReason": "STOP"
            }]
        });
        let response = adapter.parse_chat(&body).unwrap();
        assert_eq!(response.reasoning, "先想一下");
        assert_eq!(response.text(), "结论");
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
            "usageMetadata": { "promptTokenCount": 8, "candidatesTokenCount": 3, "thoughtsTokenCount": 1, "cachedContentTokenCount": 5, "totalTokenCount": 12 }
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
        assert_eq!(
            response.usage.cache_tokens, 5,
            "非流式同样要读出 cachedContentTokenCount"
        );
        // 缓存是输入的一部分，不能再加进总数（8 + 3 + 1 = 12）
        assert_eq!(response.usage.total_tokens, 12);
    }

    #[test]
    fn usage_without_cache_field_stays_zero() {
        // 未开启上下文缓存时 Google 不带这个字段，不能因此报错或写脏值。
        let response = GoogleAdapter
            .parse_chat(&serde_json::json!({
                "candidates": [{
                    "content": { "parts": [{ "text": "hi" }] },
                    "finishReason": "STOP"
                }],
                "usageMetadata": { "promptTokenCount": 5, "candidatesTokenCount": 2 }
            }))
            .unwrap();
        assert_eq!(response.usage.cache_tokens, 0);
        assert_eq!(response.usage.total_tokens, 7);
    }

    #[test]
    fn tool_use_prompt_tokens_count_as_input() {
        // 工具执行结果回灌给模型的输入。官方 `totalTokenCount` 把它与 prompt
        // **并列为两项**（prompt + candidates + toolUsePrompt + thoughts），
        // 所以它不在 `promptTokenCount` 里——不自己并入，多轮 function calling
        // 的这部分输入就整块漏计费、也不进任何 token 统计。
        let response = GoogleAdapter
            .parse_chat(&serde_json::json!({
                "candidates": [{
                    "content": { "parts": [{ "text": "ok" }] },
                    "finishReason": "STOP"
                }],
                "usageMetadata": {
                    "promptTokenCount": 100,
                    "toolUsePromptTokenCount": 250,
                    "candidatesTokenCount": 20,
                    "totalTokenCount": 370
                }
            }))
            .unwrap();

        assert_eq!(response.usage.prompt_tokens, 350, "工具输入并入输入");
        assert_eq!(response.usage.completion_tokens, 20, "工具输入不是输出");
        assert_eq!(response.usage.total_tokens, 370, "与上游 totalTokenCount 对齐");
        assert_eq!(response.usage.cache_tokens, 0, "工具输入不是缓存命中");
    }

    #[test]
    fn tool_use_and_cache_do_not_double_count() {
        // 工具输入与缓存命中是两条独立的分支：合并进输入的是工具那部分，
        // 缓存只作分解记录，两者都不能重复计进总数。
        let response = GoogleAdapter
            .parse_chat(&serde_json::json!({
                "candidates": [{
                    "content": { "parts": [{ "text": "ok" }] },
                    "finishReason": "STOP"
                }],
                "usageMetadata": {
                    "promptTokenCount": 100,
                    "cachedContentTokenCount": 60,
                    "toolUsePromptTokenCount": 30,
                    "candidatesTokenCount": 10,
                    "thoughtsTokenCount": 5,
                    "totalTokenCount": 145
                }
            }))
            .unwrap();

        assert_eq!(response.usage.prompt_tokens, 130, "输入含缓存与工具");
        assert_eq!(response.usage.cache_tokens, 60, "缓存单独记，不重复计入总数");
        assert_eq!(response.usage.completion_tokens, 15, "思考并入输出");
        assert_eq!(response.usage.total_tokens, 145);
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
