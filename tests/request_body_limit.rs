//! 代理入口的请求体上限。
//!
//! axum 对 `Bytes` 系提取器默认只收 2 MiB，超过就以 413 拒收、请求到不了上游。
//! 长上下文对话的 body 装的是整段历史，很容易越过这个数（实测 15 万 token 的
//! 会话约 1.4 MiB，百万 token 量级会远超），所以网关分组单独放宽到 10 MiB。
//!
//! 这两个用例把边界钉死：**低于上限必须放行**、**高于上限仍然拒绝**。
//! 只改常量不改测试的话，其中一条会失败。
//!
//! 所有凭据均为测试用假数据，不涉及任何真实服务。

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{ConnectInfo, DefaultBodyLimit};
use axum::http::{Method, Request, StatusCode, header};
use axum::routing::post;
use axum::{Json, Router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use aequi::config::Settings;
use aequi::db;
use aequi::model::{ApiFormat, BlockCreate, ModelEntry, UpstreamKeyCreate};
use aequi::service::upstream as upstream_service;
use aequi::utils::random;
use aequi::{AppState, api};

/// 网关侧放宽后的上限，与 `api::router` 里的常量保持一致。
const GATEWAY_LIMIT: usize = 10 * 1024 * 1024;

const ACCESS_SECRET: &str = "mock-access-secret";

struct TestApp {
    state: Arc<AppState>,
    dir: PathBuf,
}

impl TestApp {
    async fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("aequi-body-limit-{}", random::string(12)));
        std::fs::create_dir_all(&dir).expect("创建临时目录失败");

        let store = db::Store::connect(&dir.join("test.db"), &dir.join("test-data.db"))
            .await
            .expect("建库失败");
        store.migrate().await.expect("建表失败");

        aequi::service::settings::ensure(&store.control)
            .await
            .expect("初始化设置失败");

        let state = Arc::new(AppState::new(Settings::default(), store));
        Self { state, dir }
    }

    fn router(&self) -> Router {
        api::router(self.state.clone())
    }

    /// 建块、配上上游密钥与一把无限额度的访问密钥。
    async fn seed(&self, base_url: &str) {
        let data = &self.state.store.data;

        upstream_service::create_block(
            data,
            BlockCreate {
                id: "demo-block".to_string(),
                name: "demo".to_string(),
                api_format: ApiFormat::Openai,
                weight: 1,
                max_concurrency: 0,
                base_url: base_url.to_string(),
                proxy: None,
                models: vec![ModelEntry {
                    name: "demo-model".to_string(),
                    forward: None,
                    multiplier_bp: 10_000,
                    extra_body: None,
                }],
                headers: Vec::new(),
            },
        )
        .await
        .expect("建块失败");

        upstream_service::add_keys(
            data,
            "demo-block",
            UpstreamKeyCreate {
                secrets: vec!["mock-upstream-secret".to_string()],
                weight: 1,
                never_disable: true,
            },
        )
        .await
        .expect("加上游密钥失败");

        db::keys::insert(
            data,
            &db::keys::NewApiKey {
                id: "key-1",
                name: "demo",
                secret: ACCESS_SECRET,
                key_prefix: "mock",
                credits_scaled: 500_000,
                unlimited: true,
                enabled: true,
                allowed_models: &[],
                created_at: 0,
            },
        )
        .await
        .expect("插入访问密钥失败");
    }
}

impl Drop for TestApp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 只回一个最小可用响应的上游。
///
/// 自己也要放宽 body 上限——否则它会把被测请求当成超限，测的就不是网关这一层了。
fn echo_upstream() -> Router {
    Router::new()
        .route(
            "/v1/chat/completions",
            post(|Json(body): Json<Value>| async move {
                let messages = body["messages"].as_array().map(Vec::len).unwrap_or(0);
                Json(json!({
                    "id": "mock-id",
                    "object": "chat.completion",
                    "model": "demo-model",
                    "messages_seen": messages,
                    "choices": [{
                        "index": 0,
                        "message": { "role": "assistant", "content": "mock reply" },
                        "finish_reason": "stop",
                    }],
                    "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 },
                }))
            }),
        )
        .layer(DefaultBodyLimit::disable())
}

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定 mock 端口失败");
    let addr = listener.local_addr().expect("读取 mock 端口失败");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    format!("http://{addr}")
}

