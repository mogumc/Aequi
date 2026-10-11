//! `usage_records` 表的读写。

use sqlx::sqlite::{SqliteConnection, SqliteRow};
use sqlx::{Row, SqlitePool};

use crate::model::{UsageRecord, UsageSummary};

const SQL_INSERT: &str = "\
INSERT INTO usage_records \
(key_id, model, raw_tokens, multiplier_bp, final_tokens, credits_scaled, created_at) \
VALUES (?, ?, ?, ?, ?, ?, ?)";

const SQL_LIST_BY_KEY: &str = "\
SELECT id, key_id, model, raw_tokens, multiplier_bp, final_tokens, credits_scaled, created_at \
FROM usage_records WHERE key_id = ? ORDER BY created_at DESC, id DESC LIMIT ?";

const SQL_SUMMARY: &str = "\
SELECT COUNT(*) AS requests, \
       COALESCE(SUM(raw_tokens), 0)     AS raw_tokens, \
       COALESCE(SUM(final_tokens), 0)   AS final_tokens, \
       COALESCE(SUM(credits_scaled), 0) AS credits_scaled \
FROM usage_records WHERE key_id = ?";

/// 写入一条用量记录所需的字段。
#[derive(Debug)]
pub struct NewUsage<'a> {
    pub key_id: &'a str,
    pub model: &'a str,
    pub raw_tokens: i64,
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
        multiplier_bp: row.get("multiplier_bp"),
        final_tokens: row.get("final_tokens"),
        credits_scaled: row.get("credits_scaled"),
        created_at: row.get("created_at"),
    }
}

/// 写入用量记录。
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
        .bind(usage.multiplier_bp)
        .bind(usage.final_tokens)
        .bind(usage.credits_scaled)
        .bind(usage.created_at)
        .execute(executor)
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
/// `limit` 会被收敛到 `1..=MAX_USAGE_LIMIT`，无论调用方传什么。
pub async fn list_by_key(
    pool: &SqlitePool,
    key_id: &str,
    limit: i64,
) -> Result<Vec<UsageRecord>, sqlx::Error> {
    let rows = sqlx::query(SQL_LIST_BY_KEY)
        .bind(key_id)
        .bind(limit.clamp(1, MAX_USAGE_LIMIT))
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(map_row).collect())
}

/// 汇总某密钥的累计用量。
pub async fn summary(pool: &SqlitePool, key_id: &str) -> Result<UsageSummary, sqlx::Error> {
    let row = sqlx::query(SQL_SUMMARY).bind(key_id).fetch_one(pool).await?;

    Ok(UsageSummary::from_scaled(
        row.get("requests"),
        row.get("raw_tokens"),
        row.get("final_tokens"),
        row.get("credits_scaled"),
    ))
}
