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
 status, error_detail, duration_ms, prompt_tokens, completion_tokens, cache_tokens, estimated, stream, credits_scaled) \
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";

/// 带筛选与游标的列表。每个条件都是 `(? IS NULL OR ...)`——传 NULL 即不限制，
/// 这样一条固定 SQL 能覆盖全部组合，不必拼接字符串。
///
/// `before_id` 是游标分页的锚点：0 表示从最新开始，传上一页最后一条的 id
/// 即取更早的一页。排序用 `id DESC`——id 自增与时间同向，与
/// `created_at DESC, id DESC` 等价且让游标条件能吃上主键索引。
const SQL_LIST_FILTERED: &str = "\
SELECT id, created_at, access_key_id, block_id, upstream_key_id, model, ip, user_agent, \
       status, error_detail, duration_ms, prompt_tokens, completion_tokens, cache_tokens, estimated, stream, credits_scaled \
FROM proxy_logs \
WHERE (? IS NULL OR created_at >= ?) \
  AND (? IS NULL OR created_at <= ?) \
  AND (? IS NULL OR status = ?) \
  AND (? IS NULL OR access_key_id = ?) \
  AND (? IS NULL OR model = ?) \
  AND (? IS NULL OR block_id = ?) \
  AND (? = 0 OR id < ?) \
ORDER BY id DESC LIMIT ?";

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
        stream: row.get::<i64, _>("stream") != 0,
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
        .bind(i64::from(log.stream))
        .bind(log.credits_scaled)
        .execute(pool)
        .await?;
    Ok(())
}

/// 日志筛选条件。字段为 `None` 表示该维度不限制。
#[derive(Debug, Default, Clone)]
pub struct LogFilter {
    /// 起始时间（含），Unix 秒。
    pub since: Option<i64>,
    /// 截止时间（含），Unix 秒。
    pub until: Option<i64>,
    /// `success` 或 `error`。
    pub status: Option<String>,
    /// 访问密钥 id。
    pub access_key_id: Option<String>,
    pub model: Option<String>,
    pub block_id: Option<String>,
    /// 游标分页：0 表示从最新开始，传上一页最后一条的 id 取更早一页。
    pub before_id: i64,
    pub limit: i64,
}

/// 按条件筛选日志，最近的排在前面。
///
/// 多取一条判断 `has_more`，调用方按它决定是否展示「查询更多」。
pub async fn list_filtered(
    pool: &SqlitePool,
    filter: &LogFilter,
) -> Result<(Vec<ProxyLog>, bool), sqlx::Error> {
    let clamped = filter.limit.clamp(1, MAX_LOG_LIMIT);
    let rows = sqlx::query(SQL_LIST_FILTERED)
        .bind(filter.since)
        .bind(filter.since)
        .bind(filter.until)
        .bind(filter.until)
        .bind(filter.status.as_deref())
        .bind(filter.status.as_deref())
        .bind(filter.access_key_id.as_deref())
        .bind(filter.access_key_id.as_deref())
        .bind(filter.model.as_deref())
        .bind(filter.model.as_deref())
        .bind(filter.block_id.as_deref())
        .bind(filter.block_id.as_deref())
        .bind(filter.before_id)
        .bind(filter.before_id)
        .bind(clamped + 1)
        .fetch_all(pool)
        .await?;
    let has_more = rows.len() as i64 > clamped;
    let logs: Vec<ProxyLog> = rows.iter().take(clamped as usize).map(map_row).collect();
    Ok((logs, has_more))
}

/// 回收过期日志。
pub async fn purge_before(pool: &SqlitePool, cutoff: i64) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(SQL_PURGE).bind(cutoff).execute(pool).await?;
    Ok(result.rows_affected())
}
