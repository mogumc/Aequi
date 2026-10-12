//! 计费：把一次调用的 token 用量折算成积分并落账。

use sqlx::SqlitePool;

use crate::db;
use crate::db::usage::NewUsage;
use crate::error::{Error, Result};
use crate::model::{ApiKey, credits};
use crate::proxy::scheme::Usage;
use crate::utils::time;

/// 一次记账的结果。
#[derive(Debug, Clone)]
pub struct ChargeOutcome {
    pub model: String,
    /// 上游报（或本地估算）的用量。
    pub usage: Usage,
    pub multiplier_bp: i64,
    pub final_tokens: i64,
    pub credits_scaled: i64,
    /// 扣减后的余额。
    pub remaining_scaled: i64,
}

/// 记账：校验 → 折算 → 扣费 → 写用量。
///
/// 倍率由调用方传入——它是**块内模型条目**的属性，只有调度层知道这次用了哪个块。
/// 扣费与写记录放在同一个事务里，不会出现「扣了钱没账单」或反过来的情况。
/// 用量是完整的分解（输入/输出/缓存），缓存部分只记录、不参与折扣。
pub async fn charge(
    pool: &SqlitePool,
    key: &ApiKey,
    // 实际命中并已向上游发起请求的块——白名单按「块 + 模型」判定，
    // 所以这里必须知道是哪个块，光有模型名无法确认这次调用是否被授权。
    block_id: &str,
    model: &str,
    usage: Usage,
    multiplier_bp: i64,
    stream: bool,
) -> Result<ChargeOutcome> {
    ensure_usable(key, block_id, model)?;

    if usage.total_tokens < 0 {
        return Err(Error::BadRequest("token 用量不能为负".to_string()));
    }

    let final_tokens = credits::final_tokens(usage.total_tokens, multiplier_bp);
    let credits_scaled = credits::cost_scaled(usage.total_tokens, multiplier_bp);
    let now = time::now_secs();

    let mut tx = pool.begin().await?;

    // 无限额度的密钥不走余额判定：只更新使用时间，消耗照记进用量表。
    // 有限额度的走原子扣费——语句自带余额与启用状态判断，
    // 不满足时一条都不会命中，此时事务没有任何改动，回滚即可；
    // 但必须把「已停用」和「余额不足」区分开，
    // 否则调用方拿到的错误会指向完全错误的方向。
    let Some(remaining_scaled) = (if key.unlimited {
        db::keys::touch(&mut *tx, &key.id, now).await?
    } else {
        db::keys::charge(&mut *tx, &key.id, credits_scaled, now).await?
    }) else {
        let failure = classify_charge_failure(pool, &key.id).await;
        // 余额不足的那笔消费收不回账，剩余零头也一并作废（清零）——
        // 否则余额永远停在正数，入口检查形同虚设，任何消耗大于余额的
        // 密钥都能无限免费调用。清零后必须充值才能再次发起请求。
        if matches!(failure, Error::InsufficientCredits) {
            if let Err(error) = db::keys::zero_credits(pool, &key.id, now).await {
                tracing::warn!(key_id = %key.id, error = %error, "余额不足清零失败");
            }
        }
        return Err(failure);
    };

    db::usage::insert(
        &mut *tx,
        &NewUsage {
            key_id: &key.id,
            model,
            raw_tokens: usage.total_tokens,
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            cache_tokens: usage.cache_tokens,
            estimated: usage.estimated,
            stream,
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
        usage,
        multiplier_bp,
        final_tokens,
        credits_scaled,
        remaining_scaled,
    })
}

/// 校验密钥本身可用，且允许调用该块的该模型。
fn ensure_usable(key: &ApiKey, block_id: &str, model: &str) -> Result<()> {
    if !key.enabled {
        return Err(Error::Forbidden("该密钥已被停用".to_string()));
    }
    if !key.allows_capability(block_id, model) {
        return Err(Error::Forbidden(format!(
            "该密钥无权使用模型 {model}（块 {block_id}）"
        )));
    }
    Ok(())
}

/// 扣费未命中时回查一次密钥状态，给出准确的失败原因。
///
/// 传进来的 `key` 可能是稍早读到的快照，期间管理员完全可能刚把它停用或删除，
/// 所以这里以库里的当前状态为准。
async fn classify_charge_failure(pool: &SqlitePool, key_id: &str) -> Error {
    match db::keys::find_by_id(pool, key_id).await {
        Ok(Some(current)) if !current.enabled => Error::Forbidden("该密钥已被停用".to_string()),
        Ok(Some(_)) => Error::InsufficientCredits,
        Ok(None) => Error::NotFound("密钥不存在".to_string()),
        Err(error) => error.into(),
    }
}
