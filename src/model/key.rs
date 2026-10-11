use serde::{Deserialize, Serialize};

use crate::model::credits;

/// 一个访问密钥。积分直接挂在它身上。
///
/// 明文密钥只在创建时返回一次，库里只留散列与展示用前缀。
#[derive(Debug, Clone)]
pub struct ApiKey {
    pub id: String,
    pub name: String,
    /// 密钥的 SHA-256 散列。
    pub key_hash: String,
    /// 展示用前缀，便于在列表里辨认是哪一把。
    pub key_prefix: String,
    /// 剩余积分，整数最小单位（1 积分 = 10000）。
    pub credits_scaled: i64,
    pub enabled: bool,
    /// 允许使用的模型。空表示不限制。
    ///
    /// 本轮只做校验，模型的维护接口留待模型模块。
    pub allowed_models: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_used_at: Option<i64>,
}

impl ApiKey {
    /// 是否允许调用该模型。白名单为空时视为不限制。
    pub fn allows_model(&self, model: &str) -> bool {
        self.allowed_models.is_empty() || self.allowed_models.iter().any(|item| item == model)
    }
}

/// 密钥的管理视图。不含散列。
#[derive(Debug, Clone, Serialize)]
pub struct KeyView {
    pub id: String,
    pub name: String,
    /// 展示用前缀；完整密钥不会再次出现。
    pub key_prefix: String,
    /// 剩余积分，四位小数字符串。
    pub credits: String,
    /// 同一数值的整数形式，便于前端精确处理。
    pub credits_scaled: i64,
    pub enabled: bool,
    pub allowed_models: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_used_at: Option<i64>,
}

impl From<&ApiKey> for KeyView {
    fn from(key: &ApiKey) -> Self {
        Self {
            id: key.id.clone(),
            name: key.name.clone(),
            key_prefix: key.key_prefix.clone(),
            credits: credits::format_credits(key.credits_scaled),
            credits_scaled: key.credits_scaled,
            enabled: key.enabled,
            allowed_models: key.allowed_models.clone(),
            created_at: key.created_at,
            updated_at: key.updated_at,
            last_used_at: key.last_used_at,
        }
    }
}

/// 密钥的创建请求。
#[derive(Debug, Deserialize)]
pub struct KeyCreate {
    #[serde(default)]
    pub name: String,
    /// 初始积分，四位小数以内。缺省为 0。
    #[serde(default)]
    pub credits: Option<String>,
    /// 允许使用的模型；留空表示不限制。
    #[serde(default)]
    pub allowed_models: Vec<String>,
}

/// 密钥的元信息更新。只提交需要改的字段。
#[derive(Debug, Default, Deserialize)]
pub struct KeyUpdate {
    pub name: Option<String>,
    pub enabled: Option<bool>,
    pub allowed_models: Option<Vec<String>>,
}

/// 密钥的资源限制更新。
#[derive(Debug, Deserialize)]
pub struct KeyCreditsUpdate {
    /// 新的积分余额，四位小数以内。
    pub credits: String,
}
