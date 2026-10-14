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
use tokio::sync::OwnedSemaphorePermit;

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
pub(crate) mod diagnostics;
pub mod estimate;
pub mod image;
pub mod openai_in;
pub mod scheme;
pub mod sse;

/// 一次请求最多换几把密钥重试。
///
/// 只为可恢复的错误重试（限流、上游 5xx、网络故障）；请求本身有问题时换密钥没有意义。
const MAX_ATTEMPTS: usize = 3;

/// 在块的并发闸门前排队等一个位子。
///
/// 块没配上限（`max_concurrency` 为 0）时不等，直接放行。等不到就明说
/// 「通道繁忙」——客户端已经等了 [`upstream::GATE_WAIT_TIMEOUT`]，再重试
/// 一遍无非是再等一遍，不如让它带着明确的错误回去。
///
/// 返回的位子与调用方的动作同生命周期：非流式请求持有到响应解析完，
/// 流式请求一直持有到流结束（流占着上游连接，那正是要限流的东西）。
pub(crate) async fn acquire_gate(
    state: &AppState,
    block: &UpstreamBlock,
) -> Result<Option<OwnedSemaphorePermit>> {
    let Some(gate) = state.upstream.gate(block) else {
        return Ok(None);
    };

    match tokio::time::timeout(upstream::GATE_WAIT_TIMEOUT, gate.acquire()).await {
        Ok(Ok(permit)) => Ok(Some(permit)),
        // 超时会连排队计数一起收（守卫靠 Drop 结账），面板上的排队数不会虚高。
        Ok(Err(error)) => Err(error),
        Err(_) => Err(Error::UpstreamUnavailable(
            "上游通道繁忙，排队超时".to_string(),
        )),
    }
}

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
///
/// 整个编排跑在一个**脱离的 spawn 任务**里，调用方只负责等它的结果。
/// 原因是客户端断开时 axum 会直接丢弃 handler 的 future——如果记账写在
/// 这个 future 里，上游已经把 token 生成出来了，这笔钱却不会有人付账，
/// 日志里也查无此请求。`tokio::spawn` 的任务不随 JoinHandle 被丢弃而取消，
/// 所以无论客户端怎么跑路，账都一定落下来。
pub async fn chat_completion(
    state: &Arc<AppState>,
    key: &ApiKey,
    body: &Value,
    ip: &str,
    user_agent: Option<&str>,
    header_block: Option<&str>,
) -> Result<Value> {
    let state = Arc::clone(state);
    let key = key.clone();
    let body = body.clone();
    let ip = ip.to_string();
    let user_agent = user_agent.map(str::to_string);
    let header_block = header_block.map(str::to_string);

    let handle = tokio::spawn(async move {
        chat_completion_inner(&state, &key, &body, &ip, user_agent.as_deref(), header_block.as_deref())
            .await
    });

    handle
        .await
        .map_err(|error| Error::Internal(anyhow::anyhow!("对话请求任务异常: {error}")))?
}

