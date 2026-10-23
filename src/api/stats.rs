//! 仪表盘统计接口（管理员专用）。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;

use crate::AppState;
use crate::error::Result;
use crate::model::{DashboardView, StatsView};
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

/// `GET /api/stats/dashboard`
///
/// 首页专用：一次拿齐当日成功/失败/总数、最近几条错误、密钥计数、
/// 额度预警与最近 24 个整点的请求量。参数一个都不收——这些口径都不该由
/// 客户端决定，免得两边算出两个「今天」。
pub async fn dashboard(State(state): State<Arc<AppState>>) -> Result<Json<DashboardView>> {
    Ok(Json(stats::dashboard(&state.store.data).await?))
}
