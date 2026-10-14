//! 块的并发上限与排队：配了上限就限，没配就不限。
//!
//! 起一个会「数自己同时在飞几个请求」的本地 mock 上游，用真实的
//! `proxy::chat_completion` 打过去，验证：
//! - 上限之内全部成功，且在飞数从不超过上限（排队真的在限流）；
//! - 排队的请求不会失败——等到位子后照常完成；
//! - 上限为 0（不限制）时在飞数可以超过小上限（对照组，证明限得上）；
//! - 管理面的列表接口能实时看见在飞数与排队数。
//!
//! 所有凭据均为测试用假数据，不涉及任何真实服务。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::routing::post;

use aequi::config::Settings;
use aequi::db;
use aequi::model::{ApiFormat, BlockCreate, ModelEntry, UpstreamKeyCreate};
use aequi::service::upstream as upstream_service;
use aequi::utils::random;
use aequi::{AppState, model::ApiKey, proxy};

/// 每个 mock 请求停留的时间：足够让多个请求在飞上重叠，
/// 又不会把测试拖得太久。
const HOLD: Duration = Duration::from_millis(150);

/// 同时打到 mock 上游的请求数。
const CONCURRENT_REQUESTS: usize = 6;

struct TestApp {
    state: Arc<AppState>,
    dir: PathBuf,
}

