//! 密钥、积分与体系隔离测试。
//!
//! 所有凭据均为测试用假数据，不涉及任何真实服务。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use aequi::config::Settings;
use aequi::db;
use aequi::model::{credits, KeyCreate};
use aequi::service::{keys as key_service, password};
use aequi::utils::random;
use aequi::{api, AppState, Error};

/// 计费测试里假装命中的块 id（白名单是全名，所以要有个块）。
const DEMO_BLOCK_ID: &str = "demo-block";

const DEMO_USERNAME: &str = "mock-operator";
const DEMO_PASSWORD: &str = "mock-password-for-tests";

struct TestApp {
    state: Arc<AppState>,
    dir: PathBuf,
}

impl TestApp {
    async fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("aequi-keys-{}", random::string(12)));
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

    fn router(&self) -> Router {
        api::router(self.state.clone())
    }

    /// 数据面连接池。
    fn data(&self) -> &sqlx::SqlitePool {
        &self.state.store.data
    }

    async fn seed_account(&self) -> i64 {
        let hash = password::hash(DEMO_PASSWORD.to_string())
            .await
            .expect("计算密码哈希失败");

        db::account::insert(self.state.pool(), DEMO_USERNAME, &hash, 0)
            .await
            .expect("写入账号失败")
            .id
    }

    async fn login(&self) -> String {
        let response = self
            .router()
            .oneshot(json_request(
                Method::POST,
                "/api/auth/login",
                json!({ "username": DEMO_USERNAME, "password": DEMO_PASSWORD }),
            ))
            .await
            .expect("登录请求失败");

        assert_eq!(response.status(), StatusCode::OK, "登录应当成功");
        json_body(response).await["token"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// 经管理员接口创建一把密钥，返回明文。
    async fn create_key(&self, admin_token: &str, credits: &str) -> String {
        let response = self
            .router()
            .oneshot(authorized(
                json_request(
                    Method::POST,
                    "/api/keys",
                    json!({ "name": "mock-key", "credits": credits }),
                ),
                admin_token,
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK, "创建密钥应当成功");
        json_body(response).await["secret"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// 直接建一个业务层密钥对象（绕过 HTTP）。
    async fn make_key(&self, credits_text: &str, models: Vec<String>) -> aequi::model::ApiKey {
        self.make_key_with(credits_text, models, false).await
    }

    /// 同上，可指定是否无限额度。
    async fn make_key_with(
        &self,
        credits_text: &str,
        models: Vec<String>,
        unlimited: bool,
    ) -> aequi::model::ApiKey {
        key_service::create(
            self.data(),
            KeyCreate {
                name: "mock-key".to_string(),
                credits: Some(credits_text.to_string()),
                unlimited,
                allowed_models: models,
            },
        )
        .await
        .expect("创建密钥失败")
    }
}

impl Drop for TestApp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn with_connect_info(mut request: Request<Body>) -> Request<Body> {
    request
        .extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 43210))));
    request
}

fn get(uri: &str) -> Request<Body> {
    with_connect_info(
        Request::builder()
            .method(Method::GET)
            .uri(uri)
            .body(Body::empty())
            .unwrap(),
    )
}

fn json_request(method: Method, uri: &str, payload: Value) -> Request<Body> {
    with_connect_info(
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(payload.to_string()))
            .unwrap(),
    )
}

fn authorized(mut request: Request<Body>, token: &str) -> Request<Body> {
    request.headers_mut().insert(
        header::AUTHORIZATION,
        format!("Bearer {token}").parse().unwrap(),
    );
    request
}

async fn json_body(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).expect("响应不是合法 JSON")
}

/// 便捷包装：按总 token 数构造一次简单计费（输入为 0、输出为总量），默认 1.0 倍率。
async fn charge_total(
    pool: &sqlx::SqlitePool,
    key: &aequi::model::ApiKey,
    model: &str,
    total_tokens: i64,
) -> Result<aequi::service::billing::ChargeOutcome, aequi::Error> {
    charge_with_multiplier(
        pool,
        key,
        model,
        total_tokens,
        aequi::model::credits::DEFAULT_MULTIPLIER_BP,
    )
    .await
}

