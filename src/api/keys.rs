//! 密钥管理接口（管理员专用）。
//!
//! 这里的每一次操作都要求管理员令牌，走的 [`crate::api::middleware::require_auth`]；
//! 密钥体系自己那套只读接口在 [`crate::api::key_portal`]。

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Serialize;

use crate::api::{MessageResponse, UsageListResponse, UsageQuery};
use crate::error::Result;
use crate::model::{KeyCreate, KeyCreditsUpdate, KeyUpdate, KeyView};
use crate::service::keys;
use crate::AppState;

/// 创建密钥的响应。
///
/// `secret` 是完整密钥，**只会在这里出现这一次**，之后库里只有散列。
#[derive(Debug, Serialize)]
pub struct KeyCreatedResponse {
    pub key: KeyView,
    pub secret: String,
}

/// 密钥列表响应。
#[derive(Debug, Serialize)]
pub struct KeyListResponse {
    pub keys: Vec<KeyView>,
}

/// `GET /api/keys`
pub async fn list(State(state): State<Arc<AppState>>) -> Result<Json<KeyListResponse>> {
    let keys = keys::list(&state.store.data).await?;

    Ok(Json(KeyListResponse {
        keys: keys.iter().map(Into::into).collect(),
    }))
}

/// `POST /api/keys`
pub async fn create(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<KeyCreate>,
) -> Result<Json<KeyCreatedResponse>> {
    let (key, secret) = keys::create(&state.store.data, payload).await?;

    tracing::info!(key_prefix = %key.key_prefix, name = %key.name, "已创建访问密钥");

    Ok(Json(KeyCreatedResponse {
        key: KeyView::from(&key),
        secret,
    }))
}

/// `GET /api/keys/{id}`
pub async fn detail(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<KeyView>> {
    let key = keys::get(&state.store.data, &id).await?;
    Ok(Json(KeyView::from(&key)))
}

/// `PATCH /api/keys/{id}`
///
/// 改备注名、启用状态与可用模型；积分走单独接口。
pub async fn update(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(payload): Json<KeyUpdate>,
) -> Result<Json<KeyView>> {
    let key = keys::update(&state.store.data, &id, payload).await?;
    Ok(Json(KeyView::from(&key)))
}

/// `POST /api/keys/{id}/credits`
///
/// 直接设定余额，用于充值或人工校正。
pub async fn set_credits(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(payload): Json<KeyCreditsUpdate>,
) -> Result<Json<KeyView>> {
    let key = keys::set_credits(&state.store.data, &id, &payload.credits).await?;

    tracing::info!(key_prefix = %key.key_prefix, credits = %key.credits_scaled, "已调整密钥积分");

    Ok(Json(KeyView::from(&key)))
}

/// `DELETE /api/keys/{id}`
pub async fn delete(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<MessageResponse>> {
    keys::delete(&state.store.data, &id).await?;
    Ok(Json(MessageResponse::ok("密钥已删除")))
}

/// `GET /api/keys/{id}/usage`
pub async fn usage(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<UsageQuery>,
) -> Result<Json<UsageListResponse>> {
    let summary = keys::usage_summary(&state.store.data, &id).await?;
    let records = keys::usage(&state.store.data, &id, query.limit()).await?;

    Ok(Json(UsageListResponse::new(
        summary,
        records.iter().map(Into::into).collect(),
    )))
}
