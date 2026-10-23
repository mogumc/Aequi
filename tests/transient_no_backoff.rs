//! 上游故障（5xx）**不挂退避**：一次请求内部会换密钥试三次，若这三次把块内
//! 每把钥匙都按住，整块就从「这次失败」升级成「上游不可用」——上游缓过来时
//! 一把能试的都没有。
//!
//! 断言三件事——
//! 1) 一场 5xx 之后，块内密钥仍然可调度：`cooldown_until` 为空、失败计数不涨，
//!    `last_error` 照记（后台看得见最近一次失败）；
//! 2) 上游恢复后紧接着的请求能立刻成功，不用等任何退避到期；
//! 3) 只有 429 才退避——同一个上游先 5xx 再 429，退避仍从阶梯第一档起算。
//!
//! 所有凭据均为测试用假数据，不涉及任何真实服务。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Json;
use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use serde_json::{Value, json};

use aequi::config::Settings;
use aequi::db;
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
        let dir = std::env::temp_dir().join(format!("aequi-transient-{}", random::string(12)));
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

    async fn keys(&self) -> Vec<aequi::model::UpstreamKey> {
        db::block::list_keys(self.data(), "demo-block")
            .await
            .expect("读密钥失败")
    }
}

impl Drop for TestApp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 上游故障体：照真实网关的写法。
fn upstream_error_body(message: &str) -> Value {
    json!({ "error": { "message": message, "type": "server_error" } })
}

/// 前 `fail_first` 次回 `status`，之后回一个最小可用的对话响应。
fn flaky_upstream(hits: Arc<AtomicUsize>, fail_first: usize, status: StatusCode) -> Router {
    Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let hits = Arc::clone(&hits);
            async move {
                let hit = hits.fetch_add(1, Ordering::SeqCst);

                if hit < fail_first {
                    return (
                        status,
                        Json(upstream_error_body("upstream exploded")),
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

/// 前 `fail_first` 次回 502，之后一直回 429——模拟「上游先病、后限流」。
fn failing_upstream(hits: Arc<AtomicUsize>, fail_first: usize) -> Router {
    Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let hits = Arc::clone(&hits);
            async move {
                let hit = hits.fetch_add(1, Ordering::SeqCst);
                let status = if hit < fail_first {
                    StatusCode::BAD_GATEWAY
                } else {
                    StatusCode::TOO_MANY_REQUESTS
                };
                (status, Json(upstream_error_body("mock upstream failure"))).into_response()
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

async fn call_chat(state: &Arc<AppState>) -> Result<Value, Error> {
    proxy::chat_completion(
        state,
        &demo_key(),
        &json!({
            "model": "demo-model",
            "messages": [{ "role": "user", "content": "hi" }],
        }),
        "127.0.0.1",
        Some("mock-agent"),
        None,
    )
    .await
}

/// 一场持续的 5xx：请求失败，但块内密钥一把都没被按住。
#[tokio::test]
async fn upstream_failures_leave_the_keys_schedulable() {
    let app = TestApp::new().await;
    let hits = Arc::new(AtomicUsize::new(0));
    let base_url = serve(flaky_upstream(
        Arc::clone(&hits),
        usize::MAX,
        StatusCode::BAD_GATEWAY,
    ))
    .await;
    app.seed(&base_url, &["mock-upstream-1", "mock-upstream-2"]).await;

    let error = call_chat(&app.state).await.expect_err("上游一直 502，应当失败");
    assert!(
        matches!(error, Error::UpstreamUnavailable(_)),
        "上游故障对下游是 502 语义，实际 {error:?}"
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        3,
        "三次尝试用完就收手，不无限重试"
    );

    for key in app.keys().await {
        assert!(
            key.cooldown_until.is_none(),
            "上游故障不该给密钥挂退避（{}）",
            key.id
        );
        assert_eq!(key.failure_count, 0, "不是这套钥匙的错，计数别涨");
        assert_eq!(
            key.last_error.as_deref(),
            Some("上游返回 502 upstream exploded"),
            "原因仍要留痕，后台查得出这次为什么没成"
        );
        assert_eq!(
            key.status,
            aequi::model::KeyStatus::Active,
            "上游故障不该停用密钥"
        );
    }

    // 再打一次：钥匙都还在队里，所以仍然会去打上游，而不是回「无可用密钥」。
    let again = call_chat(&app.state).await.expect_err("上游还没好，仍然失败");
    assert!(
        matches!(again, Error::UpstreamUnavailable(_)),
        "第二次仍该是上游故障语义，实际 {again:?}"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 6, "钥匙没被按住，第二次照常打三回");
}

/// 上游恢复后紧接着的请求就能成功——不用等任何退避到期。
#[tokio::test]
async fn a_recovered_upstream_is_usable_immediately() {
    let app = TestApp::new().await;
    let hits = Arc::new(AtomicUsize::new(0));
    // 第一次请求的三次尝试正好把前三个 502 吃掉，第四次起上游恢复正常。
    let base_url = serve(flaky_upstream(
        Arc::clone(&hits),
        3,
        StatusCode::SERVICE_UNAVAILABLE,
    ))
    .await;
    app.seed(&base_url, &["mock-upstream-1", "mock-upstream-2"]).await;

    call_chat(&app.state)
        .await
        .expect_err("前三次都是 503，第一次请求应当失败");

    call_chat(&app.state)
        .await
        .expect("上游已经好了，紧接着的请求应当成功");
    assert_eq!(hits.load(Ordering::SeqCst), 4, "第二次请求一次就成");

    let keys = app.keys().await;
    assert!(
        keys.iter().any(|key| key.last_used_at.is_some()),
        "成功用过的钥匙要记 last_used_at"
    );
    assert!(
        keys.iter().all(|key| key.cooldown_until.is_none()),
        "全程没有任何一把钥匙进过退避"
    );
}

/// 先 5xx 再 429：退避仍从阶梯第一档（10 秒）起算，没被上游故障顶上去。
#[tokio::test]
async fn upstream_failures_do_not_stretch_the_rate_limit_ladder() {
    let app = TestApp::new().await;
    let hits = Arc::new(AtomicUsize::new(0));
    // 前三次（第一次请求的三次尝试）回 502，之后一直 429：
    // 第一次请求把三次上游故障留在 last_error 里，第二次请求才撞上真正的限流。
    let base_url = serve(failing_upstream(Arc::clone(&hits), 3)).await;
    app.seed(&base_url, &["mock-upstream-1", "mock-upstream-2"]).await;

    let first = call_chat(&app.state).await.expect_err("前三次都是 502");
    assert!(matches!(first, Error::UpstreamUnavailable(_)), "实际 {first:?}");

    let second = call_chat(&app.state).await.expect_err("上游一直 429");
    assert!(matches!(second, Error::RateLimited { .. }), "实际 {second:?}");

    let now = aequi::utils::time::now_secs();
    for key in app.keys().await {
        let until = key.cooldown_until.expect("撞上 429 的钥匙要退避");
        assert_eq!(
            key.failure_count, 1,
            "限流计数从 1 起——之前的 502 不该计进去"
        );
        assert!(
            until - now <= 10,
            "退避该是 10 秒档（阶梯第一档），实际还剩 {} 秒",
            until - now
        );
    }
}
