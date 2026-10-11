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
//! 之所以在中间放一层标准模型，是因为「三家格式两两互转」是六条路径，
//! 而「每家 ↔ 标准」只有三条，再加一家也不用动已有的代码。

use std::sync::Arc;

use serde_json::Value;

use crate::error::{Error, Result};
use crate::model::{ApiKey, UpstreamBlock};
use crate::proxy::adapter::{adapter_for, AuthStyle, UpstreamRequest};
use crate::service::{billing, upstream};
use crate::utils::{random, time};
use crate::AppState;

pub mod adapter;
pub mod openai_in;
pub mod scheme;

/// 一次请求最多换几把密钥重试。
///
/// 只为可恢复的错误重试（限流、上游 5xx、网络故障）；请求本身有问题时换密钥没有意义。
const MAX_ATTEMPTS: usize = 3;

/// 处理一次对话请求，返回要交给用户的响应体。
pub async fn chat_completion(state: &Arc<AppState>, key: &ApiKey, body: &Value) -> Result<Value> {
    let request = openai_in::parse_request(body)?;

    // 余额为零时直接拒绝，免得替用户白白消耗上游。
    // 真实扣减由 charge 的原子 UPDATE 兜底，这里只是省一次无谓的上游调用。
    if key.credits_scaled <= 0 {
        return Err(Error::InsufficientCredits);
    }

    let pool = &state.store.data;
    let mut last_error = None;

    for _ in 0..MAX_ATTEMPTS {
        // 每一轮都重新挑：上一把密钥进入退避后，这里会自然落到下一把。
        let block = upstream::select_block(pool, &state.upstream, &request.model).await?;
        let upstream_key = upstream::select_key(pool, &state.upstream, &block).await?;

        let adapter = adapter_for(block.api_format);
        let outgoing = adapter.build_chat(&request, &block.target_model(&request.model));
        let client = state.upstream.client(&block)?;

        let sent =
            send(&client, &block, &outgoing, adapter.auth_style(), &upstream_key.secret).await;

        match sent {
            Ok((status, response_body)) => {
                if status == 429 {
                    upstream::report_rate_limited(pool, &upstream_key).await;
                    last_error = Some(Error::UpstreamUnavailable("上游限流".to_string()));
                    continue;
                }

                // 密钥被拒或上游故障：记一次失败，换一把再试。
                if status == 401 || status == 403 || status >= 500 {
                    upstream::report_failure(pool, &upstream_key, &format!("上游返回 {status}"))
                        .await;
                    last_error = Some(Error::UpstreamUnavailable(format!("上游返回 {status}")));
                    continue;
                }

                if !(200..300).contains(&status) {
                    // 请求本身的问题（参数、模型名等），换密钥也不会好。
                    let detail = response_body
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    return Err(Error::UpstreamUnavailable(format!(
                        "上游返回 {status} {detail}"
                    )));
                }

                let response = match adapter.parse_chat(&response_body) {
                    Ok(response) => response,
                    Err(error) => {
                        // 200 却解析不出内容，多半是上游网关吐了故障页——
                        // 这属于上游的问题，记一次失败并换一把再试。
                        upstream::report_failure(pool, &upstream_key, &error.to_string()).await;
                        last_error = Some(error);
                        continue;
                    }
                };
                upstream::report_success(pool, &upstream_key.id).await;

                let charge =
                    billing::charge(pool, key, &request.model, response.usage.total_tokens).await?;

                tracing::info!(
                    model = %request.model,
                    block = %block.id,
                    tokens = response.usage.total_tokens,
                    credits = %charge.credits_scaled,
                    "对话请求完成"
                );

                let id = format!("chatcmpl-{}", random::string(24));
                let created = time::now_secs();

                return Ok(openai_in::render_response(&response, &id, created));
            }
            Err(error) => {
                // 网络层面的失败同样换一把再试。
                upstream::report_failure(pool, &upstream_key, &error.to_string()).await;
                last_error = Some(error);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| Error::UpstreamUnavailable("上游不可用".to_string())))
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
    let url = format!("{}{}", block.base_url.trim_end_matches('/'), request.path);

    let mut builder = match &request.body {
        Some(body) => client.post(&url).json(body),
        None => client.get(&url),
    };

    builder = match style {
        AuthStyle::Bearer => builder.bearer_auth(key),
        AuthStyle::XApiKey => builder.header("x-api-key", key),
        AuthStyle::GoogleApiKey => builder.header("x-goog-api-key", key),
    };

    for (name, value) in adapter_for(block.api_format).extra_headers() {
        builder = builder.header(name, value);
    }

    let response = builder
        .send()
        .await
        .map_err(|error| Error::UpstreamUnavailable(format!("请求上游失败: {error}")))?;

    let status = response.status().as_u16();
    // 上游在出错时可能返回空体或 HTML，解析不出就按空对象处理。
    let body = response.json::<Value>().await.unwrap_or(Value::Null);

    Ok((status, body))
}
