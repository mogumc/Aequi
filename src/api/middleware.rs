//! Bearer 令牌认证中间件。
//!
//! 校验通过后把账号与会话放进请求扩展，后续 handler 直接取用，
//! 不必重复查询数据库。

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::Response;

use crate::error::{Error, Result};
use crate::model::{Account, Session};
use crate::service::auth;
use crate::AppState;

/// 一次请求的认证上下文。
#[derive(Debug, Clone)]
pub struct AuthContext {
    pub account: Account,
    /// 当前请求所属的会话，也就是「当前设备」。
    pub session: Session,
}

/// 要求携带有效的 `Authorization: Bearer <token>`。
pub async fn require_auth(
    State(state): State<Arc<AppState>>,
    mut request: Request,
    next: Next,
) -> Result<Response> {
    let token = bearer_token(&request)
        .ok_or_else(|| Error::Unauthorized("缺少 Authorization 头".to_string()))?;

    let (account, session) = auth::authenticate(state.pool(), &token).await?;

    request
        .extensions_mut()
        .insert(AuthContext { account, session });

    Ok(next.run(request).await)
}

/// 从请求头取出 Bearer 令牌。
///
/// 管理员令牌与密钥共用这一段解析，但各自走独立的校验流程。
pub(crate) fn bearer_token(request: &Request) -> Option<String> {
    let value = request.headers().get(AUTHORIZATION)?.to_str().ok()?;

    // 前缀按 RFC 6750 大小写不敏感处理。
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }

    let token = token.trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}
