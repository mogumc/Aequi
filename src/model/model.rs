//! 模型的计费配置。
//!
//! 这里管的是「对外模型名 → 倍率」这张表；模型**能不能用**由上游块的
//! 模型列表决定，两者互不干涉——倍率没配的模型照常可用，只是按默认倍率计费。

use serde::{Deserialize, Serialize};

use crate::model::credits;

/// 一个模型的计费配置。
#[derive(Debug, Clone)]
pub struct ModelConfig {
    pub name: String,
    /// 倍率，万分比整数（10000 = 1.0 倍）。
    pub multiplier_bp: i64,
    /// 配置是否生效；停用时计费回落到默认倍率。
    pub enabled: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

impl ModelConfig {
    /// 视图里要用的倍率字符串。
    pub fn multiplier(&self) -> String {
        credits::format_multiplier(self.multiplier_bp)
    }
}

/// 模型的管理视图。
#[derive(Debug, Clone, Serialize)]
pub struct ModelView {
    pub name: String,
    /// 倍率，四位小数字符串。
    pub multiplier: String,
    /// 同一数值的万分比整数，便于前端精确处理。
    pub multiplier_bp: i64,
    pub enabled: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

impl From<&ModelConfig> for ModelView {
    fn from(config: &ModelConfig) -> Self {
        Self {
            name: config.name.clone(),
            multiplier: config.multiplier(),
            multiplier_bp: config.multiplier_bp,
            enabled: config.enabled,
            created_at: config.created_at,
            updated_at: config.updated_at,
        }
    }
}

/// 创建模型配置。
#[derive(Debug, Deserialize)]
pub struct ModelCreate {
    pub name: String,
    /// 倍率，四位小数以内；缺省 1.0。
    #[serde(default)]
    pub multiplier: Option<String>,
}

/// 更新模型配置。只提交要改的字段。
#[derive(Debug, Default, Deserialize)]
pub struct ModelUpdate {
    pub multiplier: Option<String>,
    pub enabled: Option<bool>,
}
