//! 上游密钥的「上次使用」与失败计数，在**真实使用**后要更新。
//!
//! 流式这条路上曾经漏了「记一次成功」这一步：`last_used_at` 一直是空的
//! （后台显示「从未」），失败计数也永远不清零——一把出过两次软错的钥匙
//! 哪怕之后成功服务了成千上万条流式请求，仍带着 2 次计数躺在那里，
//! 再吃一次硬失败就被自动停用。非流式与生图没这个问题。
//!
//! 所有凭据均为测试用假数据，不涉及任何真实服务。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::routing::post;
use futures_util::StreamExt;
use serde_json::{Value, json};

use aequi::config::Settings;
use aequi::db;
use aequi::model::{ApiFormat, ApiKey, BlockCreate, ModelEntry, UpstreamKeyCreate};
use aequi::service::upstream as upstream_service;
use aequi::utils::{random, time};
use aequi::{AppState, proxy};

struct TestApp {
    state: Arc<AppState>,
    dir: PathBuf,
}

impl TestApp {
    async fn new() -> TestApp {
        let dir = std::env::temp_dir().join(format!("aequi-last-used-{}", random::string(12)));
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

    /// 建块 + 一把上游密钥 + 一把有余额的访问密钥；返回上游密钥 id。
    async fn seed(&self, base_url: &str) -> String {
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
                secrets: vec!["mock-upstream-secret".to_string()],
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
            .next()
            .expect("应当有一把上游密钥")
            .id
    }

    /// 先给这把钥匙记上「用过两次、还在报错」，用来验证成功会把它们清掉。
    async fn seed_prior_failures(&self, key_id: &str) {
        let mut connection = self.data().acquire().await.expect("取连接失败");
        db::block::update_key_runtime(
            connection.as_mut(),
            key_id,
            2,
            None,
            Some("旧错误：上游返回 502".to_string()),
            time::now_secs(),
        )
        .await
        .expect("写入旧失败失败");
    }
}

impl Drop for TestApp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 流式 mock 上游：两帧内容 + 结束哨兵。
fn streaming_upstream() -> Router {
    Router::new().route(
        "/v1/chat/completions",
        post(move || async move {
            let stream = async_stream::stream! {
                for index in 0..2 {
                    let chunk = json!({
                        "id": "mock-id",
                        "object": "chat.completion.chunk",
                        "model": "demo-model",
                        "choices": [{
                            "index": 0,
                            "delta": { "content": format!("tok{index} ") },
                            "finish_reason": Value::Null,
                        }],
                    });
                    yield Ok::<_, std::io::Error>(format!("data: {chunk}\n\n"));
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                yield Ok::<_, std::io::Error>("data: [DONE]\n\n".to_string());
            };

            (
                [("content-type", "text/event-stream")],
                axum::body::Body::from_stream(stream),
            )
        }),
    )
}

/// 非流式 mock 上游：一个最小可用响应。
fn plain_upstream() -> Router {
    Router::new().route(
        "/v1/chat/completions",
        post(|| async {
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

fn request_body(stream: bool) -> Value {
    json!({
        "model": "demo-model",
        "messages": [{ "role": "user", "content": "hi" }],
        "stream": stream,
    })
}

/// 库里那把上游密钥的运行状态。
async fn key_state(app: &TestApp, key_id: &str) -> (Option<i64>, u32, Option<String>) {
    let key = db::block::find_key(app.data(), "demo-block", key_id)
        .await
        .expect("读密钥失败")
        .expect("密钥应当存在");
    (key.last_used_at, key.failure_count, key.last_error)
}

#[tokio::test]
async fn streaming_marks_the_key_as_used_and_clears_failures() {
    let app = TestApp::new().await;
    let base_url = serve(streaming_upstream()).await;
    let key_id = app.seed(&base_url).await;
    app.seed_prior_failures(&key_id).await;

    let before = time::now_secs();
    let mut stream = Box::pin(
        proxy::chat_completion_stream(
            app.state.clone(),
            demo_key(),
            request_body(true),
            "127.0.0.1".to_string(),
            Some("mock-agent".to_string()),
            None,
        )
        .await
        .expect("建流应当成功"),
    );
    while let Some(chunk) = stream.next().await {
        chunk.expect("流式帧应当正常");
    }

    let (last_used_at, failure_count, last_error) = key_state(&app, &key_id).await;
    let used = last_used_at.expect("流式用过之后必须写下上次使用时间");
    assert!(used >= before, "上次使用时间应当是刚刚，实际 {used} < {before}");
    assert_eq!(failure_count, 0, "一次成功要清零失败计数");
    assert_eq!(last_error, None, "成功要把旧的错误信息清掉");
}

#[tokio::test]
async fn non_streaming_marks_the_key_as_used_too() {
    let app = TestApp::new().await;
    let base_url = serve(plain_upstream()).await;
    let key_id = app.seed(&base_url).await;

    let before = time::now_secs();
    proxy::chat_completion(
        &app.state,
        &demo_key(),
        &request_body(false),
        "127.0.0.1",
        Some("mock-agent"),
        None,
    )
    .await
    .expect("请求应当成功");

    let (last_used_at, failure_count, _) = key_state(&app, &key_id).await;
    let used = last_used_at.expect("用过之后必须写下上次使用时间");
    assert!(used >= before, "上次使用时间应当是刚刚，实际 {used} < {before}");
    assert_eq!(failure_count, 0);
}
