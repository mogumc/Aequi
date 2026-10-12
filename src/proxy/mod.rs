//! 上游代理层。
//!
//! 一次请求的走向：
//!
//! ```text
//! 用户（只认 OpenAI 格式）
//!   → openai_in 解析成标准请求
//!   → 按模型挑块、块内按权重挑密钥
//!   → adapter 转成该块的格式并发出
//!   → adapter 把响应转回标准响应
//!   → 按 token 用量计费落账
//!   → openai_in 封装成 OpenAI 响应交还用户
//! ```
//!
//! 日志分两轨：详细日志（`proxy_logs`，管理员）记录排障需要的一切；
//! 用量记录（`usage_records`，密钥侧）只有计费数字与一句脱敏错误。
//! 两张表物理分开，脱敏不靠查询时筛选。

use std::sync::Arc;
use std::time::Instant;

use futures_util::{Stream as FutureStream, StreamExt};
use serde_json::{Value, json};

use crate::AppState;
use crate::error::{Error, Result};
use crate::model::{ApiKey, NewProxyLog, UpstreamBlock, credits};
use crate::proxy::adapter::{AuthStyle, UpstreamRequest, adapter_for};
use crate::proxy::scheme::{
    ChatRequest, ChatResponse, FinishReason, StreamEvent, ToolCallDelta, Usage,
};
use crate::service::{billing, upstream};
use crate::utils::{random, time};

pub mod adapter;
pub mod estimate;
pub mod image;
pub mod openai_in;
pub mod scheme;
pub mod sse;

/// 一次请求最多换几把密钥重试。
///
/// 只为可恢复的错误重试（限流、上游 5xx、网络故障）；请求本身有问题时换密钥没有意义。
const MAX_ATTEMPTS: usize = 3;

/// 一次失败的结构化描述。
///
/// 管理员看到 `error` 全文，密钥侧只看到 `user_message`——
/// 上游的原话（比如它对密钥的具体抱怨）不进密钥能查到的那张表。
struct Failure {
    error: Error,
    user_message: String,
    context: Option<(String, String)>,
}

impl Failure {
    /// 无上下文的失败（还没选中块或密钥）。
    fn bare(error: Error) -> Self {
        let user_message = sanitize_user_message(&error);
        Self {
            error,
            user_message,
            context: None,
        }
    }

    /// 带上下文的失败。
    ///
    /// `user_message` 由调用方按场景构造；内部错误的原文在这里再拦一道，
    /// 数据库细节之类的东西不该出现在密钥能查到的那张表里。
    fn with_context(error: Error, user_message: String, context: Option<(String, String)>) -> Self {
        let user_message = if matches!(error, Error::Internal(_) | Error::Database(_)) {
            "服务暂时不可用，请稍后重试".to_string()
        } else {
            user_message
        };

        Self {
            error,
            user_message,
            context,
        }
    }
}

/// 密钥侧允许看到的错误描述。
///
/// 参数类、限额类的错误本就是给用户看的，原样透传；
/// 上游错误（[`Error::UpstreamUnavailable`]）只给一句固定文案——上游的
/// 原话可能回显请求内容、账号信息或网关内部细节，只进管理员的日志；
/// 在手有上游状态码的调用点用「上游响应异常: 返回 {status}」更有排障价值；
/// 内部错误（数据库等）同样只给一句通用提示。
fn sanitize_user_message(error: &Error) -> String {
    match error {
        Error::BadRequest(_)
        | Error::NotFound(_)
        | Error::Forbidden(_)
        | Error::InsufficientCredits => error.to_string(),
        Error::UpstreamUnavailable(_) => "上游响应异常".to_string(),
        _ => "服务暂时不可用，请稍后重试".to_string(),
    }
}

/// 详细错误的最长保留长度。
///
/// 有些网关会把整个请求回显进错误体，不截断的话一条日志能撑到几 MB。
const MAX_ERROR_DETAIL: usize = 2000;

fn truncate_detail(detail: &str) -> String {
    if detail.chars().count() <= MAX_ERROR_DETAIL {
        return detail.to_string();
    }

    let truncated: String = detail.chars().take(MAX_ERROR_DETAIL).collect();
    format!("{truncated}…（已截断）")
}