/// 同上，但指定倍率——现实中倍率来自块内的模型条目，由调度层传入。
async fn charge_with_multiplier(
    pool: &sqlx::SqlitePool,
    key: &aequi::model::ApiKey,
    model: &str,
    total_tokens: i64,
    multiplier_bp: i64,
) -> Result<aequi::service::billing::ChargeOutcome, aequi::Error> {
    aequi::service::billing::charge(
        pool,
        key,
        DEMO_BLOCK_ID,
        model,
        aequi::proxy::scheme::Usage::new(0, total_tokens),
        multiplier_bp,
        false,
    )
    .await
}

#[tokio::test]
async fn created_key_can_authenticate_and_admin_can_read_it_back() {
    let app = TestApp::new().await;
    app.seed_account().await;
    let admin_token = app.login().await;
    let secret = app.create_key(&admin_token, "10").await;

    assert!(secret.starts_with("ak_"), "密钥应带可辨识前缀");

    // 密钥能读到自己的信息；自助视图不回显明文
    let me = json_body(
        app.router()
            .oneshot(authorized(get("/api/key/me"), &secret))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(me["credits"], "10.0000");
    assert_eq!(me["enabled"], true);
    assert!(
        me.get("secret").is_none(),
        "自助视图不该回显明文密钥，持有者本来就拿着它"
    );

    // 管理员随时能取回完整密钥，忘了可以来找
    let list = json_body(
        app.router()
            .oneshot(authorized(get("/api/keys"), &admin_token))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(list["keys"][0]["secret"], secret.as_str(), "管理员应能取回明文");
    assert!(list["keys"][0]["key_prefix"]
        .as_str()
        .unwrap()
        .starts_with("ak_"));

    // 单条详情同样带明文
    let key_id = list["keys"][0]["id"].as_str().unwrap();
    let detail = json_body(
        app.router()
            .oneshot(authorized(get(&format!("/api/keys/{key_id}")), &admin_token))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(detail["secret"], secret.as_str());
}

#[tokio::test]
async fn the_two_auth_systems_do_not_overlap() {
    let app = TestApp::new().await;
    app.seed_account().await;
    let admin_token = app.login().await;
    let secret = app.create_key(&admin_token, "10").await;

    // 管理员令牌进不了密钥自助接口
    let response = app
        .router()
        .oneshot(authorized(get("/api/key/me"), &admin_token))
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "管理员令牌不应被密钥体系接受"
    );

    // 访问密钥进不了任何管理员接口
    for path in [
        "/api/keys",
        "/api/auth/me",
        "/api/devices",
        "/api/settings",
        "/api/otp",
    ] {
        let response = app
            .router()
            .oneshot(authorized(get(path), &secret))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{path} 不应接受访问密钥"
        );
    }

    // 无凭证同样拒绝
    let response = app.router().oneshot(get("/api/key/me")).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn charging_deducts_credits_and_records_usage() {
    let app = TestApp::new().await;
    let key = app.make_key("1", Vec::new()).await;

    // 10000 token、1.0 倍 → 恰好 1 积分
    let outcome = charge_total(app.data(), &key, "demo-model", 10_000)
        .await
        .expect("扣费应当成功");

    assert_eq!(outcome.multiplier_bp, credits::DEFAULT_MULTIPLIER_BP);
    assert_eq!(outcome.credits_scaled, credits::CREDIT_SCALE);
    assert_eq!(outcome.remaining_scaled, 0);

    // 余额已空，再扣一分都不行
    let error = charge_total(app.data(), &key, "demo-model", 1)
        .await
        .expect_err("余额不足应当报错");
    assert!(matches!(error, Error::InsufficientCredits));
    assert_eq!(error.status(), StatusCode::PAYMENT_REQUIRED);

    // 用量落账
    let summary = key_service::usage_summary(app.data(), &key.id).await.unwrap();
    assert_eq!(summary.requests, 1, "失败的那次不应留记录");
    assert_eq!(summary.raw_tokens, 10_000);
    assert_eq!(summary.credits, "1.0000");

    let records = key_service::usage(app.data(), &key.id, 10).await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].model, "demo-model");
}

