//! 路由装配。
//!
//! # 挂载点
//!
//! | 前缀 | 归属 | 认证 |
//! | --- | --- | --- |
//! | `/` | 跳转到面板 | 公开 |
//! | `/health` | 健康检查 | 公开 |
//! | `/api/auth/login`、`/api/auth/providers` | 登录相关 | 公开 |
//! | `/api/*`（管理员） | 账号、设备、OTP、设置、密钥管理 | 管理员令牌 |
//! | `/api/key/*` | 密钥自助（只读） | 访问密钥 |
//! | `/web/*` | 管理面板静态资源 | 公开（页面内自行守卫） |
//!
//! 两套认证各挂各的中间件，**互不越权**：管理员令牌进不了 `/api/key/*`，
//! 访问密钥也进不了管理员接口。它们的数据更是分在两个数据库文件里。
//!
//! 新增模块时在对应分组里追加路由即可；每个模块提供自己的 `router()` 片段，
//! 这里只负责拼装。

use std::sync::Arc;

use axum::extract::State;
use axum::http::HeaderValue;
use axum::response::Redirect;
use axum::routing::{delete, get, post};
use axum::{Json, Router, middleware};
use serde::Serialize;
use tower_http::cors::{Any, CorsLayer};

use crate::AppState;
use crate::api::{
    auth, blocks, device, key_auth, key_portal, keys, logs, middleware as auth_middleware, otp,
    proxy_api, settings, stats,
};
use crate::db;
use crate::error::Result;
use crate::service::provider::ProviderInfo;
use crate::web;

/// 构建完整路由。
pub fn router(state: Arc<AppState>) -> Router {
    // CORS 在启动时依据数据库里的设置固化；改动需重启。
    let cors = cors_layer(&state.settings().cors_origins);

    // 管理员接口：一律要求管理员令牌。
    let admin = Router::new()
        .route("/auth/me", get(auth::me))
        .route("/auth/logout", post(auth::logout))
        .route("/auth/password", post(auth::change_password))
        .route("/auth/username", post(auth::change_username))
        .route("/devices", get(device::list).delete(device::revoke_others))
        .route("/devices/{id}", delete(device::revoke))
        .route("/otp", get(otp::status))
        .route("/otp/setup", post(otp::setup))
        .route("/otp/enable", post(otp::enable))
        .route("/otp/disable", post(otp::disable))
        .route("/settings", get(settings::get).patch(settings::update))
        .route("/keys", get(keys::list).post(keys::create))
        .route(
            "/keys/{id}",
            get(keys::detail).patch(keys::update).delete(keys::delete),
        )
        .route("/keys/{id}/credits", post(keys::set_credits))
        .route("/keys/{id}/usage", get(keys::usage))
        .route("/blocks", get(blocks::list).post(blocks::create))
        .route(
            "/blocks/{id}",
            get(blocks::detail)
                .patch(blocks::update)
                .delete(blocks::delete),
        )
        .route(
            "/blocks/{id}/keys",
            get(blocks::list_keys).post(blocks::add_keys),
        )
        .route("/blocks/{id}/keys/remove", post(blocks::remove_keys))
        .route("/blocks/{id}/keys/pause", post(blocks::pause_keys))
        .route("/blocks/{id}/keys/resume", post(blocks::resume_keys))
        .route(
            "/blocks/{id}/keys/never-disable",
            post(blocks::never_disable_keys),
        )
        .route("/blocks/{id}/keys/weight", post(blocks::set_keys_weight))
        .route("/blocks/{id}/keys/{key_id}/test", post(blocks::test_key))
        .route("/blocks/{id}/test", post(blocks::test_block))
        .route("/blocks/{id}/models/fetch", post(blocks::fetch_models))
        .route("/logs", get(logs::list))
        .route("/stats", get(stats::overview))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware::require_auth,
        ));

    // 密钥自助：另一套认证，只读，且只作用在自己身上。
    let portal = Router::new()
        .route("/key/me", get(key_portal::me))
        .route("/key/usage", get(key_portal::usage))
        .route("/key/models", get(key_portal::models))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            key_auth::require_key,
        ));

    // 代理入口：对外协议只有 OpenAI 的一种，同样用访问密钥认证。
    let gateway = Router::new()
        .route("/models", get(proxy_api::list_models))
        .route("/chat/completions", post(proxy_api::chat_completions))
        .route("/images/generations", post(proxy_api::images_generations))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            key_auth::require_key,
        ));

    Router::new()
        .route("/", get(panel_redirect))
        .route("/health", get(health))
        .route("/api/auth/login", post(auth::login))
        .route("/api/auth/providers", get(auth::providers))
        .nest("/api", admin.merge(portal))
        .nest("/v1", gateway)
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
        version: crate::version(),
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
