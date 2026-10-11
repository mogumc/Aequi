//! 后台设置接口。
//!
//! 设置存在数据库里，这里是唯一的修改入口。

use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde::Serialize;

use crate::config::{Settings, SettingsPatch};
use crate::error::Result;
use crate::service::settings as settings_service;
use crate::AppState;

/// 设置响应。设置项平铺，另附是否需要重启。
#[derive(Debug, Serialize)]
pub struct SettingsResponse {
    #[serde(flatten)]
    pub settings: Settings,
    /// 本次改动是否需要重启服务才能生效。
    pub restart_required: bool,
}

/// `GET /api/settings`
pub async fn get(State(state): State<Arc<AppState>>) -> Json<SettingsResponse> {
    Json(SettingsResponse {
        settings: state.settings(),
        restart_required: false,
    })
}

/// `PATCH /api/settings`
///
/// 只提交需要改动的字段。令牌有效期即时生效，
/// 监听地址与 CORS 需要重启，返回体里的 `restart_required` 会说明。
pub async fn update(
    State(state): State<Arc<AppState>>,
    Json(patch): Json<SettingsPatch>,
) -> Result<Json<SettingsResponse>> {
    let current = state.settings();
    let next = settings_service::update(state.pool(), &current, patch).await?;

    // 只有这两项真正变了才需要重启；提交了同值不算。
    let restart_required =
        next.listen != current.listen || next.cors_origins != current.cors_origins;

    state.replace_settings(next.clone());

    Ok(Json(SettingsResponse {
        settings: next,
        restart_required,
    }))
}
