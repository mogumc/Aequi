//! 认证相关接口。

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Extension, State};
use axum::http::{header, HeaderMap};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::api::middleware::AuthContext;
use crate::api::MessageResponse;
use crate::error::{Error, Result};
use crate::model::AccountView;
use crate::service::auth::{self, LoginRequest};
use crate::service::provider::ProviderInfo;
use crate::AppState;

/// 登录请求体。
#[derive(Debug, Deserialize)]
pub struct LoginPayload {
    pub username: String,
    pub password: String,
    /// 账号启用 OTP 后必须提供。
    #[serde(default)]
    pub otp: Option<String>,
}

/// 登录响应体。
#[derive(Debug, Serialize)]
pub struct LoginResponse {
    /// 明文令牌，仅在本次响应中出现。
    pub token: String,
    pub expires_at: i64,
    pub account: AccountView,
}

/// `POST /api/auth/login`
pub async fn login(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<LoginPayload>,
) -> Result<Json<LoginResponse>> {
    let settings = state.settings();
    let ip = resolve_client_ip(&headers, addr, settings.trust_proxy);
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_string());

    let outcome = auth::login(
        state.pool(),
        &settings,
        LoginRequest {
            username: &payload.username,
            password: &payload.password,
            totp_code: payload.otp.as_deref(),
            ip: Some(&ip),
            user_agent: user_agent.as_deref(),
        },
    )
    .await?;

    tracing::info!(account = %outcome.account.username, ip = %ip, "登录成功");

    Ok(Json(LoginResponse {
        token: outcome.token,
        expires_at: outcome.session.expires_at,
        account: AccountView::from(&outcome.account),
    }))
}

/// `POST /api/auth/logout`
pub async fn logout(
    State(state): State<Arc<AppState>>,
    Extension(context): Extension<AuthContext>,
) -> Result<Json<MessageResponse>> {
    auth::logout(state.pool(), &context.session.id, context.account.id).await?;
    Ok(Json(MessageResponse::ok("已退出当前设备")))
}

/// `GET /api/auth/me`
pub async fn me(Extension(context): Extension<AuthContext>) -> Json<AccountView> {
    Json(AccountView::from(&context.account))
}

/// `GET /api/auth/providers`
///
/// 目前恒为空数组，前端可据此判断是否展示第三方登录入口。
pub async fn providers(State(state): State<Arc<AppState>>) -> Json<Vec<ProviderInfo>> {
    Json(state.providers.list())
}

/// 修改密码的请求体。
#[derive(Debug, Deserialize)]
pub struct PasswordPayload {
    pub current_password: String,
    pub new_password: String,
    /// 新密码的第二次输入。
    pub confirm_password: String,
}

/// 修改密码的响应。
#[derive(Debug, Serialize)]
pub struct PasswordChangedResponse {
    /// 被一并撤销的其它设备数量。
    pub revoked_devices: u64,
}

/// `POST /api/auth/password`
///
/// 需要提供当前密码；改完后其它设备会被要求重新登录，本机保持登录。
pub async fn change_password(
    State(state): State<Arc<AppState>>,
    Extension(context): Extension<AuthContext>,
    Json(payload): Json<PasswordPayload>,
) -> Result<Json<PasswordChangedResponse>> {
    // 前端已经比对过一次，这里再比一次——正确性不能只押在客户端上。
    if payload.new_password != payload.confirm_password {
        return Err(Error::BadRequest("两次输入的新密码不一致".to_string()));
    }

    let revoked_devices = auth::change_password(
        state.pool(),
        &context.account,
        &payload.current_password,
        &payload.new_password,
        &context.session.id,
    )
    .await?;

    Ok(Json(PasswordChangedResponse { revoked_devices }))
}

/// 修改用户名的请求体。
#[derive(Debug, Deserialize)]
pub struct UsernamePayload {
    pub current_password: String,
    pub username: String,
}

/// `POST /api/auth/username`
///
/// 需要提供当前密码。返回更新后的账号视图，前端直接替换，省一次往返。
pub async fn change_username(
    State(state): State<Arc<AppState>>,
    Extension(context): Extension<AuthContext>,
    Json(payload): Json<UsernamePayload>,
) -> Result<Json<AccountView>> {
    let account = auth::change_username(
        state.pool(),
        &context.account,
        &payload.current_password,
        &payload.username,
    )
    .await?;

    Ok(Json(AccountView::from(&account)))
}

/// 解析客户端地址。
///
/// 只有开启了 `server.trust_proxy` 才采信转发头。直连场景下 `X-Forwarded-For`
/// 完全由客户端掌控，无条件采信等于让登录记录里的 IP 可以随便伪造。
fn resolve_client_ip(headers: &HeaderMap, addr: SocketAddr, trust_proxy: bool) -> String {
    if trust_proxy {
        if let Some(forwarded) = forwarded_ip(headers) {
            return forwarded;
        }
    }

    addr.ip().to_string()
}

/// 从转发头里取最左边那一跳（即原始客户端）。
fn forwarded_ip(headers: &HeaderMap) -> Option<String> {
    let value = headers
        .get("x-forwarded-for")
        .or_else(|| headers.get("x-real-ip"))?
        .to_str()
        .ok()?;

    // 形如「客户端, 代理1, 代理2」，取第一段。
    let first = value.split(',').next()?.trim();

    if first.is_empty() {
        None
    } else {
        Some(first.to_string())
    }
}
