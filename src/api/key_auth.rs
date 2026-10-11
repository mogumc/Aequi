//! 密钥认证中间件。
//!
//! 与管理员认证中间件**完全独立**：它只查数据面库，拿不到任何管理员数据；
//! 注入的上下文类型也不同（[`KeyContext`] vs [`crate::api::middleware::AuthContext`]），
//! 两套体系不会互相串门。

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;

use crate::AppState;
use crate::api::middleware::bearer_token;
use crate::error::{Error, Result};
use crate::model::ApiKey;
use crate::service::keys;

/// 一次密钥请求的上下文。
#[derive(Debug, Clone)]
pub struct KeyContext {
    pub key: ApiKey,
}

/// 要求携带有效的 `Authorization: Bearer <key>`。
pub async fn require_key(
    State(state): State<Arc<AppState>>,
    mut request: Request,
    next: Next,
) -> Result<Response> {
    let secret = bearer_token(&request)
        .ok_or_else(|| Error::Unauthorized("缺少 Authorization 头".to_string()))?;

    // 只查数据面库——密钥体系在数据层面就碰不到管理员数据。
    let key = keys::authenticate(&state.store.data, &secret).await?;

    request.extensions_mut().insert(KeyContext { key });
    Ok(next.run(request).await)
}