/// [`chat_completion`] 的实际编排体，跑在脱离的任务里。
async fn chat_completion_inner(
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

            // 对外模型名（剥掉块前缀的那个）——与流式帧、日志、用量同一口径。
            // 刻意不用上游回显：Google 的响应里根本没有模型名。
            Ok(openai_in::render_response(
                &outcome.response,
                &id,
                created,
                &request.model,
            ))
        }
        Err(failure) => {
            // 失败也要留下痕迹：管理员看到细节，用户只看到脱敏描述。
            // 块 id 要先取出来——`.failed_with_context()` 会把它移走。
            let failed_block = failure.context.as_ref().map(|(block, _)| block.clone());
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
                // 拿不到块（还没选中就失败）时降级为空，不影响记账。
                failed_block.as_deref(),
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
    // 并发闸门的位子：建流成功后要跟着流走（流占着上游连接直到结束），
    // 所以声明在循环外；失败轮次里它是循环内局部，随手释放。
    let mut stream_permit: Option<OwnedSemaphorePermit> = None;

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
        // 额外请求体只来自管理端的块配置；下游请求体里的同名键在入口解析层
        // 就被丢了（`openai_in::parse_request` 是白名单解析）。
        outgoing.apply_extra_body(block.extra_body(&request.model));
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

        // 并发闸门：块配了上限就在这里排队。位子跟本轮 attempt 同生死——
        // 失败换块时先释放，新一轮重新排；成功则移交给流（见下方）。
        let permit = match acquire_gate(&state, &block).await {
            Ok(permit) => permit,
            Err(error) => {
                // 排队超时也落两轨日志：请求进来了却没被服务，排障时
                // 要能看出是块堵了，而不是上游病了。
                let user_message = sanitize_user_message(&error);
                write_log(
                    pool,
                    base_log(&key, &ip, user_agent.as_deref(), started)
                        .model(request.model.clone())
                        .block(&block.id)
                        .streaming()
                        .failed(&error),
                )
                .await;
                if let Err(log_error) = crate::db::usage::insert_error(
                    pool,
                    &key.id,
                    &request.model,
                    Some(block.id.as_str()),
                    &user_message,
                    true,
                    time::now_secs(),
                )
                .await
                {
                    tracing::warn!(error = %log_error, "写入失败用量记录失败");
                }
                return Err(error);
            }
        };

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
                    // 位子移交给流：整条流占着这个上游连接，直到它结束。
                    stream_permit = permit;
                    break 'attempt;
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
                        upstream::report_failure(
                            pool,
                            &upstream_key,
                            upstream::FailureKind::RateLimited,
                            "被限流，暂时退避",
                        )
                        .await;
                        last_failure = Some(Failure::with_context(
                            Error::UpstreamUnavailable("上游限流".to_string()),
                            "上游限流".to_string(),
                            context,
                        ));
                        continue 'attempt;
                    }
                    if status == 401 || status == 403 {
                        upstream::report_failure(
                            pool,
                            &upstream_key,
                            upstream::FailureKind::Hard,
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
                    if status >= 500 {
                        upstream::report_failure(
                            pool,
                            &upstream_key,
                            upstream::FailureKind::Transient,
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
                        Some(block.id.as_str()),
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
                    upstream::report_failure(
                        pool,
                        &upstream_key,
                        upstream::FailureKind::Transient,
                        &error.to_string(),
                    )
                    .await;
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
        // 同上：块 id 要先取出来，`.failed_with_context()` 会移走 context。
        let failed_block = failure.context.as_ref().map(|(block, _)| block.clone());
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
            failed_block.as_deref(),
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

    // 计费与日志跑在一个**独立任务**里，不挂在响应流的生命周期上。
    //
    // 这是「客户端中途断开也要算账」的关键：客户端一断，持有接收端的一方
    // 被销毁，下面每次发送随即失败——但这个任务本身不会被销毁，它跳出
    // 转发循环后照常走到收尾，把中断前已产出的内容按估算计费并落日志。
    // 反过来，账也就不会漏：流式请求一旦发出，上游就开始消耗额度，
    // 这笔消耗由我们自己向上游付，不该因为客户端改主意就白送。
    //
    // 中断前一个字都没产出时，收尾的 `!produced` 分支照旧按失败处理，
    // 不估算也不计费——那种情况上游也没真正开始生成。
    let (tx, rx) = tokio::sync::mpsc::channel::<std::result::Result<Vec<u8>, std::io::Error>>(32);

    tokio::spawn(async move {
        // 并发闸门的位子跟着流走：整条流结束（或客户端断开、转发停止）
        // 才释放。提前 drop 会让后面的请求挤占一个还在飞的上游连接。
        let _stream_permit = stream_permit;
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
        //（最常见的原因是块格式与上游不匹配，解析器什么都提不出来）。
        let mut last_payload: Option<String> = None;
        let mut sse = std::pin::pin!(payload_stream);
        // 客户端是否在流中途离开。转发到此为止，账照算。
        let mut client_gone = false;

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

        // 开头那一帧发不出去就没什么可算的（上游尚未产出任何内容）。
        if tx.send(frame(openai_in::render_stream_role(&id, created, &model))).await.is_err() {
            return;
        }

        // 主循环。客户端断开时跳出转发，落到下方收尾照常记账。
        'upstream: while let Some(item) = sse.next().await {
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
                            .block(&block_id)
                            .upstream_key(&upstream_key_id)
                            .streaming()
                            .failed(&error),
                    )
                    .await;
                    if let Err(log_error) = crate::db::usage::insert_error(
                        pool,
                        &key.id,
                        &model,
                        Some(block_id.as_str()),
                        &user_message,
                        true,
                        time::now_secs(),
                    )
                    .await
                    {
                        tracing::warn!(error = %log_error, "写入失败用量记录失败");
                    }
                    // 客户端已经走了就别费劲发错误帧，但仍要把已产出的
                    // 内容按估算记上去——上游那部分额度是真花掉了的。
                    if !produced {
                        let _ = tx.send(Ok(error_frame(&user_message))).await;
                        return;
                    }
                    client_gone = true;
                    break 'upstream;
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
                            .block(&block_id)
                            .upstream_key(&upstream_key_id)
                            .streaming()
                            .failed(&error),
                    )
                    .await;
                    if let Err(log_error) = crate::db::usage::insert_error(
                        pool,
                        &key.id,
                        &model,
                        Some(block_id.as_str()),
                        &user_message,
                        true,
                        time::now_secs(),
                    )
                    .await
                    {
                        tracing::warn!(error = %log_error, "写入失败用量记录失败");
                    }
                    if !produced {
                        let _ = tx.send(Ok(error_frame(&user_message))).await;
                        return;
                    }
                    client_gone = true;
                    break 'upstream;
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
                        if tx
                            .send(frame(openai_in::render_stream_delta(
                                &id,
                                created,
                                &model,
                                &reasoning,
                                &text,
                                &tool_calls,
                            )))
                            .await
                            .is_err()
                        {
                            client_gone = true;
                            break 'upstream;
                        }
                    }
                    StreamEvent::Finish { finish_reason } => {
                        if !finished {
                            finished = true;
                            if tx
                                .send(frame(openai_in::render_stream_finish(
                                    &id, created, &model, finish_reason,
                                )))
                                .await
                                .is_err()
                            {
                                client_gone = true;
                                break 'upstream;
                            }
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
                            if tx
                                .send(frame(openai_in::render_stream_delta(
                                    &id,
                                    created,
                                    &model,
                                    "",
                                    "",
                                    &deltas,
                                )))
                                .await
                                .is_err()
                            {
                                client_gone = true;
                                break 'upstream;
                            }
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
                Some(block_id.as_str()),
                "上游响应异常: 上游没有返回任何内容",
                true,
                time::now_secs(),
            )
            .await
            {
                tracing::warn!(error = %log_error, "写入失败用量记录失败");
            }
            // 给客户端的是不含内部细节的短句；原始载荷进日志供排障。
            let _ = tx
                .send(Ok(error_frame("上游提前结束了响应且没有返回任何内容，请重试")))
                .await;
            return;
        }

        // 上游没发结束原因就补一个 Stop，避免客户端把流挂起。
        // 客户端已经断开时这一步没有意义，也发不出去。
        if !finished && !client_gone {
            let _ = tx
                .send(frame(openai_in::render_stream_finish(
                    &id, created, &model, FinishReason::Stop,
                )))
                .await;
        }

        // 计费：上游报的用量聚合；缺哪一项就用本地估算补哪一项。
        // 思考也是输出成本——估算路径把推理与正文拼在一起算，
        // 与上游 usage 里 completion 已含 reasoning 的口径一致。
        //
        // 客户端中途断开走到这里时，上游多半没能发出 usage（usage 一般在
        // 流的末尾），于是走补齐：已产出的文本一个字都不会少算。
        let output = format!("{reasoning_acc}{text_acc}");
        let usage = estimate::complete_usage(
            usage_acc.prompt_tokens,
            usage_acc.completion_tokens,
            usage_acc.total_tokens,
            usage_acc.cache_tokens,
            &request,
            &output,
        );

        match billing::charge(pool, &key, &block_id, &model, usage, multiplier_bp, true).await {
            Ok(charge) => {
                // 客户端中途断开要看得见：账照扣，但日志里注明原因，
                // 否则管理员看到一条成功记录却对不上客户端的体验。
                write_log(
                    pool,
                    base_log(&key, &ip, user_agent.as_deref(), started)
                        .model(request.model.clone())
                        .block(&block_id)
                        .upstream_key(&upstream_key_id)
                        .streaming()
                        .client_gone(client_gone)
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
                    client_gone,
                    "对话请求完成"
                );

                // 客户端已断开，usage 帧与 [DONE] 都无处可送。
                if !client_gone {
                    let _ = tx
                        .send(frame(openai_in::render_stream_usage(
                            &id, created, &model, charge.usage,
                        )))
                        .await;
                    let _ = tx.send(Ok(b"data: [DONE]\n\n".to_vec())).await;
                }
            }
            Err(error) => {
                let user_message = sanitize_user_message(&error);
                // 这里的 block_id 会随 context 移进日志，先留一份给用量记录。
                let failed_block = block_id.clone();
                write_log(
                    pool,
                    base_log(&key, &ip, user_agent.as_deref(), started)
                        .model(request.model.clone())
                        .block(&block_id)
                        .upstream_key(&upstream_key_id)
                        .streaming()
                        .failed_with_context(&error, Some((block_id, upstream_key_id))),
                )
                .await;
                if let Err(log_error) = crate::db::usage::insert_error(
                    pool,
                    &key.id,
                    &model,
                    Some(failed_block.as_str()),
                    &user_message,
                    true,
                    time::now_secs(),
                )
                .await
                {
                    tracing::warn!(error = %log_error, "写入失败用量记录失败");
                }
                if !client_gone {
                    let _ = tx.send(Ok(error_frame(&user_message))).await;
                }
            }
        }
    });

    Ok(tokio_stream::wrappers::ReceiverStream::new(rx))
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
        let mut outgoing = adapter.build_chat(request, &block.target_model(&request.model));
        // 同流式：额外请求体只认管理端配置（下游键已被白名单解析丢弃）。
        outgoing.apply_extra_body(block.extra_body(&request.model));
        let client = match state.upstream.client(&block) {
            Ok(client) => client,
            Err(error) => {
                let context = Some((block.id.clone(), upstream_key.id.clone()));
                // 代理地址之类的出口细节不进用户能查到的表。
                let user_message = sanitize_user_message(&error);
                return Err(Failure::with_context(error, user_message, context));
            }
        };

        // 并发闸门：块配了上限就排队等位子，位子持有到本轮结束——
        // 重试换块时旧位子先释放，下一轮重新排。
        let _permit = match acquire_gate(state, &block).await {
            Ok(permit) => permit,
            Err(error) => {
                let context = Some((block.id.clone(), upstream_key.id.clone()));
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
                    upstream::report_failure(
                        pool,
                        &upstream_key,
                        upstream::FailureKind::RateLimited,
                        "被限流，暂时退避",
                    )
                    .await;
                    last_failure = Some(Failure::with_context(
                        Error::UpstreamUnavailable("上游限流".to_string()),
                        "上游限流".to_string(),
                        context,
                    ));
                    continue;
                }

                // 密钥被拒或上游故障：记一次失败，换一把再试。
                // 上游的原话只进管理员日志，密钥侧只看到状态码。
                if status == 401 || status == 403 {
                    let detail = response_body
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    upstream::report_failure(
                        pool,
                        &upstream_key,
                        upstream::FailureKind::Hard,
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

                // 上游自己的故障（5xx）不算密钥的账：指数退避后换一把，
                // 不停用——上游打个嗝不该把好密钥永久禁掉。
                if status >= 500 {
                    let detail = response_body
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    upstream::report_failure(
                        pool,
                        &upstream_key,
                        upstream::FailureKind::Transient,
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
                        upstream::report_failure(
                            pool,
                            &upstream_key,
                            upstream::FailureKind::Transient,
                            &error.to_string(),
                        )
                        .await;
                        last_failure = Some(Failure::with_context(error, user_message, context));
                        continue;
                    }
                };
                upstream::report_success(pool, &upstream_key.id).await;

                // 上游没报用量、或只报了一半，就用本地估算补齐缺的那项，
                // 记账不能被钻空子。估算口径含推理文本——思考模型的思考
                // 部分不是免费的。
                let usage = estimate::complete_usage(
                    response.usage.prompt_tokens,
                    response.usage.completion_tokens,
                    response.usage.total_tokens,
                    response.usage.cache_tokens,
                    request,
                    &response.output_for_estimate(),
                );

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
                upstream::report_failure(
                    pool,
                    &upstream_key,
                    upstream::FailureKind::Transient,
                    &error.to_string(),
                )
                .await;
                last_failure = Some(Failure::with_context(error, user_message, context));
            }
        }
    }

    Err(last_failure
        .unwrap_or_else(|| Failure::bare(Error::UpstreamUnavailable("上游不可用".to_string()))))
}

