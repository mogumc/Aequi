//! 应用状态装配。

use std::sync::RwLock;
use std::time::{Duration, Instant};

use sqlx::SqlitePool;

use crate::config::Settings;
use crate::db::Store;
use crate::service::provider::ProviderRegistry;
use crate::service::upstream::UpstreamState;

/// 全局共享状态，以 `Arc<AppState>` 的形式注入各 handler。
pub struct AppState {
    /// 后台设置。权威副本在数据库里，这里是内存缓存，改设置时同步更新。
    ///
    /// 读多写极少，且不跨 `await` 持锁，用标准库的读写锁即可。
    settings: RwLock<Settings>,
    /// 数据层。按域划分连接池，业务模块只认域，不认文件。
    pub store: Store,
    /// 已接入的第三方登录方式。当前为空。
    pub providers: ProviderRegistry,
    /// 上游调度的运行时状态：轮询累积值与每块的 HTTP 客户端。
    pub upstream: UpstreamState,
    started_at: Instant,
}

impl AppState {
    /// 依据设置与存储装配状态。
    pub fn new(settings: Settings, store: Store) -> Self {
        // 接入第三方登录时在此注册实现：
        // registry.register(Box::new(ExampleProvider::new(...)))
        let providers = ProviderRegistry::new();

        Self {
            settings: RwLock::new(settings),
            store,
            providers,
            upstream: UpstreamState::default(),
            started_at: Instant::now(),
        }
    }

    /// 控制面连接池。账号、会话、设置都在这里。
    pub fn pool(&self) -> &SqlitePool {
        &self.store.control
    }

    /// 取当前设置的一份快照。
    pub fn settings(&self) -> Settings {
        self.settings.read().expect("设置读写锁被污染").clone()
    }

    /// 覆盖内存中的设置。落库由 service 层负责。
    pub fn replace_settings(&self, settings: Settings) {
        *self.settings.write().expect("设置读写锁被污染") = settings;
    }

    /// 进程已运行时长。
    pub fn uptime(&self) -> Duration {
        self.started_at.elapsed()
    }
}
