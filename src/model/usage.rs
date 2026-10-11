//! 用量记录（密钥侧可见的扣费与普通错误日志）。
//!
//! 这套日志是**脱敏**的：只有计费数字、模型与一句普通错误描述，
//! 上游密钥、块、IP、UA 与详细报错都在 [`crate::model::ProxyLog`] 里，
//! 只对管理员开放。

use serde::Serialize;

use crate::model::credits;

/// 一条用量记录。
///
/// 成功请求带完整的 token 分解与扣费；失败请求的 token 与扣费为零，
/// 只带一句脱敏后的错误。
#[derive(Debug, Clone)]
pub struct UsageRecord {
    pub id: i64,
    pub key_id: String,
    pub model: String,
    /// 上游报的总 token 数；失败请求为 0。
    pub raw_tokens: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    /// 命中缓存的输入 token。
    pub cache_tokens: i64,
    /// token 数是否为本地估算。
    pub estimated: bool,
    /// 是否为流式请求。
    pub stream: bool,
    /// 记账当时的倍率快照（万分比）。
    pub multiplier_bp: i64,
    /// 按倍率折算后的 token 数。
    pub final_tokens: i64,
    /// 本次扣减的积分，整数最小单位。
    pub credits_scaled: i64,
    /// success | error
    pub status: String,
    /// 脱敏后的错误信息，仅失败请求有。
    pub error: Option<String>,
    pub created_at: i64,
}

/// 用量记录视图。
///
/// 给密钥自助侧看的，天然不含任何敏感信息。
#[derive(Debug, Clone, Serialize)]
pub struct UsageView {
    pub id: i64,
    pub model: String,
    pub status: String,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cache_tokens: i64,
    pub estimated: bool,
    pub stream: bool,
    pub raw_tokens: i64,
    /// 倍率，四位小数字符串。
    pub multiplier: String,
    pub final_tokens: i64,
    /// 消耗积分，四位小数字符串。
    pub credits: String,
    pub error: Option<String>,
    pub created_at: i64,
}

impl From<&UsageRecord> for UsageView {
    fn from(record: &UsageRecord) -> Self {
        Self {
            id: record.id,
            model: record.model.clone(),
            status: record.status.clone(),
            prompt_tokens: record.prompt_tokens,
            completion_tokens: record.completion_tokens,
            cache_tokens: record.cache_tokens,
            estimated: record.estimated,
            stream: record.stream,
            raw_tokens: record.raw_tokens,
            multiplier: credits::format_multiplier(record.multiplier_bp),
            final_tokens: record.final_tokens,
            credits: credits::format_credits(record.credits_scaled),
            error: record.error.clone(),
            created_at: record.created_at,
        }
    }
}

/// 某密钥的累计用量。
#[derive(Debug, Clone, Serialize)]
pub struct UsageSummary {
    /// 请求次数（含失败）。
    pub requests: i64,
    /// 其中失败的次数。
    pub failed: i64,
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
        failed: i64,
        raw_tokens: i64,
        final_tokens: i64,
        credits_scaled: i64,
    ) -> Self {
        Self {
            requests,
            failed,
            raw_tokens,
            final_tokens,
            credits: credits::format_credits(credits_scaled),
            credits_scaled,
        }
    }
}
