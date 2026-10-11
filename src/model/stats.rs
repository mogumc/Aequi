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
