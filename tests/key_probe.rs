//! 测试上游密钥走的是**真实对话请求**，不是列模型接口。
//!
//! 起因：探测曾经复用 `GET /v1/models`，但不少网关、中转站与自建代理
//! 根本没实现这个接口，于是「密钥有效」被误判成「密钥被拒」并直接停用。
//! 这里用 mock 上游固定住三件事——
//! 1) 探测打的是对话端点，带上指定的模型名；
//! 2) 模型名按块的 `forward` 配置替换，不拿对外名去打上游；
//! 3) 「拉取模型列表」仍走列模型接口，两者互不影响。
//!
//! 所有凭据均为测试用假数据，不涉及任何真实服务。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::Router;
use axum::routing::{get, post};
use serde_json::{Value, json};

use aequi::config::Settings;
use aequi::db;
use aequi::model::{ApiFormat, BlockCreate, ModelEntry, UpstreamKeyCreate};
use aequi::proxy::adapter::ProbeOutcome;
use aequi::service::upstream as upstream_service;
use aequi::utils::random;
use aequi::{AppState, Error};

/// mock 上游记下的每次请求：`(路径, 请求体)`。GET 请求体为 `Null`。
type Hits = Arc<Mutex<Vec<(String, Value)>>>;

struct TestApp {
    state: Arc<AppState>,
    dir: PathBuf,
}

