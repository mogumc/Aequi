//! `api_keys` 表的读写。

use sqlx::sqlite::SqliteRow;
use sqlx::{Executor, Row, Sqlite, SqlitePool};

use crate::model::ApiKey;

const SQL_FIND_BY_ID: &str = "\
SELECT id, name, secret, key_prefix, credits_scaled, unlimited, enabled, allowed_models, \
       created_at, updated_at, last_used_at \
FROM api_keys WHERE id = ?";

const SQL_FIND_BY_SECRET: &str = "\
SELECT id, name, secret, key_prefix, credits_scaled, unlimited, enabled, allowed_models, \
       created_at, updated_at, last_used_at \
FROM api_keys WHERE secret = ?";

const SQL_LIST: &str = "\
SELECT id, name, secret, key_prefix, credits_scaled, unlimited, enabled, allowed_models, \
       created_at, updated_at, last_used_at \
FROM api_keys ORDER BY created_at DESC";

/// 分页查询；`search` 非空时按名称或密钥前缀模糊匹配。
const SQL_LIST_PAGED: &str = "\
SELECT id, name, secret, key_prefix, credits_scaled, unlimited, enabled, allowed_models, \
       created_at, updated_at, last_used_at \
FROM api_keys \
WHERE (? IS NULL OR name LIKE '%' || ? || '%' OR key_prefix LIKE '%' || ? || '%') \
ORDER BY created_at DESC, id DESC LIMIT ? OFFSET ?";

/// 与分页查询同口径的匹配总数。
const SQL_COUNT_PAGED: &str = "\
SELECT COUNT(*) FROM api_keys \
WHERE (? IS NULL OR name LIKE '%' || ? || '%' OR key_prefix LIKE '%' || ? || '%')";

/// 密钥总数与启用数。
const SQL_COUNT_ENABLED: &str = "\
SELECT COUNT(*) AS total, COALESCE(SUM(enabled), 0) AS enabled FROM api_keys";

/// 余额偏低的启用密钥计数（与列表同口径）。
const SQL_COUNT_LOW_CREDITS: &str = "\
SELECT COUNT(*) FROM api_keys \
WHERE enabled = 1 AND unlimited = 0 AND credits_scaled < ?";

/// 余额偏低的启用密钥明细，余额低的在前。
const SQL_LIST_LOW_CREDITS: &str = "\
SELECT id, name, credits_scaled FROM api_keys \
WHERE enabled = 1 AND unlimited = 0 AND credits_scaled < ? \
ORDER BY credits_scaled ASC, id ASC LIMIT ?";

const SQL_INSERT: &str = "\
INSERT INTO api_keys \
(id, name, secret, key_prefix, credits_scaled, unlimited, enabled, allowed_models, created_at, updated_at) \
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";

const SQL_UPDATE_META: &str = "\
UPDATE api_keys SET name = ?, enabled = ?, allowed_models = ?, updated_at = ? WHERE id = ?";

const SQL_SET_CREDITS: &str = "UPDATE api_keys SET credits_scaled = ?, updated_at = ? WHERE id = ?";

const SQL_SET_UNLIMITED: &str = "UPDATE api_keys SET unlimited = ?, updated_at = ? WHERE id = ?";

/// 换掉凭证本身：只动 `secret` 与展示前缀，其余字段（余额、白名单、启用状态）原样保留。
///
/// `WHERE id = ?` 用的是主键，SET 里没有 id——**重置不可能改主键**。
/// 用 `RETURNING` 一次拿到写入后的整行（与 [`SQL_CHARGE`] 同一手法）：
/// 「写完再另起一条 SELECT」在两次并发重置之间会读到对方写的那串，
/// 这里返回值恒等于本次写入的内容。
const SQL_RESET_SECRET: &str = "\
UPDATE api_keys SET secret = ?, key_prefix = ?, updated_at = ? WHERE id = ? \
RETURNING id, name, secret, key_prefix, credits_scaled, unlimited, enabled, allowed_models, \
          created_at, updated_at, last_used_at";

/// 原子扣费：`credits_scaled >= ?` 保证不会透支，余额不足时一条都不会命中。
///
/// 用 `RETURNING` 一次拿到扣减后的余额，省掉「先查余额再更新」的竞态与额外往返。
const SQL_CHARGE: &str = "\
UPDATE api_keys SET credits_scaled = credits_scaled - ?, updated_at = ?, last_used_at = ? \
WHERE id = ? AND enabled = 1 AND credits_scaled >= ? \
RETURNING credits_scaled";

/// 无限额度的扣费：不动余额，只记使用时间——用量由调用方照常写进记录表。
const SQL_TOUCH: &str = "\
UPDATE api_keys SET updated_at = ?, last_used_at = ? WHERE id = ? AND enabled = 1 \
RETURNING credits_scaled";

const SQL_DELETE: &str = "DELETE FROM api_keys WHERE id = ?";

/// 写入新密钥所需的字段。
#[derive(Debug)]
pub struct NewApiKey<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub secret: &'a str,
    pub key_prefix: &'a str,
    pub credits_scaled: i64,
    /// 无限额度：扣费跳过余额判断，用量照样记。
    pub unlimited: bool,
    pub enabled: bool,
    pub allowed_models: &'a [String],
    pub created_at: i64,
}