/// 编排一次对话请求，返回要交给用户的响应体。
///
/// 无论成败都会落两轨日志；日志写入失败只留警告，不拖垮业务请求。
pub async fn chat_completion(
    state: &Arc<AppState>,
    key: &ApiKey,
    body: &Value,
    ip: &str,
    user_agent: Option<&str>,
    header_block: Option<&str>,
) -> Result<Value> {
    let started = Instant::now();
    let pool = &state.store.data;

    let mut request = match openai_in::parse_request(body) {
        Ok(request) => request,
        Err(error) => {
            // 参数错误：还没有块与上游密钥可记，但请求本身进日志。
            write_log(
                pool,
                base_log(key, ip, user_agent, started)
                    .model(String::new())
                    .failed(&error),
            )
            .await;
            return Err(error);
        }
    };

    // 解析目标：归一化模型名（剥掉块前缀），并记下是否锁定了某个块。
    let target = upstream::resolve_with_header(pool, &request.model, header_block).await?;
    request.model = target.model;

    // 余额为零（且不是无限额度）时直接拒绝，免得替用户白白消耗上游。
    // 真实扣减由 charge 的原子 UPDATE 兜底，这里只是省一次无谓的上游调用。
    if !key.unlimited && key.credits_scaled <= 0 {
        let error = Error::InsufficientCredits;
        write_log(
            pool,
            base_log(key, ip, user_agent, started)
                .model(request.model.clone())
                .failed(&error),
        )
        .await;
        return Err(error);
    }

    match dispatch(state, key, &request, started, target.block_id.as_deref()).await {
        Ok(outcome) => {
            write_log(
                pool,
                base_log(key, ip, user_agent, started)
                    .model(request.model.clone())
                    .block(&outcome.block_id)
                    .upstream_key(&outcome.upstream_key_id)
                    .succeeded(outcome.response.usage, outcome.charge.credits_scaled)
                    .to_success_log(),
            )
            .await;

            let id = format!("chatcmpl-{}", random::string(24));
            let created = time::now_secs();

            Ok(openai_in::render_response(&outcome.response, &id, created))
        }
        Err(failure) => {
            // 失败也要留下痕迹：管理员看到细节，用户只看到脱敏描述。
            write_log(
                pool,
                base_log(key, ip, user_agent, started)
                    .model(request.model.clone())
                    .failed_with_context(&failure.error, failure.context),
            )
            .await;

            if let Err(log_error) = crate::db::usage::insert_error(
                pool,
                &key.id,
                &request.model,
                &failure.user_message,
                false,
                time::now_secs(),
            )
            .await
            {
                tracing::warn!(error = %log_error, "写入失败用量记录失败");
            }

            Err(failure.error)
        }
    }
}

/// 一次成功编排出站后的上下文。
struct DispatchOutcome {
    response: ChatResponse,
    charge: billing::ChargeOutcome,
    block_id: String,
    upstream_key_id: String,
}

/// 用量聚合：各字段取较大值。
///
/// 上游可能分多次报用量（如 Anthropic 的输入在流开头、输出在流末尾），
/// 后到的累计值只会更大。
fn merge_usage(into: &mut Usage, from: Usage) {
    into.prompt_tokens = into.prompt_tokens.max(from.prompt_tokens);
    into.completion_tokens = into.completion_tokens.max(from.completion_tokens);
    into.cache_tokens = into.cache_tokens.max(from.cache_tokens);
    into.total_tokens = into.total_tokens.max(from.total_tokens);
}

