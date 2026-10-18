//! 密钥自助接口。
//!
//! 能看自己的信息和用量、拉一份脱敏的模型路由，以及**重置自己的密钥**。
//! 能做的仅限这些——碰不到管理员体系的任何数据，也改不了自己的额度与白名单。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, Query, State};
use serde::Serialize;

use crate::AppState;
use crate::api::key_auth::KeyContext;
use crate::api::{UsageListResponse, UsageQuery};
use crate::error::Result;
use crate::model::KeySelfView;
use crate::service::{keys, upstream};

/// `GET /api/key/me`
pub async fn me(Extension(context): Extension<KeyContext>) -> Json<KeySelfView> {
    Json(KeySelfView::from(&context.key))
}

/// 自助重置的响应。
///
/// 明文只在这个响应里出现一次：常规的自助视图刻意不带明文，
/// 但重置后旧密钥立刻失效，不给出新的那串就等于把人锁在门外。
#[derive(Debug, Serialize)]
pub struct KeyResetResponse {
    pub secret: String,
    /// 重置后的自身信息（`key_prefix` 已同步为新值）。
    pub key: KeySelfView,
}

/// `POST /api/key/me/reset`
///
/// 用当前密钥认证，换一串新的、`id` 不变，额度与白名单照旧。
/// 这是一次不可逆操作：响应之后旧密钥就认证不过了。
pub async fn reset(
    State(state): State<Arc<AppState>>,
    Extension(context): Extension<KeyContext>,
) -> Result<Json<KeyResetResponse>> {
    let key = keys::reset(&state.store.data, &context.key.id).await?;

    tracing::info!(
        key_id = %key.id,
        key_prefix = %key.key_prefix,
        "访问密钥持有者自助重置了密钥"
    );

    Ok(Json(KeyResetResponse {
        secret: key.secret.clone(),
        key: KeySelfView::from(&key),
    }))
}

/// `GET /api/key/usage`
///
/// 只能查到自己的记录：key_id 取自认证上下文，不接受外部传入。
pub async fn usage(
    State(state): State<Arc<AppState>>,
    Extension(context): Extension<KeyContext>,
    Query(query): Query<UsageQuery>,
) -> Result<Json<UsageListResponse>> {
    let summary = keys::usage_summary(&state.store.data, &context.key.id).await?;
    let (records, has_more) =
        keys::usage(&state.store.data, &context.key.id, query.limit(), query.before_id()).await?;

    Ok(Json(UsageListResponse::new(
        summary,
        records.iter().map(Into::into).collect(),
        has_more,
        state.settings().log_retention_days,
    )))
}

/// 可用模型列表的响应。
#[derive(Debug, serde::Serialize)]
pub struct KeyModelsResponse {
    pub models: Vec<upstream::KeyModelRoute>,
}

/// `GET /api/key/models`
///
/// 这把密钥实际能调用的模型路由（调用写法与计费倍率），脱敏视图：
/// 只有块 id、模型名与倍率，转发名与上游出口不出这个接口。
pub async fn models(
    State(state): State<Arc<AppState>>,
    Extension(context): Extension<KeyContext>,
) -> Result<Json<KeyModelsResponse>> {
    let models = upstream::key_model_routes(&state.store.data, &context.key).await?;
    Ok(Json(KeyModelsResponse { models }))
}
