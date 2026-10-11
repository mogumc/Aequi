//! 配置。
//!
//! 分两层，边界清晰：
//!
//! - [`Bootstrap`]：启动引导参数。必须在连接数据库**之前**确定，
//!   因此只能来自环境变量或内置默认值，不可能存在数据库里。
//! - [`Settings`]：后台设置。存在数据库的 `settings` 表中，
//!   首次启动导入默认值，之后一律以数据库为准，用户在后台自行修改。
//!
//! 没有配置文件。除首次启动的初始账号密码外，服务不认可任何外部输入。

mod bootstrap;
mod settings;

pub use bootstrap::Bootstrap;
pub use settings::{
    KEY_CORS_ORIGINS, KEY_LISTEN, KEY_TOKEN_TTL_HOURS, KEY_TRUST_PROXY, Settings, SettingsPatch,
};

/// 覆盖数据库文件路径的环境变量。
pub const DATABASE_PATH_ENV: &str = "AEQUI_DATABASE_PATH";
/// 覆盖数据面数据库路径的环境变量。
pub const DATA_DATABASE_PATH_ENV: &str = "AEQUI_DATA_DATABASE_PATH";
/// 初始账号用户名的环境变量，**仅在首次启动时读取**。
pub const ADMIN_USERNAME_ENV: &str = "AEQUI_ADMIN_USERNAME";
/// 初始账号密码的环境变量，**仅在首次启动时读取**。
pub const ADMIN_PASSWORD_ENV: &str = "AEQUI_ADMIN_PASSWORD";
/// 默认数据库文件名，位于进程工作目录下。
pub const DEFAULT_DATABASE_FILE: &str = "aequi.db";

/// 读取环境变量，空值视为未设置。
pub fn non_empty_env(key: &str) -> Option<String> {
    let value = std::env::var(key).ok()?;
    let value = value.trim().to_string();
    if value.is_empty() { None } else { Some(value) }
}
