use serde::Serialize;

/// 账号实体。
///
/// 当前系统只维护一个账号，表结构保留多行能力以便后续扩展。
#[derive(Debug, Clone)]
pub struct Account {
    pub id: i64,
    pub username: String,
    /// Argon2 PHC 字符串，绝不出现在任何响应里。
    pub password_hash: String,
    /// 是否已启用 TOTP 二次验证，默认关闭。
    pub totp_enabled: bool,
    /// TOTP 密钥（base32），仅在绑定流程中写入。
    pub totp_secret: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 账号的对外视图，不含任何敏感字段。
#[derive(Debug, Clone, Serialize)]
pub struct AccountView {
    pub id: i64,
    pub username: String,
    pub totp_enabled: bool,
    pub created_at: i64,
}

impl From<&Account> for AccountView {
    fn from(account: &Account) -> Self {
        Self {
            id: account.id,
            username: account.username.clone(),
            totp_enabled: account.totp_enabled,
            created_at: account.created_at,
        }
    }
}
