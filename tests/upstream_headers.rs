//! 块的自定义请求头：存得住、改得掉、真的发得出去。
//!
//! 分层验：先建库建块、把头条目存进去，再拿**真实的请求路径**打一个本地
//! mock 上游，确认它收到的就是配置里的那张头表（含覆盖内置认证头）。
//! 单测只证明「头拼对了」，这里证明「从配置到上游这一段是通的」。
//!
//! 所有凭据均为测试用假数据，不涉及任何真实服务。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::routing::get;

use aequi::config::Settings;
use aequi::db;
use aequi::model::{
    ApiFormat, BlockCreate, BlockUpdate, HeaderEntry, ModelEntry, UpstreamKeyCreate,
};
use aequi::proxy::adapter::ProbeOutcome;
use aequi::service::upstream as upstream_service;
use aequi::utils::random;
use aequi::{AppState, Error};

struct TestApp {
    state: Arc<AppState>,
    dir: PathBuf,
}

impl TestApp {
    async fn new() -> TestApp {
        let dir = std::env::temp_dir().join(format!("aequi-headers-{}", random::string(12)));
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

    /// 建一个带自定义头的块，并给它配一把上游密钥。
    async fn seed_block(&self, base_url: &str, headers: Vec<HeaderEntry>) {
        upstream_service::create_block(
            self.data(),
            BlockCreate {
                id: "demo-block".to_string(),
                name: "demo".to_string(),
                api_format: ApiFormat::Openai,
                weight: 1,
                base_url: base_url.to_string(),
                proxy: None,
                models: vec![ModelEntry {
                    name: "demo-model".to_string(),
                    forward: None,
                    multiplier_bp: 10_000,
                }],
                headers,
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
            },
        )
        .await
        .expect("加上游密钥失败");
    }
}

impl Drop for TestApp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn header(name: &str, value: &str) -> HeaderEntry {
    HeaderEntry {
        name: name.to_string(),
        value: value.to_string(),
    }
}

/// mock 上游：把收到的头抄一份下来，再回一个合法的列模型响应。
fn mock_upstream(sink: Arc<Mutex<Vec<(String, String)>>>) -> Router {
    Router::new().route(
        "/v1/models",
        get(move |headers: axum::http::HeaderMap| {
            let sink = Arc::clone(&sink);
            async move {
                let mut captured = sink.lock().expect("锁住抄表");
                for (name, value) in headers.iter() {
                    captured.push((
                        name.as_str().to_string(),
                        value.to_str().unwrap_or_default().to_string(),
                    ));
                }
                drop(captured);

                axum::Json(serde_json::json!({ "data": [{ "id": "demo-model" }] }))
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

#[tokio::test]
async fn custom_headers_reach_the_upstream_from_both_entry_points() {
    let app = TestApp::new().await;
    let sink: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let base_url = serve(mock_upstream(Arc::clone(&sink))).await;

    app.seed_block(
        &base_url,
        vec![
            header("X-Tenant", "demo-tenant"),
            // 上游要什么写法由配置决定：这条要盖住内置的 Bearer 认证头。
            header("Authorization", "literal-token"),
        ],
    )
    .await;

    // 入口一：从上游拉模型（管理面「拉取模型」）。
    let models = upstream_service::fetch_models(app.data(), &app.state.upstream, "demo-block")
        .await
        .expect("拉取模型失败");
    assert_eq!(models, vec!["demo-model".to_string()]);

    let seen = sink.lock().expect("读抄表").clone();
    let value_of = |name: &str| {
        seen.iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    };

    assert_eq!(value_of("x-tenant").as_deref(), Some("demo-tenant"));
    assert_eq!(
        value_of("authorization").as_deref(),
        Some("literal-token"),
        "块上配的头必须盖过内置认证头"
    );
    assert_eq!(
        seen.iter()
            .filter(|(header, _)| header.eq_ignore_ascii_case("authorization"))
            .count(),
        1,
        "同名头只能发出一条，不能两个值一起飞出去"
    );

    // 入口二：测试密钥。走的是同一个出口配置，头必须一样。
    let keys = upstream_service::list_keys(app.data(), "demo-block")
        .await
        .expect("读密钥失败");
    let outcome = upstream_service::test_key(
        app.data(),
        &app.state.upstream,
        "demo-block",
        &keys[0].id,
    )
    .await
    .expect("测试密钥失败");
    assert!(
        matches!(outcome, ProbeOutcome::Valid),
        "带自定义头的探测应当通过"
    );

    let after = sink.lock().expect("读抄表").clone();
    assert!(
        after.len() > seen.len(),
        "探测应当也打到了 mock 上游（说明头没把它挡在门外）"
    );
    assert_eq!(
        after
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case("x-tenant"))
            .map(|(_, value)| value.as_str()),
        Some("demo-tenant"),
        "探测与拉模型用的是同一套头"
    );
}

#[tokio::test]
async fn headers_survive_create_update_and_clear() {
    let app = TestApp::new().await;
    app.seed_block(
        "https://mock-upstream.invalid",
        vec![header("X-Tenant", "demo-tenant"), header("X-Scope", "demo")],
    )
    .await;

    let view = upstream_service::block_view(app.data(), "demo-block")
        .await
        .expect("读块失败");
    assert_eq!(view.headers.len(), 2, "建块时的自定义头要存下来");
    assert_eq!(view.headers[0].name, "X-Tenant", "顺序即配置顺序");

    // 不传 headers 的 PATCH：保持原值（与 proxy 的「不传即不改」同一口径）。
    upstream_service::update_block(
        app.data(),
        &app.state.upstream,
        "demo-block",
        BlockUpdate {
            name: Some("demo-renamed".to_string()),
            ..Default::default()
        },
    )
    .await
    .expect("改名失败");
    let view = upstream_service::block_view(app.data(), "demo-block")
        .await
        .expect("读块失败");
    assert_eq!(view.name, "demo-renamed");
    assert_eq!(view.headers.len(), 2, "没传 headers 不该动它");

    // 传空数组：整份清空。
    upstream_service::update_block(
        app.data(),
        &app.state.upstream,
        "demo-block",
        BlockUpdate {
            headers: Some(Vec::new()),
            ..Default::default()
        },
    )
    .await
    .expect("清空失败");
    let view = upstream_service::block_view(app.data(), "demo-block")
        .await
        .expect("读块失败");
    assert!(view.headers.is_empty(), "传 [] 应当清空");

    // 脏数据在入口就挡住，而且不该留下半截写入。
    let error = upstream_service::update_block(
        app.data(),
        &app.state.upstream,
        "demo-block",
        BlockUpdate {
            headers: Some(vec![header("Content-Length", "1")]),
            ..Default::default()
        },
    )
    .await
    .expect_err("HTTP 栈管的头必须被拒");
    assert!(matches!(error, Error::BadRequest(_)), "应当是参数错误");
    assert!(
        error.to_string().contains("HTTP 栈"),
        "错误要说清为什么: {error}"
    );

    let view = upstream_service::block_view(app.data(), "demo-block")
        .await
        .expect("读块失败");
    assert!(view.headers.is_empty(), "被拒的配置不该写进库");
}
