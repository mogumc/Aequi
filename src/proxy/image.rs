//! OpenAI 生图协议的编排。
//!
//! 入口与上游都只有 OpenAI 一种格式（Google 与 Anthropic 的块没有生图端点），
//! 一对一的协议不需要中间标准层，直接透传——等第二家格式支持生图时再抽。
//!
//! 调度、重试与双轨日志和对话共用同一套规则。

use std::sync::Arc;
use std::time::Instant;

use serde_json::{Value, json};

use crate::AppState;
use crate::error::{Error, Result};
use crate::model::ApiKey;
use crate::proxy::adapter::{AuthStyle, UpstreamRequest};
use crate::proxy::scheme::Usage;
use crate::proxy::{Failure, MAX_ATTEMPTS, base_log, estimate, sanitize_user_message, write_log};
use crate::service::{billing, upstream};
use crate::utils::time;

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
    let target = upstream::resolve_with_header(pool, &model, header_block).await?;
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
            write_log(
                pool,
                base_log(key, ip, user_agent, started)
                    .model(model.clone())
                    .failed_with_context(&failure.error, failure.context),
            )
            .await;

            if let Err(log_error) = crate::db::usage::insert_error(
                pool,
                &key.id,
                &model,
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

/// 挑块、挑密钥、发送、计费。
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
    let mut last_failure = None;

    for _ in 0..MAX_ATTEMPTS {
        let block = match upstream::select_block(pool, &state.upstream, key, model, only).await {
            Ok(block) => block,
            Err(error) => return Err(Failure::bare(error)),
        };

        // 生图目前只有 OpenAI 格式的上游有对应端点。
        if block.api_format != crate::model::ApiFormat::Openai {
            let message = format!("上游块 {} 的格式不支持生图", block.id);
            return Err(Failure::with_context(
                Error::BadRequest(message.clone()),
                message,
                Some((block.id, String::new())),
            ));
        }

        let upstream_key = match upstream::select_key(pool, &state.upstream, &block).await {
            Ok(key) => key,
            Err(error) => {
                let context = Some((block.id.clone(), String::new()));
                let user_message = sanitize_user_message(&error);
                return Err(Failure::with_context(error, user_message, context));
            }
        };

        // 模型名替换成块里配置的转发名，其余字段原样透传。
        let mut outgoing = body.clone();
        outgoing["model"] = json!(block.target_model(model));

        let request = UpstreamRequest {
            path: IMAGES_PATH.to_string(),
            body: Some(outgoing),
        };
        let client = match state.upstream.client(&block) {
            Ok(client) => client,
            Err(error) => {
                let context = Some((block.id.clone(), upstream_key.id.clone()));
                // 代理地址之类的出口细节不进用户能查到的表。
                let user_message = sanitize_user_message(&error);
                return Err(Failure::with_context(error, user_message, context));
            }
        };

        let context = Some((block.id.clone(), upstream_key.id.clone()));

        match crate::proxy::send(
            &client,
            &block,
            &request,
            AuthStyle::Bearer,
            &upstream_key.secret,
        )
        .await
        {
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

                // 200 却没有 data，多半是网关吐了故障页，换一把再试。
                let data = match response_body.get("data").and_then(Value::as_array) {
                    Some(data) => data,
                    None => {
                        let error = Error::UpstreamUnavailable("上游响应里没有 data".to_string());
                        let user_message = sanitize_user_message(&error);
                        upstream::report_failure(pool, &upstream_key, &error.to_string()).await;
                        last_failure = Some(Failure::with_context(error, user_message, context));
                        continue;
                    }
                };

                upstream::report_success(pool, &upstream_key.id).await;

                // gpt-image-1 等新一代接口自带 usage；DALL-E 系列按张估算。
                let usage = parse_usage(&response_body).unwrap_or_else(|| {
                    Usage::estimated(
                        estimate::estimate_text(
                            body.get("prompt").and_then(Value::as_str).unwrap_or(""),
                        ),
                        IMAGE_TOKENS_PER_PICTURE * data.len().max(1) as i64,
                    )
                });

                let charge = match billing::charge(
                    pool,
                    key,
                    &block.id,
                    model,
                    usage,
                    block.multiplier_bp(model),
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
                    model = %model,
                    block = %block.id,
                    pictures = data.len(),
                    tokens = charge.usage.total_tokens,
                    estimated = charge.usage.estimated,
                    credits = %charge.credits_scaled,
                    duration_ms = started.elapsed().as_millis() as u64,
                    "生图请求完成"
                );

                return Ok(ImageOutcome {
                    response: response_body,
                    usage: charge.usage,
                    charge,
                    block_id: block.id,
                    upstream_key_id: upstream_key.id,
                });
            }
            Err(error) => {
                // reqwest 的报错可能带 URL，不进用户能查到的表。
                let user_message = sanitize_user_message(&error);
                upstream::report_failure(pool, &upstream_key, &error.to_string()).await;
                last_failure = Some(Failure::with_context(error, user_message, context));
            }
        }
    }

    Err(last_failure
        .unwrap_or_else(|| Failure::bare(Error::UpstreamUnavailable("上游不可用".to_string()))))
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