/// 编排一次流式对话，返回 SSE 字节流（`data: <json>\n\n` 帧）。
///
/// 上游错误在**流建立之前**才重试——此刻还没向客户端输出任何字节；
/// 流一旦开始，中途错误以内联 error 帧发出（SSE 无法中途改状态码），
/// 计费与日志在流收尾时落账。
pub async fn chat_completion_stream(
    state: Arc<AppState>,
    key: ApiKey,
    body: Value,
    ip: String,
    user_agent: Option<String>,
    header_block: Option<String>,
) -> Result<impl FutureStream<Item = std::result::Result<Vec<u8>, std::io::Error>> + 'static> {
    let started = Instant::now();
    let pool = &state.store.data;

    let mut request = openai_in::parse_request(&body)?;
    // 解析目标：归一化模型名（剥掉块前缀），并记下是否锁定了某个块。
    let target =
        upstream::resolve_with_header(pool, &request.model, header_block.as_deref()).await?;
    request.model = target.model;

    if !key.unlimited && key.credits_scaled <= 0 {
        return Err(Error::InsufficientCredits);
    }

    let only = target.block_id.as_deref();

    // 建流前的重试窗口。
    let mut upstream = None;
    let mut chosen: Option<(String, String, crate::model::ApiFormat)> = None;
    // 倍率取自最终选中的那个块的模型条目；块本身不会进到流里，这里先取出来。
    let mut multiplier_bp = credits::DEFAULT_MULTIPLIER_BP;
    let mut last_failure = None;

    'attempt: for _ in 0..MAX_ATTEMPTS {
        let block =
            match upstream::select_block(pool, &state.upstream, &key, &request.model, only).await {
                Ok(block) => block,
                Err(error) => return Err(error),
            };
        let upstream_key = match upstream::select_key(pool, &state.upstream, &block).await {
            Ok(key) => key,
            Err(error) => return Err(error),
        };

        let adapter = adapter_for(block.api_format);
        let mut outgoing = adapter.build_chat_stream(&request, &block.target_model(&request.model));
        // OpenAI 系流式必须带 stream_options.include_usage 上游才会在流末尾
        // 回 usage chunk，否则只能估算计费。默认注入；上游不认识这个字段时
        // 会 400，此时拉黑该块并剥离重试（自动降级，替代旧版的配置白名单）。
        if block.api_format == crate::model::ApiFormat::Openai
            && state.upstream.usage_inject_allowed(&block.id)
        {
            outgoing.inject_stream_usage();
        }
        let client = state.upstream.client(&block)?;

        let context = Some((block.id.clone(), upstream_key.id.clone()));

        // 发送当前尝试；注入字段被上游 400 拒绝时剥离后原样重发一次
        //（'send 循环），不消耗重试名额。
        'send: loop {
            match send_stream(
                &client,
                &block,
                &outgoing,
                adapter.auth_style(),
                &upstream_key.secret,
            )
            .await
            {
                Ok(response) if response.status().is_success() => {
                    multiplier_bp = block.multiplier_bp(&request.model);
                    upstream = Some(response);
                    chosen = Some((block.id, upstream_key.id, block.api_format));
                    break;
                }
                Ok(response) => {
                    let status = response.status().as_u16();
                    let detail = response
                        .text()
                        .await
                        .ok()
                        .and_then(|text| {
                            serde_json::from_str::<Value>(&text).ok().and_then(|value| {
                                value
                                    .pointer("/error/message")
                                    .and_then(Value::as_str)
                                    .map(str::to_string)
                            })
                        })
                        .unwrap_or_default();

                    // 上游用 400 拒绝了注入字段：拉黑该块，剥离后重发。
                    if status == 400
                        && (detail.contains("stream_options")
                            || detail.contains("include_usage"))
                    {
                        state.upstream.disable_usage_inject(&block.id);
                        outgoing.strip_stream_usage();
                        continue 'send;
                    }

                    if status == 429 {
                        upstream::report_rate_limited(pool, &upstream_key).await;
                        last_failure = Some(Failure::with_context(
                            Error::UpstreamUnavailable("上游限流".to_string()),
                            "上游限流".to_string(),
                            context,
                        ));
                        continue 'attempt;
                    }
                    if status == 401 || status == 403 || status >= 500 {
                        upstream::report_failure(
                            pool,
                            &upstream_key,
                            &format!("上游返回 {status} {detail}"),
                        )
                        .await;
                        last_failure = Some(Failure::with_context(
                            Error::UpstreamUnavailable(format!("上游返回 {status} {detail}")),
                            format!("上游响应异常: 返回 {status}"),
                            context,
                        ));
                        continue 'attempt;
                    }

                    // 400 这类参数错误重试也不会好，直接报错。但必须落日志：
                    // 否则排障时只能看到 tracing 输出，请求流水里查无此请求。
                    // 上游对参数的说明可能回显敏感内容，用户端只看状态码。
                    let message = format!("上游返回 {status} {detail}");
                    write_log(
                        pool,
                        base_log(&key, &ip, user_agent.as_deref(), started)
                            .model(request.model.clone())
                            .streaming()
                            .failed(&Error::UpstreamUnavailable(message.clone())),
                    )
                    .await;
                    if let Err(log_error) = crate::db::usage::insert_error(
                        pool,
                        &key.id,
                        &request.model,
                        &format!("上游响应异常: 返回 {status}"),
                        true,
                        time::now_secs(),
                    )
                    .await
                    {
                        tracing::warn!(error = %log_error, "写入失败用量记录失败");
                    }
                    return Err(Error::UpstreamUnavailable(message));
                }
                Err(error) => {
                    let user_message = sanitize_user_message(&error);
                    upstream::report_failure(pool, &upstream_key, &error.to_string()).await;
                    last_failure = Some(Failure::with_context(error, user_message, context));
                    // 网络层失败：换一把密钥进入下一轮 attempt。
                    continue 'attempt;
                }
            }
        }
    }

    let Some(response) = upstream else {
        let failure = last_failure
            .unwrap_or_else(|| Failure::bare(Error::UpstreamUnavailable("上游不可用".to_string())));
        write_log(
            pool,
            base_log(&key, &ip, user_agent.as_deref(), started)
                .model(request.model.clone())
                .streaming()
                .failed_with_context(&failure.error, failure.context),
        )
        .await;
        if let Err(error) = crate::db::usage::insert_error(
            pool,
            &key.id,
            &request.model,
            &failure.user_message,
            true,
            time::now_secs(),
        )
        .await
        {
            tracing::warn!(error = %error, "写入失败用量记录失败");
        }
        return Err(failure.error);
    };

    let Some((block_id, upstream_key_id, format)) = chosen else {
        return Err(Error::UpstreamUnavailable("上游不可用".to_string()));
    };

    // 流的生命周期长于本函数，所有引用全部转为所有权。
    let adapter = adapter_for(format);
    let id = format!("chatcmpl-{}", random::string(24));
    let created = time::now_secs();

    let payload_stream = sse::sse_data_stream(response.bytes_stream());

    let output = async_stream::stream! {
        let pool = &state.store.data;
        let mut text_acc = String::new();
        // 推理内容单独累计：估算用量时与正文一起算（思考 token 也是输出
        // 成本），但发给客户端的 chunk 里两者分属不同字段。
        let mut reasoning_acc = String::new();
        let mut usage_acc = Usage::default();
        let mut finished = false;
        // 上游是否真的产出过内容（思考、文本或工具调用）。
        let mut produced = false;
        // 是否收到过工具调用的分片增量——决定收尾快照是否需要补发。
        let mut tools_seen = false;
        // 最后一条原始载荷：一条内容都没有时，它是排障的唯一线索
        // （最常见的原因是块格式与上游不匹配，解析器什么都提不出来）。
        let mut last_payload: Option<String> = None;
        let mut sse = std::pin::pin!(payload_stream);

        // 流内只读名字，拷一份所有权（生命周期长于本函数）。
        let model = request.model.clone();

        // 每条输出都是一行 `data: <json>` 加空行。
        let frame = |value: Value| {
            Ok::<Vec<u8>, std::io::Error>(format!("data: {value}\n\n").into_bytes())
        };
        let error_frame = |message: &str| {
            format!(
                "data: {}\n\ndata: [DONE]\n\n",
                json!({ "error": { "message": message, "type": "upstream_unavailable" } })
            )
            .into_bytes()
        };

        yield frame(openai_in::render_stream_role(&id, created, &model));

        while let Some(item) = sse.next().await {
            let payload = match item {
                Ok(payload) => payload,
                Err(error) => {
                    // 上游流中断：与其它中途错误一致，以内联错误帧收尾并落日志。
                    // 客户端与用户端记录都只给脱敏文案。
                    let user_message = sanitize_user_message(&error);
                    write_log(
                        pool,
                        base_log(&key, &ip, user_agent.as_deref(), started)
                            .model(request.model.clone())
                            .streaming()
                            .failed(&error),
                    )
                    .await;
                    if let Err(log_error) = crate::db::usage::insert_error(
                        pool,
                        &key.id,
                        &model,
                        &user_message,
                        true,
                        time::now_secs(),
                    )
                    .await
                    {
                        tracing::warn!(error = %log_error, "写入失败用量记录失败");
                    }
                    yield Ok(error_frame(&user_message));
                    return;
                }
            };

            // `[DONE]` 是 OpenAI 风格的流结束哨兵，不是载荷——各家解析器都不该
            // 见到它。在这里统一截住，省得每个 adapter 各自处理一遍。
            if payload.trim() == "[DONE]" {
                continue;
            }
            last_payload = Some(payload.chars().take(400).collect());

            let events = match adapter.parse_stream_event(&payload) {
                Ok(events) => events,
                Err(error) => {
                    let user_message = sanitize_user_message(&error);
                    write_log(
                        pool,
                        base_log(&key, &ip, user_agent.as_deref(), started)
                            .model(request.model.clone())
                            .streaming()
                            .failed(&error),
                    )
                    .await;
                    if let Err(log_error) = crate::db::usage::insert_error(
                        pool,
                        &key.id,
                        &model,
                        &user_message,
                        true,
                        time::now_secs(),
                    )
                    .await
                    {
                        tracing::warn!(error = %log_error, "写入失败用量记录失败");
                    }
                    yield Ok(error_frame(&user_message));
                    return;
                }
            };

            for event in events {
                match event {
                    StreamEvent::Delta {
                        reasoning,
                        text,
                        tool_calls,
                    } => {
                        if !reasoning.is_empty() || !text.is_empty() || !tool_calls.is_empty() {
                            produced = true;
                        }
                        if !tool_calls.is_empty() {
                            tools_seen = true;
                        }
                        text_acc.push_str(&text);
                        reasoning_acc.push_str(&reasoning);
                        yield frame(openai_in::render_stream_delta(
                            &id,
                            created,
                            &model,
                            &reasoning,
                            &text,
                            &tool_calls,
                        ));
                    }
                    StreamEvent::Finish { finish_reason } => {
                        if !finished {
                            finished = true;
                            yield frame(openai_in::render_stream_finish(
                                &id, created, &model, finish_reason,
                            ));
                        }
                    }
                    StreamEvent::Snapshot { tool_calls } => {
                        // 收尾快照是上游给的权威全集；只有增量从未到达时才补发，
                        // 避免与已收到的分片叠加。快照到达本身说明上游确实
                        // 发起过调用——这也计入产出。
                        if !tool_calls.is_empty() && !tools_seen {
                            produced = true;
                            tools_seen = true;
                            let deltas = tool_calls
                                .iter()
                                .enumerate()
                                .map(|(index, call)| ToolCallDelta {
                                    index,
                                    id: Some(call.id.clone()),
                                    name: Some(call.name.clone()),
                                    arguments_fragment: Some(call.arguments.clone()),
                                })
                                .collect::<Vec<_>>();
                            yield frame(openai_in::render_stream_delta(
                                &id,
                                created,
                                &model,
                                "",
                                "",
                                &deltas,
                            ));
                        }
                    }
                    StreamEvent::Usage(usage) => merge_usage(&mut usage_acc, usage),
                    StreamEvent::End => {}
                }
            }
        }

        // 整个流没有任何实际产出（无思考、无文本、无工具调用）时按失败处理，
        // 不估算也不计费——上游可能返回了 200 加一段错误事件，或根本不是
        // 我们以为的格式（解析器什么都提不出来）。哪怕它报了用量，客户端
        // 什么都没收到，这钱也不能收。之前这里会按请求体估算照样扣费。
        if !produced {
            let detail = match &last_payload {
                Some(payload) => format!(
                    "上游没有返回任何可识别的内容，最后一条原始载荷: {payload}（请检查块的 API 格式是否与上游匹配）"
                ),
                None => "上游没有返回任何内容".to_string(),
            };
            write_log(
                pool,
                base_log(&key, &ip, user_agent.as_deref(), started)
                    .model(request.model.clone())
                    .block(&block_id)
                    .upstream_key(&upstream_key_id)
                    .streaming()
                    .failed(&Error::UpstreamUnavailable(detail.clone())),
            )
            .await;
            if let Err(log_error) = crate::db::usage::insert_error(
                pool,
                &key.id,
                &model,
                "上游响应异常: 上游没有返回任何内容",
                true,
                time::now_secs(),
            )
            .await
            {
                tracing::warn!(error = %log_error, "写入失败用量记录失败");
            }
            // 给客户端的是不含内部细节的短句；原始载荷进日志供排障。
            yield Ok(error_frame("上游提前结束了响应且没有返回任何内容，请重试"));
            return;
        }

        // 上游没发结束原因就补一个 Stop，避免客户端把流挂起。
        if !finished {
            yield frame(openai_in::render_stream_finish(
                &id, created, &model, FinishReason::Stop,
            ));
        }

        // 计费：上游报的用量聚合；没报就按聚合出来的文本估算。
        // 思考也是输出成本——估算路径把推理与正文拼在一起算，
        // 与上游 usage 里 completion 已含 reasoning 的口径一致。
        let mut usage = usage_acc;
        if usage.total_tokens == 0 {
            let output = format!("{reasoning_acc}{text_acc}");
            usage = estimate::estimate_usage(&request, &output);
        } else {
            usage.total_tokens = usage.prompt_tokens + usage.completion_tokens;
        }

        match billing::charge(pool, &key, &block_id, &model, usage, multiplier_bp, true).await {
            Ok(charge) => {
                write_log(
                    pool,
                    base_log(&key, &ip, user_agent.as_deref(), started)
                        .model(request.model.clone())
                        .block(&block_id)
                        .upstream_key(&upstream_key_id)
                        .streaming()
                        .succeeded(charge.usage, charge.credits_scaled)
                        .to_success_log(),
                )
                .await;

                tracing::info!(
                    model = %request.model,
                    block = %block_id,
                    tokens = charge.usage.total_tokens,
                    estimated = charge.usage.estimated,
                    credits = %charge.credits_scaled,
                    duration_ms = started.elapsed().as_millis() as u64,
                    stream = true,
                    "对话请求完成"
                );

                yield frame(openai_in::render_stream_usage(
                    &id, created, &model, charge.usage,
                ));
                yield Ok(b"data: [DONE]\n\n".to_vec());
            }
            Err(error) => {
                let user_message = sanitize_user_message(&error);
                write_log(
                    pool,
                    base_log(&key, &ip, user_agent.as_deref(), started)
                        .model(request.model.clone())
                        .streaming()
                        .failed_with_context(&error, Some((block_id, upstream_key_id))),
                )
                .await;
                if let Err(log_error) = crate::db::usage::insert_error(
                    pool,
                    &key.id,
                    &model,
                    &user_message,
                    true,
                    time::now_secs(),
                )
                .await
                {
                    tracing::warn!(error = %log_error, "写入失败用量记录失败");
                }
                yield Ok(error_frame(&user_message));
            }
        }
    };

    Ok(output)
}

