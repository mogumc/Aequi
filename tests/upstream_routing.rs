//! 请求目标的解析：模型名前缀（块命名空间）与 `X-Aequi-Block` 请求头。
//!
//! 所有凭据均为测试用假数据，不涉及任何真实服务。

use std::path::PathBuf;
use std::sync::Arc;

use aequi::config::Settings;
use aequi::db;
use aequi::model::BlockCreate;
use aequi::service::upstream as upstream_service;
use aequi::utils::random;
use aequi::{model::ApiFormat, AppState, Error};

const DEMO_MOCK_URL: &str = "https://mock-upstream.invalid";

struct TestApp {
    state: Arc<AppState>,
    dir: PathBuf,
}

impl TestApp {
    async fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("aequi-routing-{}", random::string(12)));
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

    fn data(&self) -> &sqlx::SqlitePool {
        &self.state.store.data
    }

    /// 建一个块，模型条目按 `(name, multiplier_bp)` 给。
    async fn add_block(&self, id: &str, models: &[&str]) -> Result<(), Error> {
        upstream_service::create_block(
            self.data(),
            BlockCreate {
                id: id.to_string(),
                name: id.to_string(),
                api_format: ApiFormat::Openai,
                weight: 1,
                base_url: DEMO_MOCK_URL.to_string(),
                proxy: None,
                models: models
                    .iter()
                    .map(|name| aequi::model::ModelEntry {
                        name: name.to_string(),
                        forward: None,
                        multiplier_bp: 10_000,
                    })
                    .collect(),
            },
        )
        .await
        .map(|_| ())
    }
}

impl Drop for TestApp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[tokio::test]
async fn plain_model_name_does_not_lock_a_block() {
    let app = TestApp::new().await;
    app.add_block("block-a", &["demo-model"]).await.unwrap();
    app.add_block("block-b", &["demo-model"]).await.unwrap();

    let target = upstream_service::resolve_target(app.data(), "demo-model")
        .await
        .unwrap();

    assert_eq!(target.model, "demo-model");
    assert_eq!(target.block_id, None, "短名应当交给加权轮询");
}

#[tokio::test]
async fn block_prefix_locks_that_block() {
    let app = TestApp::new().await;
    app.add_block("block-a", &["demo-model"]).await.unwrap();
    app.add_block("block-b", &["demo-model"]).await.unwrap();

    let target = upstream_service::resolve_target(app.data(), "block-b/demo-model")
        .await
        .unwrap();

    assert_eq!(target.model, "demo-model", "前缀要剥掉，发给上游的是模型名");
    assert_eq!(target.block_id.as_deref(), Some("block-b"));
}

#[tokio::test]
async fn whole_name_wins_over_prefix_splitting() {
    let app = TestApp::new().await;
    // 块 id 恰好叫 demo，模型名自带斜杠——整名优先，不能被拆成「块 demo 的模型 x」
    app.add_block("demo", &["other-model"]).await.unwrap();
    app.add_block("block-a", &["demo/x"]).await.unwrap();

    let target = upstream_service::resolve_target(app.data(), "demo/x")
        .await
        .unwrap();

    assert_eq!(target.model, "demo/x");
    assert_eq!(
        target.block_id, None,
        "整名能匹配上时不该拆前缀，否则本身带斜杠的模型名永远调不到"
    );
}

#[tokio::test]
async fn unknown_prefix_or_model_is_reported() {
    let app = TestApp::new().await;
    app.add_block("block-a", &["demo-model"]).await.unwrap();

    // 前缀不存在
    let error = upstream_service::resolve_target(app.data(), "no-such/model")
        .await
        .unwrap_err();
    assert!(matches!(error, Error::NotFound(_)));

    // 前缀存在，但那个块没有该模型
    let error = upstream_service::resolve_target(app.data(), "block-a/no-such")
        .await
        .unwrap_err();
    assert!(matches!(error, Error::NotFound(_)));

    // 两种都不是
    let error = upstream_service::resolve_target(app.data(), "nothing")
        .await
        .unwrap_err();
    assert!(matches!(error, Error::NotFound(_)));
}

#[tokio::test]
async fn header_block_is_merged_and_conflicts_are_rejected() {
    let app = TestApp::new().await;
    app.add_block("block-a", &["demo-model"]).await.unwrap();
    app.add_block("block-b", &["demo-model"]).await.unwrap();

    // 头指定块
    let target = upstream_service::resolve_with_header(app.data(), "demo-model", Some("block-b"))
        .await
        .unwrap();
    assert_eq!(target.block_id.as_deref(), Some("block-b"));

    // 头与前缀一致：放行
    let target =
        upstream_service::resolve_with_header(app.data(), "block-b/demo-model", Some("block-b"))
            .await
            .unwrap();
    assert_eq!(target.block_id.as_deref(), Some("block-b"));

    // 头与前缀矛盾：报错而不是猜
    let error =
        upstream_service::resolve_with_header(app.data(), "block-a/demo-model", Some("block-b"))
            .await
            .unwrap_err();
    assert!(matches!(error, Error::BadRequest(_)));
}

#[tokio::test]
async fn block_id_must_be_prefix_safe() {
    let app = TestApp::new().await;

    // 含大写、斜杠、空格都不行
    for bad in ["Block-A", "a/b", "with space", "中文"] {
        let error = app.add_block(bad, &["demo-model"]).await.unwrap_err();
        assert!(
            matches!(error, Error::BadRequest(_)),
            "块 id {bad} 应当被拒绝，实际 {error:?}"
        );
    }

    // 合法的：小写字母、数字、点、下划线、连字符
    app.add_block("block-a.v2_test", &["demo-model"]).await.unwrap();
}

