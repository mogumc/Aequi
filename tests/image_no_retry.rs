//! 生图**不重试**：一次请求一次结果，错误如实返回。
//!
//! 这是生图与对话的关键差异。对话换把钥匙重试几乎免费（上游没产出就不计费），
//! 出图却是有副作用的慢操作——单张图动辄几十秒，重试把最长的等待翻倍，而且
//! 上游很可能**已经出图并计了费**，我们却拿不到结果。所以生图只发一次，
//! 失败就把错误交回调用方；但「为什么会失败」仍照记到密钥头上。
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
        let dir = std::env::temp_dir().join(format!("aequi-image-{}", random::string(12)));
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

    /// 建块 + 两把上游密钥 + 一把有余额的访问密钥。
    ///
    /// **必须放两把**：只有一把钥匙时「没重试」与「重试但挑不出第二把」会
    /// 得到同样的调用次数，测不出区别。
    async fn seed(&self, base_url: &str) -> Vec<String> {
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
                secrets: vec![
                    "mock-upstream-secret-1".to_string(),
                    "mock-upstream-secret-2".to_string(),
                ],
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

        upstream_service::list_keys(self.data(), "demo-block")
            .await
            .expect("读密钥失败")
            .into_iter()
            .map(|key| key.id)
            .collect()
    }
}

impl Drop for TestApp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 生图 mock 上游：每次调用都按给定的状态码回应，并记下被调用几次。
fn fail_upstream(hits: Arc<AtomicUsize>, status: StatusCode, body: Value) -> Router {
    Router::new().route(
        "/v1/images/generations",
        post(move || {
            let hits = Arc::clone(&hits);
            let body = body.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                (status, Json(body)).into_response()
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

fn image_body() -> Value {
    json!({ "model": "demo-model", "prompt": "一只猫" })
}

async fn call_image(state: &Arc<AppState>) -> Result<Value, Error> {
    proxy::image::images_generations(state, &demo_key(), &image_body(), "127.0.0.1", None, None)
        .await
}

/// 5xx：只打上游一次，错误如实返回。
#[tokio::test]
async fn image_generation_does_not_retry_on_5xx() {
    let app = TestApp::new().await;
    let hits = Arc::new(AtomicUsize::new(0));
    let base_url = serve(fail_upstream(
        Arc::clone(&hits),
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({ "error": { "message": "upstream exploded" } }),
    ))
    .await;
    app.seed(&base_url).await;

    let error = call_image(&app.state)
        .await
        .expect_err("上游 5xx，生图应当失败");

    assert!(
        matches!(error, Error::UpstreamUnavailable(_)),
        "应当如实报上游异常，实际 {error:?}"
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "生图绝不能重试——块内有两把钥匙，多打一次说明换了钥匙重发"
    );
}

/// 被限流：同样只打一次，但把 429 语义与等待提示带给调用方。
#[tokio::test]
async fn image_generation_does_not_retry_on_rate_limit() {
    let app = TestApp::new().await;
    let hits = Arc::new(AtomicUsize::new(0));
    let base_url = serve(fail_upstream(
        Arc::clone(&hits),
        StatusCode::TOO_MANY_REQUESTS,
        json!({ "error": { "message": "rate limit exceeded" } }),
    ))
    .await;
    let key_ids = app.seed(&base_url).await;

    let error = call_image(&app.state)
        .await
        .expect_err("上游限流，生图应当失败");

    match &error {
        Error::RateLimited { retry_after, .. } => {
            assert!(
                retry_after.is_some_and(|wait| wait > 0),
                "限流要带等多久再来，实际 {retry_after:?}"
            );
        }
        other => panic!("下游应当拿到限流语义，实际 {other:?}"),
    }
    assert_eq!(hits.load(Ordering::SeqCst), 1, "生图不重试");

    // 「不重试」不等于「不记账」：这次失败仍记在那把钥匙头上（进了退避）。
    let mut cooling = 0;
    for key_id in &key_ids {
        let key = db::block::find_key(app.data(), "demo-block", key_id)
            .await
            .expect("读密钥失败")
            .expect("密钥应当存在");
        if key.cooldown_until.is_some() {
            cooling += 1;
        }
    }
    assert_eq!(cooling, 1, "被打中的那把钥匙应当进入退避，另一把不受牵连");
}

/// 4xx（尺寸不支持这类）：不重试，原样报错。
#[tokio::test]
async fn image_generation_does_not_retry_on_bad_request() {
    let app = TestApp::new().await;
    let hits = Arc::new(AtomicUsize::new(0));
    let base_url = serve(fail_upstream(
        Arc::clone(&hits),
        StatusCode::BAD_REQUEST,
        json!({ "error": { "message": "unsupported size" } }),
    ))
    .await;
    app.seed(&base_url).await;

    call_image(&app.state).await.expect_err("参数被拒应当失败");
    assert_eq!(hits.load(Ordering::SeqCst), 1, "生图不重试");
}
