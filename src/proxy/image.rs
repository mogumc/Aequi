//! OpenAI 生图协议的编排。
//!
//! 入口与上游都只有 OpenAI 一种格式（Google 与 Anthropic 的块没有生图端点），
//! 一对一的协议不需要中间标准层，直接透传——等第二家格式支持生图时再抽。
//!
//! 调度与双轨日志和对话共用同一套规则，**但不重试**：出图是带副作用的慢操作，
//! 详见 [`dispatch`] 的说明。

use std::sync::Arc;
use std::time::Instant;

use serde_json::{Value, json};

use crate::AppState;
use crate::error::{Error, Result};
use crate::model::ApiKey;
use crate::proxy::adapter::{AuthStyle, EXTRA_BODY_KEY, UpstreamRequest};
use crate::proxy::scheme::Usage;
use crate::proxy::{
    Failure, UpstreamResponse, base_log, estimate, log_failure, sanitize_user_message, write_log,
};
use crate::service::{billing, upstream};

/// 生图接口的路径，与入口同名。
const IMAGES_PATH: &str = "/v1/images/generations";

/// 每张生成图片的估算 token 数。
///
/// 只用于不报 usage 的上游（DALL-E 系列）；常数是粗略量级，
/// 记得它带着 estimated 标记——日志里看得到哪些数字是估的。
const IMAGE_TOKENS_PER_PICTURE: i64 = 5000;

/// 一次成功生图的产出。
struct ImageOutcome {
    response: Value,
    usage: Usage,
    charge: billing::ChargeOutcome,
    block_id: String,
    upstream_key_id: String,
}

/// 编排一次生图请求。
///
/// 无论成败都会落两轨日志；日志写入失败只留警告，不拖垮业务请求。
///
/// 与对话的非流式路径同理：编排跑在脱离的 spawn 任务里，客户端中途断开
/// 时 handler 的 future 被丢弃，但这笔已经付给上游的钱照常记账落日志。
pub async fn images_generations(
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
        dispatch_entry(
            &state, &key, &body, &ip, user_agent.as_deref(), header_block.as_deref(),
        )
        .await
    });

    handle
        .await
        .map_err(|error| Error::Internal(anyhow::anyhow!("生图请求任务异常: {error}")))?
}

