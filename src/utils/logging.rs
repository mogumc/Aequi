//! 日志初始化。

use tracing_subscriber::EnvFilter;

/// 初始化全局日志订阅者。
///
/// 日志级别只由环境变量 `RUST_LOG` 决定，未设置时用 `info`。
/// 它属于启动引导参数，不放进后台设置——出问题时需要能在配置生效前就看到日志。
pub fn init() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}
