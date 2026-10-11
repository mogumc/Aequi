//! 模型计费配置的维护。
//!
//! 只管「倍率」这一件事：模型能不能用由上游块决定，
//! 没配过倍率的模型照常可用，只是按默认倍率计费。

use sqlx::SqlitePool;

use crate::db;
use crate::error::{Error, Result};
use crate::model::{credits, ModelConfig, ModelCreate, ModelUpdate};
use crate::utils::time;

/// 模型名长度上限，与用户名同一口径。
const MAX_NAME_LEN: usize = 128;

/// 列出全部模型配置。
pub async fn list(pool: &SqlitePool) -> Result<Vec<ModelConfig>> {
    Ok(db::models::list(pool).await?)
}

/// 创建模型配置。
///
/// 重名直接报冲突：这类同名不同价的存在只会让计费变得不可解释。
pub async fn create(pool: &SqlitePool, payload: ModelCreate) -> Result<ModelConfig> {
    let name = payload.name.trim();
    if name.is_empty() {
        return Err(Error::BadRequest("模型名不能为空".to_string()));
    }
    if name.chars().count() > MAX_NAME_LEN {
        return Err(Error::BadRequest(format!(
            "模型名不能超过 {MAX_NAME_LEN} 个字符"
        )));
    }

    let multiplier_bp = match payload.multiplier.as_deref() {
        Some(text) => parse_multiplier(text)?,
        None => credits::DEFAULT_MULTIPLIER_BP,
    };

    if db::models::find(pool, name).await?.is_some() {
        return Err(Error::BadRequest(format!("模型 {name} 已存在")));
    }

    db::models::insert(pool, name, multiplier_bp, true, time::now_secs()).await?;

    db::models::find(pool, name)
        .await?
        .ok_or_else(|| Error::Internal(anyhow::anyhow!("模型创建后读不回来")))
}

/// 更新倍率或启用状态。未提交的字段保持原值。
pub async fn update(pool: &SqlitePool, name: &str, patch: ModelUpdate) -> Result<ModelConfig> {
    let current = db::models::find(pool, name)
        .await?
        .ok_or_else(|| Error::NotFound(format!("模型 {name} 不存在")))?;

    let multiplier_bp = match patch.multiplier.as_deref() {
        Some(text) => parse_multiplier(text)?,
        None => current.multiplier_bp,
    };
    let enabled = patch.enabled.unwrap_or(current.enabled);

    db::models::update(pool, name, multiplier_bp, enabled, time::now_secs()).await?;

    db::models::find(pool, name)
        .await?
        .ok_or_else(|| Error::Internal(anyhow::anyhow!("模型更新后读不回来")))
}

/// 删除配置。
///
/// 删掉之后该模型回落到默认倍率——它本来就不是「模型是否存在」的记录，
/// 所以删除不会让模型不可用。
pub async fn delete(pool: &SqlitePool, name: &str) -> Result<()> {
    if !db::models::delete(pool, name).await? {
        return Err(Error::NotFound(format!("模型 {name} 不存在")));
    }
    Ok(())
}

/// 解析倍率字符串。
fn parse_multiplier(text: &str) -> Result<i64> {
    credits::parse_multiplier(text)
        .ok_or_else(|| Error::BadRequest("倍率格式不正确（四位小数以内的非负数）".to_string()))
}