fn map_row(row: &SqliteRow) -> ApiKey {
    ApiKey {
        id: row.get("id"),
        name: row.get("name"),
        secret: row.get("secret"),
        key_prefix: row.get("key_prefix"),
        credits_scaled: row.get("credits_scaled"),
        unlimited: row.get::<i64, _>("unlimited") != 0,
        enabled: row.get::<i64, _>("enabled") != 0,
        allowed_models: parse_models(row.get("allowed_models")),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        last_used_at: row.get("last_used_at"),
    }
}

/// 解析模型白名单。
///
/// 格式错误时按「不限制」处理而不是报错——一条脏配置不该让整个密钥不可用，
/// 但这属于异常情况，仍然留下警告。
fn parse_models(raw: String) -> Vec<String> {
    match serde_json::from_str::<Vec<String>>(&raw) {
        Ok(models) => models,
        Err(_) => {
            tracing::warn!(value = %raw, "密钥的可用模型配置格式非法，按不限制处理");
            Vec::new()
        }
    }
}

fn encode_models(models: &[String]) -> String {
    serde_json::to_string(models).unwrap_or_else(|_| "[]".to_string())
}

/// 按主键查找。
pub async fn find_by_id(pool: &SqlitePool, id: &str) -> Result<Option<ApiKey>, sqlx::Error> {
    let row = sqlx::query(SQL_FIND_BY_ID)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(map_row))
}

/// 按密钥散列查找（鉴权用）。
pub async fn find_by_secret(
    pool: &SqlitePool,
    secret: &str,
) -> Result<Option<ApiKey>, sqlx::Error> {
    let row = sqlx::query(SQL_FIND_BY_SECRET)
        .bind(secret)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(map_row))
}

/// 列出全部密钥，最近创建的排在前面。
pub async fn list(pool: &SqlitePool) -> Result<Vec<ApiKey>, sqlx::Error> {
    let rows = sqlx::query(SQL_LIST).fetch_all(pool).await?;
    Ok(rows.iter().map(map_row).collect())
}

/// 分页列出密钥，最近的排在前面。
pub async fn list_paged(
    pool: &SqlitePool,
    search: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<Vec<ApiKey>, sqlx::Error> {
    let rows = sqlx::query(SQL_LIST_PAGED)
        .bind(search)
        .bind(search)
        .bind(search)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(map_row).collect())
}

/// 与 [`list_paged`] 同口径的匹配总数。
pub async fn count_paged(pool: &SqlitePool, search: Option<&str>) -> Result<i64, sqlx::Error> {
    let row = sqlx::query(SQL_COUNT_PAGED)
        .bind(search)
        .bind(search)
        .bind(search)
        .fetch_one(pool)
        .await?;
    Ok(row.get(0))
}

/// 密钥总数与启用数。
pub async fn count_enabled(pool: &SqlitePool) -> Result<crate::model::ResourceCount, sqlx::Error> {
    let row = sqlx::query(SQL_COUNT_ENABLED).fetch_one(pool).await?;
    Ok(crate::model::ResourceCount {
        total: row.get("total"),
        enabled: row.get("enabled"),
    })
}

/// 余额低于 `threshold` 的启用密钥有多少把。
///
/// 无限额度的密钥不参与余额判定，停用的也不提醒——两者都不该出现在
/// 首页的额度预警里。
pub async fn count_low_credits(pool: &SqlitePool, threshold: i64) -> Result<i64, sqlx::Error> {
    let row = sqlx::query(SQL_COUNT_LOW_CREDITS)
        .bind(threshold)
        .fetch_one(pool)
        .await?;
    Ok(row.get(0))
}

/// 余额最低的几把启用密钥，从低到高——最危险的排在最前面。
pub async fn list_low_credits(
    pool: &SqlitePool,
    threshold: i64,
    limit: i64,
) -> Result<Vec<crate::model::LowCreditKey>, sqlx::Error> {
    let rows = sqlx::query(SQL_LIST_LOW_CREDITS)
        .bind(threshold)
        .bind(limit.clamp(1, 50))
        .fetch_all(pool)
        .await?;

    Ok(rows
        .iter()
        .map(|row| {
            let credits_scaled: i64 = row.get("credits_scaled");
            crate::model::LowCreditKey {
                id: row.get("id"),
                name: row.get("name"),
                credits: crate::model::credits::format_credits(credits_scaled),
                credits_scaled,
            }
        })
        .collect())
}

/// 创建密钥。
///
/// `id` 与 `secret` 都是随机值且有唯一约束。碰撞概率可忽略，处理方式刻意与登录不同：
/// 建密钥是低频的人工操作，真撞上会直接抛出约束冲突、管理员重试一次即可；
/// 而登录是高频自动路径，撞车失败对用户就是「莫名其妙的错误」，所以那里做了自动重试。
pub async fn insert(pool: &SqlitePool, key: &NewApiKey<'_>) -> Result<(), sqlx::Error> {
    sqlx::query(SQL_INSERT)
        .bind(key.id)
        .bind(key.name)
        .bind(key.secret)
        .bind(key.key_prefix)
        .bind(key.credits_scaled)
        .bind(i64::from(key.unlimited))
        .bind(i64::from(key.enabled))
        .bind(encode_models(key.allowed_models))
        .bind(key.created_at)
        .bind(key.created_at)
        .execute(pool)
        .await?;
    Ok(())
}