#[tokio::test]
async fn model_multiplier_scales_the_bill() {
    let app = TestApp::new().await;
    let key = app.make_key("5", Vec::new()).await;

    // 倍率由调度层按块内模型条目给出，这里直接传进去。
    let outcome = charge_with_multiplier(app.data(), &key, "demo-double", 10_000, 20_000)
        .await
        .expect("扣费应当成功");

    assert_eq!(outcome.final_tokens, 20_000, "倍率应放大折算后的 token");
    assert_eq!(outcome.credits_scaled, 2 * credits::CREDIT_SCALE);
    assert_eq!(
        credits::format_credits(outcome.remaining_scaled),
        "3.0000"
    );

    // 不传倍率的那条路走默认 1.0 倍
    let plain = charge_total(app.data(), &key, "demo-plain", 10_000)
        .await
        .unwrap();
    assert_eq!(plain.multiplier_bp, credits::DEFAULT_MULTIPLIER_BP);
    assert_eq!(plain.credits_scaled, credits::CREDIT_SCALE);
}

/// 块内模型条目自带倍率：显式配置生效，缺省按 1.0。
#[test]
fn model_entry_multiplier_defaults_to_one() {
    let premium: aequi::model::ModelEntry =
        serde_json::from_value(serde_json::json!({ "name": "premium", "multiplier_bp": 25000 }))
            .expect("条目应当能解析");
    assert_eq!(premium.multiplier_bp, 25_000);
    assert_eq!(premium.target(), "premium");

    // 旧数据没有倍率字段，也必须能解析（按 1.0 处理）。
    let legacy: aequi::model::ModelEntry = serde_json::from_value(serde_json::json!({
        "name": "plain",
        "forward": "upstream-plain"
    }))
    .expect("缺少倍率字段的旧条目也应当能解析");
    assert_eq!(legacy.multiplier_bp, credits::DEFAULT_MULTIPLIER_BP);
    assert_eq!(legacy.target(), "upstream-plain");
}

