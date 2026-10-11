//! 代理入口。
//!
//! 用户只认 OpenAI 格式；这里薄薄一层，把认证上下文、来源信息与请求体
//! 递给编排函数——日志需要 IP 和 UA，所以在这里取好。

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Extension, State};
use axum::http::{header, HeaderMap};
use axum::Json;
use serde_json::Value;

use crate::api::key_auth::KeyContext;
use crate::api::middleware::resolve_client_ip;
use crate::error::Result;
use crate::proxy;
use crate::AppState;

/// `POST /v1/chat/completions`
pub async fn chat_completions(
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

    let response = proxy::chat_completion(
        &state,
        &context.key,
        &body,
        &ip,
        user_agent.as_deref(),
    )
    .await?;

    Ok(Json(response))
}
