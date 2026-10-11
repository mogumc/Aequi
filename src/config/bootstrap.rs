//! 启动引导参数。
//!
//! 这些值决定了「去哪里读配置」，所以必须在连库之前就确定，
//! 只能来自环境变量或内置默认值。

use std::path::{Path, PathBuf};

use super::{DATA_DATABASE_PATH_ENV, DATABASE_PATH_ENV, DEFAULT_DATABASE_FILE, non_empty_env};

/// 启动引导参数。
#[derive(Debug, Clone)]
pub struct Bootstrap {
    /// 控制面库：账号、会话、设置等网关自身数据。
    pub database: PathBuf,
    /// 数据面库：密钥、积分、用量等业务数据。
    ///
    /// 与控制面分开存放，两套体系在文件层面就不共享读写，
    /// 密钥侧的任何查询都碰不到管理员数据。
    pub data_database: PathBuf,
}

impl Bootstrap {
    /// 从环境变量读取。
    ///
    /// 两个库都可用环境变量覆盖；数据面未指定时，按控制面文件名派生
    /// （`aequi.db` → `aequi-data.db`），保证它们落在同一目录便于备份。
    pub fn from_env() -> Self {
        let database = non_empty_env(DATABASE_PATH_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_DATABASE_FILE));

        let data_database = non_empty_env(DATA_DATABASE_PATH_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| derive_data_path(&database));

        Self {
            database,
            data_database,
        }
    }

    /// 本次启动是否为首次。
    ///
    /// 判据是控制面库文件尚不存在。必须在 [`crate::db::Store::connect`] 之前调用——
    /// 连库会创建文件，之后就再也分不出首次与否了。
    ///
    /// 只有首次启动才接受环境变量形式的初始账号密码；数据库一旦存在，
    /// 后续启动完全忽略这些环境变量。
    pub fn is_first_run(&self) -> bool {
        !self.database.exists()
    }
}

/// 由控制面路径派生出数据面路径。
///
/// `data/aequi.db` → `data/aequi-data.db`；没有父目录时退回当前目录。
fn derive_data_path(control: &Path) -> PathBuf {
    let stem = control
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("aequi");
    let file_name = format!("{stem}-data.db");

    match control.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(file_name),
        _ => PathBuf::from(file_name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_sibling_path_for_data_database() {
        assert_eq!(
            derive_data_path(Path::new("data/aequi.db")),
            PathBuf::from("data/aequi-data.db")
        );
        assert_eq!(
            derive_data_path(Path::new("aequi.db")),
            PathBuf::from("aequi-data.db")
        );
    }
}
