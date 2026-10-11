//! HTTP 服务启停。

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use tokio::net::TcpListener;

use crate::api;
use crate::AppState;

/// 启动 HTTP 服务，直到收到退出信号。
///
/// 使用 `into_make_service_with_connect_info` 让 handler 能拿到对端地址，
/// 登录时记录的 IP 即来源于此。
pub async fn serve(state: Arc<AppState>) -> anyhow::Result<()> {
    let listen = state.settings().listen;

    let listener = TcpListener::bind(listen)
        .await
        .with_context(|| format!("监听地址失败: {listen}"))?;

    tracing::info!(%listen, "HTTP 服务已就绪");

    axum::serve(
        listener,
        api::router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .context("HTTP 服务异常退出")
}

/// 等待 Ctrl-C 或 SIGTERM。
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }

    tracing::info!("收到退出信号，开始优雅关闭");
}
