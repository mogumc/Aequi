use serde::Serialize;

use crate::model::credits;

/// 一条用量记录。
#[derive(Debug, Clone)]
pub struct UsageRecord {
    pub id: i64,
    pub key_id: String,
    pub model: String,
    /// 上游实际产生的 token 数。
    pub raw_tokens: i64,
    /// 记账当时的倍率快照（万分比）。
    pub multiplier_bp: i64,
    /// 按倍率折算后的 token 数。
    pub final_tokens: i64,
    /// 本次扣减的积分，整数最小单位。
    pub credits_scaled: i64,
    pub created_at: i64,
}

/// 用量记录视图。
#[derive(Debug, Clone, Serialize)]
pub struct UsageView {
    pub id: i64,
    pub model: String,
    pub raw_tokens: i64,
    /// 倍率，四位小数字符串。
    pub multiplier: String,
    pub final_tokens: i64,
    /// 消耗积分，四位小数字符串。
    pub credits: String,
    pub created_at: i64,
}

impl From<&UsageRecord> for UsageView {
    fn from(record: &UsageRecord) -> Self {
        Self {
            id: record.id,
            model: record.model.clone(),
            raw_tokens: record.raw_tokens,
            multiplier: credits::format_multiplier(record.multiplier_bp),
            final_tokens: record.final_tokens,
            credits: credits::format_credits(record.credits_scaled),
            created_at: record.created_at,
        }
    }
}

/// 某密钥的累计用量。
#[derive(Debug, Clone, Serialize)]
pub struct UsageSummary {
    /// 请求次数。
    pub requests: i64,
    pub raw_tokens: i64,
    pub final_tokens: i64,
    /// 累计消耗积分，四位小数字符串。
    pub credits: String,
    pub credits_scaled: i64,
}

impl UsageSummary {
    /// 由数据库返回的整数汇总组装视图。
    pub fn from_scaled(
        requests: i64,
        raw_tokens: i64,
        final_tokens: i64,
        credits_scaled: i64,
    ) -> Self {
        Self {
            requests,
            raw_tokens,
            final_tokens,
            credits: credits::format_credits(credits_scaled),
            credits_scaled,
        }
    }
}
