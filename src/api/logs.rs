//! 代理日志查询接口（管理员专用）。
//!
//! 详细日志只在这里露出；密钥侧能看到的在 [`crate::api::key_portal`]，
//! 两边物理分表，不存在越权查看的问题。

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::db;
use crate::error::Result;
use crate::model::ProxyLogView;
use crate::AppState;

/// 日志查询参数。
#[derive(Debug, Deserialize)]
pub struct LogQuery {
    #[serde(default = "default_limit")]
    pub limit: i64,
}

fn default_limit() -> i64 {
    100
}

impl LogQuery {
    /// 收敛到数据层允许的区间。
    pub fn limit(&self) -> i64 {
        self.limit.clamp(1, db::proxy_log::MAX_LOG_LIMIT)
    }
}

/// 日志列表响应。
#[derive(Debug, Serialize)]
pub struct LogListResponse {
    pub logs: Vec<ProxyLogView>,
}

/// `GET /api/logs`
pub async fn list(
    State(state): State<Arc<AppState>>,
    Query(query): Query<LogQuery>,
) -> Result<Json<LogListResponse>> {
    let logs = db::proxy_log::list(&state.store.data, query.limit()).await?;

    Ok(Json(LogListResponse {
        logs: logs.iter().map(Into::into).collect(),
    }))
}
