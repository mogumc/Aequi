//! 上游限流的下游出参：**429 + `Retry-After`**，以及重试在请求流水里留痕。
//!
//! 镜像真实场景：上游回 429（端点级 RPM 超限），块内只有一两把密钥。
//! 断言四件事——
//! 1) 下游拿到 429 `rate_limited` 并带 `Retry-After`，不是 403「无可用密钥」；
//! 2) 密钥进限流退避后**不会再打上游**，紧接着的请求仍然回 429；
//! 3) 请求流水里留下重试链，能看出「先被限流、换一把就成了」；
//! 4) 换钥匙重试成功时记的是成功，但重试链照留。
//!
//! 所有凭据均为测试用假数据，不涉及任何真实服务。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Json;
use axum::Router;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::post;
use serde_json::{Value, json};

use aequi::config::Settings;
use aequi::db::{self, proxy_log};
use aequi::model::{ApiFormat, ApiKey, BlockCreate, ModelEntry, UpstreamKeyCreate};
use aequi::service::upstream as upstream_service;
use aequi::utils::random;
use aequi::{AppState, Error, proxy};

struct TestApp {
    state: Arc<AppState>,
    dir: PathBuf,
}

impl TestApp {
    async fn new() -> TestApp {
        let dir = std::env::temp_dir().join(format!("aequi-rate-limit-{}", random::string(12)));
        std::fs::create_dir_all(&dir).expect("创建临时目录失败");

        let store = db::Store::connect(&dir.join("test.db"), &dir.join("test-data.db"))
            .await
            .expect("建库失败");
        store.migrate().await.expect("建表失败");

        aequi::service::settings::ensure(&store.control)
            .await
            .expect("初始化设置失败");

        let state = Arc::new(AppState::new(Settings::default(), store));
        TestApp { state, dir }
    }

    fn data(&self) -> &sqlx::SqlitePool {
        &self.state.store.data
    }

    /// 建块 + 若干上游密钥 + 一把有余额的访问密钥。
    async fn seed(&self, base_url: &str, secrets: &[&str]) {
        upstream_service::create_block(
            self.data(),
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
            self.data(),
            "demo-block",
            UpstreamKeyCreate {
                secrets: secrets.iter().map(|s| s.to_string()).collect(),
                weight: 1,
                never_disable: false,
            },
        )
        .await
        .expect("加上游密钥失败");

        db::keys::insert(
            self.data(),
            &db::keys::NewApiKey {
                id: "key-1",
                name: "demo",
                secret: "mock-secret",
                key_prefix: "mock",
                credits_scaled: 500_000,
                unlimited: false,
                enabled: true,
                allowed_models: &[],
                created_at: 0,
            },
        )
        .await
        .expect("插入访问密钥失败");
    }

    /// 最近的一条请求流水。
    async fn latest_log_row(&self) -> aequi::model::ProxyLog {
        let filter = proxy_log::LogFilter {
            limit: 10,
            ..Default::default()
        };
        let (logs, _) = proxy_log::list_filtered(self.data(), &filter)
            .await
            .expect("读日志失败");
        logs.into_iter().next().expect("应当留下一条请求流水")
    }
}

impl Drop for TestApp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 上游的 429：消息与 code 照抄真实网关的写法。
fn rate_limited_body() -> Value {
    json!({
        "error": {
            "message": "inference exceeds tpm/rpm limit",
            "type": "rate_limit_error",
            "code": "RateLimitExceeded.EndpointRPMExceeded",
        }
    })
}

/// 前 `fail_first` 次回 429（带 `Retry-After: 7`），之后回一个最小可用响应。
fn flaky_upstream(hits: Arc<AtomicUsize>, fail_first: usize) -> Router {
    Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let hits = Arc::clone(&hits);
            async move {
                let hit = hits.fetch_add(1, Ordering::SeqCst);

                if hit < fail_first {
                    let mut headers = HeaderMap::new();
                    headers.insert(header::RETRY_AFTER, HeaderValue::from_static("7"));
                    return (
                        StatusCode::TOO_MANY_REQUESTS,
                        headers,
                        Json(rate_limited_body()),
                    )
                        .into_response();
                }

                Json(json!({
                    "id": "mock-id",
                    "object": "chat.completion",
                    "model": "demo-model",
                    "choices": [{
                        "index": 0,
                        "message": { "role": "assistant", "content": "mock reply" },
                        "finish_reason": "stop",
                    }],
                    "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 },
                }))
                .into_response()
            }
        }),
    )
}

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定 mock 上游端口失败");
    let addr = listener.local_addr().expect("读取 mock 端口失败");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    format!("http://{addr}")
}

fn demo_key() -> ApiKey {
    ApiKey {
        id: "key-1".to_string(),
        name: "demo".to_string(),
        secret: "mock-secret".to_string(),
        key_prefix: "mock".to_string(),
        credits_scaled: 500_000,
        unlimited: false,
        enabled: true,
        allowed_models: Vec::new(),
        created_at: 0,
        updated_at: 0,
        last_used_at: None,
    }
}