/// 挑块、挑密钥、发送、解析、计费。
///
/// 失败返回 [`Failure`]：管理员错误与密钥侧的脱敏描述分开展示。
async fn dispatch(
    state: &Arc<AppState>,
    key: &ApiKey,
    request: &ChatRequest,
    started: Instant,
    // 请求里显式锁定的块；None 即按权重轮询所有能处理该模型的块。
    only: Option<&str>,
) -> std::result::Result<DispatchOutcome, Failure> {
    let pool = &state.store.data;
    let mut last_failure = None;

    for _ in 0..MAX_ATTEMPTS {
        // 每一轮都重新挑：上一把密钥进入退避后，这里会自然落到下一把。
        let block =
            match upstream::select_block(pool, &state.upstream, &key, &request.model, only).await {
                Ok(block) => block,
                Err(error) => return Err(Failure::bare(error)),
            };
        let upstream_key = match upstream::select_key(pool, &state.upstream, &block).await {
            Ok(key) => key,
            Err(error) => {
                let context = Some((block.id.clone(), String::new()));
                let user_message = sanitize_user_message(&error);
                return Err(Failure::with_context(error, user_message, context));
            }
        };

        let adapter = adapter_for(block.api_format);
        let outgoing = adapter.build_chat(request, &block.target_model(&request.model));
        let client = match state.upstream.client(&block) {
            Ok(client) => client,
            Err(error) => {
                let context = Some((block.id.clone(), upstream_key.id.clone()));
                // 代理地址之类的出口细节不进用户能查到的表。
                let user_message = sanitize_user_message(&error);
                return Err(Failure::with_context(error, user_message, context));
            }
        };

        let sent = send(
            &client,
            &block,
            &outgoing,
            adapter.auth_style(),
            &upstream_key.secret,
        )
        .await;

        let context = Some((block.id.clone(), upstream_key.id.clone()));

        match sent {
            Ok((status, response_body)) => {
                if status == 429 {
                    upstream::report_rate_limited(pool, &upstream_key).await;
                    last_failure = Some(Failure::with_context(
                        Error::UpstreamUnavailable("上游限流".to_string()),
                        "上游限流".to_string(),
                        context,
                    ));
                    continue;
                }

                // 密钥被拒或上游故障：记一次失败，换一把再试。
                // 上游的原话只进管理员日志，密钥侧只看到状态码。
                if status == 401 || status == 403 || status >= 500 {
                    let detail = response_body
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    upstream::report_failure(
                        pool,
                        &upstream_key,
                        &format!("上游返回 {status} {detail}"),
                    )
                    .await;
                    last_failure = Some(Failure::with_context(
                        Error::UpstreamUnavailable(format!("上游返回 {status} {detail}")),
                        format!("上游响应异常: 返回 {status}"),
                        context,
                    ));
                    continue;
                }

                if !(200..300).contains(&status) {
                    // 请求本身的问题（参数、模型名等），换密钥也不会好。
                    // 上游的说明只进管理员日志；用户端同样只看状态码，
                    // 不排除上游在错误体里回显敏感内容的可能。
                    let detail = response_body
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let message = format!("上游返回 {status} {detail}");
                    return Err(Failure::with_context(
                        Error::UpstreamUnavailable(message.clone()),
                        format!("上游响应异常: 返回 {status}"),
                        context,
                    ));
                }

                let response = match adapter.parse_chat(&response_body) {
                    Ok(response) => response,
                    Err(error) => {
                        // 200 却解析不出内容，多半是上游网关吐了故障页——
                        // 这属于上游的问题，记一次失败并换一把再试。
                        let user_message = sanitize_user_message(&error);
                        upstream::report_failure(pool, &upstream_key, &error.to_string()).await;
                        last_failure = Some(Failure::with_context(error, user_message, context));
                        continue;
                    }
                };
                upstream::report_success(pool, &upstream_key.id).await;

                // 上游没报用量就用本地估算补齐，记账不能被钻空子。
                // 估算口径含推理文本——思考模型的思考部分不是免费的。
                let usage = if response.usage.total_tokens == 0 {
                    estimate::estimate_usage(request, &response.output_for_estimate())
                } else {
                    response.usage
                };

                let charge = match billing::charge(
                    pool,
                    key,
                    &block.id,
                    &request.model,
                    usage,
                    block.multiplier_bp(&request.model),
                    false,
                )
                .await
                {
                    Ok(charge) => charge,
                    Err(error) => {
                        let user_message = sanitize_user_message(&error);
                        return Err(Failure::with_context(error, user_message, context));
                    }
                };

                tracing::info!(
                    model = %request.model,
                    block = %block.id,
                    tokens = charge.usage.total_tokens,
                    estimated = charge.usage.estimated,
                    credits = %charge.credits_scaled,
                    duration_ms = started.elapsed().as_millis() as u64,
                    "对话请求完成"
                );

                return Ok(DispatchOutcome {
                    response,
                    charge,
                    block_id: block.id,
                    upstream_key_id: upstream_key.id,
                });
            }
            Err(error) => {
                // 网络层面的失败同样换一把再试；reqwest 的报错可能带 URL，
                // 不进用户能查到的表。
                let user_message = sanitize_user_message(&error);
                upstream::report_failure(pool, &upstream_key, &error.to_string()).await;
                last_failure = Some(Failure::with_context(error, user_message, context));
            }
        }
    }

    Err(last_failure
        .unwrap_or_else(|| Failure::bare(Error::UpstreamUnavailable("上游不可用".to_string()))))
}

