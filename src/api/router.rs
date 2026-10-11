//! 路由装配。
//!
//! # 挂载点
//!
//! | 前缀 | 归属 | 认证 |
//! | --- | --- | --- |
//! | `/` | 跳转到面板 | 公开 |
//! | `/health` | 健康检查 | 公开 |
//! | `/api/auth/login`、`/api/auth/providers` | 登录相关 | 公开 |
//! | `/api/*`（其余） | 业务接口 | 需登录 |
//! | `/web/*` | 管理面板静态资源 | 公开（页面内自行守卫） |
//!
//! 新增模块时，在对应的分组里追加自己的路由即可：
//! 需要认证的挂进 `protected`，公开的合并到最外层。
//! 每个模块自己提供一个 `router()` 返回路由片段，这里只负责拼装。

use std::sync::Arc;

use axum::extract::State;
use axum::http::HeaderValue;
use axum::response::Redirect;
use axum::routing::{delete, get, post};
use axum::{middleware, Json, Router};
use serde::Serialize;
use tower_http::cors::{Any, CorsLayer};

use crate::api::{auth, device, middleware as auth_middleware, otp, settings};
use crate::db;
use crate::error::Result;
use crate::service::provider::ProviderInfo;
use crate::web;
use crate::AppState;

/// 构建完整路由。
pub fn router(state: Arc<AppState>) -> Router {
    // CORS 在启动时依据数据库里的设置固化；改动需重启。
    let cors = cors_layer(&state.settings().cors_origins);

    // 需要登录才能访问的接口。
    let protected = Router::new()
        .route("/auth/me", get(auth::me))
        .route("/auth/logout", post(auth::logout))
        .route("/devices", get(device::list).delete(device::revoke_others))
        .route("/devices/{id}", delete(device::revoke))
        .route("/otp", get(otp::status))
        .route("/otp/setup", post(otp::setup))
        .route("/otp/enable", post(otp::enable))
        .route("/otp/disable", post(otp::disable))
        .route("/settings", get(settings::get).patch(settings::update))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware::require_auth,
        ));

    Router::new()
        .route("/", get(panel_redirect))
        .route("/health", get(health))
        .route("/api/auth/login", post(auth::login))
        .route("/api/auth/providers", get(auth::providers))
        .nest("/api", protected)
        .merge(web::router())
        .layer(cors)
        .with_state(state)
}

/// `GET /`：直接访问根域时把人送到面板。
async fn panel_redirect() -> Redirect {
    Redirect::temporary(web::PANEL_PREFIX)
}

/// 健康检查响应。
#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
    version: &'static str,
    uptime_seconds: u64,
    /// 是否已初始化账号。false 说明数据库是新建的。
    account_configured: bool,
    /// 可用的第三方登录方式。
    providers: Vec<ProviderInfo>,
}

/// `GET /health`
async fn health(State(state): State<Arc<AppState>>) -> Result<Json<HealthResponse>> {
    let account_configured = db::account::count(state.pool()).await? > 0;

    Ok(Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        uptime_seconds: state.uptime().as_secs(),
        account_configured,
        providers: state.providers.list(),
    }))
}

/// 依据设置构造 CORS 层。
///
/// 出现 `*` 时放开全部来源；否则只允许列出的来源。
fn cors_layer(origins: &[String]) -> CorsLayer {
    let layer = CorsLayer::new().allow_methods(Any).allow_headers(Any);

    if origins.iter().any(|origin| origin == "*") {
        return layer.allow_origin(Any);
    }

    let allowed: Vec<HeaderValue> = origins
        .iter()
        .filter_map(|origin| origin.parse::<HeaderValue>().ok())
        .collect();

    layer.allow_origin(allowed)
}
