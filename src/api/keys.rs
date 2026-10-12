//! 密钥管理接口（管理员专用）。
//!
//! 这里的每一次操作都要求管理员令牌，走的 [`crate::api::middleware::require_auth`]；
//! 密钥体系自己那套只读接口在 [`crate::api::key_portal`]。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::api::{MessageResponse, UsageListResponse, UsageQuery};
use crate::error::Result;
use crate::model::{KeyCreate, KeyCreditsUpdate, KeyUpdate, KeyView};
use crate::service::keys;

/// 密钥列表响应。
#[derive(Debug, Serialize)]
pub struct KeyListResponse {
    pub keys: Vec<KeyView>,
    /// 与筛选条件匹配的总数（不受分页影响）。
    pub total: i64,
    pub page: i64,
    pub page_size: i64,
}

/// 列表查询参数。
#[derive(Debug, Default, Deserialize)]
pub struct KeyListQuery {
    /// 页码，从 1 开始。
    pub page: Option<i64>,
    /// 每页条数，收敛到 1-100。
    pub page_size: Option<i64>,
    /// 按名称或密钥前缀搜索。
    pub search: Option<String>,
}

/// `GET /api/keys`
pub async fn list(
    State(state): State<Arc<AppState>>,
    Query(query): Query<KeyListQuery>,
) -> Result<Json<KeyListResponse>> {
    let search = query
        .search
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let page = keys::list_page(&state.store.data, search, query.page, query.page_size).await?;

    Ok(Json(KeyListResponse {
        keys: page.keys.iter().map(Into::into).collect(),
        total: page.total,
        page: page.page,
        page_size: page.page_size,
    }))
}

/// `POST /api/keys`
///
/// 返回值里就带着完整密钥。后端不再有「只显示一次」的限制——
/// 后续所有请求数据都绑定在这串密钥上，忘了就找不回来，所以允许随时查看。
pub async fn create(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<KeyCreate>,
) -> Result<Json<KeyView>> {
    let key = keys::create(&state.store.data, payload).await?;

    tracing::info!(key_prefix = %key.key_prefix, name = %key.name, "已创建访问密钥");

    Ok(Json(KeyView::from(&key)))
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
    let key = keys::set_credits(&state.store.data, &id, payload).await?;

    tracing::info!(
        key_prefix = %key.key_prefix,
        credits = %key.credits_scaled,
        unlimited = key.unlimited,
        "已调整密钥额度"
    );

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
    let (records, has_more) =
        keys::usage(&state.store.data, &id, query.limit(), query.before_id()).await?;

    Ok(Json(UsageListResponse::new(
        summary,
        records.iter().map(Into::into).collect(),
        has_more,
        state.settings().log_retention_days,
    )))
}
