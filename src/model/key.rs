use serde::{Deserialize, Serialize};

use crate::model::credits;

/// 一个访问密钥。积分直接挂在它身上。
///
/// 密钥以**明文**保存，管理员可随时在后台查看与复制。
/// 这么设计是因为后续所有请求数据都绑定在密钥上，一旦忘记就无从挽回；
/// 代价是数据库文件本身即等同于一份凭证集合——这与「把库文件当最高权限资产」
/// 的部署前提是一致的。
#[derive(Debug, Clone)]
pub struct ApiKey {
    pub id: String,
    pub name: String,
    /// 完整密钥。
    pub secret: String,
    /// 展示用前缀，便于在列表里一眼辨认。
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

/// 密钥的管理视图。
///
/// 这是唯一会返回明文密钥的地方，只有管理员接口使用。
#[derive(Debug, Clone, Serialize)]
pub struct KeyView {
    pub id: String,
    pub name: String,
    /// 完整密钥。前端默认打码，按需展开显示。
    pub secret: String,
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
            secret: key.secret.clone(),
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

/// 密钥的自助视图。
///
/// 刻意不含明文：持有者本来就拿着它，回显没有意义，
/// 也免得在自助页的响应里多留一份凭证副本。
#[derive(Debug, Clone, Serialize)]
pub struct KeySelfView {
    pub name: String,
    pub key_prefix: String,
    pub credits: String,
    pub credits_scaled: i64,
    pub enabled: bool,
    pub allowed_models: Vec<String>,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

impl From<&ApiKey> for KeySelfView {
    fn from(key: &ApiKey) -> Self {
        Self {
            name: key.name.clone(),
            key_prefix: key.key_prefix.clone(),
            credits: credits::format_credits(key.credits_scaled),
            credits_scaled: key.credits_scaled,
            enabled: key.enabled,
            allowed_models: key.allowed_models.clone(),
            created_at: key.created_at,
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