fn chat_body() -> Value {
    json!({
        "model": "demo-model",
        "messages": [{ "role": "user", "content": "hi" }],
    })
}

async fn call_chat(state: &Arc<AppState>) -> Result<Value, Error> {
    proxy::chat_completion(
        state,
        &demo_key(),
        &chat_body(),
        "127.0.0.1",
        Some("mock-agent"),
        None,
    )
    .await
}

/// 上游限流：下游拿到 429 + `Retry-After`，且只打了上游一次。
#[tokio::test]
async fn upstream_rate_limit_becomes_429_with_retry_after() {
    let app = TestApp::new().await;
    let hits = Arc::new(AtomicUsize::new(0));
    let base_url = serve(flaky_upstream(Arc::clone(&hits), usize::MAX)).await;
    app.seed(&base_url, &["mock-upstream-secret"]).await;

    let error = call_chat(&app.state)
        .await
        .expect_err("上游一直 429，请求应当失败");

    match &error {
        Error::RateLimited { retry_after, .. } => {
            assert_eq!(*retry_after, Some(7), "采纳上游的 Retry-After");
        }
        other => panic!("下游应当拿到限流语义，实际是 {other:?}"),
    }

    // 真正的下游契约：状态码 + 响应头 + 错误类型。
    let response = error.into_response();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        response
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok()),
        Some("7"),
        "429 必须带 Retry-After，客户端据此退避"
    );

    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("读错误响应体失败");
    let value: Value = serde_json::from_slice(&body).expect("错误体应当是 JSON");
    assert_eq!(value["error"]["type"], "rate_limited");

    // 唯一一把密钥已被退避，第二次尝试连上游都没碰到——不能拿 429 反复捶上游。
    assert_eq!(hits.load(Ordering::SeqCst), 1, "退避期内不该再打上游");

    let log = app.latest_log_row().await;
    assert_eq!(log.status, "error");
    let detail = log.error_detail.unwrap_or_default();
    assert!(
        detail.contains("共 2 次尝试") && detail.contains("429"),
        "请求流水要留下每一轮的经过，实际是：{detail}"
    );
    assert!(
        detail.contains("inference exceeds tpm/rpm limit"),
        "日志要留下上游原话（对下游只给稳妥话术），实际是：{detail}"
    );
    assert!(
        detail.contains("都在退避中"),
        "第二轮「选不出密钥」也是这次失败的一部分，实际是：{detail}"
    );
}

/// 密钥都还在退避里时，紧接着的请求仍回 429 + `Retry-After`，而不是「无可用密钥」。
#[tokio::test]
async fn requests_during_cooldown_still_report_rate_limited() {
    let app = TestApp::new().await;
    let hits = Arc::new(AtomicUsize::new(0));
    let base_url = serve(flaky_upstream(Arc::clone(&hits), usize::MAX)).await;
    app.seed(&base_url, &["mock-upstream-secret"]).await;

    // 第一次：被上游 429，密钥进入退避。
    let first = call_chat(&app.state).await.expect_err("第一次应当失败");
    assert!(matches!(first, Error::RateLimited { .. }));

    // 第二次：块内没有可调度的密钥，但语义仍是「限流」，不是 403。
    let second = call_chat(&app.state).await.expect_err("退避期内应当失败");

    match &second {
        Error::RateLimited { retry_after, .. } => {
            let wait = retry_after.expect("退避期内应当给出剩余等待秒数");
            assert!(
                (1..=7).contains(&wait),
                "剩余等待应当落在退避窗口内，实际 {wait}"
            );
        }
        other => panic!("退避期内应当是 429，实际是 {other:?}"),
    }

    let response = second.into_response();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(response.headers().contains_key(header::RETRY_AFTER));

    assert_eq!(hits.load(Ordering::SeqCst), 1, "两次请求只该打上游一次");
}

/// 换钥匙重试成功：日志记成功，但重试链照留——「先被限流、换一把就成了」看得见。
#[tokio::test]
async fn successful_retry_keeps_the_chain_in_the_log() {
    let app = TestApp::new().await;
    let hits = Arc::new(AtomicUsize::new(0));
    // 第一把密钥吃 429，第二把密钥成功。
    let base_url = serve(flaky_upstream(Arc::clone(&hits), 1)).await;
    app.seed(&base_url, &["mock-key-1", "mock-key-2"]).await;

    call_chat(&app.state).await.expect("换一把密钥后应当成功");

    assert_eq!(hits.load(Ordering::SeqCst), 2, "应当换了一把钥匙再试");

    let log = app.latest_log_row().await;
    assert_eq!(log.status, "success");
    let detail = log.error_detail.unwrap_or_default();
    assert!(
        detail.contains("共 1 次尝试") && detail.contains("最终成功"),
        "成功行也要留下重试链，实际是：{detail}"
    );
}
