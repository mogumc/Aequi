//! 仪表盘统计。

use serde::Serialize;

/// 一段时间的用量汇总。
#[derive(Debug, Clone, Serialize)]
pub struct StatsCounters {
    /// 请求次数（含失败）。
    pub requests: i64,
    pub failed: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cache_tokens: i64,
    /// 消耗积分，四位小数字符串。
    pub credits: String,
    pub credits_scaled: i64,
}

/// 资源计数。
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ResourceCount {
    pub total: i64,
    pub enabled: i64,
}

/// 趋势图上的一天。
#[derive(Debug, Clone, Serialize)]
pub struct StatsPoint {
    /// 本地时区的日期，`YYYY-MM-DD`。
    pub date: String,
    pub requests: i64,
    pub failed: i64,
    pub tokens: i64,
    /// 消耗积分，四位小数字符串。
    pub credits: String,
    pub credits_scaled: i64,
}

/// 仪表盘视图。
#[derive(Debug, Clone, Serialize)]
pub struct StatsView {
    /// 全部时间的用量。
    pub usage: StatsCounters,
    /// 今日（本地时区）的用量。
    pub today: StatsCounters,
    /// 访问密钥计数。
    pub keys: ResourceCount,
    /// 上游块计数。
    pub blocks: ResourceCount,
    pub trend: Vec<StatsPoint>,
}

/// 一段时间的请求计数（成功与失败分开）。
#[derive(Debug, Clone, Copy, Serialize)]
pub struct DayCounters {
    pub requests: i64,
    pub success: i64,
    pub failed: i64,
}

/// 首页「最近错误」列表里的一条。
///
/// 只带首页要显示的那几列：详细日志那条记录有 IP、UA、时长等一整套，
/// 首页为了三行提示去拉整行不划算。
#[derive(Debug, Clone, Serialize)]
pub struct RecentError {
    pub id: i64,
    pub created_at: i64,
    /// 对外的模型名。
    pub model: String,
    pub block_id: Option<String>,
    /// 详细错误（可能含上游原话与整条重试链）。
    pub error_detail: Option<String>,
}

/// 额度预警里的一把密钥。
#[derive(Debug, Clone, Serialize)]
pub struct LowCreditKey {
    pub id: String,
    pub name: String,
    /// 剩余积分，四位小数字符串。
    pub credits: String,
    pub credits_scaled: i64,
}

/// 额度预警：命中多少把，以及最危险的几把明细。
#[derive(Debug, Clone, Serialize)]
pub struct LowCreditAlert {
    /// 判定线，四位小数字符串——由服务端定义，界面直接显示，不必两边各写一份。
    pub threshold: String,
    pub threshold_scaled: i64,
    /// 命中的总数；`keys` 只给前几条明细。
    pub count: i64,
    pub keys: Vec<LowCreditKey>,
}

/// 按整点统计的一个点。
#[derive(Debug, Clone, Copy, Serialize)]
pub struct HourlyPoint {
    /// 该小时的起点，Unix 秒（整点）。
    pub hour: i64,
    pub requests: i64,
    pub failed: i64,
}

/// 首页看板的一次快照。
///
/// 存在的理由：首页原来拉 500 条详细日志到内存里自己数，既不是「当日」的
/// 语义（那 500 条可能只覆盖几个小时），又把 IP、UA、重试链这些用不上的
/// 字段全搬进了浏览器。聚合放在库里，一次查完。
#[derive(Debug, Clone, Serialize)]
pub struct DashboardView {
    /// 今日（本地时区当天 0 点起）。
    pub today: DayCounters,
    /// 最近几条失败，新的在前。
    pub recent_errors: Vec<RecentError>,
    /// 访问密钥计数。
    pub keys: ResourceCount,
    /// 余额偏低的密钥。
    pub low_credits: LowCreditAlert,
    /// 最近 24 个整点，从旧到新；最后一个桶是当前这一小时（尚未走完）。
    pub hourly: Vec<HourlyPoint>,
}
