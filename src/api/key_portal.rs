//! 密钥自助接口。
//!
//! 只读：能看自己的信息和用量，**不具备任何管理能力**，
//! 也无法触达管理员体系的任何数据。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, Query, State};

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
