//! 登录设备（会话）管理接口。

use std::sync::Arc;

use axum::extract::{Extension, Path, State};
use axum::Json;
use serde::Serialize;

use crate::api::middleware::AuthContext;
use crate::api::MessageResponse;
use crate::error::Result;
use crate::model::SessionView;
use crate::service::device;
use crate::AppState;

/// 设备列表响应。
#[derive(Debug, Serialize)]
pub struct DeviceListResponse {
    pub devices: Vec<SessionView>,
}

/// 批量撤销结果。
#[derive(Debug, Serialize)]
pub struct RevokeOthersResponse {
    pub revoked: u64,
}

/// `GET /api/devices`
///
/// 列出全部登录设备。会话之间不做互斥，多端可同时在线。
pub async fn list(
    State(state): State<Arc<AppState>>,
    Extension(context): Extension<AuthContext>,
) -> Result<Json<DeviceListResponse>> {
    let devices = device::list(state.pool(), context.account.id, &context.session.id).await?;
    Ok(Json(DeviceListResponse { devices }))
}

/// `DELETE /api/devices/{id}`
///
/// 撤销指定设备。撤销自己等价于退出登录。
pub async fn revoke(
    State(state): State<Arc<AppState>>,
    Extension(context): Extension<AuthContext>,
    Path(session_id): Path<String>,
) -> Result<Json<MessageResponse>> {
    device::revoke(state.pool(), context.account.id, &session_id).await?;
    Ok(Json(MessageResponse::ok("该设备已被移出")))
}

/// `DELETE /api/devices`
///
/// 撤销除当前设备外的全部设备。
pub async fn revoke_others(
    State(state): State<Arc<AppState>>,
    Extension(context): Extension<AuthContext>,
) -> Result<Json<RevokeOthersResponse>> {
    let revoked = device::revoke_others(state.pool(), context.account.id, &context.session.id).await?;
    Ok(Json(RevokeOthersResponse { revoked }))
}
