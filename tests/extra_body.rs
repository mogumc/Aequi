//! 模型级 `extra_body`：只认管理端配置，下游塞的同名键一律丢弃。
//!
//! mock 上游把收到的请求体原样记下来，再回一个最小可用响应，然后断言
//! 「管理端配的到了、下游塞的没到」。对话走标准层的白名单解析（天然丢弃
//! 不认识的键），生图是透传路径（要显式剥离）——两条路各验一遍。
//!
//! 所有凭据均为测试用假数据，不涉及任何真实服务。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::Router;
use axum::routing::post;
use serde_json::{Value, json};

use aequi::config::Settings;
use aequi::db;
use aequi::model::{ApiFormat, ApiKey, BlockCreate, ModelEntry, UpstreamKeyCreate};
use aequi::service::upstream as upstream_service;
use aequi::utils::random;
use aequi::{AppState, proxy};

struct TestApp {
    state: Arc<AppState>,
    dir: PathBuf,
}

impl TestApp {
    async fn new() -> TestApp {
        let dir = std::env::temp_dir().join(format!("aequi-extra-body-{}", random::string(12)));
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

    /// 建块：模型条目带上指定的 extra_body，配一把上游密钥和一把有余额的访问密钥。
    async fn seed(&self, base_url: &str, extra_body: Option<Value>) {
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
                    extra_body,
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
    }
}

impl Drop for TestApp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 记下每个上游请求的 body，再回一个最小可用响应。
///
/// 对话与非流式生图各一个端点；哨兵下标用来区分（测试按顺序取最后一条即可）。
fn recording_upstream(sink: Arc<Mutex<Vec<Value>>>) -> Router {
    let chat_sink = Arc::clone(&sink);
    let image_sink = Arc::clone(&sink);

    Router::new()
        .route(
            "/v1/chat/completions",
            post(move |Json(body): Json<Value>| {
                let sink = Arc::clone(&chat_sink);
                async move {
                    sink.lock().expect("锁").push(body);
                    Json(json!({
                        "id": "mock-id",
                        "object": "chat.completion",
                        "model": "demo-model",
                        "choices": [{
                            "index": 0,
                            "message": { "role": "assistant", "content": "mock reply" },
                            "finish_reason": "stop",
                        }],
                        "usage": {
                            "prompt_tokens": 10,
                            "completion_tokens": 5,
                            "total_tokens": 15,
                        },
                    }))
                }
            }),
        )
        .route(
            "/v1/images/generations",
            post(move |Json(body): Json<Value>| {
                let sink = Arc::clone(&image_sink);
                async move {
                    sink.lock().expect("锁").push(body);
                    Json(json!({
                        "created": 1,
                        "data": [{ "b64_json": "bW9jaw==" }],
                    }))
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

/// 取出上游收到的最后一个请求体。
fn last_seen(sink: &Arc<Mutex<Vec<Value>>>) -> Value {
    sink.lock()
        .expect("锁")
        .last()
        .cloned()
        .expect("上游应当收到请求")
}

#[tokio::test]
async fn configured_extra_body_reaches_the_upstream() {
    let app = TestApp::new().await;
    let sink = Arc::new(Mutex::new(Vec::new()));
    let base_url = serve(recording_upstream(Arc::clone(&sink))).await;
    app.seed(&base_url, Some(json!({ "top_k": 40, "seed": 7 })))
        .await;

    proxy::chat_completion(
        &app.state,
        &demo_key(),
        &chat_body(),
        "127.0.0.1",
        Some("mock-agent"),
        None,
    )
    .await
    .expect("请求应当成功");

    let body = last_seen(&sink);
    assert_eq!(body["top_k"], 40, "管理端配的参数要并入请求体");
    assert_eq!(body["seed"], 7);
    assert_eq!(body["model"], "demo-model", "原本的字段不受影响");
    assert!(body.get("extra_body").is_none(), "包裹层不该出现在上游请求里");
}

#[tokio::test]
async fn downstream_extra_body_is_ignored() {
    let app = TestApp::new().await;
    let sink = Arc::new(Mutex::new(Vec::new()));
    let base_url = serve(recording_upstream(Arc::clone(&sink))).await;
    app.seed(&base_url, Some(json!({ "top_k": 40 }))).await;

    // 下游想自己塞额外参数（数值与配置故意不同，便于分辨谁生效）。
    let mut body = chat_body();
    body["extra_body"] = json!({ "top_k": 999, "seed": 123 });

    proxy::chat_completion(&app.state, &demo_key(), &body, "127.0.0.1", Some("mock-agent"), None)
        .await
        .expect("请求应当成功");

    let seen = last_seen(&sink);
    assert_eq!(
        seen["top_k"], 40,
        "生效的必须是管理端配置的值——999 说明下游注入成功了"
    );
    assert!(seen.get("seed").is_none(), "下游塞的键绝不能进上游请求体");
    assert!(
        seen.get("extra_body").is_none(),
        "包裹层本身也不该被转发"
    );
}

#[tokio::test]
async fn image_strips_downstream_extra_body_and_applies_configured_one() {
    let app = TestApp::new().await;
    let sink = Arc::new(Mutex::new(Vec::new()));
    let base_url = serve(recording_upstream(Arc::clone(&sink))).await;
    app.seed(&base_url, Some(json!({ "quality": "hd" }))).await;

    // 生图是透传路径：下游自己塞的 `extra_body` 不会被白名单解析挡住。
    let body = json!({
        "model": "demo-model",
        "prompt": "一只猫",
        "extra_body": { "quality": "low", "watermark": true },
    });

    proxy::image::images_generations(&app.state, &demo_key(), &body, "127.0.0.1", None, None)
        .await
        .expect("生图应当成功");

    let seen = last_seen(&sink);
    assert!(
        seen.get("extra_body").is_none(),
        "透传路径也要剥掉下游的包裹层"
    );
    assert!(
        seen.get("watermark").is_none(),
        "下游塞的键不能进上游请求体"
    );
    assert_eq!(seen["quality"], "hd", "管理端配置的值生效");
    assert_eq!(seen["prompt"], "一只猫", "透传的其余字段不受影响");
}
