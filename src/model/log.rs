//! 代理请求的详细日志（管理员可见）。
//!
//! 这套日志存的是「排障需要的一切」：块、上游密钥、IP、UA、详细报错。
//! 它与密钥侧的用量记录（[`crate::model::UsageRecord`]）**物理分表**：
//! 敏感字段根本不会出现在密钥能查到的那张表里，脱敏不靠查询时筛选。

use serde::Serialize;

use crate::model::credits;

/// 一条详细日志。
#[derive(Debug, Clone)]
pub struct ProxyLog {
    pub id: i64,
    pub created_at: i64,
    /// 受理请求的下游访问密钥。
    pub access_key_id: String,
    /// 被选中的块。
    pub block_id: Option<String>,
    /// 被选中的上游密钥。
    pub upstream_key_id: Option<String>,
    /// 对外的模型名。
    pub model: String,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    /// success | error
    pub status: String,
    /// 详细错误。只在失败时有，可能包含上游返回的原话。
    pub error_detail: Option<String>,
    pub duration_ms: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cache_tokens: i64,
    pub estimated: bool,
    /// 是否为流式请求。
    pub stream: bool,
    pub credits_scaled: i64,
}

/// 管理员视角的日志视图。
#[derive(Debug, Clone, Serialize)]
pub struct ProxyLogView {
    pub id: i64,
    pub created_at: i64,
    pub access_key_id: String,
    pub block_id: Option<String>,
    pub upstream_key_id: Option<String>,
    pub model: String,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    pub status: String,
    pub error_detail: Option<String>,
    pub duration_ms: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cache_tokens: i64,
    pub estimated: bool,
    pub stream: bool,
    /// 消耗积分，四位小数字符串。
    pub credits: String,
}

impl From<&ProxyLog> for ProxyLogView {
    fn from(log: &ProxyLog) -> Self {
        Self {
            id: log.id,
            created_at: log.created_at,
            access_key_id: log.access_key_id.clone(),
            block_id: log.block_id.clone(),
            upstream_key_id: log.upstream_key_id.clone(),
            model: log.model.clone(),
            ip: log.ip.clone(),
            user_agent: log.user_agent.clone(),
            status: log.status.clone(),
            error_detail: log.error_detail.clone(),
            duration_ms: log.duration_ms,
            prompt_tokens: log.prompt_tokens,
            completion_tokens: log.completion_tokens,
            cache_tokens: log.cache_tokens,
            estimated: log.estimated,
            stream: log.stream,
            credits: credits::format_credits(log.credits_scaled),
        }
    }
}

/// 写入一条详细日志所需的字段。
#[derive(Debug, Clone)]
pub struct NewProxyLog {
    pub access_key_id: String,
    pub block_id: Option<String>,
    pub upstream_key_id: Option<String>,
    pub model: String,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    pub status: String,
    pub error_detail: Option<String>,
    pub duration_ms: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cache_tokens: i64,
    pub estimated: bool,
    pub stream: bool,
    pub credits_scaled: i64,
    pub created_at: i64,
}
