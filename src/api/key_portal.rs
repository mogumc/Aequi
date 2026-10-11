//! 密钥自助接口。
//!
//! 只读：能看自己的信息和用量，**不具备任何管理能力**，
//! 也无法触达管理员体系的任何数据。

use std::sync::Arc;

use axum::extract::{Extension, Query, State};
use axum::Json;

use crate::api::key_auth::KeyContext;
use crate::api::{UsageListResponse, UsageQuery};
use crate::error::Result;
use crate::model::KeySelfView;
use crate::service::keys;
use crate::AppState;

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
    let records = keys::usage(&state.store.data, &context.key.id, query.limit()).await?;

    Ok(Json(UsageListResponse::new(
        summary,
        records.iter().map(Into::into).collect(),
    )))
}
