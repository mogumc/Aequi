//! 启动引导参数。
//!
//! 这些值决定了「去哪里读配置」，所以必须在连库之前就确定，
//! 只能来自环境变量或内置默认值。

use std::path::PathBuf;

use super::{non_empty_env, DATABASE_PATH_ENV, DEFAULT_DATABASE_FILE};

/// 启动引导参数。
#[derive(Debug, Clone)]
pub struct Bootstrap {
    /// SQLite 文件路径。
    pub database: PathBuf,
}

impl Bootstrap {
    /// 从环境变量读取。
    ///
    /// 数据库路径可用 `AEQUI_DATABASE_PATH` 覆盖，默认是工作目录下的 `aequi.db`。
    pub fn from_env() -> Self {
        let database = non_empty_env(DATABASE_PATH_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_DATABASE_FILE));

        Self { database }
    }

    /// 本次启动是否为首次。
    ///
    /// 判据是数据库文件尚不存在。必须在 [`crate::db::connect`] 之前调用——
    /// 连库会创建文件，之后就再也分不出首次与否了。
    ///
    /// 只有首次启动才接受环境变量形式的初始账号密码；数据库一旦存在，
    /// 后续启动完全忽略这些环境变量。
    pub fn is_first_run(&self) -> bool {
        !self.database.exists()
    }
}
