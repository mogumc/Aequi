//! 统一错误类型。
//!
//! 对外统一为 `{"error": {"type": "...", "message": "..."}}`，
//! 前端只需处理一种错误结构。

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

/// 业务错误。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    BadRequest(String),

    #[error("{0}")]
    Unauthorized(String),

    /// 用户名或密码错误。不区分二者，避免账号枚举。
    #[error("用户名或密码错误")]
    InvalidCredentials,

    /// 账号已启用 OTP，但请求未携带验证码。
    #[error("该账号需要动态验证码")]
    TotpRequired,

    #[error("动态验证码不正确")]
    InvalidTotp,

    /// 密钥余额不足以支付本次调用。
    #[error("积分不足")]
    InsufficientCredits,

    #[error("{0}")]
    NotFound(String),

    #[error("{0}")]
    Forbidden(String),

    #[error(transparent)]
    Database(#[from] sqlx::Error),

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl Error {
    /// 对应的 HTTP 状态码。
    pub fn status(&self) -> StatusCode {
        match self {
            Error::BadRequest(_) => StatusCode::BAD_REQUEST,
            Error::Unauthorized(_)
            | Error::InvalidCredentials
            | Error::TotpRequired
            | Error::InvalidTotp => StatusCode::UNAUTHORIZED,
            Error::Forbidden(_) => StatusCode::FORBIDDEN,
            // 402 比 403 更贴切：不是没权限，是余额不够。
            Error::InsufficientCredits => StatusCode::PAYMENT_REQUIRED,
            Error::NotFound(_) => StatusCode::NOT_FOUND,
            Error::Database(_) | Error::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// 机器可读的错误类型，前端据此分支处理。
    pub fn kind(&self) -> &'static str {
        match self {
            Error::BadRequest(_) => "bad_request",
            Error::Unauthorized(_) => "unauthorized",
            Error::InvalidCredentials => "invalid_credentials",
            Error::TotpRequired => "totp_required",
            Error::InvalidTotp => "invalid_totp",
            Error::Forbidden(_) => "forbidden",
            Error::InsufficientCredits => "insufficient_credits",
            Error::NotFound(_) => "not_found",
            Error::Database(_) => "database_error",
            Error::Internal(_) => "internal_error",
        }
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = self.status();

        if status.is_server_error() {
            tracing::error!(error = %self, "请求处理失败");
        } else {
            tracing::debug!(error = %self, "请求被拒绝");
        }

        // 内部错误不把细节透出去，只留日志。
        let message = if status.is_server_error() {
            "服务内部错误".to_string()
        } else {
            self.to_string()
        };

        let body = Json(serde_json::json!({
            "error": {
                "type": self.kind(),
                "message": message,
            }
        }));

        (status, body).into_response()
    }
}

/// 业务代码统一使用的 `Result`。
pub type Result<T, E = Error> = std::result::Result<T, E>;