/// 更新备注名、启用状态与可用模型。
///
/// 积分不在这里改：它有自己的入口，避免和元信息搅在一起。
pub async fn update_meta(
    pool: &SqlitePool,
    id: &str,
    name: &str,
    enabled: bool,
    allowed_models: &[String],
    now: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(SQL_UPDATE_META)
        .bind(name)
        .bind(i64::from(enabled))
        .bind(encode_models(allowed_models))
        .bind(now)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// 直接设定余额（充值或人工校正）。
pub async fn set_credits(
    pool: &SqlitePool,
    id: &str,
    credits_scaled: i64,
    now: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(SQL_SET_CREDITS)
        .bind(credits_scaled)
        .bind(now)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// 读取当前余额。
///
/// 透支结算时用它确定实际扣款额（= 全部剩余）；接受任意 Executor，
/// 与扣费、清零在同一个事务里读，不受并发写干扰。
pub async fn credits_of<'e, E>(executor: E, id: &str) -> Result<i64, sqlx::Error>
where
    E: Executor<'e, Database = Sqlite>,
{
    let value: i64 =
        sqlx::query_scalar("SELECT credits_scaled FROM api_keys WHERE id = ?")
            .bind(id)
            .fetch_one(executor)
            .await?;
    Ok(value)
}

/// 把余额清零。
///
/// 结算发现余额不足时调用：按透支处理——这笔照常记账、客户端照常拿到
/// 响应，但剩余积分全部扣光。不清零的话余额永远停在正数，入口的
/// `credits_scaled <= 0` 检查形同虚设，任何消耗大于余额的密钥都能无限
/// 免费调用。清零之后必须充值才能再次发起请求。
pub async fn zero_credits<'e, E>(executor: E, id: &str, now: i64) -> Result<(), sqlx::Error>
where
    E: Executor<'e, Database = Sqlite>,
{
    sqlx::query(SQL_SET_CREDITS)
        .bind(0)
        .bind(now)
        .bind(id)
        .execute(executor)
        .await?;
    Ok(())
}

/// 扣减余额。
///
/// 返回扣减后的余额；`None` 表示余额不足或密钥已停用，本次未扣除任何积分。
/// 接受任意 Executor，便于和用量记录放进同一个事务。
pub async fn charge<'e, E>(
    executor: E,
    id: &str,
    amount_scaled: i64,
    now: i64,
) -> Result<Option<i64>, sqlx::Error>
where
    E: Executor<'e, Database = Sqlite>,
{
    let row = sqlx::query(SQL_CHARGE)
        .bind(amount_scaled)
        .bind(now)
        .bind(now)
        .bind(id)
        .bind(amount_scaled)
        .fetch_optional(executor)
        .await?;

    Ok(row.map(|row| row.get("credits_scaled")))
}

/// 无限额度的"扣费"：余额不动，只更新使用时间。
///
/// 返回余额原值（供调用方回显）；`None` 表示密钥已停用。
/// 用量记录仍由调用方照常写入——无限额度指的是不设上限，不是不记账。
pub async fn touch<'e, E>(executor: E, id: &str, now: i64) -> Result<Option<i64>, sqlx::Error>
where
    E: Executor<'e, Database = Sqlite>,
{
    let row = sqlx::query(SQL_TOUCH)
        .bind(now)
        .bind(now)
        .bind(id)
        .fetch_optional(executor)
        .await?;

    Ok(row.map(|row| row.get("credits_scaled")))
}

/// 切换无限额度。
pub async fn set_unlimited(
    pool: &SqlitePool,
    id: &str,
    unlimited: bool,
    now: i64,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(SQL_SET_UNLIMITED)
        .bind(i64::from(unlimited))
        .bind(now)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// 重置密钥值。
///
/// 主键不变，所以用量记录、积分与白名单都自然跟着走——换掉的只是凭证。
/// 返回写入后的整行，`None` 表示没有这行（密钥已被删）。
///
/// `secret` 上有唯一约束，撞车会抛出约束冲突：与创建同口径，
/// 这是低频的人工操作，重试一次即可。
pub async fn reset_secret(
    pool: &SqlitePool,
    id: &str,
    secret: &str,
    key_prefix: &str,
    now: i64,
) -> Result<Option<ApiKey>, sqlx::Error> {
    let row = sqlx::query(SQL_RESET_SECRET)
        .bind(secret)
        .bind(key_prefix)
        .bind(now)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(map_row))
}

/// 删除密钥。用量记录随外键级联清理。
pub async fn delete(pool: &SqlitePool, id: &str) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(SQL_DELETE).bind(id).execute(pool).await?;
    Ok(result.rows_affected() > 0)
}
