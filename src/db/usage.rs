//! `usage_records` 表的读写——密钥侧可见的扣费与普通错误日志。

use sqlx::sqlite::{SqliteConnection, SqliteRow};
use sqlx::{Row, SqlitePool};

use crate::model::{UsageRecord, UsageSummary};

/// 成功请求：与扣费同事务写入。
const SQL_INSERT: &str = "\
INSERT INTO usage_records \
(key_id, model, raw_tokens, prompt_tokens, completion_tokens, cache_tokens, estimated, stream, \
 multiplier_bp, final_tokens, credits_scaled, status, created_at) \
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'success', ?)";

/// 失败请求：tokens 与扣费为零，只留一句脱敏错误。
const SQL_INSERT_ERROR: &str = "\
INSERT INTO usage_records \
(key_id, model, status, error, stream, created_at) \
VALUES (?, ?, 'error', ?, ?, ?)";

/// 某密钥的用量记录，按 id 倒序（与时间倒序等价，id 自增）。
///
/// `before_id` 是游标分页的锚点：传 0 表示从最新开始，传上一页最后一条
/// 的 id 即取更早的一页——数据持续写入，偏移量分页会漂移，游标不会。
const SQL_LIST_BY_KEY: &str = "\
SELECT id, key_id, model, raw_tokens, prompt_tokens, completion_tokens, cache_tokens, estimated, stream, \
       multiplier_bp, final_tokens, credits_scaled, status, error, created_at \
FROM usage_records \
WHERE key_id = ? AND (? = 0 OR id < ?) \
ORDER BY id DESC LIMIT ?";

const SQL_SUMMARY: &str = "\
SELECT COUNT(*)                                            AS requests, \
       COALESCE(SUM(status = 'error'), 0)                  AS failed, \
       COALESCE(SUM(raw_tokens), 0)                        AS raw_tokens, \
       COALESCE(SUM(final_tokens), 0)                      AS final_tokens, \
       COALESCE(SUM(prompt_tokens), 0)                     AS prompt_tokens, \
       COALESCE(SUM(completion_tokens), 0)                 AS completion_tokens, \
       COALESCE(SUM(cache_tokens), 0)                      AS cache_tokens, \
       COALESCE(SUM(credits_scaled), 0)                    AS credits_scaled \
FROM usage_records WHERE key_id = ?";

/// 回收给定时间之前的记录，返回删除的行数。
const SQL_PURGE: &str = "DELETE FROM usage_records WHERE created_at <= ?";

/// 从给定时间起的用量汇总。
const SQL_TOTALS: &str = "\
SELECT COUNT(*)                            AS requests, \
       COALESCE(SUM(status = 'error'), 0)  AS failed, \
       COALESCE(SUM(prompt_tokens), 0)     AS prompt_tokens, \
       COALESCE(SUM(completion_tokens), 0) AS completion_tokens, \
       COALESCE(SUM(cache_tokens), 0)      AS cache_tokens, \
       COALESCE(SUM(credits_scaled), 0)    AS credits_scaled \
FROM usage_records WHERE created_at >= ?";

/// 按天聚合（按本地时区切天）。
const SQL_TREND: &str = "\
SELECT date(created_at, 'unixepoch', 'localtime') AS day, \
       COUNT(*)                                   AS requests, \
       COALESCE(SUM(status = 'error'), 0)         AS failed, \
       COALESCE(SUM(raw_tokens), 0)               AS tokens, \
       COALESCE(SUM(credits_scaled), 0)           AS credits_scaled \
FROM usage_records WHERE created_at >= ? \
GROUP BY day ORDER BY day";

/// 写入一条成功用量记录所需的字段。
#[derive(Debug)]
pub struct NewUsage<'a> {
    pub key_id: &'a str,
    pub model: &'a str,
    pub raw_tokens: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cache_tokens: i64,
    pub estimated: bool,
    pub stream: bool,
    pub multiplier_bp: i64,
    pub final_tokens: i64,
    pub credits_scaled: i64,
    pub created_at: i64,
}

fn map_row(row: &SqliteRow) -> UsageRecord {
    UsageRecord {
        id: row.get("id"),
        key_id: row.get("key_id"),
        model: row.get("model"),
        raw_tokens: row.get("raw_tokens"),
        prompt_tokens: row.get("prompt_tokens"),
        completion_tokens: row.get("completion_tokens"),
        cache_tokens: row.get("cache_tokens"),
        estimated: row.get::<i64, _>("estimated") != 0,
        stream: row.get::<i64, _>("stream") != 0,
        multiplier_bp: row.get("multiplier_bp"),
        final_tokens: row.get("final_tokens"),
        credits_scaled: row.get("credits_scaled"),
        status: row.get("status"),
        error: row.get("error"),
        created_at: row.get("created_at"),
    }
}

