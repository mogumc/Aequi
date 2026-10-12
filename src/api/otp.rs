//! OTP 绑定与开关接口。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, State};
use serde::Deserialize;

use crate::AppState;
use crate::api::MessageResponse;
use crate::api::middleware::AuthContext;
use crate::error::Result;
use crate::service::otp::{self, OtpSetup, OtpStatus};

/// 需要验证码的请求体。
#[derive(Debug, Deserialize)]
pub struct CodePayload {
    pub code: String,
}

/// 关闭 OTP 的请求体。未启用时可以不提供验证码。
#[derive(Debug, Deserialize)]
pub struct DisablePayload {
    #[serde(default)]
    pub code: Option<String>,
}

/// `GET /api/otp`
pub async fn status(
    State(_state): State<Arc<AppState>>,
    Extension(context): Extension<AuthContext>,
) -> Json<OtpStatus> {
    Json(otp::status(&context.account))
}

/// `POST /api/otp/setup`
///
/// 取回密钥与 otpauth 地址，此时尚未启用。
pub async fn setup(
    State(state): State<Arc<AppState>>,
    Extension(context): Extension<AuthContext>,
) -> Result<Json<OtpSetup>> {
    let setup = otp::begin_setup(state.pool(), &context.account).await?;
    Ok(Json(setup))
}

/// `POST /api/otp/enable`
pub async fn enable(
    State(state): State<Arc<AppState>>,
    Extension(context): Extension<AuthContext>,
    Json(payload): Json<CodePayload>,
) -> Result<Json<MessageResponse>> {
    otp::enable(state.pool(), &context.account, &payload.code).await?;
    Ok(Json(MessageResponse::ok("OTP 已启用")))
}

/// `POST /api/otp/disable`
pub async fn disable(
    State(state): State<Arc<AppState>>,
    Extension(context): Extension<AuthContext>,
    Json(payload): Json<DisablePayload>,
) -> Result<Json<MessageResponse>> {
    otp::disable(state.pool(), &context.account, payload.code.as_deref()).await?;
    Ok(Json(MessageResponse::ok("OTP 已关闭")))
}