#[tokio::test]
async fn model_whitelist_and_disabled_state_are_enforced() {
    let app = TestApp::new().await;

    let limited = app
        .make_key("10", vec![format!("{DEMO_BLOCK_ID}/demo-allowed")])
        .await;
    assert!(limited.allows_capability(DEMO_BLOCK_ID, "demo-allowed"));
    assert!(!limited.allows_capability(DEMO_BLOCK_ID, "demo-denied"));

    let error = charge_total(app.data(), &limited, "demo-denied", 100)
        .await
        .expect_err("白名单外的模型应当被拒");
    assert!(matches!(error, Error::Forbidden(_)));

    // 白名单为空表示不限制
    let open = app.make_key("10", Vec::new()).await;
    assert!(open.allows_capability(DEMO_BLOCK_ID, "anything"));
    assert!(charge_total(app.data(), &open, "anything", 100)
        .await
        .is_ok());

    // 停用后连认证都过不去
    key_service::update(
        app.data(),
        &open.id,
        aequi::model::KeyUpdate {
            enabled: Some(false),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let error = key_service::authenticate(app.data(), &open.secret)
        .await
        .expect_err("已停用的密钥应当被拒");
    assert!(matches!(error, Error::Forbidden(_)));

    // 不存在的密钥是另一类错误
    let error = key_service::authenticate(app.data(), "ak_does-not-exist")
        .await
        .expect_err("不存在的密钥应当报错");
    assert!(matches!(error, Error::Unauthorized(_)));
}

#[tokio::test]
async fn credit_format_is_validated_at_the_boundary() {
    let app = TestApp::new().await;
    app.seed_account().await;
    let admin_token = app.login().await;

    // 超过四位小数直接拒绝，不悄悄截断
    let response = app
        .router()
        .oneshot(authorized(
            json_request(
                Method::POST,
                "/api/keys",
                json!({ "name": "bad", "credits": "1.23456" }),
            ),
            &admin_token,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // 四位小数正常接受
    let secret = app.create_key(&admin_token, "1.2345").await;
    let me = json_body(
        app.router()
            .oneshot(authorized(get("/api/key/me"), &secret))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(me["credits"], "1.2345");
    assert_eq!(me["credits_scaled"], 12_345);
}

#[tokio::test]
async fn key_only_reads_its_own_usage() {
    let app = TestApp::new().await;
    app.seed_account().await;
    let admin_token = app.login().await;

    let first = app.create_key(&admin_token, "10").await;
    let second = app.create_key(&admin_token, "10").await;

    // 第一把密钥产生一条用量
    let first_key = key_service::authenticate(app.data(), &first).await.unwrap();
    charge_total(app.data(), &first_key, "demo-model", 5_000)
        .await
        .unwrap();

    let mine = json_body(
        app.router()
            .oneshot(authorized(get("/api/key/usage"), &first))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(mine["summary"]["requests"], 1);
    assert_eq!(mine["summary"]["credits"], "0.5000");

    // 第二把密钥看不到别人的记录
    let theirs = json_body(
        app.router()
            .oneshot(authorized(get("/api/key/usage"), &second))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(theirs["summary"]["requests"], 0);
    assert_eq!(theirs["records"].as_array().unwrap().len(), 0);

    // 管理员能看到第一把的明细
    let managed = json_body(
        app.router()
            .oneshot(authorized(
                get(&format!("/api/keys/{}/usage", first_key.id)),
                &admin_token,
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(managed["summary"]["requests"], 1);
    assert_eq!(managed["records"][0]["credits"], "0.5000");
    assert_eq!(managed["records"][0]["multiplier"], "1.0000");
}

#[tokio::test]
async fn admin_can_top_up_and_limits_are_sane() {
    let app = TestApp::new().await;
    app.seed_account().await;
    let admin_token = app.login().await;
    let secret = app.create_key(&admin_token, "0").await;

    let key_id = key_service::authenticate(app.data(), &secret).await.unwrap().id;

    let body = json_body(
        app.router()
            .oneshot(authorized(
                json_request(
                    Method::POST,
                    &format!("/api/keys/{key_id}/credits"),
                    json!({ "credits": "12.5" }),
                ),
                &admin_token,
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(body["credits"], "12.5000");

    // 充值后密钥侧立刻能读到新余额
    let me = json_body(
        app.router()
            .oneshot(authorized(get("/api/key/me"), &secret))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(me["credits"], "12.5000");

    // 用量查询的条数会被收敛，不会因为传大值而拉全表
    let response = app
        .router()
        .oneshot(authorized(
            get(&format!("/api/keys/{key_id}/usage?limit=999999")),
            &admin_token,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn charging_a_disabled_key_reports_forbidden_not_insufficient() {
    let app = TestApp::new().await;
    let key = app.make_key("10", Vec::new()).await;

    // 管理员把它停用，但调用方手里还是停用前的快照
    key_service::update(
        app.data(),
        &key.id,
        aequi::model::KeyUpdate {
            enabled: Some(false),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let error = charge_total(app.data(), &key, "demo-model", 100)
        .await
        .expect_err("停用的密钥不应扣费成功");
    assert!(
        matches!(error, Error::Forbidden(_)),
        "应报权限问题，而不是误导成余额问题"
    );
    assert_eq!(error.status(), StatusCode::FORBIDDEN);

    // 余额没有被误扣
    let current = key_service::get(app.data(), &key.id).await.unwrap();
    assert_eq!(credits::format_credits(current.credits_scaled), "10.0000");

    // 确实余额不足时，仍然是 402
    let poor = app.make_key("0.0001", Vec::new()).await;
    let error = charge_total(app.data(), &poor, "demo-model", 100)
        .await
        .expect_err("余额不足应当报错");
    assert!(matches!(error, Error::InsufficientCredits));
    assert_eq!(error.status(), StatusCode::PAYMENT_REQUIRED);
}

#[tokio::test]
async fn deleting_a_key_cascades_its_usage_records() {
    let app = TestApp::new().await;
    let key = app.make_key("10", Vec::new()).await;

    charge_total(app.data(), &key, "demo-model", 5_000)
        .await
        .unwrap();
    assert_eq!(
        key_service::usage(app.data(), &key.id, 10).await.unwrap().len(),
        1
    );

    key_service::delete(app.data(), &key.id).await.unwrap();

    // 记录随密钥一起清掉，不留下无主的孤儿数据
    assert_eq!(
        key_service::usage(app.data(), &key.id, 10).await.unwrap().len(),
        0
    );
    assert_eq!(
        key_service::usage_summary(app.data(), &key.id)
            .await
            .unwrap()
            .requests,
        0
    );
}

#[tokio::test]
async fn usage_limit_is_clamped_by_the_data_layer() {
    let app = TestApp::new().await;
    let key = app.make_key("10", Vec::new()).await;

    for _ in 0..3 {
        charge_total(app.data(), &key, "demo-model", 1)
            .await
            .unwrap();
    }

    // 负数在 SQLite 里表示「不设上限」，必须被数据层挡住
    let records = key_service::usage(app.data(), &key.id, -1).await.unwrap();
    assert_eq!(records.len(), 1, "负数 limit 应被收敛为 1");

    // 超上限的值收敛到数据层允许的最大值，这里只有 3 条，就返回 3 条
    let records = key_service::usage(app.data(), &key.id, 999_999)
        .await
        .unwrap();
    assert_eq!(records.len(), 3);
}

#[tokio::test]
async fn unlimited_key_never_runs_out_but_still_records_usage() {
    let app = TestApp::new().await;
    // 余额为 0 且无限额度：这正是「初始额度无限」的常见形态。
    let key = app.make_key_with("0", Vec::new(), true).await;

    for _ in 0..3 {
        let outcome = charge_total(app.data(), &key, "demo-model", 10_000)
            .await
            .expect("无限额度不该因余额不足失败");
        assert_eq!(outcome.credits_scaled, credits::CREDIT_SCALE);
    }

    // 余额始终不动。
    let current = key_service::get(app.data(), &key.id).await.unwrap();
    assert_eq!(current.credits_scaled, 0, "无限额度不应扣减余额");
    assert!(current.unlimited);

    // 但消耗照记：三笔记录，合计 3 积分。
    let records = key_service::usage(app.data(), &key.id, 10).await.unwrap();
    assert_eq!(records.len(), 3, "无限额度同样每次请求都记账");
    let total: i64 = records.iter().map(|record| record.credits_scaled).sum();
    assert_eq!(total, 3 * credits::CREDIT_SCALE);
}

#[tokio::test]
async fn switching_off_unlimited_restores_the_balance_check() {
    let app = TestApp::new().await;
    let key = app.make_key_with("0", Vec::new(), true).await;

    // 无限时随便花。
    charge_total(app.data(), &key, "demo-model", 10_000)
        .await
        .unwrap();

    // 切回有限额度：余额还是 0，下一次就该被拒。
    let updated = key_service::set_credits(
        app.data(),
        &key.id,
        aequi::model::KeyCreditsUpdate {
            credits: Some("0".to_string()),
            unlimited: Some(false),
        },
    )
    .await
    .unwrap();
    assert!(!updated.unlimited);

    let error = charge_total(app.data(), &updated, "demo-model", 1)
        .await
        .expect_err("切回有限额度后余额为 0，应拒绝");
    assert!(matches!(error, aequi::Error::InsufficientCredits));
}
