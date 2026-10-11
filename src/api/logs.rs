//! 代理日志查询接口（管理员专用）。
//!
//! 详细日志只在这里露出；密钥侧能看到的在 [`crate::api::key_portal`]，
//! 两边物理分表，不存在越权查看的问题。

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::db;
use crate::db::proxy_log::LogFilter;
use crate::error::Result;
use crate::model::ProxyLogView;
use crate::AppState;

/// 日志查询参数。除 `limit` 外都可省略，省略即该维度不筛选。
#[derive(Debug, Default, Deserialize)]
pub struct LogQuery {
    pub limit: Option<i64>,
    /// 起始时间（含），Unix 秒。
    pub since: Option<i64>,
    /// 截止时间（含），Unix 秒。
    pub until: Option<i64>,
    /// `success` 或 `error`。
    pub status: Option<String>,
    /// 访问密钥 id。
    pub key_id: Option<String>,
    pub model: Option<String>,
    pub block_id: Option<String>,
}

impl LogQuery {
    /// 收敛成数据层的筛选条件。
    fn into_filter(self) -> LogFilter {
        let clean = |value: Option<String>| {
            value
                .map(|text| text.trim().to_string())
                .filter(|text| !text.is_empty())
        };

        LogFilter {
            since: self.since,
            until: self.until,
            status: clean(self.status),
            access_key_id: clean(self.key_id),
            model: clean(self.model),
            block_id: clean(self.block_id),
            limit: self.limit.unwrap_or(100),
        }
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
    let logs = db::proxy_log::list_filtered(&state.store.data, &query.into_filter()).await?;

    Ok(Json(LogListResponse {
        logs: logs.iter().map(Into::into).collect(),
    }))
}
