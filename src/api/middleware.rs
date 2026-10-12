//! Bearer 令牌认证中间件。
//!
//! 校验通过后把账号与会话放进请求扩展，后续 handler 直接取用，
//! 不必重复查询数据库。

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, header::AUTHORIZATION};
use axum::middleware::Next;
use axum::response::Response;

use crate::AppState;
use crate::error::{Error, Result};
use crate::model::{Account, Session};
use crate::service::auth;

/// 解析客户端 IP。
///
/// 只有开启了 `server.trust_proxy` 才采信转发头。直连场景下 `X-Forwarded-For`
/// 完全由客户端掌控，无条件采信等于让日志里的 IP 可以随便伪造。
pub(crate) fn resolve_client_ip(
    headers: &HeaderMap,
    addr: SocketAddr,
    trust_proxy: bool,
) -> String {
    if trust_proxy {
        if let Some(forwarded) = forwarded_ip(headers) {
            return forwarded;
        }
    }

    addr.ip().to_string()
}

fn forwarded_ip(headers: &HeaderMap) -> Option<String> {
    let value = headers
        .get("x-forwarded-for")
        .or_else(|| headers.get("x-real-ip"))?
        .to_str()
        .ok()?;

    // 形如「客户端, 代理1, 代理2」，取第一段。
    let first = value.split(',').next()?.trim();
    (!first.is_empty()).then(|| first.to_string())
}

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
