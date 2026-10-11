//! 代理入口。
//!
//! 用户只认 OpenAI 格式；这里薄薄一层，把认证上下文、来源信息与请求体
//! 递给编排函数——日志需要 IP 和 UA，所以在这里取好。

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{ConnectInfo, Extension, State};
use axum::http::{header, HeaderMap};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::Value;

use crate::api::key_auth::KeyContext;
use crate::api::middleware::resolve_client_ip;
use crate::error::Result;
use crate::proxy;
use crate::service::upstream;
use crate::utils::time;
use crate::AppState;

/// `GET /v1/models`
///
/// 下游客户端（OpenAI SDK 之类）靠它列可用模型。返回的是**这个密钥实际能调的**
/// 模型：块里配置的全部模型，再按密钥白名单收敛；白名单为空即不限制。
pub async fn list_models(
    Extension(context): Extension<KeyContext>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>> {
    let models = upstream::available_models(&state.store.data).await?;
    let models = upstream::filter_by_whitelist(models, &context.key);

    // 各家客户端对 created / owned_by 的处理不一，但 OpenAI 的形状里带着，
    // 缺了会让部分 SDK 解析失败，所以补齐。
    let created = time::now_secs();
    let data: Vec<Value> = models
        .iter()
        .map(|name| {
            serde_json::json!({
                "id": name,
                "object": "model",
                "created": created,
                "owned_by": "aequi",
            })
        })
        .collect();

    Ok(Json(
        serde_json::json!({ "object": "list", "data": data }),
    ))
}

/// 请求头里指定的块（`X-Aequi-Block`）。空值当作没给。
///
/// 这是「模型名前缀」之外的另一条指定路径：不想改模型名的客户端可以用头，
/// 两者冲突时 service 层会报错。
fn header_block(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-aequi-block")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// `POST /v1/chat/completions`
///
/// `stream: true` 走 SSE（text/event-stream），其余返回普通 JSON。
pub async fn chat_completions(
    State(state): State<Arc<AppState>>,
    Extension(context): Extension<KeyContext>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response> {
    let settings = state.settings();
    let ip = resolve_client_ip(&headers, addr, settings.trust_proxy);
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);

    if body.get("stream").and_then(Value::as_bool) == Some(true) {
        let byte_stream = proxy::chat_completion_stream(
            state.clone(),
            context.key.clone(),
            body,
            ip,
            user_agent,
            header_block(&headers),
        )
        .await?;

        return Ok(Response::builder()
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .body(Body::from_stream(byte_stream))
            .expect("SSE 响应的固定头不会构建失败"));
    }

    let response = proxy::chat_completion(
        &state,
        &context.key,
        &body,
        &ip,
        user_agent.as_deref(),
        header_block(&headers).as_deref(),
    )
    .await?;
    Ok(Json(response).into_response())
}

/// `POST /v1/images/generations`
///
/// 生图协议与对话共用同一套认证、调度与双轨日志；上游侧目前只有
/// OpenAI 格式的块支持。
pub async fn images_generations(
    State(state): State<Arc<AppState>>,
    Extension(context): Extension<KeyContext>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>> {
    let settings = state.settings();
    let ip = resolve_client_ip(&headers, addr, settings.trust_proxy);
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);

    let response = proxy::image::images_generations(
        &state,
        &context.key,
        &body,
        &ip,
        user_agent.as_deref(),
        header_block(&headers).as_deref(),
    )
    .await?;

    Ok(Json(response))
}
