//! 统一错误类型。
//!
//! 对外统一为 `{"error": {"type": "...", "message": "..."}}`，
//! 前端只需处理一种错误结构。

use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

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

    /// 上游返回了无法使用的结果（结构不对、5xx 等）。
    ///
    /// 502 比内部错误更贴切：错不在本服务，是上游的问题。
    #[error("上游响应异常: {0}")]
    UpstreamUnavailable(String),

    /// 上游限流：本服务或上游短时间内不受理。
    ///
    /// 对下游就是 **429**，并把等待秒数写进 `Retry-After`——上游怎么限的
    /// 就照实往下传，别翻成「无可用密钥」这类不相干的说法：下游要据此
    /// 退避，而不是以为自己没权限。
    #[error("{message}")]
    RateLimited {
        message: String,
        /// 建议下游等多少秒再来；拿不到（上游没给提示）时为 `None`，
        /// 此时不发 `Retry-After` 头。
        retry_after: Option<i64>,
    },

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
            // 验证码不对是「输入错了」，不是「身份失效」——归 400。
            // 之前归 401，前端会把验证码输错当成会话过期直接踢回登录页。
            Error::InvalidTotp => StatusCode::BAD_REQUEST,
            Error::Unauthorized(_)
            | Error::InvalidCredentials
            | Error::TotpRequired => StatusCode::UNAUTHORIZED,
            Error::Forbidden(_) => StatusCode::FORBIDDEN,
            // 402 比 403 更贴切：不是没权限，是余额不够。
            Error::InsufficientCredits => StatusCode::PAYMENT_REQUIRED,
            // 与上游同一口径：被限流就是 429，配 `Retry-After`。
            Error::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
            // 上游的问题用 502 表达，别让用户以为是我们坏了。
            Error::UpstreamUnavailable(_) => StatusCode::BAD_GATEWAY,
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
            Error::RateLimited { .. } => "rate_limited",
            Error::UpstreamUnavailable(_) => "upstream_unavailable",
            Error::NotFound(_) => "not_found",
            Error::Database(_) => "database_error",
            Error::Internal(_) => "internal_error",
        }
    }

    /// 建议下游等多少秒再来（目前只有上游限流带这个信息）。
    pub fn retry_hint(&self) -> Option<i64> {
        match self {
            Error::RateLimited { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    /// 补上等待提示；已有提示或不是限流类错误时原样返回。
    ///
    /// 用在「块内密钥全在退避」的时刻：上一轮的限流错误本来没带秒数，
    /// 这里能拿到块内最早的恢复时间，就把它补成对下游有用的 `Retry-After`。
    pub fn with_retry_hint(self, hint: Option<i64>) -> Self {
        match (self, hint) {
            (
                Error::RateLimited {
                    message,
                    retry_after: None,
                },
                Some(secs),
            ) => Error::RateLimited {
                message,
                retry_after: Some(secs),
            },
            (error, _) => error,
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

        // 上游限流要把「多久后再来」一并告诉下游——客户端据此退避，
        // 比让它盲猜重试节奏靠谱得多。
        let retry_after = self.retry_hint();

        let body = Json(serde_json::json!({
            "error": {
                "type": self.kind(),
                "message": message,
            }
        }));

        let mut response = (status, body).into_response();
        if let Some(secs) = retry_after {
            if let Ok(value) = HeaderValue::from_str(&secs.to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
        }
        response
    }
}

/// 业务代码统一使用的 `Result`。
pub type Result<T, E = Error> = std::result::Result<T, E>;