/// 造一个指定近似体积的合法对话请求体，用 `messages[0].content` 撑体积。
fn chat_payload(target_bytes: usize) -> Vec<u8> {
    let mut body = json!({
        "model": "demo-model",
        "messages": [{ "role": "user", "content": "" }],
    });

    let base = serde_json::to_vec(&body).expect("序列化失败").len();
    // 空串占位，补足差额；差额可能为负，用 saturating 兜住。
    let fill = target_bytes.saturating_sub(base);
    body["messages"][0]["content"] = Value::String("x".repeat(fill));

    let raw = serde_json::to_vec(&body).expect("序列化失败");
    // 断言造出来的确实在目标附近，避免测试自身失真。
    assert!(
        raw.len() >= target_bytes && raw.len() < target_bytes + 64,
        "构造的请求体大小偏离目标: 期望约 {target_bytes}，实际 {}",
        raw.len()
    );
    raw
}

fn big_request(body: Vec<u8>) -> Request<Body> {
    let mut request = Request::builder()
        .method(Method::POST)
        .uri("/v1/chat/completions")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {ACCESS_SECRET}"))
        .body(Body::from(body))
        .expect("构造请求失败");
    request
        .extensions_mut()
        .insert(ConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 43210))));
    request
}

async fn body_text(response: axum::response::Response) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8_lossy(&bytes).to_string()
}

/// 3 MiB：超过 axum 默认的 2 MiB，但低于网关的 10 MiB —— 必须放行。
///
/// 这是本次修复的核心用例：改动前它会以 413 失败。
#[tokio::test]
async fn body_above_axum_default_reaches_the_upstream() {
    let app = TestApp::new().await;
    let base_url = serve(echo_upstream()).await;
    app.seed(&base_url).await;

    let payload = chat_payload(3 * 1024 * 1024);
    let response = app.router().oneshot(big_request(payload)).await.unwrap();

    let status = response.status();
    let text = body_text(response).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "3 MiB 的请求应当放行到上游，实际 {status}，响应: {text}"
    );
    assert!(
        !text.contains("length limit exceeded"),
        "不该出现 axum 的长度限制报错，响应: {text}"
    );
}

/// 略超 10 MiB：上限仍然生效，必须 413 拒绝。
///
/// 放宽不等于取消——对外入口需要一个兜底，否则超大 body 会拖住进程内存。
#[tokio::test]
async fn body_above_gateway_limit_is_still_rejected() {
    let app = TestApp::new().await;
    let base_url = serve(echo_upstream()).await;
    app.seed(&base_url).await;

    let payload = chat_payload(GATEWAY_LIMIT + 4096);
    let response = app.router().oneshot(big_request(payload)).await.unwrap();

    assert_eq!(
        response.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "超过 10 MiB 应当仍被拒绝"
    );
}

/// 管理端接口不受影响，仍沿用 axum 默认的 2 MiB。
///
/// 放宽只针对代理入口，管理端没有接收大 body 的需求，不该跟着一起放开。
#[tokio::test]
async fn admin_routes_keep_the_default_limit() {
    let app = TestApp::new().await;

    // 登录接口是管理端里唯一的公开 POST，拿它验证默认上限未被改动。
    let mut body = json!({
        "username": "mock-operator",
        "password": "mock-password-for-tests",
    });
    body["password"] = Value::String("x".repeat(3 * 1024 * 1024));
    let raw = serde_json::to_vec(&body).expect("序列化失败");

    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/auth/login")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(raw))
        .expect("构造请求失败");

    let mut request = request;
    request
        .extensions_mut()
        .insert(ConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 43210))));

    let response = app.router().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "管理端接口应仍受 2 MiB 默认上限约束"
    );
}