/// [`images_generations`] 的实际编排体，跑在脱离的任务里。
async fn dispatch_entry(
    state: &Arc<AppState>,
    key: &ApiKey,
    body: &Value,
    ip: &str,
    user_agent: Option<&str>,
    header_block: Option<&str>,
) -> Result<Value> {
    let started = Instant::now();
    let pool = &state.store.data;

    let model = match body
        .get("model")
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty())
    {
        Some(model) => model.to_string(),
        None => {
            let error = Error::BadRequest("请求里缺少 model".to_string());
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
    let target = match upstream::resolve_with_header(pool, &model, header_block).await {
        Ok(target) => target,
        Err(error) => {
            let failure = Failure::bare(error);
            log_failure(
                pool,
                key,
                ip,
                user_agent,
                &model,
                started,
                false,
                &failure,
                &[],
            )
            .await;
            return Err(failure.error);
        }
    };
    let model = &target.model;
    let only = target.block_id.as_deref();

    // 余额为零（且不是无限额度）时直接拒绝，免得替用户白白消耗上游。
    if !key.unlimited && key.credits_scaled <= 0 {
        let error = Error::InsufficientCredits;
        write_log(
            pool,
            base_log(key, ip, user_agent, started)
                .model(model.clone())
                .failed(&error),
        )
        .await;
        return Err(error);
    }

    match dispatch(state, key, body, model, started, only).await {
        Ok(outcome) => {
            write_log(
                pool,
                base_log(key, ip, user_agent, started)
                    .model(model.clone())
                    .block(&outcome.block_id)
                    .upstream_key(&outcome.upstream_key_id)
                    .succeeded(outcome.usage, outcome.charge.credits_scaled)
                    .to_success_log(),
            )
            .await;

            Ok(outcome.response)
        }
        Err(failure) => {
            // 管理员看到细节，密钥侧只看到脱敏描述。
            // 生图不重试，所以没有重试链可带。
            log_failure(pool, key, ip, user_agent, model, started, false, &failure, &[]).await;

            Err(failure.error)
        }
    }
}

/// 挑块、挑密钥、发送、计费——**只发一次，失败即返回**。
///
/// 生图与对话最大的不同在这里：对话换把钥匙重试几乎是免费的（上游没产出
/// 就不计费），出图却是有副作用的慢操作——单张图动辄几十秒，重试等于把最长
/// 的等待再翻一倍，而且上游很可能**已经出图并计了费**，我们却拿不到结果。
/// 所以宁可如实把错误交回调用方（由它决定要不要重发），也不替它把时间与
/// 额度一起赌进去。
///
/// 「不重试」不等于「不记账」：上游为什么会失败（限流 / 密钥被拒 / 5xx）
/// 仍照记到密钥头上，该退避的退避、该停用的停用。
async fn dispatch(
    state: &Arc<AppState>,
    key: &ApiKey,
    body: &Value,
    model: &str,
    started: Instant,
    // 请求里显式锁定的块；None 即按权重轮询所有能处理该模型的块。
    only: Option<&str>,
) -> std::result::Result<ImageOutcome, Failure> {
    let pool = &state.store.data;

    // `select_block` 的失败是确定性的（模型没上游 / 块不提供该模型 /
    // 密钥无权用它），不需要留上一轮——这里只有一轮。
    let block = upstream::select_block(pool, &state.upstream, key, model, only)
        .await
        .map_err(Failure::bare)?;

    // 生图目前只有 OpenAI 格式的上游有对应端点。
    if block.api_format != crate::model::ApiFormat::Openai {
        let message = format!("上游块 {} 的格式不支持生图", block.id);
        return Err(Failure::with_context(
            Error::BadRequest(message.clone()),
            message,
            Some((block.id, String::new())),
        ));
    }

    // 块内选不出密钥（配置问题 403 / 全在退避 429）——照实回给调用方。
    let upstream_key = upstream::select_key(pool, &state.upstream, &block)
        .await
        .map_err(|error| {
            let user_message = sanitize_user_message(&error);
            Failure::with_context(error, user_message, Some((block.id.clone(), String::new())))
        })?;

    // 模型名替换成块里配置的转发名，其余字段原样透传。
    let mut outgoing = body.clone();
    outgoing["model"] = json!(block.target_model(model));
    // 额外请求体只认管理端配置的。生图是透传路径，下游塞的同名键
    // 不会被任何白名单解析挡住，必须显式丢掉。
    if let Some(object) = outgoing.as_object_mut() {
        object.remove(EXTRA_BODY_KEY);
    }

    let mut request = UpstreamRequest {
        path: IMAGES_PATH.to_string(),
        body: Some(outgoing),
    };
    request.apply_extra_body(block.extra_body(model));

    let client = state.upstream.client(&block).map_err(|error| {
        // 代理地址之类的出口细节不进用户能查到的表。
        let user_message = sanitize_user_message(&error);
        Failure::with_context(error, user_message, Some((block.id.clone(), upstream_key.id.clone())))
    })?;

    let context = Some((block.id.clone(), upstream_key.id.clone()));

    // 并发闸门：与对话同一套规则——块配了上限就排队等位子。
    let _permit = crate::proxy::acquire_gate(state, &block)
        .await
        .map_err(|error| {
            let user_message = sanitize_user_message(&error);
            Failure::with_context(error, user_message, context.clone())
        })?;

    let sent = crate::proxy::send(
        &client,
        &block,
        &request,
        AuthStyle::Bearer,
        &upstream_key.secret,
    )
    .await;

    let response = match sent {
        Ok(response) => response,
        Err(error) => {
            // reqwest 的报错可能带 URL，不进用户能查到的表。
            // 超时也走这里：生图没有单独的超时，与对话共用那条 120 秒总超时。
            let user_message = sanitize_user_message(&error);
            upstream::report_failure(
                pool,
                &state.upstream,
                &upstream_key,
                upstream::FailureKind::Transient,
                &error.to_string(),
                None,
            )
            .await;
            return Err(Failure::with_context(error, user_message, context));
        }
    };

    let UpstreamResponse {
        status,
        body: response_body,
        retry_after,
    } = response;

    if status == 429 {
        let detail = error_message(&response_body);
        let wait = upstream::report_failure(
            pool,
            &state.upstream,
            &upstream_key,
            upstream::FailureKind::RateLimited,
            &format!("上游返回 429 {detail}"),
            retry_after,
        )
        .await;
        // 上游没给提示时退回这把钥匙刚挂上的退避秒数；下游至少别比我们更早回头打。
        return Err(Failure::with_context(
            Error::RateLimited {
                message: "上游限流，请稍后重试".to_string(),
                retry_after: retry_after.or(wait),
            },
            "上游限流，请稍后重试".to_string(),
            context,
        )
        // 对下游只能给句稳妥话；日志里要留下上游的原话。
        .with_detail(format!("上游返回 429 {detail}")));
    }

    // 密钥被拒：下游绝不能收到 401/403——那是我们的钥匙坏了，
    // 不是客户端凭证有问题；对他们来说是上游不可用。
    if status == 401 || status == 403 {
        let detail = error_message(&response_body);
        upstream::report_failure(
            pool,
            &state.upstream,
            &upstream_key,
            upstream::FailureKind::Hard,
            &format!("上游返回 {status} {detail}"),
            // 硬失败不退避，上游提示（若有）也无处可用。
            None,
        )
        .await;
        return Err(Failure::with_context(
            Error::UpstreamUnavailable(format!("上游返回 {status} {detail}")),
            format!("上游响应异常: 返回 {status}"),
            context,
        ));
    }

    // 上游自己的故障（5xx）不算密钥的账：只留痕，不退避不停用——
    // 按住一把钥匙只会让下一个请求撞上另一把，整块被同一个坏上游拖住。
    if status >= 500 {
        let detail = error_message(&response_body);
        upstream::report_failure(
            pool,
            &state.upstream,
            &upstream_key,
            upstream::FailureKind::Transient,
            &format!("上游返回 {status} {detail}"),
            // 不退避，响应头里的 Retry-After 也无处可用。
            None,
        )
        .await;
        return Err(Failure::with_context(
            Error::UpstreamUnavailable(format!("上游返回 {status} {detail}")),
            format!("上游响应异常: 返回 {status}"),
            context,
        ));
    }

    if !(200..300).contains(&status) {
        // 请求本身的问题（参数、尺寸不支持等）：重发也不会好。
        let detail = error_message(&response_body);
        return Err(Failure::with_context(
            Error::UpstreamUnavailable(format!("上游返回 {status} {detail}")),
            format!("上游响应异常: 返回 {status}"),
            context,
        ));
    }

    // 200 却没有 data，多半是网关吐了故障页。
    let Some(data) = response_body.get("data").and_then(Value::as_array) else {
        let error = Error::UpstreamUnavailable("上游响应里没有 data".to_string());
        let user_message = sanitize_user_message(&error);
        upstream::report_failure(
            pool,
            &state.upstream,
            &upstream_key,
            upstream::FailureKind::Transient,
            &error.to_string(),
            // 200 的响应没有可用的限流提示。
            None,
        )
        .await;
        return Err(Failure::with_context(error, user_message, context));
    };

    upstream::report_success(pool, &state.upstream, &upstream_key.id).await;

    // gpt-image-1 等新一代接口自带 usage；DALL-E 系列按张估算。
    let usage = parse_usage(&response_body).unwrap_or_else(|| {
        Usage::estimated(
            estimate::estimate_text(body.get("prompt").and_then(Value::as_str).unwrap_or("")),
            IMAGE_TOKENS_PER_PICTURE * data.len().max(1) as i64,
        )
    });

    let charge = billing::charge(
        pool,
        key,
        &block.id,
        model,
        usage,
        block.multiplier_bp(model),
        false,
    )
    .await
    .map_err(|error| {
        let user_message = sanitize_user_message(&error);
        Failure::with_context(error, user_message, context)
    })?;

    tracing::info!(
        model = %model,
        block = %block.id,
        pictures = data.len(),
        tokens = charge.usage.total_tokens,
        estimated = charge.usage.estimated,
        credits = %charge.credits_scaled,
        duration_ms = started.elapsed().as_millis() as u64,
        "生图请求完成"
    );

    Ok(ImageOutcome {
        response: response_body,
        usage: charge.usage,
        charge,
        block_id: block.id,
        upstream_key_id: upstream_key.id,
    })
}

/// 上游错误体里的可读说明；错误体可能是 HTML、空体或没有这个字段，此时给空串。
fn error_message(body: &Value) -> &str {
    body.pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("")
}

/// gpt-image-1 等新一代生图接口会在响应里带 usage；没有则返回 None 走估算。
fn parse_usage(body: &Value) -> Option<Usage> {
    let usage = body.get("usage")?;
    let total = usage.get("total_tokens").and_then(Value::as_i64)?;
    if total <= 0 {
        return None;
    }

    Some(Usage {
        prompt_tokens: usage
            .get("input_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        completion_tokens: usage
            .get("output_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        total_tokens: total,
        cache_tokens: 0,
        estimated: false,
    })
}
