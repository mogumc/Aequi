//! 仪表盘统计接口（管理员专用）。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;

use crate::AppState;
use crate::error::Result;
use crate::model::StatsView;
use crate::service::stats;

/// 统计查询参数。
#[derive(Debug, Deserialize)]
pub struct StatsQuery {
    /// 趋势图回看天数，默认 7，收敛到 1-90。
    #[serde(default = "default_days")]
    pub days: i64,
}

fn default_days() -> i64 {
    7
}

/// `GET /api/stats`
pub async fn overview(
    State(state): State<Arc<AppState>>,
    Query(query): Query<StatsQuery>,
) -> Result<Json<StatsView>> {
    Ok(Json(stats::overview(&state.store.data, query.days).await?))
}
