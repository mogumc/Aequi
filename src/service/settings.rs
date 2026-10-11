//! 后台设置的读取与更新。

use sqlx::SqlitePool;

use crate::config::{Settings, SettingsPatch};
use crate::db;
use crate::error::{Error, Result};
use crate::utils::time;

/// 读取后台设置。
///
/// 数据库里还没有设置项时（首次启动）导入默认值；有个别缺失项时补齐，
/// 保证后台看到的始终是一组完整设置。
pub async fn ensure(pool: &SqlitePool) -> Result<Settings> {
    let stored = db::settings::load_all(pool).await?;

    if stored.is_empty() {
        let defaults = Settings::default();
        db::settings::upsert_many(pool, &defaults.to_entries(), time::now_secs()).await?;
        tracing::info!("数据库中尚无设置项，已导入默认值");
        return Ok(defaults);
    }

    let mut settings = Settings::default();
    settings.apply(&stored);

    let missing: Vec<(&str, String)> = settings
        .to_entries()
        .into_iter()
        .filter(|(key, _)| !stored.contains_key(*key))
        .collect();

    if !missing.is_empty() {
        tracing::info!(count = missing.len(), "补齐缺失的设置项");
        db::settings::upsert_many(pool, &missing, time::now_secs()).await?;
    }

    Ok(settings)
}

/// 应用部分更新并落库，返回更新后的完整设置。
pub async fn update(
    pool: &SqlitePool,
    current: &Settings,
    patch: SettingsPatch,
) -> Result<Settings> {
    let mut next = current.clone();

    if let Some(listen) = patch.listen {
        next.listen = listen;
    }
    if let Some(origins) = patch.cors_origins {
        next.cors_origins = origins;
    }
    if let Some(flag) = patch.trust_proxy {
        next.trust_proxy = flag;
    }
    if let Some(hours) = patch.token_ttl_hours {
        if hours <= 0 {
            return Err(Error::BadRequest("令牌有效期必须大于 0 小时".to_string()));
        }
        next.token_ttl_hours = hours;
    }
    if let Some(days) = patch.log_retention_days {
        if !(1..=365).contains(&days) {
            return Err(Error::BadRequest(
                "日志保留天数必须在 1 到 365 之间".to_string(),
            ));
        }
        next.log_retention_days = days;
    }

    db::settings::upsert_many(pool, &next.to_entries(), time::now_secs()).await?;

    tracing::info!(
        listen = %next.listen,
        token_ttl_hours = next.token_ttl_hours,
        log_retention_days = next.log_retention_days,
        "后台设置已更新"
    );

    Ok(next)
}
