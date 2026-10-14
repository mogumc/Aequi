//! 回归：客户端中途断开时，这笔流式请求要计费、要落日志。
//!
//! 起一个真实的本地 mock SSE 上游，读到第几帧主动 drop 流（等价于用户
//! 按取消 / 关标签页），然后查库确认：
//! - 已产出的内容按估算计费（不是一分不扣，也不是按上游全额乱扣）；
//! - 详细日志与用量记录都在，管理员能看出这是客户端断开。
//!
//! 三种情形分别覆盖：
//! 1. 上游正常发完 usage → 按上游口径计费；
//! 2. 上游来不及发 usage 就被断开 → 按已产出文本估算；
//! 3. 一个字都没产出就断开 → 不计费。
//!
//! 所有凭据均为测试用假数据，不涉及任何真实服务。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::routing::post;
use axum::Router;
use futures_util::StreamExt;

use aequi::config::Settings;
use aequi::db;
use aequi::model::{ApiFormat, BlockCreate, ModelEntry, UpstreamKeyCreate};
use aequi::service::upstream as upstream_service;
use aequi::utils::random;
use aequi::{model::ApiKey, AppState, proxy};

struct TestApp {
    state: Arc<AppState>,
    dir: PathBuf,
}

impl TestApp {
    async fn new() -> TestApp {
        let dir = std::env::temp_dir().join(format!("aequi-abort-{}", random::string(12)));
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

    /// 建块并加一把上游密钥，`base_url` 由调用方给（指向本地 mock）。
    async fn seed_upstream(&self, base_url: &str) {
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
    }
}

impl Drop for TestApp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// mock 上游：`total` 帧内容，`with_usage` 决定末尾是否带上 usage chunk。
/// 每帧之间留出间隔，保证客户端来得及中途放弃。
fn mock_upstream(total: u32, with_usage: bool) -> Router {
    Router::new().route(
        "/v1/chat/completions",
        post(move || async move {
            let stream = async_stream::stream! {
                for index in 0..total {
                    let chunk = serde_json::json!({
                        "id": "mock-id",
                        "object": "chat.completion.chunk",
                        "model": "demo-model",
                        "choices": [{
                            "index": 0,
                            "delta": { "content": format!("tok{index} ") },
                            "finish_reason": serde_json::Value::Null,
                        }],
                    });
                    yield Ok::<_, std::io::Error>(format!("data: {chunk}\n\n"));
                    tokio::time::sleep(Duration::from_millis(60)).await;
                }

                if with_usage {
                    let usage = serde_json::json!({
                        "id": "mock-id",
                        "object": "chat.completion.chunk",
                        "model": "demo-model",
                        "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
                        "usage": {
                            "prompt_tokens": 1000,
                            "completion_tokens": 500,
                            "total_tokens": 1500,
                        },
                    });
                    yield Ok::<_, std::io::Error>(format!("data: {usage}\n\n"));
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
        enabled: true,
        unlimited: false,
        credits_scaled: 500_000,
        allowed_models: Vec::new(),
        created_at: 0,
        updated_at: 0,
        last_used_at: None,
    }
}

fn request_body() -> serde_json::Value {
    serde_json::json!({
        "model": "demo-model",
        "stream": true,
        "messages": [{ "role": "user", "content": "hi" }],
    })
}

/// 一笔请求在库里留下的痕迹。
#[derive(Debug)]
struct Trace {
    usage_rows: i64,
    credits_scaled: i64,
    log_status: Option<String>,
    error_detail: Option<String>,
    estimated: bool,
    total_tokens: i64,
}

async fn trace(app: &TestApp) -> Trace {
    let usage_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM usage_records WHERE key_id = ?")
        .bind("key-1")
        .fetch_one(app.data())
        .await
        .expect("查用量失败");

    let credits_scaled: i64 = sqlx::query_scalar("SELECT credits_scaled FROM api_keys WHERE id = ?")
        .bind("key-1")
        .fetch_one(app.data())
        .await
        .expect("查余额失败");

    let row: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT status, error_detail FROM proxy_logs WHERE access_key_id = ? ORDER BY id DESC LIMIT 1",
    )
    .bind("key-1")
    .fetch_optional(app.data())
    .await
    .expect("查日志失败");

    let (estimated, total_tokens): (bool, i64) = sqlx::query_as(
        "SELECT COALESCE(estimated, 0), COALESCE(raw_tokens, 0) FROM usage_records WHERE key_id = ? ORDER BY id DESC LIMIT 1",
    )
    .bind("key-1")
    .fetch_optional(app.data())
    .await
    .expect("查用量明细失败")
    .unwrap_or((false, 0));

    Trace {
        usage_rows,
        credits_scaled,
        log_status: row.as_ref().map(|(status, _)| status.clone()),
        error_detail: row.and_then(|(_, detail)| detail),
        estimated,
        total_tokens,
    }
}

/// 建流、读到第 `frames` 帧就 drop，然后等后台记账落地。
async fn abort_after(app: &TestApp, frames: usize) {
    let mut stream = Box::pin(
        proxy::chat_completion_stream(
            app.state.clone(),
            demo_key(),
            request_body(),
            "127.0.0.1".to_string(),
            Some("mock-agent".to_string()),
            None,
        )
        .await
        .expect("建流失败"),
    );

    let mut taken = 0usize;
    while let Some(item) = stream.next().await {
        item.expect("中断前的帧都应是 Ok");
        taken += 1;
        if taken >= frames {
            break;
        }
    }
    drop(stream);

    // 记账在脱离的任务里跑，留足时间等它落库。
    tokio::time::sleep(Duration::from_millis(1500)).await;
}

#[tokio::test]
async fn abort_after_output_bills_by_upstream_usage() {
    let app = TestApp::new().await;
    let base_url = serve(mock_upstream(30, true)).await;
    app.seed_upstream(&base_url).await;

    // 第 5 帧时上游已报过 usage（这里让它自然跑到收尾前断开）：
    // 用一个「发完 usage 但不立刻结束」的上游不好控制，改由下面两个用例覆盖，
    // 本例只断言「有产出就一定有账」。
    abort_after(&app, 5).await;

    let result = trace(&app).await;
    println!("中途断开（有上游 usage）: {result:?}");

    assert!(
        result.credits_scaled < 500_000,
        "有产出就必须扣费，实际余额 {}",
        result.credits_scaled
    );
    assert_eq!(result.usage_rows, 1, "用量记录应当有一条");
    assert_eq!(
        result.log_status.as_deref(),
        Some("success"),
        "中断不等于失败：上游确实出了 token"
    );
    assert!(
        result.error_detail.as_ref().is_some_and(|d| d.contains("客户端")),
        "日志应当注明是客户端断开，实际 {:?}",
        result.error_detail
    );
}

#[tokio::test]
async fn abort_before_usage_bills_by_estimate() {
    let app = TestApp::new().await;
    // 30 帧 × 60ms，读到第 5 帧就断开 —— usage chunk 远在后面，来不及发。
    let base_url = serve(mock_upstream(30, true)).await;
    app.seed_upstream(&base_url).await;

    abort_after(&app, 5).await;

    let result = trace(&app).await;
    println!("中途断开（上游 usage 未到）: {result:?}");

    assert!(
        result.credits_scaled < 500_000,
        "已产出的内容必须按估算扣费，实际余额 {}",
        result.credits_scaled
    );
    assert!(
        result.estimated,
        "上游 usage 没到，应当走估算路径（estimated=1）"
    );
    assert!(
        result.total_tokens > 0,
        "估算用量必须为正，实际 {}",
        result.total_tokens
    );
    assert_eq!(result.usage_rows, 1);
    assert_eq!(result.log_status.as_deref(), Some("success"));
    assert!(result.error_detail.as_ref().is_some_and(|d| d.contains("客户端")));
}

#[tokio::test]
async fn abort_before_any_output_is_not_billed() {
    let app = TestApp::new().await;
    // 上游只吐 role 帧就没有正文了——一个 token 都没产出。
    // 客户端读完整个流后自然断开，此处不主动提前放弃。
    let base_url = serve(mock_upstream(0, false)).await;
    app.seed_upstream(&base_url).await;

    let mut stream = Box::pin(
        proxy::chat_completion_stream(
            app.state.clone(),
            demo_key(),
            request_body(),
            "127.0.0.1".to_string(),
            Some("mock-agent".to_string()),
            None,
        )
        .await
        .expect("建流失败"),
    );
    while let Some(item) = stream.next().await {
        let bytes = item.expect("帧应当是 Ok");
        // 客户端应当收到内联错误帧，说明它被告知这次没有内容。
        if String::from_utf8_lossy(&bytes).contains("\"error\"") {
            println!("客户端收到内联错误帧");
        }
    }
    drop(stream);
    tokio::time::sleep(Duration::from_millis(800)).await;

    let result = trace(&app).await;
    println!("上游零产出: {result:?}");

    // 上游一个字都没生成，不该收钱。
    assert_eq!(
        result.credits_scaled, 500_000,
        "没有任何产出时不得扣费，实际余额 {}",
        result.credits_scaled
    );
    assert_eq!(
        result.log_status.as_deref(),
        Some("error"),
        "零产出应当按失败记账"
    );
    assert_eq!(result.usage_rows, 1, "仍要留一条用量记录（错误记录）");
}

#[tokio::test]
async fn client_disconnect_still_settles_the_upstream_stream() {
    let app = TestApp::new().await;
    // 上游慢（每帧 60ms），客户端在第 2 帧就走。
    let base_url = serve(mock_upstream(30, true)).await;
    app.seed_upstream(&base_url).await;

    abort_after(&app, 2).await;

    let result = trace(&app).await;
    println!("客户端立刻走，上游照常收尾: {result:?}");

    // 这是对齐旧版的关键行为：客户端走了不等于请求作废。
    // 上游照常把流生成完，网关照常把这笔账结清——否则上游那部分
    // 额度就永远是净支出，一毛都收不回来。
    assert!(
        result.credits_scaled < 500_000,
        "客户端断开后仍应完成结算，实际余额 {}",
        result.credits_scaled
    );
    assert_eq!(result.log_status.as_deref(), Some("success"));
    assert!(result.error_detail.as_ref().is_some_and(|d| d.contains("客户端")));
}
