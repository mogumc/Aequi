use serde::Serialize;

/// 一次登录产生的会话，等价于一台已登录的设备。
///
/// 会话之间互不干扰：允许多端同时在线，不做互斥。
#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub account_id: i64,
    /// 令牌的 SHA-256 散列，明文令牌只返回给客户端一次。
    pub token_hash: String,
    /// 登录方式，例如 `password`、`password+totp`。
    pub login_type: String,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    /// 登录时间。
    pub created_at: i64,
    /// 最近一次使用时间。
    pub last_seen_at: i64,
    pub expires_at: i64,
    /// 撤销时间，为空表示仍然有效。
    pub revoked_at: Option<i64>,
}

impl Session {
    /// 在给定时刻是否仍然有效。
    pub fn is_active(&self, now: i64) -> bool {
        self.revoked_at.is_none() && self.expires_at > now
    }
}

/// 写入新会话所需的字段。
#[derive(Debug)]
pub struct NewSession<'a> {
    pub id: &'a str,
    pub account_id: i64,
    pub token_hash: &'a str,
    pub login_type: &'a str,
    pub ip: Option<&'a str>,
    pub user_agent: Option<&'a str>,
    pub created_at: i64,
    pub expires_at: i64,
}

/// 会话的对外视图。
#[derive(Debug, Clone, Serialize)]
pub struct SessionView {
    pub id: String,
    pub login_type: String,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    pub created_at: i64,
    pub last_seen_at: i64,
    pub expires_at: i64,
    /// 是否为发起本次请求的设备。
    pub current: bool,
    pub active: bool,
}

impl SessionView {
    /// 由会话记录生成视图，`current_id` 用于标记当前设备。
    pub fn new(session: &Session, current_id: &str, now: i64) -> Self {
        Self {
            id: session.id.clone(),
            login_type: session.login_type.clone(),
            ip: session.ip.clone(),
            user_agent: session.user_agent.clone(),
            created_at: session.created_at,
            last_seen_at: session.last_seen_at,
            expires_at: session.expires_at,
            current: session.id == current_id,
            active: session.is_active(now),
        }
    }
}
