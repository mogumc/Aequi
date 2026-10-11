//! 代理入口。
//!
//! 用户只认 OpenAI 格式；这里薄薄一层，把认证上下文和请求体递给编排函数。

use std::sync::Arc;

use axum::extract::{Extension, State};
use axum::Json;
use serde_json::Value;

use crate::api::key_auth::KeyContext;
use crate::error::Result;
use crate::proxy;
use crate::AppState;

/// `POST /v1/chat/completions`
pub async fn chat_completions(
    State(state): State<Arc<AppState>>,
    Extension(context): Extension<KeyContext>,
    Json(body): Json<Value>,
) -> Result<Json<Value>> {
    let response = proxy::chat_completion(&state, &context.key, &body).await?;
    Ok(Json(response))
}
