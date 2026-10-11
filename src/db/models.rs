//! `models` 表的读写。
//!
//! 本轮只提供「按模型取倍率」这一个读取入口：倍率参与计费换算，
//! 而模型的增删改查属于模型管理模块，尚未实现，位置预留在表结构里。
//!
//! 表里没有该模型时返回 `None`，由调用方决定兜底倍率——
//! 默认值属于业务规则，不放在数据层。

use sqlx::{Row, SqlitePool};

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
