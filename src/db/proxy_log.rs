//! `proxy_logs` 表的读写——管理员可见的详细日志。
//!
//! 密钥侧的用量记录在 [`crate::db::usage`]，两张表物理分开：
//! IP、UA、上游密钥与详细报错只存在于这里。

use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

use crate::model::{NewProxyLog, ProxyLog};

const SQL_INSERT: &str = "\
INSERT INTO proxy_logs \
(created_at, access_key_id, block_id, upstream_key_id, model, ip, user_agent, \
 status, error_detail, duration_ms, prompt_tokens, completion_tokens, cache_tokens, estimated, credits_scaled) \
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";

const SQL_LIST: &str = "\
SELECT id, created_at, access_key_id, block_id, upstream_key_id, model, ip, user_agent, \
       status, error_detail, duration_ms, prompt_tokens, completion_tokens, cache_tokens, estimated, credits_scaled \
FROM proxy_logs ORDER BY created_at DESC, id DESC LIMIT ?";

const SQL_PURGE: &str = "DELETE FROM proxy_logs WHERE created_at <= ?";

/// 单次查询能返回的最大日志条数。
pub const MAX_LOG_LIMIT: i64 = 500;

fn map_row(row: &SqliteRow) -> ProxyLog {
    ProxyLog {
        id: row.get("id"),
        created_at: row.get("created_at"),
        access_key_id: row.get("access_key_id"),
        block_id: row.get("block_id"),
        upstream_key_id: row.get("upstream_key_id"),
        model: row.get("model"),
        ip: row.get("ip"),
        user_agent: row.get("user_agent"),
        status: row.get("status"),
        error_detail: row.get("error_detail"),
        duration_ms: row.get("duration_ms"),
        prompt_tokens: row.get("prompt_tokens"),
        completion_tokens: row.get("completion_tokens"),
        cache_tokens: row.get("cache_tokens"),
        estimated: row.get::<i64, _>("estimated") != 0,
        credits_scaled: row.get("credits_scaled"),
    }
}

/// 写入一条详细日志。
///
/// 日志写入失败不该拖垮业务请求，调用方应吞掉错误只留警告。
pub async fn insert(pool: &SqlitePool, log: &NewProxyLog) -> Result<(), sqlx::Error> {
    sqlx::query(SQL_INSERT)
        .bind(log.created_at)
        .bind(&log.access_key_id)
        .bind(&log.block_id)
        .bind(&log.upstream_key_id)
        .bind(&log.model)
        .bind(&log.ip)
        .bind(&log.user_agent)
        .bind(&log.status)
        .bind(&log.error_detail)
        .bind(log.duration_ms)
        .bind(log.prompt_tokens)
        .bind(log.completion_tokens)
        .bind(log.cache_tokens)
        .bind(i64::from(log.estimated))
        .bind(log.credits_scaled)
        .execute(pool)
        .await?;
    Ok(())
}

/// 列出最近的日志，`limit` 收敛到 `1..=MAX_LOG_LIMIT`。
pub async fn list(pool: &SqlitePool, limit: i64) -> Result<Vec<ProxyLog>, sqlx::Error> {
    let rows = sqlx::query(SQL_LIST)
        .bind(limit.clamp(1, MAX_LOG_LIMIT))
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(map_row).collect())
}

/// 回收过期日志。
pub async fn purge_before(pool: &SqlitePool, cutoff: i64) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(SQL_PURGE).bind(cutoff).execute(pool).await?;
    Ok(result.rows_affected())
}
