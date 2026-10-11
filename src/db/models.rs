//! `models` 表的读写——模型的计费配置。
//!
//! 表里没有该模型时返回 `None`，由调用方决定兜底倍率——
//! 默认值属于业务规则，不放在数据层。

use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

use crate::model::ModelConfig;

/// 取模型倍率（万分比）。模型不存在或已停用时返回 `None`。
pub async fn find_multiplier_bp(
    pool: &SqlitePool,
    name: &str,
) -> Result<Option<i64>, sqlx::Error> {
    let row = sqlx::query("SELECT multiplier_bp FROM models WHERE name = ? AND enabled = 1")
        .bind(name)
        .fetch_optional(pool)
        .await?;

    Ok(row.map(|row| row.get("multiplier_bp")))
}

const SQL_LIST: &str = "\
SELECT name, multiplier_bp, enabled, created_at, updated_at FROM models ORDER BY name";

const SQL_FIND: &str = "\
SELECT name, multiplier_bp, enabled, created_at, updated_at FROM models WHERE name = ?";

const SQL_INSERT: &str = "\
INSERT INTO models (name, multiplier_bp, enabled, created_at, updated_at) \
VALUES (?, ?, ?, ?, ?)";

const SQL_UPDATE: &str = "\
UPDATE models SET multiplier_bp = ?, enabled = ?, updated_at = ? WHERE name = ?";

const SQL_DELETE: &str = "DELETE FROM models WHERE name = ?";

fn map_row(row: &SqliteRow) -> ModelConfig {
    ModelConfig {
        name: row.get("name"),
        multiplier_bp: row.get("multiplier_bp"),
        enabled: row.get::<i64, _>("enabled") != 0,
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

/// 列出全部模型配置，按名称排序。
pub async fn list(pool: &SqlitePool) -> Result<Vec<ModelConfig>, sqlx::Error> {
    let rows = sqlx::query(SQL_LIST).fetch_all(pool).await?;
    Ok(rows.iter().map(map_row).collect())
}

/// 按名称取配置。
pub async fn find(pool: &SqlitePool, name: &str) -> Result<Option<ModelConfig>, sqlx::Error> {
    let row = sqlx::query(SQL_FIND).bind(name).fetch_optional(pool).await?;
    Ok(row.as_ref().map(map_row))
}

/// 插入一条配置。名称有唯一约束，重复会直接报冲突。
pub async fn insert(
    pool: &SqlitePool,
    name: &str,
    multiplier_bp: i64,
    enabled: bool,
    now: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(SQL_INSERT)
        .bind(name)
        .bind(multiplier_bp)
        .bind(i64::from(enabled))
        .bind(now)
        .bind(now)
        .execute(pool)
        .await?;
    Ok(())
}

/// 更新倍率与启用状态。返回是否命中。
pub async fn update(
    pool: &SqlitePool,
    name: &str,
    multiplier_bp: i64,
    enabled: bool,
    now: i64,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(SQL_UPDATE)
        .bind(multiplier_bp)
        .bind(i64::from(enabled))
        .bind(now)
        .bind(name)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// 删除配置。
pub async fn delete(pool: &SqlitePool, name: &str) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(SQL_DELETE).bind(name).execute(pool).await?;
    Ok(result.rows_affected() > 0)
}
