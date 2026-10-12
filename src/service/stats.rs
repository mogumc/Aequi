//! 仪表盘统计。
//!
//! 聚合放在数据库侧：日志表可能很大，拉到内存里再汇总既慢又占资源。
//! 「今日」与趋势按**本地时区**切天——用户看的是本地的今天。

use chrono::{Local, TimeZone};
use sqlx::SqlitePool;

use crate::db;
use crate::error::Result;
use crate::model::StatsView;
use crate::utils::time;

/// 趋势图最多回看的天数。
const MAX_TREND_DAYS: i64 = 90;

/// 汇总仪表盘需要的全部数据。
pub async fn overview(pool: &SqlitePool, days: i64) -> Result<StatsView> {
    let days = days.clamp(1, MAX_TREND_DAYS);
    let now = time::now_secs();
    let today_start = local_day_start(now);
    let trend_start = today_start - (days - 1) * 86_400;

    Ok(StatsView {
        usage: db::usage::totals_since(pool, 0).await?,
        today: db::usage::totals_since(pool, today_start).await?,
        keys: db::keys::count_enabled(pool).await?,
        blocks: db::block::count_enabled(pool).await?,
        trend: db::usage::trend_since(pool, trend_start).await?,
    })
}

/// 本地时区里某个时间戳所在那天的 0 点。
///
/// 按 UTC 切天会把跨零点的数据算到昨天。时区数据拿不到时退回 UTC 天——
/// 宁可偏一点，也不能算不出来。
fn local_day_start(ts: i64) -> i64 {
    Local
        .timestamp_opt(ts, 0)
        .single()
        .and_then(|datetime| datetime.date_naive().and_hms_opt(0, 0, 0))
        .and_then(|naive| Local.from_local_datetime(&naive).single())
        .map(|datetime| datetime.timestamp())
        .unwrap_or_else(|| ts - ts.rem_euclid(86_400))
}
