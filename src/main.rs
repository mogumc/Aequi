//! 二进制入口。
//!
//! 不带参数时启动服务；带子命令时执行一次性操作（详见 [`aequi::cli`]）。
//!
//! 启动顺序：读引导参数 → 初始化日志 → 建库建表 → 载入设置 → 初始化账号
//! → 启动后台维护任务 → 启动服务。
//!
//! 只有首次启动（数据库文件尚不存在）才接受环境变量形式的初始账号密码；
//! 后台设置一律以数据库为准，没有配置文件。

use std::sync::Arc;

use aequi::cli::{self, Command};
use aequi::config::{Bootstrap, ADMIN_PASSWORD_ENV};
use aequi::service::auth::InitialCredentials;
use aequi::{db, server, service, utils, AppState};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match Command::parse(&args)? {
        Command::Help => {
            cli::print_help();
            Ok(())
        }
        Command::ResetPassword => {
            utils::logging::init();
            cli::reset_password(&Bootstrap::from_env()).await
        }
        Command::Serve => serve().await,
    }
}

/// 启动 HTTP 服务。
async fn serve() -> anyhow::Result<()> {
    // 引导参数必须在连库之前确定：首次判定依赖数据库文件是否存在。
    let bootstrap = Bootstrap::from_env();
    let is_first_run = bootstrap.is_first_run();

    utils::logging::init();

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        database = %bootstrap.database.display(),
        data_database = %bootstrap.data_database.display(),
        first_run = is_first_run,
        "Aequi 启动中"
    );

    // 控制面与数据面各自独立，密钥体系在文件层面就碰不到管理员数据。
    let store = db::Store::connect(&bootstrap.database, &bootstrap.data_database).await?;
    store.migrate().await?;

    // 首次启动导入默认设置，之后一律以数据库为准。
    let settings = service::settings::ensure(&store.control).await?;
    tracing::info!(listen = %settings.listen, "后台设置已载入");

    match service::auth::initialize_account(&store.control, is_first_run).await? {
        Some(credentials) => print_initial_credentials(&credentials),
        None => tracing::info!("账号已存在，跳过初始化"),
    }

    let state = Arc::new(AppState::new(settings, store));
    service::maintenance::spawn(state.clone());

    server::serve(state).await
}

/// 把初始凭据打印到控制台。
///
/// 密码只在这一次输出中出现，之后系统仅保存其哈希。
fn print_initial_credentials(credentials: &InitialCredentials) {
    println!();
    println!("================= Aequi 初始账号 =================");
    println!("  用户名 : {}", credentials.username);
    if credentials.from_env {
        println!("  密码   : （由环境变量 {ADMIN_PASSWORD_ENV} 提供，此处不回显）");
    } else {
        println!("  密码   : {}", credentials.password);
    }
    println!();
    if credentials.from_env {
        println!("  凭据来自环境变量，仅首次启动生效，后续启动不再读取。");
    } else {
        println!("  该密码为随机生成，仅显示这一次，请立即妥善保存。");
    }
    println!("=================================================");
    println!();
}