/// 组装发往上游的请求（路径、认证头、各家额外头）。
///
/// 对块外也可见：管理面的「拉取上游模型」用的是同一个探测请求，
/// 认证规则不该在那里再抄一遍。
pub(crate) fn prepare_request(
    client: &reqwest::Client,
    block: &UpstreamBlock,
    request: &UpstreamRequest,
    style: AuthStyle,
    key: &str,
) -> reqwest::RequestBuilder {
    let url = format!("{}{}", block.base_url.trim_end_matches('/'), request.path);

    let builder = match &request.body {
        Some(body) => client.post(&url).json(body),
        None => client.get(&url),
    };

    let builder = match style {
        AuthStyle::Bearer => builder.bearer_auth(key),
        AuthStyle::XApiKey => builder.header("x-api-key", key),
        AuthStyle::GoogleApiKey => builder.header("x-goog-api-key", key),
    };

    let mut builder = builder;
    for (name, value) in adapter_for(block.api_format).extra_headers() {
        builder = builder.header(name, value);
    }
    builder
}

/// 把请求发给上游，返回状态码与响应体。
///
/// 网络错误与超时都归为 Err；HTTP 状态码原样返回，由调用方分类处理——
/// 429、401 与 5xx 的后续动作完全不同，不能在这里替调用方做决定。
pub async fn send(
    client: &reqwest::Client,
    block: &UpstreamBlock,
    request: &UpstreamRequest,
    style: AuthStyle,
    key: &str,
) -> Result<(u16, Value)> {
    let response = prepare_request(client, block, request, style, key)
        .send()
        .await
        .map_err(|error| Error::UpstreamUnavailable(format!("请求上游失败: {error}")))?;

    let status = response.status().as_u16();
    // 上游在出错时可能返回空体或 HTML，解析不出就按空对象处理。
    let body = response.json::<Value>().await.unwrap_or(Value::Null);

    Ok((status, body))
}

