//! 计费：把一次调用的 token 用量折算成积分并落账。

use sqlx::SqlitePool;

use crate::db;
use crate::db::usage::NewUsage;
use crate::error::{Error, Result};
use crate::model::{credits, ApiKey};
use crate::utils::time;

/// 试算结果。
#[derive(Debug, Clone, Copy)]
pub struct Quote {
    pub multiplier_bp: i64,
    pub final_tokens: i64,
    pub credits_scaled: i64,
    /// 当前余额是否够付。
    pub affordable: bool,
}

/// 一次记账的结果。
#[derive(Debug, Clone)]
pub struct ChargeOutcome {
    pub model: String,
    pub raw_tokens: i64,
    pub multiplier_bp: i64,
    pub final_tokens: i64,
    pub credits_scaled: i64,
    /// 扣减后的余额。
    pub remaining_scaled: i64,
}

/// 取模型倍率；表里没有该模型时用默认倍率。
///
/// 倍率的维护接口属于模型模块，本轮只消费它。
pub async fn multiplier_bp(pool: &SqlitePool, model: &str) -> Result<i64> {
    Ok(db::models::find_multiplier_bp(pool, model)
        .await?
        .unwrap_or(credits::DEFAULT_MULTIPLIER_BP))
}

/// 试算：只算不落账。
///
/// 便于调用方在真正发起上游请求前先确认余额够不够。
pub async fn quote(pool: &SqlitePool, key: &ApiKey, model: &str, raw_tokens: i64) -> Result<Quote> {
    ensure_usable(key, model)?;

    let multiplier_bp = multiplier_bp(pool, model).await?;
    let credits_scaled = credits::cost_scaled(raw_tokens, multiplier_bp);

    Ok(Quote {
        multiplier_bp,
        final_tokens: credits::final_tokens(raw_tokens, multiplier_bp),
        credits_scaled,
        affordable: key.credits_scaled >= credits_scaled,
    })
}

/// 记账：校验 → 折算 → 扣费 → 写用量。
///
/// 扣费与写记录放在同一个事务里，不会出现「扣了钱没账单」或反过来的情况。
pub async fn charge(
    pool: &SqlitePool,
    key: &ApiKey,
    model: &str,
    raw_tokens: i64,
) -> Result<ChargeOutcome> {
    ensure_usable(key, model)?;

    if raw_tokens < 0 {
        return Err(Error::BadRequest("token 用量不能为负".to_string()));
    }

    let multiplier_bp = multiplier_bp(pool, model).await?;
    let final_tokens = credits::final_tokens(raw_tokens, multiplier_bp);
    let credits_scaled = credits::cost_scaled(raw_tokens, multiplier_bp);
    let now = time::now_secs();

    let mut tx = pool.begin().await?;

    // 扣费语句自带余额判断，余额不足时一条都不会命中；
    // 此时事务没有任何改动，直接返回错误即可回滚。
    let remaining_scaled = db::keys::charge(&mut *tx, &key.id, credits_scaled, now)
        .await?
        .ok_or(Error::InsufficientCredits)?;

    db::usage::insert(
        &mut *tx,
        &NewUsage {
            key_id: &key.id,
            model,
            raw_tokens,
            multiplier_bp,
            final_tokens,
            credits_scaled,
            created_at: now,
        },
    )
    .await?;

    tx.commit().await?;

    Ok(ChargeOutcome {
        model: model.to_string(),
        raw_tokens,
        multiplier_bp,
        final_tokens,
        credits_scaled,
        remaining_scaled,
    })
}

/// 校验密钥本身可用，且允许调用该模型。
fn ensure_usable(key: &ApiKey, model: &str) -> Result<()> {
    if !key.enabled {
        return Err(Error::Forbidden("该密钥已被停用".to_string()));
    }
    if !key.allows_model(model) {
        return Err(Error::Forbidden(format!("该密钥无权使用模型 {model}")));
    }
    Ok(())
}
