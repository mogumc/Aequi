//! 模型计费配置接口（管理员专用）。

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::Json;
use serde::Serialize;

use crate::api::MessageResponse;
use crate::error::Result;
use crate::model::{ModelCreate, ModelUpdate, ModelView};
use crate::service::models;
use crate::AppState;

/// 模型列表响应。
#[derive(Debug, Serialize)]
pub struct ModelListResponse {
    pub models: Vec<ModelView>,
}

/// `GET /api/models`
pub async fn list(State(state): State<Arc<AppState>>) -> Result<Json<ModelListResponse>> {
    let models = models::list(&state.store.data).await?;

    Ok(Json(ModelListResponse {
        models: models.iter().map(Into::into).collect(),
    }))
}

/// `POST /api/models`
pub async fn create(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<ModelCreate>,
) -> Result<Json<ModelView>> {
    let config = models::create(&state.store.data, payload).await?;

    tracing::info!(model = %config.name, multiplier_bp = config.multiplier_bp, "已创建模型配置");

    Ok(Json(ModelView::from(&config)))
}

/// `PATCH /api/models/{name}`
///
/// 模型名走路径参数，含 `/` 的名称需要调用方先做 URL 编码。
pub async fn update(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(payload): Json<ModelUpdate>,
) -> Result<Json<ModelView>> {
    let config = models::update(&state.store.data, &name, payload).await?;

    tracing::info!(model = %config.name, multiplier_bp = config.multiplier_bp, "已调整模型倍率");

    Ok(Json(ModelView::from(&config)))
}

/// `DELETE /api/models/{name}`
pub async fn delete(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<MessageResponse>> {
    models::delete(&state.store.data, &name).await?;

    tracing::info!(model = %name, "已删除模型配置（回落到默认倍率）");

    Ok(Json(MessageResponse::ok("已删除，该模型回落到默认倍率")))
}