/// 建立流式上游请求，返回原始响应供调用方消费字节流。
pub async fn send_stream(
    client: &reqwest::Client,
    block: &UpstreamBlock,
    request: &UpstreamRequest,
    style: AuthStyle,
    key: &str,
) -> Result<reqwest::Response> {
    prepare_request(client, block, request, style, key)
        .send()
        .await
        .map_err(|error| Error::UpstreamUnavailable(format!("请求上游失败: {error}")))
}

/// 一条日志的雏形：请求方信息固定，结果由各分支补齐。
fn base_log<'a>(
    key: &'a ApiKey,
    ip: &'a str,
    user_agent: Option<&'a str>,
    started: Instant,
) -> LogDraft<'a> {
    LogDraft {
        key,
        ip,
        user_agent,
        started,
        model: String::new(),
        block_id: None,
        upstream_key_id: None,
        usage: crate::proxy::scheme::Usage::default(),
        credits_scaled: 0,
        stream: false,
    }
}

/// 日志草稿：链式填入结果，最后落库。
struct LogDraft<'a> {
    key: &'a ApiKey,
    ip: &'a str,
    user_agent: Option<&'a str>,
    started: Instant,
    model: String,
    block_id: Option<String>,
    upstream_key_id: Option<String>,
    usage: crate::proxy::scheme::Usage,
    credits_scaled: i64,
    stream: bool,
}