/// 认证头名。写成常量是为了和 [`AuthStyle`] 的三种放法一一对应地读。
const HEADER_X_API_KEY: &str = "x-api-key";
const HEADER_X_GOOG_API_KEY: &str = "x-goog-api-key";

/// 把一个头名与头值变成 HTTP 头，塞不进去就在**发请求之前**报错。
///
/// 写入口（[`crate::service::upstream::create_block`]）已经校验过一遍，
/// 走到这里还不合法说明库里是脏数据——手改的、或更早的版本写进去的。
/// 用 [`Error::Internal`] 而不是 `BadRequest`：这是我们这边的配置问题，
/// 不该让密钥持有者从错误文案里读出块的配置细节；管理员日志里有原文。
fn build_header(
    name: &str,
    value: &str,
) -> Result<(reqwest::header::HeaderName, reqwest::header::HeaderValue)> {
    let name = reqwest::header::HeaderName::try_from(name)
        .map_err(|_| Error::Internal(anyhow::anyhow!("块的请求头名不合法: {name}")))?;
    let value = reqwest::header::HeaderValue::try_from(value)
        .map_err(|_| Error::Internal(anyhow::anyhow!("块的请求头 {name} 的值不合法")))?;
    Ok((name, value))
}

/// 组装发往上游的请求（路径、各家的固定头、认证头、块上配的自定义头）。
///
/// 对块外也可见：管理面的「拉取上游模型」与「测试密钥」走的是同一个出口，
/// 认证与自定义头不该在那里再抄一遍。
///
/// 头是拼成**一张表**而不是一路 `.header()` 叠上去的：`RequestBuilder::header`
/// 是 append 语义，同名会变成两个值一起发出去。顺序是各家固定头 → 认证头 →
/// 自定义头（`insert` 覆盖），于是**块上配的说了算**——「上游要的头名不是
/// 标准那三个」「版本头得换一个值」这类要求，配一下就行，不用改代码。
pub(crate) fn prepare_request(
    client: &reqwest::Client,
    block: &UpstreamBlock,
    request: &UpstreamRequest,
    style: AuthStyle,
    key: &str,
) -> Result<reqwest::RequestBuilder> {
    let url = format!("{}{}", block.base_url.trim_end_matches('/'), request.path);

    let mut headers = reqwest::header::HeaderMap::new();

    for (name, value) in adapter_for(block.api_format).extra_headers() {
        let (name, value) = build_header(name, value)?;
        headers.insert(name, value);
    }

    let (auth_name, auth_value) = match style {
        AuthStyle::Bearer => (
            reqwest::header::AUTHORIZATION.as_str(),
            format!("Bearer {key}"),
        ),
        AuthStyle::XApiKey => (HEADER_X_API_KEY, key.to_string()),
        AuthStyle::GoogleApiKey => (HEADER_X_GOOG_API_KEY, key.to_string()),
    };
    let (auth_name, mut auth_value) = build_header(auth_name, &auth_value)?;
    // 上游密钥是凭证：标成 sensitive，免得被任何日志/调试输出带出去。
    auth_value.set_sensitive(true);
    headers.insert(auth_name, auth_value);

    for entry in &block.headers {
        let (name, value) = build_header(&entry.name, &entry.value)?;
        headers.insert(name, value);
    }

    let builder = match &request.body {
        Some(body) => client.post(&url).headers(headers).json(body),
        None => client.get(&url).headers(headers),
    };

    Ok(builder)
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
    let response = prepare_request(client, block, request, style, key)?
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
    prepare_request(client, block, request, style, key)?
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
        client_gone: false,
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
    /// 客户端在流中途断开：账照扣，日志里要能看出原因。
    client_gone: bool,
}

