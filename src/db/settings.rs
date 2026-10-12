//! `settings` 表的读写。
//!
//! 纯键值结构，值的类型由 [`crate::config::Settings`] 负责解释。

use std::collections::HashMap;

use sqlx::{Row, SqlitePool};

/// 读取全部设置项。
pub async fn load_all(pool: &SqlitePool) -> Result<HashMap<String, String>, sqlx::Error> {
    let rows = sqlx::query("SELECT key, value FROM settings")
        .fetch_all(pool)
        .await?;

    Ok(rows
        .iter()
        .map(|row| (row.get::<String, _>("key"), row.get::<String, _>("value")))
        .collect())
}

/// 写入或覆盖若干设置项。
pub async fn upsert_many(
    pool: &SqlitePool,
    entries: &[(&str, String)],
    now: i64,
) -> Result<(), sqlx::Error> {
    if entries.is_empty() {
        return Ok(());
    }

    let mut tx = pool.begin().await?;
    for (key, value) in entries {
        sqlx::query(
            "INSERT INTO settings (key, value, updated_at) VALUES (?, ?, ?) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        )
        .bind(key)
        .bind(value)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    Ok(())
}
