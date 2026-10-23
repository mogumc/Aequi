//! 上游块与密钥的读写。
//!
//! 块是调度单位：一种格式、一个出口（URL + 可选代理）、一组密钥。
//! 密钥的生命周期管理（增删、暂停、停用、恢复、退避）都落在这里，
//! 「什么时候用哪把」的决策在 service 层。

use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqliteConnection, SqlitePool};

use crate::model::{ApiFormat, HeaderEntry, KeyStatus, ModelEntry, UpstreamBlock, UpstreamKey};

fn map_block(row: &SqliteRow) -> UpstreamBlock {
    UpstreamBlock {
        id: row.get("id"),
        name: row.get("name"),
        api_format: ApiFormat::parse(&row.get::<String, _>("api_format"))
            .unwrap_or(ApiFormat::Openai),
        weight: row.get::<i64, _>("weight") as u32,
        // 库被手改出负数时按 0（不限制）读——它随后会撞上写入口的校验，
        // 而不是变成一个巨大的无符号数把信号量撑爆。
        max_concurrency: row.get::<i64, _>("max_concurrency").max(0) as u32,
        base_url: row.get("base_url"),
        proxy: row.get("proxy"),
        models: parse_models(row.get("models")),
        headers: parse_headers(row.get("headers")),
        enabled: row.get::<i64, _>("enabled") != 0,
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

/// 模型列表以 JSON 存储，损坏时退回空列表——块还在，只是暂时不接模型。
fn parse_models(raw: String) -> Vec<ModelEntry> {
    serde_json::from_str::<Vec<ModelEntry>>(&raw).unwrap_or_default()
}

/// 自定义请求头同样以 JSON 存储，损坏时退回空列表——宁可少带几个头，
/// 也不该让整个块（连同它的全部密钥）因为一行坏数据就取不出来。
///
/// 读成 `Option<String>` 而不是 `String`：这一列是 `NOT NULL DEFAULT '[]'`，
/// 正常读不到 NULL。留着这条容错是上一条原则的延伸——真碰上（库被人手改过、
/// 或以后把列改成可空）也只是当作「没有自定义头」，而不是在 map_block 里
/// 解码失败把整个 list_blocks 炸掉。
fn parse_headers(raw: Option<String>) -> Vec<HeaderEntry> {
    raw.and_then(|text| serde_json::from_str::<Vec<HeaderEntry>>(&text).ok())
        .unwrap_or_default()
}

fn map_key(row: &SqliteRow) -> UpstreamKey {
    UpstreamKey {
        id: row.get("id"),
        block_id: row.get("block_id"),
        secret: row.get("secret"),
        status: KeyStatus::parse(&row.get::<String, _>("status")).unwrap_or(KeyStatus::Active),
        never_disable: row.get::<i64, _>("never_disable") != 0,
        weight: row.get::<i64, _>("weight") as u32,
        failure_count: row.get::<i64, _>("failure_count") as u32,
        cooldown_until: row.get("cooldown_until"),
        last_error: row.get("last_error"),
        last_used_at: row.get("last_used_at"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

/// 写入一个块。
pub async fn insert_block(pool: &SqlitePool, block: &UpstreamBlock) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO upstream_blocks \
         (id, name, api_format, weight, max_concurrency, base_url, proxy, models, headers, enabled, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&block.id)
    .bind(&block.name)
    .bind(block.api_format.as_str())
    .bind(block.weight as i64)
    .bind(block.max_concurrency as i64)
    .bind(&block.base_url)
    .bind(&block.proxy)
    .bind(serde_json::to_string(&block.models).unwrap_or_else(|_| "[]".to_string()))
    .bind(serde_json::to_string(&block.headers).unwrap_or_else(|_| "[]".to_string()))
    .bind(i64::from(block.enabled))
    .bind(block.created_at)
    .bind(block.updated_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// 整体更新一个块（service 层先把字段合并好）。
pub async fn update_block(pool: &SqlitePool, block: &UpstreamBlock) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE upstream_blocks \
         SET name = ?, api_format = ?, weight = ?, max_concurrency = ?, base_url = ?, proxy = ?, models = ?, headers = ?, enabled = ?, updated_at = ? \
         WHERE id = ?",
    )
    .bind(&block.name)
    .bind(block.api_format.as_str())
    .bind(block.weight as i64)
    .bind(block.max_concurrency as i64)
    .bind(&block.base_url)
    .bind(&block.proxy)
    .bind(serde_json::to_string(&block.models).unwrap_or_else(|_| "[]".to_string()))
    .bind(serde_json::to_string(&block.headers).unwrap_or_else(|_| "[]".to_string()))
    .bind(i64::from(block.enabled))
    .bind(block.updated_at)
    .bind(&block.id)
    .execute(pool)
    .await?;
    Ok(())
}

/// 按 ID 查块。
const SQL_FIND_BLOCK: &str = "\
SELECT id, name, api_format, weight, max_concurrency, base_url, proxy, models, headers, enabled, created_at, updated_at \
FROM upstream_blocks WHERE id = ?";

/// 全部块。
const SQL_LIST_BLOCKS: &str = "\
SELECT id, name, api_format, weight, max_concurrency, base_url, proxy, models, headers, enabled, created_at, updated_at \
FROM upstream_blocks ORDER BY created_at, id";

/// 某块的全部密钥。
const SQL_LIST_KEYS: &str = "\
SELECT id, block_id, secret, status, never_disable, weight, failure_count, cooldown_until, last_error, \
last_used_at, created_at, updated_at \
FROM upstream_keys WHERE block_id = ? ORDER BY created_at, id";

/// 按块与密钥 ID 查一把密钥。
const SQL_FIND_KEY: &str = "\
SELECT id, block_id, secret, status, never_disable, weight, failure_count, cooldown_until, last_error, \
last_used_at, created_at, updated_at \
FROM upstream_keys WHERE block_id = ? AND id = ?";

/// 块总数与启用数。
const SQL_COUNT_ENABLED: &str = "\
SELECT COUNT(*) AS total, COALESCE(SUM(enabled), 0) AS enabled FROM upstream_blocks";

/// 按 ID 查块。
pub async fn find_block(pool: &SqlitePool, id: &str) -> Result<Option<UpstreamBlock>, sqlx::Error> {
    let row = sqlx::query(SQL_FIND_BLOCK)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(map_block))
}

/// 全部块，按创建顺序。
pub async fn list_blocks(pool: &SqlitePool) -> Result<Vec<UpstreamBlock>, sqlx::Error> {
    let rows = sqlx::query(SQL_LIST_BLOCKS).fetch_all(pool).await?;
    Ok(rows.iter().map(map_block).collect())
}

/// 删除块。密钥随外键级联清理。
pub async fn delete_block(pool: &SqlitePool, id: &str) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("DELETE FROM upstream_blocks WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// 列出某块的全部密钥。
pub async fn list_keys(pool: &SqlitePool, block_id: &str) -> Result<Vec<UpstreamKey>, sqlx::Error> {
    let rows = sqlx::query(SQL_LIST_KEYS)
        .bind(block_id)
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(map_key).collect())
}

/// 按块与密钥 ID 查一把密钥。
pub async fn find_key(
    pool: &SqlitePool,
    block_id: &str,
    key_id: &str,
) -> Result<Option<UpstreamKey>, sqlx::Error> {
    let row = sqlx::query(SQL_FIND_KEY)
        .bind(block_id)
        .bind(key_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(map_key))
}

/// 批量写入密钥。
///
/// 接受连接而非连接池：调用方应当包在事务里，保证要么全进要么全不进。
pub async fn insert_keys(
    executor: &mut SqliteConnection,
    entries: &[(String, String)],
    block_id: &str,
    weight: u32,
    never_disable: bool,
    now: i64,
) -> Result<(), sqlx::Error> {
    for (id, secret) in entries {
        sqlx::query(
            "INSERT INTO upstream_keys \
             (id, block_id, secret, status, never_disable, weight, created_at, updated_at) \
             VALUES (?, ?, ?, 'active', ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(block_id)
        .bind(secret)
        .bind(i64::from(never_disable))
        .bind(weight as i64)
        .bind(now)
        .bind(now)
        .execute(&mut *executor)
        .await?;
    }
    Ok(())
}

/// 删除一批密钥，返回删除的行数。
pub async fn delete_keys(
    pool: &SqlitePool,
    block_id: &str,
    ids: &[String],
) -> Result<u64, sqlx::Error> {
    let mut total = 0;
    for id in ids {
        let result = sqlx::query("DELETE FROM upstream_keys WHERE block_id = ? AND id = ?")
            .bind(block_id)
            .bind(id)
            .execute(pool)
            .await?;
        total += result.rows_affected();
    }
    Ok(total)
}

/// 改密钥状态（暂停 / 恢复 / 停用）。
///
/// 带上 block_id 一起匹配：密钥只应通过它所属的块来操作。
/// 恢复的同时清掉失败计数与错误信息——重新上岗就该干净地上岗。
pub async fn set_key_status(
    pool: &SqlitePool,
    block_id: &str,
    key_id: &str,
    status: KeyStatus,
    now: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE upstream_keys \
         SET status = ?, \
             failure_count = CASE WHEN ? = 'active' THEN 0 ELSE failure_count END, \
             cooldown_until = CASE WHEN ? = 'active' THEN NULL ELSE cooldown_until END, \
             last_error = CASE WHEN ? = 'active' THEN NULL ELSE last_error END, \
             updated_at = ? \
         WHERE id = ? AND block_id = ?",
    )
    .bind(status.as_str())
    .bind(status.as_str())
    .bind(status.as_str())
    .bind(status.as_str())
    .bind(now)
    .bind(key_id)
    .bind(block_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// 更新密钥的运行时状态（失败计数、退避、错误信息）。
pub async fn update_key_runtime(
    executor: &mut SqliteConnection,
    key_id: &str,
    failure_count: u32,
    cooldown_until: Option<i64>,
    last_error: Option<String>,
    now: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE upstream_keys \
         SET failure_count = ?, cooldown_until = ?, last_error = ?, updated_at = ? \
         WHERE id = ?",
    )
    .bind(failure_count as i64)
    .bind(cooldown_until)
    .bind(last_error)
    .bind(now)
    .bind(key_id)
    .execute(&mut *executor)
    .await?;
    Ok(())
}

/// 只记一条错误信息，不动失败计数与退避。
///
/// 给「上游自己的毛病」用：后台要看得见最近一次失败的原因，但那不是这把
/// 钥匙的账——计数别涨、退避别挂，否则一个坏掉的上游能把整块钥匙按住。
pub async fn note_key_error(
    pool: &SqlitePool,
    key_id: &str,
    last_error: &str,
    now: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE upstream_keys SET last_error = ?, updated_at = ? WHERE id = ?")
        .bind(last_error)
        .bind(now)
        .bind(key_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// 挂上一次退避：只写退避截止与原因，不动失败计数。
///
/// 失败计数记的是「密钥被拒」（401/403），限流的连续次数活在进程内
/// （见 `service::upstream::UpstreamState::bump_rate_limit`）。两者混在一起，
/// 连吃几次限流再挨一次 401 就够把钥匙停用——负载问题不该替密钥有效性作证。
pub async fn note_key_cooldown(
    pool: &SqlitePool,
    key_id: &str,
    cooldown_until: i64,
    last_error: &str,
    now: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE upstream_keys \
         SET cooldown_until = ?, last_error = ?, updated_at = ? \
         WHERE id = ?",
    )
    .bind(cooldown_until)
    .bind(last_error)
    .bind(now)
    .bind(key_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// 设置密钥的「永不暂停」开关（免自动停用）。
///
/// 带上 block_id 一起匹配：密钥只应通过它所属的块来操作。
/// 只改这一个开关，不碰状态与计数——它是叠加在生命周期规则上的豁免，
/// 不是一次状态变更。
pub async fn set_key_never_disable(
    pool: &SqlitePool,
    block_id: &str,
    key_id: &str,
    enabled: bool,
    now: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE upstream_keys SET never_disable = ?, updated_at = ? WHERE id = ? AND block_id = ?",
    )
    .bind(i64::from(enabled))
    .bind(now)
    .bind(key_id)
    .bind(block_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// 设置密钥的调度权重，用于块内平滑加权轮询。
///
/// 与 [`set_key_never_disable`] 同一形状：带上 `block_id` 匹配，
/// 密钥只应通过它所属的块来操作。权重每次调度都从库里读，
/// 所以改完即刻生效，没有进程内的缓存要清。
/// 合法区间由服务层把关（`<= 0` 一律拒），这里只管写。
pub async fn set_key_weight(
    pool: &SqlitePool,
    block_id: &str,
    key_id: &str,
    weight: u32,
    now: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE upstream_keys SET weight = ?, updated_at = ? WHERE id = ? AND block_id = ?",
    )
    .bind(weight as i64)
    .bind(now)
    .bind(key_id)
    .bind(block_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// 记录一次成功使用：清空失败计数、写最近使用时间。
pub async fn mark_key_success(
    pool: &SqlitePool,
    key_id: &str,
    now: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE upstream_keys \
         SET failure_count = 0, cooldown_until = NULL, last_error = NULL, last_used_at = ?, updated_at = ? \
         WHERE id = ?",
    )
    .bind(now)
    .bind(now)
    .bind(key_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// 判断一个数据库错误是否为唯一约束冲突。
pub fn is_unique_violation(error: &sqlx::Error) -> bool {
    matches!(
        error,
        sqlx::Error::Database(database)
            if database.kind() == sqlx::error::ErrorKind::UniqueViolation
    )
}

/// 块总数与启用数。
pub async fn count_enabled(pool: &SqlitePool) -> Result<crate::model::ResourceCount, sqlx::Error> {
    let row = sqlx::query(SQL_COUNT_ENABLED).fetch_one(pool).await?;
    Ok(crate::model::ResourceCount {
        total: row.get("total"),
        enabled: row.get("enabled"),
    })
}
