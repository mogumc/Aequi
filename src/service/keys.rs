//! 密钥与积分的业务逻辑。
//!
//! 密钥是独立于管理员账号的另一套凭证：它只能查询自己，不具备任何管理能力。
//! 相关数据全部落在数据面库（[`crate::db::Store::data`]），与管理员数据物理分开。

use sqlx::SqlitePool;

use crate::db;
use crate::db::keys::NewApiKey;
use crate::error::{Error, Result};
use crate::model::{
    ApiKey, KeyCreate, KeyCreditsUpdate, KeyUpdate, UsageRecord, UsageSummary, credits,
};
use crate::utils::{random, time};

/// 密钥前缀长度，含 `ak_`。
const PREFIX_LEN: usize = 11;
/// 密钥随机部分长度。
const SECRET_LEN: usize = 40;
/// 密钥内部 ID 长度。
const ID_LEN: usize = 16;

/// 生成一把新密钥。明文只在创建时返回这一次。
pub fn generate() -> String {
    format!("ak_{}", random::string(SECRET_LEN))
}

/// 展示用前缀，便于在列表里辨认。
fn prefix_of(key: &str) -> String {
    key.chars().take(PREFIX_LEN).collect()
}

/// 创建密钥。
pub async fn create(pool: &SqlitePool, payload: KeyCreate) -> Result<ApiKey> {
    let plaintext = generate();
    let id = random::string(ID_LEN);

    let credits_scaled = match payload.credits.as_deref() {
        Some(text) => parse_credits(text)?,
        None => 0,
    };

    db::keys::insert(
        pool,
        &NewApiKey {
            id: &id,
            name: payload.name.trim(),
            secret: &plaintext,
            key_prefix: &prefix_of(&plaintext),
            credits_scaled,
            unlimited: payload.unlimited,
            enabled: true,
            allowed_models: &payload.allowed_models,
            created_at: time::now_secs(),
        },
    )
    .await?;

    get(pool, &id).await
}

/// 按 ID 取密钥。
pub async fn get(pool: &SqlitePool, id: &str) -> Result<ApiKey> {
    db::keys::find_by_id(pool, id)
        .await?
        .ok_or_else(|| Error::NotFound("密钥不存在".to_string()))
}

/// 列出全部密钥。
pub async fn list(pool: &SqlitePool) -> Result<Vec<ApiKey>> {
    Ok(db::keys::list(pool).await?)
}

/// 分页查询的默认页大小与上限。
const DEFAULT_PAGE_SIZE: i64 = 20;
const MAX_PAGE_SIZE: i64 = 100;

/// 单页密钥。
#[derive(Debug, Clone)]
pub struct KeyPage {
    pub keys: Vec<ApiKey>,
    pub total: i64,
    pub page: i64,
    pub page_size: i64,
}

/// 分页列出密钥，可按名称或密钥前缀搜索。
///
/// `page` 从 1 开始；页大小收敛到 `1..=MAX_PAGE_SIZE`，无论调用方传什么。
pub async fn list_page(
    pool: &SqlitePool,
    search: Option<&str>,
    page: Option<i64>,
    page_size: Option<i64>,
) -> Result<KeyPage> {
    let page_size = page_size
        .unwrap_or(DEFAULT_PAGE_SIZE)
        .clamp(1, MAX_PAGE_SIZE);
    let page = page.unwrap_or(1).max(1);
    let offset = (page - 1) * page_size;

    Ok(KeyPage {
        keys: db::keys::list_paged(pool, search, page_size, offset).await?,
        total: db::keys::count_paged(pool, search).await?,
        page,
        page_size,
    })
}

/// 更新元信息。未提交的字段保持原值。
pub async fn update(pool: &SqlitePool, id: &str, patch: KeyUpdate) -> Result<ApiKey> {
    let current = get(pool, id).await?;

    let KeyUpdate {
        name,
        enabled,
        allowed_models,
    } = patch;

    let name = name.unwrap_or(current.name);
    let enabled = enabled.unwrap_or(current.enabled);
    let allowed_models = allowed_models.unwrap_or(current.allowed_models);

    db::keys::update_meta(
        pool,
        id,
        name.trim(),
        enabled,
        &allowed_models,
        time::now_secs(),
    )
    .await?;

    get(pool, id).await
}

/// 设定余额（充值或人工校正）。
/// 调整额度设置：余额与无限开关，只改传了的那部分。
pub async fn set_credits(pool: &SqlitePool, id: &str, patch: KeyCreditsUpdate) -> Result<ApiKey> {
    if patch.credits.is_none() && patch.unlimited.is_none() {
        return Err(Error::BadRequest(
            "至少要提交 credits 或 unlimited 之一".to_string(),
        ));
    }

    // 不存在就提前报错，避免静默更新 0 行。
    get(pool, id).await?;

    if let Some(text) = patch.credits.as_deref() {
        let credits_scaled = parse_credits(text)?;
        db::keys::set_credits(pool, id, credits_scaled, time::now_secs()).await?;
    }

    if let Some(unlimited) = patch.unlimited {
        db::keys::set_unlimited(pool, id, unlimited, time::now_secs()).await?;
    }

    get(pool, id).await
}

/// 重置密钥值。
///
/// 主键不变：额度、白名单、启用状态、用量记录都留在原处，换掉的只是凭证本身。
/// 旧密钥在这一刻起立刻失效（鉴权按 `secret` 精确匹配），新明文随返回值给出。
///
/// 返回值直接取自 `UPDATE ... RETURNING`，就是本次写入的那一行——
/// 不「写完再查」，因此不存在读到并发重置结果的窗口。
pub async fn reset(pool: &SqlitePool, id: &str) -> Result<ApiKey> {
    let plaintext = generate();
    let prefix = prefix_of(&plaintext);

    db::keys::reset_secret(pool, id, &plaintext, &prefix, time::now_secs())
        .await?
        .ok_or_else(|| Error::NotFound("密钥不存在".to_string()))
}

/// 删除密钥。用量记录随外键级联清理。
pub async fn delete(pool: &SqlitePool, id: &str) -> Result<()> {
    if !db::keys::delete(pool, id).await? {
        return Err(Error::NotFound("密钥不存在".to_string()));
    }
    Ok(())
}

/// 校验密钥并返回记录。
///
/// 这里只读不写：密钥的「使用」以产生计费请求为准，
/// 单纯查询自己的信息不该刷新最近使用时间。
pub async fn authenticate(pool: &SqlitePool, plaintext: &str) -> Result<ApiKey> {
    let key = db::keys::find_by_secret(pool, plaintext)
        .await?
        .ok_or_else(|| Error::Unauthorized("密钥无效".to_string()))?;

    if !key.enabled {
        return Err(Error::Forbidden("该密钥已被停用".to_string()));
    }

    Ok(key)
}

/// 某密钥的用量记录（游标分页，`before_id` 为 0 表示从最新开始）。
pub async fn usage(
    pool: &SqlitePool,
    key_id: &str,
    limit: i64,
    before_id: i64,
) -> Result<(Vec<UsageRecord>, bool)> {
    Ok(db::usage::list_by_key(pool, key_id, limit, before_id).await?)
}

/// 某密钥的用量汇总。
pub async fn usage_summary(pool: &SqlitePool, key_id: &str) -> Result<UsageSummary> {
    Ok(db::usage::summary(pool, key_id).await?)
}

/// 解析积数字符串，格式错误给出可读提示。
fn parse_credits(text: &str) -> Result<i64> {
    credits::parse_credits(text)
        .ok_or_else(|| Error::BadRequest("积分格式非法，最多四位小数".to_string()))
}