#[tokio::test]
async fn available_models_list_both_spellings() {
    let app = TestApp::new().await;
    app.add_block("block-a", &["shared", "a-only"]).await.unwrap();
    app.add_block("block-b", &["shared"]).await.unwrap();

    let models = upstream_service::available_models(app.data(), &demo_key(Vec::new()))
        .await
        .unwrap();

    // 短名去重
    assert!(models.contains(&"shared".to_string()));
    assert_eq!(
        models.iter().filter(|name| name.as_str() == "shared").count(),
        1
    );
    // 带块前缀的全名每块一条
    assert!(models.contains(&"block-a/shared".to_string()));
    assert!(models.contains(&"block-b/shared".to_string()));
    assert!(models.contains(&"block-a/a-only".to_string()));
    assert!(!models.contains(&"block-b/a-only".to_string()));

    // 白名单决定能看到什么：只放行 block-a 的 shared
    let key = demo_key(vec!["block-a/shared".to_string()]);
    let visible = upstream_service::available_models(app.data(), &key)
        .await
        .unwrap();
    assert!(visible.contains(&"shared".to_string()), "被允许的块提供它，短名可用");
    assert!(visible.contains(&"block-a/shared".to_string()));
    assert!(!visible.contains(&"block-b/shared".to_string()), "没被允许的块不该出现");
    assert!(!visible.contains(&"a-only".to_string()));

    // 白名单为空即不限制
    let open = demo_key(Vec::new());
    let all = upstream_service::available_models(app.data(), &open)
        .await
        .unwrap();
    assert!(all.contains(&"block-a/shared".to_string()));
    assert!(all.contains(&"block-b/shared".to_string()));
}

/// 白名单必须能区分「a 的 shared」与「b 的 shared」。
///
/// 否则下游密钥只允许了 a 的同名模型，请求短名时仍可能被轮询到 b——
/// 那是它没有获得授权的上游。
#[tokio::test]
async fn whitelist_is_per_block_not_just_model_name() {
    let app = TestApp::new().await;
    app.add_block("block-a", &["shared"]).await.unwrap();
    app.add_block("block-b", &["shared"]).await.unwrap();

    // 只允许块 a 的 shared
    let key = demo_key(vec!["block-a/shared".to_string()]);

    let allowed = upstream_service::allowed_blocks(app.data(), "shared", &key)
        .await
        .unwrap();
    assert_eq!(allowed, vec!["block-a".to_string()], "只该剩下被允许的那个块");
}

/// 白名单为空 = 不限制，所有块都可用。
#[tokio::test]
async fn empty_whitelist_allows_every_block() {
    let app = TestApp::new().await;
    app.add_block("block-a", &["shared"]).await.unwrap();
    app.add_block("block-b", &["shared"]).await.unwrap();

    let key = demo_key(Vec::new());
    let allowed = upstream_service::allowed_blocks(app.data(), "shared", &key)
        .await
        .unwrap();

    assert_eq!(allowed.len(), 2);
    assert!(allowed.contains(&"block-a".to_string()) && allowed.contains(&"block-b".to_string()));
}

fn demo_key(allowed_models: Vec<String>) -> aequi::model::ApiKey {
    aequi::model::ApiKey {
        id: "k".to_string(),
        name: "k".to_string(),
        secret: "ak_demo".to_string(),
        key_prefix: "ak_demo".to_string(),
        credits_scaled: 0,
        unlimited: false,
        enabled: true,
        allowed_models,
        created_at: 0,
        updated_at: 0,
        last_used_at: None,
    }
}

/// 批量添加密钥：重复的自动跳过，其余入库——批量导入几百把时
/// 不可能让人自己去把重复的挑出来。
#[tokio::test]
async fn duplicate_keys_are_skipped_not_rejected() {
    let app = TestApp::new().await;
    app.add_block("block-a", &["demo-model"]).await.unwrap();

    let create = |secrets: Vec<&str>| upstream_service::add_keys(
        app.data(),
        "block-a",
        aequi::model::UpstreamKeyCreate {
            secrets: secrets.into_iter().map(str::to_string).collect(),
            weight: 1,
        },
    );

    // 首批：两把正常入库
    let first = create(vec!["sk-alpha", "sk-beta"]).await.unwrap();
    assert_eq!(first.keys.len(), 2);
    assert_eq!(first.skipped, 0);

    // 二批：一把与已有重复、一把与同批重复、一把全新、一把空白
    let second = create(vec!["sk-alpha", "sk-gamma", "sk-gamma", "  ", "sk-delta"])
        .await
        .unwrap();
    assert_eq!(second.keys.len(), 2, "只有 gamma 与 delta 是新的");
    assert_eq!(second.skipped, 3);
    let names: Vec<&str> = second
        .keys
        .iter()
        .map(|key| key.secret.as_str())
        .collect();
    assert_eq!(names, vec!["sk-gamma", "sk-delta"]);

    // 全部重复：不报错，返回空 + 跳过数
    let third = create(vec!["sk-alpha", "sk-beta"]).await.unwrap();
    assert!(third.keys.is_empty());
    assert_eq!(third.skipped, 2);
}
