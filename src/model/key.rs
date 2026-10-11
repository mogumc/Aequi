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
    /// 无限额度：扣费跳过余额判断，但用量与消耗照记。
    ///
    /// 与 `credits_scaled` 并存但互不影响——无限时余额字段不再参与判定，
    /// 保留原值只是为了切回有限额度时有个起点。
    pub unlimited: bool,
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
    /// 是否允许调用**某个块的**某个模型。
    ///
    /// 白名单的条目是**全名**（`块id/模型名`）。同一个模型名出现在两个块里时，
    /// 那是两条彼此独立的授权——两个块的出口、价格都可能不同，允许一个不等于
    /// 允许另一个。所以白名单不能只记模型名，否则「只允许了 a 的 gpt-4o」
    /// 会连 b 的 gpt-4o 一起放行。
    ///
    /// 白名单为空表示不限制。
    pub fn allows_capability(&self, block_id: &str, model: &str) -> bool {
        self.allowed_models.is_empty()
            || self
                .allowed_models
                .iter()
                .any(|item| item == &format!("{block_id}/{model}"))
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
    /// 无限额度：前端据此显示「无限」，不再看余额。
    pub unlimited: bool,
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
            unlimited: key.unlimited,
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
    /// 无限额度：持有者据此知道余额不是限制。
    pub unlimited: bool,
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
            unlimited: key.unlimited,
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
    /// 初始即为无限额度；此时 `credits` 只是留个起点，不参与判定。
    #[serde(default)]
    pub unlimited: bool,
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

/// 额度设置：余额与无限开关，各改各的，不传的字段保持不变。
#[derive(Debug, Default, Deserialize)]
pub struct KeyCreditsUpdate {
    /// 新的积分余额，四位小数以内。
    pub credits: Option<String>,
    /// 是否无限额度。
    pub unlimited: Option<bool>,
}