impl<'a> LogDraft<'a> {
    /// 标记为流式请求。
    fn streaming(mut self) -> Self {
        self.stream = true;
        self
    }

    /// 标记为客户端中途断开。
    fn client_gone(mut self, client_gone: bool) -> Self {
        self.client_gone = client_gone;
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
        // 客户端中断的流仍是成功账（上游确实出了 token），但错误列要留个
        // 记号，否则管理员看到成功记录却对不上客户端那边的体验。
        let error_detail = if self.client_gone {
            Some("客户端在流中途断开；已产出部分按估算计费".to_string())
        } else {
            None
        };

        NewProxyLog {
            access_key_id: self.key.id.clone(),
            block_id: self.block_id.clone(),
            upstream_key_id: self.upstream_key_id.clone(),
            model: self.model.clone(),
            ip: Some(self.ip.to_string()),
            user_agent: self.user_agent.map(str::to_string),
            status: "success".to_string(),
            error_detail,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ApiFormat, HeaderEntry};

    /// 一个不联网的块。测试只组装请求（`build`），不发出去。
    fn block(format: ApiFormat, headers: Vec<HeaderEntry>) -> UpstreamBlock {
        UpstreamBlock {
            id: "demo-block".to_string(),
            name: "demo".to_string(),
            api_format: format,
            weight: 1,
            max_concurrency: 0,
            base_url: "https://demo-upstream.invalid".to_string(),
            proxy: None,
            models: Vec::new(),
            headers,
            enabled: true,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn header(name: &str, value: &str) -> HeaderEntry {
        HeaderEntry {
            name: name.to_string(),
            value: value.to_string(),
        }
    }

    /// 走一遍真实的组装路径，取出最终会发出去的那张头表。
    fn built_headers(block: &UpstreamBlock, style: AuthStyle) -> reqwest::header::HeaderMap {
        let client = reqwest::Client::new();
        let probe = UpstreamRequest {
            path: "/v1/models".to_string(),
            body: None,
        };

        prepare_request(&client, block, &probe, style, "sk-demo-secret")
            .expect("组装请求")
            .build()
            .expect("构建请求")
            .headers()
            .clone()
    }

    fn value_of(headers: &reqwest::header::HeaderMap, name: &str) -> String {
        headers
            .get(name)
            .unwrap_or_else(|| panic!("缺少请求头 {name}"))
            .to_str()
            .expect("头值应当是可读文本")
            .to_string()
    }

    #[test]
    fn custom_headers_ride_along_with_every_request() {
        let block = block(ApiFormat::Openai, vec![header("X-Tenant", "demo-tenant")]);
        let headers = built_headers(&block, AuthStyle::Bearer);

        assert_eq!(value_of(&headers, "x-tenant"), "demo-tenant");
        assert_eq!(
            value_of(&headers, reqwest::header::AUTHORIZATION.as_str()),
            "Bearer sk-demo-secret",
            "配了自定义头也不能把认证头挤掉"
        );
    }

    #[test]
    fn without_custom_headers_defaults_are_unchanged() {
        // 回归线：没配自定义头时，各家的固定头与认证头必须与从前一字不差。
        let headers = built_headers(&block(ApiFormat::Anthropic, Vec::new()), AuthStyle::XApiKey);
        assert_eq!(value_of(&headers, "anthropic-version"), "2023-06-01");
        assert_eq!(value_of(&headers, "x-api-key"), "sk-demo-secret");

        let headers = built_headers(&block(ApiFormat::Google, Vec::new()), AuthStyle::GoogleApiKey);
        assert_eq!(value_of(&headers, "x-goog-api-key"), "sk-demo-secret");

        let headers = built_headers(&block(ApiFormat::Openai, Vec::new()), AuthStyle::Bearer);
        assert_eq!(
            value_of(&headers, reqwest::header::AUTHORIZATION.as_str()),
            "Bearer sk-demo-secret"
        );
    }

    #[test]
    fn custom_headers_override_builtin_ones() {
        // 上游要什么写法由配置决定，而不是由我们的默认值决定：
        // 版本头换一个值、认证头换个名字（甚至整条换掉）都要能表达。
        let block = block(
            ApiFormat::Anthropic,
            vec![
                header("Anthropic-Version", "2099-01-01"),
                header("x-api-key", "literal-token"),
            ],
        );
        let headers = built_headers(&block, AuthStyle::XApiKey);

        assert_eq!(value_of(&headers, "anthropic-version"), "2099-01-01");
        assert_eq!(value_of(&headers, "x-api-key"), "literal-token");
        // 覆盖而不是追加：`RequestBuilder::header` 是 append 语义，同名会变成
        // 两个值同时发出去，上游看到哪个就说不准了。
        assert_eq!(
            headers.get_all("x-api-key").iter().count(),
            1,
            "同名头必须只留一条"
        );
    }

    #[test]
    fn custom_header_name_case_is_not_significant() {
        // 头名大小写不敏感，`Authorization` 配上去一样要盖住认证头。
        let block = block(
            ApiFormat::Openai,
            vec![header("Authorization", "literal-token")],
        );
        let headers = built_headers(&block, AuthStyle::Bearer);

        assert_eq!(
            value_of(&headers, reqwest::header::AUTHORIZATION.as_str()),
            "literal-token"
        );
        assert_eq!(
            headers
                .get_all(reqwest::header::AUTHORIZATION)
                .iter()
                .count(),
            1
        );
    }

    #[test]
    fn stored_header_that_cannot_be_sent_fails_loudly() {
        // 库里若是脏数据（绕过了写入口的校验），必须在发请求前报错，
        // 而不是静默少带一个头让上游去猜。
        let block = block(ApiFormat::Openai, vec![header("X-Tenant", "bad\nvalue")]);
        let client = reqwest::Client::new();
        let probe = UpstreamRequest {
            path: "/v1/models".to_string(),
            body: None,
        };

        assert!(
            prepare_request(&client, &block, &probe, AuthStyle::Bearer, "sk-demo-secret").is_err(),
            "非法头值必须报错"
        );
    }
}