impl<'a> LogDraft<'a> {
    /// 标记为流式请求。
    fn streaming(mut self) -> Self {
        self.stream = true;
        self
    }

    fn model(mut self, model: String) -> Self {
        self.model = model;
        self
    }

    fn block(mut self, block_id: &str) -> Self {
        self.block_id = Some(block_id.to_string());
        self
    }

    fn upstream_key(mut self, key_id: &str) -> Self {
        self.upstream_key_id = Some(key_id.to_string());
        self
    }

    fn succeeded(mut self, usage: crate::proxy::scheme::Usage, credits_scaled: i64) -> Self {
        self.usage = usage;
        self.credits_scaled = credits_scaled;
        self
    }

    /// 失败：错误详情进详细日志。
    fn failed(self, error: &Error) -> NewProxyLog {
        self.failed_with_context(error, None)
    }

    /// 失败：带块与上游密钥的上下文。
    fn failed_with_context(self, error: &Error, context: Option<(String, String)>) -> NewProxyLog {
        let (block_id, upstream_key_id) = context
            .map(|(block, key)| (Some(block), Some(key)))
            .unwrap_or((self.block_id.clone(), self.upstream_key_id.clone()));

        NewProxyLog {
            access_key_id: self.key.id.clone(),
            block_id,
            upstream_key_id,
            model: self.model.clone(),
            ip: Some(self.ip.to_string()),
            user_agent: self.user_agent.map(str::to_string),
            status: "error".to_string(),
            error_detail: Some(truncate_detail(&error.to_string())),
            duration_ms: self.started.elapsed().as_millis() as i64,
            prompt_tokens: self.usage.prompt_tokens,
            completion_tokens: self.usage.completion_tokens,
            cache_tokens: self.usage.cache_tokens,
            estimated: self.usage.estimated,
            stream: self.stream,
            credits_scaled: self.credits_scaled,
            created_at: time::now_secs(),
        }
    }

    /// 成功。
    fn to_success_log(&self) -> NewProxyLog {
        NewProxyLog {
            access_key_id: self.key.id.clone(),
            block_id: self.block_id.clone(),
            upstream_key_id: self.upstream_key_id.clone(),
            model: self.model.clone(),
            ip: Some(self.ip.to_string()),
            user_agent: self.user_agent.map(str::to_string),
            status: "success".to_string(),
            error_detail: None,
            duration_ms: self.started.elapsed().as_millis() as i64,
            prompt_tokens: self.usage.prompt_tokens,
            completion_tokens: self.usage.completion_tokens,
            cache_tokens: self.usage.cache_tokens,
            estimated: self.usage.estimated,
            stream: self.stream,
            credits_scaled: self.credits_scaled,
            created_at: time::now_secs(),
        }
    }
}

/// 落库一条日志。日志写失败不该拖垮业务请求。
async fn write_log(pool: &sqlx::SqlitePool, log: NewProxyLog) {
    if let Err(error) = crate::db::proxy_log::insert(pool, &log).await {
        tracing::warn!(error = %error, "写入代理日志失败");
    }
}