/// 写入成功用量记录。
///
/// 接受连接而不是连接池，便于调用方把它和扣费放进同一个事务：
/// 要么扣了钱也有记录，要么两者都不发生。
pub async fn insert(
    executor: &mut SqliteConnection,
    usage: &NewUsage<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query(SQL_INSERT)
        .bind(usage.key_id)
        .bind(usage.model)
        .bind(usage.raw_tokens)
        .bind(usage.prompt_tokens)
        .bind(usage.completion_tokens)
        .bind(usage.cache_tokens)
        .bind(i64::from(usage.estimated))
        .bind(i64::from(usage.stream))
        .bind(usage.multiplier_bp)
        .bind(usage.final_tokens)
        .bind(usage.credits_scaled)
        .bind(usage.created_at)
        .execute(executor)
        .await?;
    Ok(())
}

/// 写入一条失败记录。
///
/// 失败也进密钥的用量列表——用户需要知道自己发过的每一次请求去了哪里、
/// 为什么没成功；但错误信息只有一句普通描述，细节在管理员的日志里。
pub async fn insert_error(
    pool: &SqlitePool,
    key_id: &str,
    model: &str,
    error: &str,
    stream: bool,
    created_at: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(SQL_INSERT_ERROR)
        .bind(key_id)
        .bind(model)
        .bind(error)
        .bind(i64::from(stream))
        .bind(created_at)
        .execute(pool)
        .await?;
    Ok(())
}

/// 单次查询能返回的最大记录数。
///
/// 放在数据层是为了给所有调用方兜底：SQLite 的 `LIMIT -1` 表示**不设上限**，
/// 直接把参数透传下去，一个负数就能把整张表拉出来。
pub const MAX_USAGE_LIMIT: i64 = 500;

/// 列出某密钥的用量记录，最近的排在前面。
///
/// `limit` 收敛到 `1..=MAX_USAGE_LIMIT`；`before_id` 为 0 表示从最新开始，
/// 否则只取 id 更小的记录。多取一条判断 `has_more`，调用方按它决定是否
/// 展示「查询更多」。
pub async fn list_by_key(
    pool: &SqlitePool,
    key_id: &str,
    limit: i64,
    before_id: i64,
) -> Result<(Vec<UsageRecord>, bool), sqlx::Error> {
    let clamped = limit.clamp(1, MAX_USAGE_LIMIT);
    let rows = sqlx::query(SQL_LIST_BY_KEY)
        .bind(key_id)
        .bind(before_id)
        .bind(before_id)
        .bind(clamped + 1)
        .fetch_all(pool)
        .await?;
    let has_more = rows.len() as i64 > clamped;
    let records: Vec<UsageRecord> = rows
        .iter()
        .take(clamped as usize)
        .map(map_row)
        .collect();
    Ok((records, has_more))
}

/// 汇总某密钥的累计用量。
pub async fn summary(pool: &SqlitePool, key_id: &str) -> Result<UsageSummary, sqlx::Error> {
    let row = sqlx::query(SQL_SUMMARY)
        .bind(key_id)
        .fetch_one(pool)
        .await?;

    Ok(crate::model::UsageSummary::from_scaled(
        row.get("requests"),
        row.get("failed"),
        row.get("raw_tokens"),
        row.get("final_tokens"),
        row.get("prompt_tokens"),
        row.get("completion_tokens"),
        row.get("cache_tokens"),
        row.get("credits_scaled"),
    ))
}

/// 回收过期记录。
pub async fn purge_before(pool: &SqlitePool, cutoff: i64) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(SQL_PURGE).bind(cutoff).execute(pool).await?;
    Ok(result.rows_affected())
}

/// 从 `since`（含）起的用量汇总。
pub async fn totals_since(
    pool: &SqlitePool,
    since: i64,
) -> Result<crate::model::StatsCounters, sqlx::Error> {
    let row = sqlx::query(SQL_TOTALS).bind(since).fetch_one(pool).await?;
    let credits_scaled = row.get("credits_scaled");

    Ok(crate::model::StatsCounters {
        requests: row.get("requests"),
        failed: row.get("failed"),
        prompt_tokens: row.get("prompt_tokens"),
        completion_tokens: row.get("completion_tokens"),
        cache_tokens: row.get("cache_tokens"),
        credits: crate::model::credits::format_credits(credits_scaled),
        credits_scaled,
    })
}

/// 从 `since`（含）起的按天聚合，按日期升序。
pub async fn trend_since(
    pool: &SqlitePool,
    since: i64,
) -> Result<Vec<crate::model::StatsPoint>, sqlx::Error> {
    let rows = sqlx::query(SQL_TREND).bind(since).fetch_all(pool).await?;

    Ok(rows
        .iter()
        .map(|row| {
            let credits_scaled = row.get("credits_scaled");
            crate::model::StatsPoint {
                date: row.get("day"),
                requests: row.get("requests"),
                failed: row.get("failed"),
                tokens: row.get("tokens"),
                credits: crate::model::credits::format_credits(credits_scaled),
                credits_scaled,
            }
        })
        .collect())
}