impl TestApp {
    async fn new() -> TestApp {
        let dir = std::env::temp_dir().join(format!("aequi-probe-{}", random::string(12)));
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

    /// 建块（可指定模型条目的转发名）+ 一把上游密钥；返回密钥 id。
    async fn seed(&self, base_url: &str, forward: Option<&str>) -> String {
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
                    forward: forward.map(str::to_string),
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

        upstream_service::list_keys(self.data(), "demo-block")
            .await
            .expect("读密钥失败")
            .into_iter()
            .next()
            .expect("应当有一把上游密钥")
            .id
    }
}

impl Drop for TestApp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// mock 上游：记下每个请求，两个端点都回最小可用响应。
fn recording_upstream(hits: Hits) -> Router {
    let chat_hits = Arc::clone(&hits);
    let models_hits = Arc::clone(&hits);

    Router::new()
        .route(
            "/v1/chat/completions",
            post(move |Json(body): Json<Value>| {
                let hits = Arc::clone(&chat_hits);
                async move {
                    hits.lock()
                        .expect("锁")
                        .push(("/v1/chat/completions".to_string(), body));
                    Json(json!({
                        "id": "mock-id",
                        "object": "chat.completion",
                        "model": "demo-model",
                        "choices": [{
                            "index": 0,
                            "message": { "role": "assistant", "content": "pong" },
                            "finish_reason": "stop",
                        }],
                        "usage": { "prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4 },
                    }))
                }
            }),
        )
        .route(
            "/v1/models",
            get(move || {
                let hits = Arc::clone(&models_hits);
                async move {
                    hits.lock()
                        .expect("锁")
                        .push(("/v1/models".to_string(), Value::Null));
                    Json(json!({ "data": [{ "id": "demo-model" }] }))
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

fn seen_paths(hits: &Hits) -> Vec<String> {
    hits.lock()
        .expect("锁")
        .iter()
        .map(|(path, _)| path.clone())
        .collect()
}

/// 探测打的是对话端点，且带上指定的模型名。
#[tokio::test]
async fn probing_uses_a_real_chat_request() {
    let app = TestApp::new().await;
    let hits: Hits = Arc::new(Mutex::new(Vec::new()));
    let base_url = serve(recording_upstream(Arc::clone(&hits))).await;
    let key_id = app.seed(&base_url, None).await;

    let outcome = upstream_service::test_key(
        app.data(),
        &app.state.upstream,
        "demo-block",
        &key_id,
        "demo-model",
    )
    .await
    .expect("测试密钥失败");

    assert!(
        matches!(outcome, ProbeOutcome::Valid),
        "mock 上游返回了正常响应"
    );

    let hits = hits.lock().expect("锁");
    assert_eq!(
        hits.len(),
        1,
        "探测应当只发一次请求，实际打到：{:?}",
        seen_paths(&Arc::new(Mutex::new(hits.clone())))
    );
    let (path, body) = &hits[0];
    assert_eq!(
        path, "/v1/chat/completions",
        "探测必须走真实对话端点——列模型接口很多上游没有实现"
    );
    assert_eq!(body["model"], "demo-model", "带上被测试的模型名");
    assert!(
        body.get("messages").is_some(),
        "真实请求要有对话内容，实际：{body}"
    );
    assert!(
        body.get("max_tokens").is_none() && body.get("max_completion_tokens").is_none(),
        "探测不带输出上限——各模型认的字段名不一样，加错了会把好密钥判成坏的"
    );
}

/// 探测按块的 `forward` 配置换名，不拿对外名去打上游。
#[tokio::test]
async fn probing_uses_the_forwarded_model_name() {
    let app = TestApp::new().await;
    let hits: Hits = Arc::new(Mutex::new(Vec::new()));
    let base_url = serve(recording_upstream(Arc::clone(&hits))).await;
    // 块里对外叫 demo-model，转发给上游时叫 mock-upstream-model。
    let key_id = app.seed(&base_url, Some("mock-upstream-model")).await;

    upstream_service::test_key(
        app.data(),
        &app.state.upstream,
        "demo-block",
        &key_id,
        "demo-model",
    )
    .await
    .expect("测试密钥失败");

    let hits = hits.lock().expect("锁");
    assert_eq!(
        hits[0].1["model"], "mock-upstream-model",
        "配了 forward 的块要拿上游认的名字去探测，否则好钥匙会被误判"
    );
}

/// 拉取模型列表仍然走列模型接口——它没有别的替代，别被探测的改动带走。
#[tokio::test]
async fn fetching_models_still_uses_the_listing_endpoint() {
    let app = TestApp::new().await;
    let hits: Hits = Arc::new(Mutex::new(Vec::new()));
    let base_url = serve(recording_upstream(Arc::clone(&hits))).await;
    app.seed(&base_url, None).await;

    let models = upstream_service::fetch_models(app.data(), &app.state.upstream, "demo-block")
        .await
        .expect("拉取模型应当成功");

    assert_eq!(models, vec!["demo-model".to_string()]);
    assert_eq!(
        seen_paths(&hits),
        vec!["/v1/models".to_string()],
        "拉模型列表只能走列模型接口"
    );
}

/// 块里一个模型都没配时，测试给的是明确的 400 而不是「密钥无效」。
#[tokio::test]
async fn probing_without_any_configured_model_is_a_clear_error() {
    let app = TestApp::new().await;
    let hits: Hits = Arc::new(Mutex::new(Vec::new()));
    let base_url = serve(recording_upstream(Arc::clone(&hits))).await;

    upstream_service::create_block(
        app.data(),
        BlockCreate {
            id: "empty-block".to_string(),
            name: "empty".to_string(),
            api_format: ApiFormat::Openai,
            weight: 1,
            max_concurrency: 0,
            base_url: base_url.clone(),
            proxy: None,
            models: Vec::new(),
            headers: Vec::new(),
        },
    )
    .await
    .expect("建块失败");

    upstream_service::add_keys(
        app.data(),
        "empty-block",
        UpstreamKeyCreate {
            secrets: vec!["mock-upstream-secret".to_string()],
            weight: 1,
            never_disable: false,
        },
    )
    .await
    .expect("加上游密钥失败");

    let block = upstream_service::get_block(app.data(), "empty-block")
        .await
        .expect("读块失败");
    let error = upstream_service::probe_model(&block).expect_err("没有模型时应当报错");
    assert!(
        matches!(error, Error::BadRequest(_)),
        "这是配置问题（400），不是密钥问题：{error:?}"
    );
    assert!(seen_paths(&hits).is_empty(), "配置不全时不该白打一次上游");
}
