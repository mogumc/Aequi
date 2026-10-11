//! 上游块与密钥的管理接口（管理员专用）。

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::Json;
use serde::Serialize;

use crate::api::MessageResponse;
use crate::error::Result;
use crate::model::{
    BlockCreate, BlockTestResult, BlockUpdate, BlockView, KeyIdsRequest, UpstreamKeyCreate,
    UpstreamKeyView,
};
use crate::service::upstream;
use crate::AppState;

/// 块列表响应。
#[derive(Debug, Serialize)]
pub struct BlockListResponse {
    pub blocks: Vec<BlockView>,
}

/// 从上游拉取的模型列表。
#[derive(Debug, Serialize)]
pub struct FetchedModelsResponse {
    /// 上游报出来的模型名，已排序去重。
    pub models: Vec<String>,
}

/// 批量添加密钥的响应：入库的密钥与被跳过的重复数量。
#[derive(Debug, Serialize)]
pub struct AddKeysResponse {
    pub keys: Vec<UpstreamKeyView>,
    pub skipped: usize,
}

/// 密钥列表响应。
#[derive(Debug, Serialize)]
pub struct KeyListResponse {
    pub keys: Vec<UpstreamKeyView>,
}

/// `GET /api/blocks`
pub async fn list(State(state): State<Arc<AppState>>) -> Result<Json<BlockListResponse>> {
    Ok(Json(BlockListResponse {
        blocks: upstream::list_blocks(&state.store.data).await?,
    }))
}

/// `POST /api/blocks`
pub async fn create(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<BlockCreate>,
) -> Result<Json<BlockView>> {
    let block = upstream::create_block(&state.store.data, payload).await?;
    Ok(Json(upstream::block_view(&state.store.data, &block.id).await?))
}

/// `GET /api/blocks/{id}`
pub async fn detail(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<BlockView>> {
    Ok(Json(upstream::block_view(&state.store.data, &id).await?))
}

/// `PATCH /api/blocks/{id}`
pub async fn update(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(payload): Json<BlockUpdate>,
) -> Result<Json<BlockView>> {
    upstream::update_block(&state.store.data, &state.upstream, &id, payload).await?;
    Ok(Json(upstream::block_view(&state.store.data, &id).await?))
}

/// `DELETE /api/blocks/{id}`
pub async fn delete(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<MessageResponse>> {
    upstream::delete_block(&state.store.data, &state.upstream, &id).await?;
    Ok(Json(MessageResponse::ok("块已删除")))
}

/// `GET /api/blocks/{id}/keys`
pub async fn list_keys(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<KeyListResponse>> {
    Ok(Json(KeyListResponse {
        keys: upstream::list_keys(&state.store.data, &id).await?,
    }))
}

/// `POST /api/blocks/{id}/keys`
pub async fn add_keys(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(payload): Json<UpstreamKeyCreate>,
) -> Result<Json<AddKeysResponse>> {
    let outcome = upstream::add_keys(&state.store.data, &id, payload).await?;
    Ok(Json(AddKeysResponse {
        keys: outcome.keys,
        skipped: outcome.skipped,
    }))
}

/// `POST /api/blocks/{id}/keys/remove`
pub async fn remove_keys(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(payload): Json<KeyIdsRequest>,
) -> Result<Json<MessageResponse>> {
    let removed = upstream::delete_keys(&state.store.data, &state.upstream, &id, &payload.ids).await?;
    Ok(Json(MessageResponse::ok(format!("已删除 {removed} 把密钥"))))
}

/// `POST /api/blocks/{id}/keys/pause`
pub async fn pause_keys(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(payload): Json<KeyIdsRequest>,
) -> Result<Json<MessageResponse>> {
    upstream::pause_keys(&state.store.data, &id, &payload.ids).await?;
    Ok(Json(MessageResponse::ok("已暂停")))
}

/// `POST /api/blocks/{id}/keys/resume`
pub async fn resume_keys(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(payload): Json<KeyIdsRequest>,
) -> Result<Json<MessageResponse>> {
    upstream::resume_keys(&state.store.data, &id, &payload.ids).await?;
    Ok(Json(MessageResponse::ok("已恢复")))
}

/// `POST /api/blocks/{id}/keys/{key_id}/test`
pub async fn test_key(
    State(state): State<Arc<AppState>>,
    Path((id, key_id)): Path<(String, String)>,
) -> Result<Json<MessageResponse>> {
    match upstream::test_key(&state.store.data, &state.upstream, &id, &key_id).await? {
        crate::proxy::adapter::ProbeOutcome::Valid => {
            Ok(Json(MessageResponse::ok("密钥可用")))
        }
        crate::proxy::adapter::ProbeOutcome::Invalid(detail) => {
            Ok(Json(MessageResponse::ok(format!("密钥无效：{detail}"))))
        }
        crate::proxy::adapter::ProbeOutcome::Transient(detail) => {
            Ok(Json(MessageResponse::ok(format!("暂时不可用：{detail}"))))
        }
    }
}

/// `POST /api/blocks/{id}/test`
///
/// 整块连通性测试：逐把测试块内全部密钥，返回每把的结果与汇总。
/// `POST /api/blocks/{id}/models/fetch`
///
/// 用块自己的出口与密钥去上游拉一份可用模型，供前端勾选加入。
/// 上游没有列模型接口、或返回的列表不全时，前端仍可手动添加。
pub async fn fetch_models(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<FetchedModelsResponse>> {
    let models = upstream::fetch_models(&state.store.data, &state.upstream, &id).await?;

    tracing::info!(block = %id, count = models.len(), "已从上游拉取模型列表");

    Ok(Json(FetchedModelsResponse { models }))
}

pub async fn test_block(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<BlockTestResult>> {
    Ok(Json(
        upstream::test_block(&state.store.data, &state.upstream, &id).await?,
    ))
}
