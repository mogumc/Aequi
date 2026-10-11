//! 后台设置。
//!
//! 存在数据库的 `settings` 表里，首次启动导入默认值，之后以数据库为准。
//! 这里只负责结构定义，以及与数据库键值对之间的互转。

use std::collections::HashMap;
use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

/// HTTP 监听地址。
pub const KEY_LISTEN: &str = "server.listen";
/// 允许跨域的来源，值为 JSON 数组。
pub const KEY_CORS_ORIGINS: &str = "server.cors_origins";
/// 是否信任反向代理传来的转发头。
pub const KEY_TRUST_PROXY: &str = "server.trust_proxy";
/// 登录令牌有效期（小时）。
pub const KEY_TOKEN_TTL_HOURS: &str = "auth.token_ttl_hours";
/// 代理日志的保留天数。
pub const KEY_LOG_RETENTION_DAYS: &str = "log.retention_days";

/// 后台设置。全部可在管理接口中修改。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// HTTP 监听地址，修改后需重启生效。
    pub listen: SocketAddr,
    /// 允许跨域的来源，`["*"]` 表示放开全部。修改后需重启生效。
    pub cors_origins: Vec<String>,
    /// 是否采信 `X-Forwarded-For` / `X-Real-IP` 来记录来源 IP。
    ///
    /// 默认关闭。直连场景下这些请求头完全由客户端掌控，采信等于让登录记录里的
    /// IP 可以随意伪造；只有确实部署在可信反向代理之后才应该打开。
    pub trust_proxy: bool,
    /// 登录令牌有效期（小时），修改后立即生效。
    pub token_ttl_hours: i64,
    /// 代理日志与用量记录的保留天数，超期由后台任务回收。
    pub log_retention_days: i64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([0, 0, 0, 0], 8080)),
            cors_origins: vec!["*".to_string()],
            trust_proxy: false,
            token_ttl_hours: 168,
            log_retention_days: 7,
        }
    }
}

impl Settings {
    /// 展开成数据库键值对。
    pub fn to_entries(&self) -> Vec<(&'static str, String)> {
        vec![
            (KEY_LISTEN, self.listen.to_string()),
            (
                KEY_CORS_ORIGINS,
                serde_json::to_string(&self.cors_origins).unwrap_or_else(|_| "[]".to_string()),
            ),
            (KEY_TRUST_PROXY, self.trust_proxy.to_string()),
            (KEY_TOKEN_TTL_HOURS, self.token_ttl_hours.to_string()),
            (KEY_LOG_RETENTION_DAYS, self.log_retention_days.to_string()),
        ]
    }

    /// 用数据库里的键值对覆盖字段。
    ///
    /// 缺失或值非法的项保留原值，只记一条警告——一条脏数据不该让服务起不来。
    pub fn apply(&mut self, entries: &HashMap<String, String>) {
        if let Some(raw) = entries.get(KEY_LISTEN) {
            match raw.parse::<SocketAddr>() {
                Ok(addr) => self.listen = addr,
                Err(_) => tracing::warn!(value = %raw, "设置项 {} 非法，沿用当前值", KEY_LISTEN),
            }
        }

        if let Some(raw) = entries.get(KEY_CORS_ORIGINS) {
            match serde_json::from_str::<Vec<String>>(raw) {
                Ok(origins) => self.cors_origins = origins,
                Err(_) => {
                    tracing::warn!(value = %raw, "设置项 {} 非法，沿用当前值", KEY_CORS_ORIGINS)
                }
            }
        }

        if let Some(raw) = entries.get(KEY_TRUST_PROXY) {
            match raw.parse::<bool>() {
                Ok(flag) => self.trust_proxy = flag,
                Err(_) => tracing::warn!(value = %raw, "设置项 {} 非法，沿用当前值", KEY_TRUST_PROXY),
            }
        }

        if let Some(raw) = entries.get(KEY_TOKEN_TTL_HOURS) {
            match raw.parse::<i64>() {
                Ok(hours) if hours > 0 => self.token_ttl_hours = hours,
                _ => {
                    tracing::warn!(value = %raw, "设置项 {} 非法，沿用当前值", KEY_TOKEN_TTL_HOURS)
                }
            }
        }

        if let Some(raw) = entries.get(KEY_LOG_RETENTION_DAYS) {
            match raw.parse::<i64>() {
                // 过短会丢排障依据，过长会撑大数据库，两端都收住。
                Ok(days) => self.log_retention_days = days.clamp(1, 365),
                Err(_) => {
                    tracing::warn!(value = %raw, "设置项 {} 非法，沿用当前值", KEY_LOG_RETENTION_DAYS)
                }
            }
        }
    }
}

/// 设置的部分更新。只提交需要改的字段。
#[derive(Debug, Default, Deserialize)]
pub struct SettingsPatch {
    pub listen: Option<SocketAddr>,
    pub cors_origins: Option<Vec<String>>,
    pub trust_proxy: Option<bool>,
    pub token_ttl_hours: Option<i64>,
    pub log_retention_days: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_round_trip() {
        let original = Settings {
            listen: SocketAddr::from(([127, 0, 0, 1], 9000)),
            cors_origins: vec!["http://localhost:5173".to_string()],
            trust_proxy: true,
            token_ttl_hours: 24,
            log_retention_days: 14,
        };

        let entries: HashMap<String, String> = original
            .to_entries()
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect();

        let mut restored = Settings::default();
        restored.apply(&entries);
        assert_eq!(restored, original);
    }

    #[test]
    fn illegal_values_fall_back_to_defaults() {
        let mut settings = Settings::default();
        let entries = HashMap::from([
            (KEY_LISTEN.to_string(), "not-an-address".to_string()),
            (KEY_CORS_ORIGINS.to_string(), "{broken json".to_string()),
            (KEY_TRUST_PROXY.to_string(), "maybe".to_string()),
            (KEY_TOKEN_TTL_HOURS.to_string(), "-5".to_string()),
        ]);

        settings.apply(&entries);
        assert_eq!(settings, Settings::default());
    }

    #[test]
    fn missing_entries_keep_current_values() {
        let mut settings = Settings::default();
        settings.apply(&HashMap::new());
        assert_eq!(settings, Settings::default());
    }

    #[test]
    fn trust_proxy_defaults_to_off() {
        assert!(!Settings::default().trust_proxy, "默认不应采信转发头");
    }
}
