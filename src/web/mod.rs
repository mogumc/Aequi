//! 前端 embed 层。
//!
//! 面板的构建产物通过 `rust-embed` 打进二进制，正式发行只需要一个可执行文件，
//! 不必再单独部署静态资源。
//!
//! 面板**源码**独立在项目同级的 `../dev-web`，不随后端一起进版本库；
//! 但它的构建**产物**输出到本项目的 `web/` 目录，后端只认这个目录。
//!
//! - debug 构建：从产物目录读磁盘，配合 `npm run dev` 迭代更快；
//! - release 构建：内容嵌入二进制，运行时不再依赖文件。
//!
//! 所有面板路由都挂在 [`PANEL_PREFIX`] 下。前端是单页应用，深层路径
//! （例如 `/web/login`）由前端路由解析——服务端找不到对应文件时回落到
//! `index.html`，而不是返回 404。

use axum::extract::Path;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use rust_embed::RustEmbed;

/// 面板的挂载前缀。
pub const PANEL_PREFIX: &str = "/web";

/// 前端构建产物。
#[derive(RustEmbed)]
#[folder = "web"]
struct Assets;

/// 面板路由。
///
/// 泛型参数让它能直接合并进任意 state 的 Router，不绑定具体应用状态，
/// 后续新增的页面模块也可以照这个形状提供自己的路由片段。
pub fn router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let catch_all = format!("{PANEL_PREFIX}/{{*path}}");

    Router::new()
        .route(PANEL_PREFIX, get(index))
        .route(&format!("{PANEL_PREFIX}/"), get(index))
        .route(&catch_all, get(asset))
}

/// `/web` 与 `/web/`：返回面板入口。
async fn index() -> Response {
    serve("index.html")
}

/// `/web/*path`：按路径取资源。
async fn asset(Path(path): Path<String>) -> Response {
    serve(&path)
}

/// 取出资源并附上正确的 Content-Type。
///
/// 找不到就回落到 `index.html`：SPA 的深层路径在服务端没有对应文件，
/// 需要交给前端路由处理。
fn serve(path: &str) -> Response {
    // 显式拒绝路径穿越。嵌入的资源名里不可能出现上级目录，出现就是有人在做手脚。
    // rust-embed 自身也做了校验，这里是不拿库的内部实现当安全边界的第二道防线。
    if path.split(['/', '\\']).any(|segment| segment == "..") {
        return not_found();
    }

    let path = path.trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    if let Some(content) = Assets::get(path) {
        return file_response(path, content);
    }

    match Assets::get("index.html") {
        Some(content) => file_response("index.html", content),
        None => not_built(),
    }
}

/// 明确拒绝的请求。
fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "资源不存在").into_response()
}

/// 组装文件响应。
fn file_response(path: &str, content: rust_embed::EmbeddedFile) -> Response {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    (
        [(header::CONTENT_TYPE, mime.as_ref())],
        content.data.into_owned(),
    )
        .into_response()
}

/// 前端尚未构建时的提示页。
///
/// 直接给空白页最难排查，这里把该执行的命令写清楚。
fn not_built() -> Response {
    const PAGE: &str = r#"<!DOCTYPE html>
<html lang="zh-CN">
<head><meta charset="utf-8"><title>Aequi 控制台</title></head>
<body style="font-family:system-ui,-apple-system,'Segoe UI','Microsoft YaHei',sans-serif;max-width:640px;margin:80px auto;padding:0 20px;line-height:1.7;color:#1f2430">
<h1 style="font-size:20px">控制台尚未构建</h1>
<p>前端资源还没有打包进来。面板源码在后端项目同级的 dev-web 目录，执行：</p>
<pre style="background:#f5f6f8;border:1px solid #e4e7ed;padding:12px;border-radius:8px">cd ../dev-web
npm install
npm run build</pre>
<p>然后重新构建并启动后端。</p>
</body>
</html>"#;

    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        PAGE,
    )
        .into_response()
}
