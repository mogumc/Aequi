//! 仪表盘统计。
//!
//! 聚合放在数据库侧：日志表可能很大，拉到内存里再汇总既慢又占资源。
//! 「今日」与趋势按**本地时区**切天——用户看的是本地的今天。

use std::collections::HashMap;

use chrono::{Local, TimeZone};
use sqlx::SqlitePool;

use crate::db;
use crate::error::Result;
use crate::model::{DashboardView, HourlyPoint, LowCreditAlert, StatsView, credits};
use crate::utils::time;

/// 趋势图最多回看的天数。
const MAX_TREND_DAYS: i64 = 90;

/// 一小时的秒数。小时图的桶宽就是它。
const HOUR: i64 = 3_600;

/// 首页小时图覆盖多少个整点。
const HOURLY_BUCKETS: i64 = 24;

/// 首页展示的最近错误条数。
const RECENT_ERRORS: i64 = 3;

/// 额度预警的判定线：剩余不足 5 积分。
///
/// 与面板原有的口径一致（启用、非无限额度、余额 < 5）。写死是刻意的：
/// 一条常量比一个没人调的设置项好维护；真需要按运营周期调整时再进设置表。
const LOW_CREDITS_SCALED: i64 = 5 * credits::CREDIT_SCALE;

/// 额度预警最多列出几把密钥的明细，总数另给。
const LOW_CREDIT_LIST_LIMIT: i64 = 20;

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

/// 首页看板的一次快照。
///
/// 所有数字都在库里算完再回来：首页原来把 500 条详细日志拉进浏览器自己数，
/// 既拿不到完整的「当日」（500 条可能只覆盖几个小时），又白搬了 IP、UA、
/// 重试链这些用不上的字段。
pub async fn dashboard(pool: &SqlitePool) -> Result<DashboardView> {
    let now = time::now_secs();
    // 当日以本地 0 点切；小时图以整点为桶，最后一个桶是当前这一小时（未满）。
    let today_start = local_day_start(now);
    let current_hour = now - now.rem_euclid(HOUR);
    let window_start = current_hour - (HOURLY_BUCKETS - 1) * HOUR;

    let today = db::proxy_log::counters_since(pool, today_start).await?;
    let counted = db::proxy_log::hourly_since(pool, window_start).await?;
    let recent_errors = db::proxy_log::recent_errors(pool, RECENT_ERRORS).await?;
    let keys = db::keys::count_enabled(pool).await?;
    let low_count = db::keys::count_low_credits(pool, LOW_CREDITS_SCALED).await?;
    let low_keys =
        db::keys::list_low_credits(pool, LOW_CREDITS_SCALED, LOW_CREDIT_LIST_LIMIT).await?;

    Ok(DashboardView {
        today,
        recent_errors,
        keys,
        low_credits: LowCreditAlert {
            threshold: credits::format_credits(LOW_CREDITS_SCALED),
            threshold_scaled: LOW_CREDITS_SCALED,
            count: low_count,
            keys: low_keys,
        },
        hourly: fill_hourly(counted, window_start),
    })
}

/// 把 SQL 返回的「有流量的整点」补成等宽的 24 个桶。
///
/// 图上的横轴必须是等间距的：半夜没人用不等于那个小时不存在。缺失的小时
/// 补 0，超出窗口的（理论上不会有）丢掉。
fn fill_hourly(counted: Vec<HourlyPoint>, window_start: i64) -> Vec<HourlyPoint> {
    let mut by_hour: HashMap<i64, HourlyPoint> =
        counted.into_iter().map(|point| (point.hour, point)).collect();

    (0..HOURLY_BUCKETS)
        .map(|offset| {
            let hour = window_start + offset * HOUR;
            by_hour.remove(&hour).unwrap_or(HourlyPoint {
                hour,
                requests: 0,
                failed: 0,
            })
        })
        .collect()
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