impl TestApp {
    async fn new() -> TestApp {
        let dir = std::env::temp_dir().join(format!("aequi-concurrency-{}", random::string(12)));
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

    /// 建一个带并发上限的块，配一把上游密钥和一把有余额的访问密钥。
    async fn seed(&self, base_url: &str, max_concurrency: u32) {
        upstream_service::create_block(
            self.data(),
            BlockCreate {
                id: "demo-block".to_string(),
                name: "demo".to_string(),
                api_format: ApiFormat::Openai,
                weight: 1,
                max_concurrency,
                base_url: base_url.to_string(),
                proxy: None,
                models: vec![ModelEntry {
                    name: "demo-model".to_string(),
                    forward: None,
                    multiplier_bp: 10_000,
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

/// 同时在飞计数：当前值、历史峰值、累计请求数。
#[derive(Default)]
struct InFlight {
    now: AtomicUsize,
    peak: AtomicUsize,
    total: AtomicUsize,
}

/// mock 上游：非流式对话接口。进来先记一笔在飞，停留 `HOLD` 再回一个
/// 合法的 OpenAI 响应——停留期间连接一直占着，闸门限的就是它。
fn counting_upstream(counter: Arc<InFlight>) -> Router {
    Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let counter = Arc::clone(&counter);
            async move {
                let now = counter.now.fetch_add(1, Ordering::SeqCst) + 1;
                counter.peak.fetch_max(now, Ordering::SeqCst);
                counter.total.fetch_add(1, Ordering::SeqCst);

                tokio::time::sleep(HOLD).await;
                counter.now.fetch_sub(1, Ordering::SeqCst);

                axum::Json(serde_json::json!({
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

fn request_body() -> serde_json::Value {
    serde_json::json!({
        "model": "demo-model",
        "messages": [{ "role": "user", "content": "hi" }],
    })
}

/// 并发打 `CONCURRENT_REQUESTS` 个请求，返回任务句柄而不等待——
/// 调用方可以一边打一边观察块的实时负载。
///
/// 排队中的请求靠 `chat_completion` 内部的 spawn 活着——客户端那侧的
/// future 被丢弃也不会影响它们，与线上客户端的行为一致。
async fn spawn_hammer(
    app: &TestApp,
) -> Vec<tokio::task::JoinHandle<aequi::Result<serde_json::Value>>> {
    let mut handles = Vec::new();
    for _ in 0..CONCURRENT_REQUESTS {
        handles.push(tokio::spawn({
            let state = Arc::clone(&app.state);
            let key = demo_key();
            let body = request_body();
            async move {
                proxy::chat_completion(&state, &key, &body, "127.0.0.1", Some("mock-agent"), None)
                    .await
            }
        }));
    }
    handles
}

/// 等所有请求结束：排队等到位子后必须成功，不该把客户端拒掉。
async fn finish(handles: Vec<tokio::task::JoinHandle<aequi::Result<serde_json::Value>>>) {
    for handle in handles {
        handle
            .await
            .expect("请求任务不应当 panic")
            .expect("排队等到位子后必须成功，不该把客户端拒掉");
    }
}

async fn hammer(app: &TestApp) {
    finish(spawn_hammer(app).await).await;
}

/// 上限 2、并发 6：在飞数不超过 2，且六个请求全部成功——
/// 证明排队在限流，而不是在拒绝。
#[tokio::test]
async fn concurrency_cap_is_enforced_and_queue_lets_everyone_through() {
    let app = TestApp::new().await;
    let counter = Arc::new(InFlight::default());
    let base_url = serve(counting_upstream(Arc::clone(&counter))).await;
    app.seed(&base_url, 2).await;

    hammer(&app).await;

    assert_eq!(
        counter.total.load(Ordering::SeqCst),
        CONCURRENT_REQUESTS,
        "每个请求都真的发到了上游"
    );
    let peak = counter.peak.load(Ordering::SeqCst);
    assert_eq!(peak, 2, "在飞峰值必须正好等于上限——超了就是闸门失灵");
    assert_eq!(
        counter.now.load(Ordering::SeqCst),
        0,
        "请求全部返回后在飞数应当归零"
    );
}

/// 对照：上限 0（不限制）时同样的六个请求应当同时在飞——
/// 证明上一用例的 2 确实是闸门限出来的，不是 mock 自己串行了。
#[tokio::test]
async fn no_limit_means_no_queue() {
    let app = TestApp::new().await;
    let counter = Arc::new(InFlight::default());
    let base_url = serve(counting_upstream(Arc::clone(&counter))).await;
    app.seed(&base_url, 0).await;

    hammer(&app).await;

    assert_eq!(counter.total.load(Ordering::SeqCst), CONCURRENT_REQUESTS);
    let peak = counter.peak.load(Ordering::SeqCst);
    assert!(
        peak > 2,
        "不限制时在飞峰值应当明显超过 2，实际 {peak}——否则上限用例证明不了排队"
    );
}

/// 管理面要能看见块的实时负载：请求在飞时列表接口报的在飞数打满上限、
/// 排队数大于 0；全部返回后两个数都归零。
#[tokio::test]
async fn api_reports_live_load_while_requests_are_in_flight() {
    let app = TestApp::new().await;
    let counter = Arc::new(InFlight::default());
    let base_url = serve(counting_upstream(Arc::clone(&counter))).await;
    app.seed(&base_url, 2).await;

    let handles = spawn_hammer(&app).await;

    // 轮询到「在飞打满且有人排队」：mock 停留 150ms，窗口足够。
    let mut loaded = None;
    for _ in 0..200 {
        let demo = demo_block_view(&app).await;
        if demo.in_flight == Some(2) && demo.queued.unwrap_or(0) >= 1 {
            loaded = Some(demo);
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let demo = loaded.expect("在飞数应当打满上限，且有人排在闸门后");
    assert_eq!(demo.in_flight, Some(2), "在飞数要正好等于上限");
    assert!(
        demo.queued.unwrap_or(0) >= 1,
        "剩下的请求应当排在闸门后，实际 {:?}",
        demo.queued
    );
    assert_eq!(demo.max_concurrency, 2, "上限本身也要如实上报");

    finish(handles).await;

    let demo = demo_block_view(&app).await;
    assert_eq!(demo.in_flight, Some(0), "请求全部返回后在飞数归零");
    assert_eq!(demo.queued, Some(0), "排队数随之归零");
}

/// 走列表接口（后台块管理页用的就是这个）取 demo 块的视图。
async fn demo_block_view(app: &TestApp) -> aequi::model::BlockView {
    upstream_service::list_blocks(app.data(), &app.state.upstream)
        .await
        .expect("列块失败")
        .into_iter()
        .find(|block| block.id == "demo-block")
        .expect("demo 块应当在列表里")
}
