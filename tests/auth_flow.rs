//! 端到端测试：走真实路由与真实 SQLite，验证完整认证链路。
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
use serde_json::json;
use tower::ServiceExt;

use aequi::config::{Settings, SettingsPatch};
use aequi::db;
use aequi::service::{password, token};
use aequi::utils::{random, time};
use aequi::{api, AppState};

const DEMO_USERNAME: &str = "mock-operator";
const DEMO_PASSWORD: &str = "mock-password-for-tests";

/// 每个测试用例使用独立的临时数据库文件。
struct TestApp {
    state: Arc<AppState>,
    dir: PathBuf,
}

impl TestApp {
    async fn new() -> Self {
        Self::with_settings(Settings::default()).await
    }

    async fn with_settings(settings: Settings) -> Self {
        let dir = std::env::temp_dir().join(format!("aequi-test-{}", random::string(12)));
        std::fs::create_dir_all(&dir).expect("创建临时目录失败");

        // 与真实启动一致：控制面与数据面分开两个库文件。
        let store = db::Store::connect(&dir.join("test.db"), &dir.join("test-data.db"))
            .await
            .expect("建库失败");
        store.migrate().await.expect("建表失败");

        // 与真实启动一致：设置先落库，再由内存缓存读用。
        aequi::service::settings::ensure(&store.control)
            .await
            .expect("初始化设置失败");

        let state = Arc::new(AppState::new(settings, store));
        Self { state, dir }
    }

    fn router(&self) -> Router {
        api::router(self.state.clone())
    }

    /// 直接写入一个账号，跳过随机初始化，便于用固定凭据测试登录。
    async fn seed_account(&self) -> i64 {
        let hash = password::hash(DEMO_PASSWORD.to_string())
            .await
            .expect("计算密码哈希失败");

        db::account::insert(self.state.pool(), DEMO_USERNAME, &hash, 0)
            .await
            .expect("写入账号失败")
            .id
    }

    /// 登录并返回令牌。
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
        let body = json_body(response).await;
        body["token"].as_str().expect("响应缺少 token").to_string()
    }
}

impl Drop for TestApp {
    fn drop(&mut self) {
        // 连接池在 Windows 上可能仍持有文件句柄，删不掉也无所谓。
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

fn json_request(method: Method, uri: &str, payload: serde_json::Value) -> Request<Body> {
    with_connect_info(
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::USER_AGENT, "aequi-tests")
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

async fn json_body(response: axum::response::Response) -> serde_json::Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).expect("响应不是合法 JSON")
}

#[tokio::test]
async fn first_start_generates_credentials_only_once() {
    let app = TestApp::new().await;

    let first = aequi::service::auth::initialize_account(app.state.pool(), true)
        .await
        .expect("初始化失败");

    let credentials = first.expect("首次启动应当生成初始凭据");
    assert_eq!(credentials.username.len(), 10);
    assert_eq!(credentials.password.len(), 24);
    assert!(
        credentials.username.chars().all(|c| c.is_ascii_alphanumeric()),
        "用户名应为字母加数字"
    );
    assert!(
        credentials.password.chars().all(|c| c.is_ascii_alphanumeric()),
        "密码应为字母加数字"
    );
    assert!(!credentials.from_env);

    let second = aequi::service::auth::initialize_account(app.state.pool(), true)
        .await
        .expect("二次初始化失败");
    assert!(second.is_none(), "已有账号时不应再生成凭据");

    // 数据库已存在（非首次）时同样不重复建账号，也不接受外部输入。
    let third = aequi::service::auth::initialize_account(app.state.pool(), false)
        .await
        .expect("三次初始化失败");
    assert!(third.is_none());
}

#[tokio::test]
async fn login_then_read_profile() {
    let app = TestApp::new().await;
    app.seed_account().await;

    let token = app.login().await;

    let response = app
        .router()
        .oneshot(authorized(get("/api/auth/me"), &token))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["username"], DEMO_USERNAME);
    assert_eq!(body["totp_enabled"], false);
    assert!(body.get("password_hash").is_none(), "不应泄露密码哈希");
}

#[tokio::test]
async fn protected_endpoint_rejects_missing_or_bad_token() {
    let app = TestApp::new().await;
    app.seed_account().await;

    let response = app.router().oneshot(get("/api/auth/me")).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = json_body(response).await;
    assert_eq!(body["error"]["type"], "unauthorized");

    let response = app
        .router()
        .oneshot(authorized(get("/api/auth/me"), "mock-invalid-token"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn wrong_password_is_rejected() {
    let app = TestApp::new().await;
    app.seed_account().await;

    let response = app
        .router()
        .oneshot(json_request(
            Method::POST,
            "/api/auth/login",
            json!({ "username": DEMO_USERNAME, "password": "wrong-password" }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = json_body(response).await;
    assert_eq!(body["error"]["type"], "invalid_credentials");
}

#[tokio::test]
async fn multiple_logins_create_independent_devices() {
    let app = TestApp::new().await;
    app.seed_account().await;

    let first = app.login().await;
    let second = app.login().await;
    assert_ne!(first, second, "两次登录应产生不同令牌");

    // 后登录的这次作为「当前设备」查看列表。
    let body = json_body(
        app.router()
            .oneshot(authorized(get("/api/devices"), &second))
            .await
            .unwrap(),
    )
    .await;

    let devices = body["devices"].as_array().expect("devices 应为数组");
    assert_eq!(devices.len(), 2, "两台设备都应在线，会话不做互斥");
    assert_eq!(
        devices.iter().filter(|d| d["current"] == true).count(),
        1,
        "应恰好标记出一台当前设备"
    );

    // 撤销第一台设备后，它的令牌立即失效。
    let other_device_id = devices
        .iter()
        .find(|device| device["current"] == false)
        .and_then(|device| device["id"].as_str())
        .expect("应能取到另一台设备的 ID")
        .to_string();

    let response = app
        .router()
        .oneshot(authorized(
            Request::builder()
                .method(Method::DELETE)
                .uri(format!("/api/devices/{other_device_id}"))
                .body(Body::empty())
                .unwrap(),
            &second,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // 被撤销的是首次登录那台，它的令牌（first）应当失效，而当前设备不受影响。
    let response = app
        .router()
        .oneshot(authorized(get("/api/auth/me"), &first))
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "被撤销的设备不应再能访问"
    );

    let response = app
        .router()
        .oneshot(authorized(get("/api/auth/me"), &second))
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "当前设备不应受其他设备被撤销的影响"
    );
}

#[tokio::test]
async fn otp_flow_requires_code_after_enabling() {
    let app = TestApp::new().await;
    let account_id = app.seed_account().await;
    let token = app.login().await;

    // 1. 取回密钥
    let body = json_body(
        app.router()
            .oneshot(authorized(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/otp/setup")
                    .body(Body::empty())
                    .unwrap(),
                &token,
            ))
            .await
            .unwrap(),
    )
    .await;

    let secret = body["secret"].as_str().expect("缺少 secret").to_string();
    assert!(body["otpauth_url"]
        .as_str()
        .expect("缺少 otpauth_url")
        .starts_with("otpauth://totp/"));

    // 2. 用验证器算法算出当前验证码并确认绑定
    let code = current_code(&secret);
    let response = app
        .router()
        .oneshot(authorized(
            json_request(Method::POST, "/api/otp/enable", json!({ "code": code })),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // 3. 此后缺少验证码的登录会被要求补验证码
    let response = app
        .router()
        .oneshot(json_request(
            Method::POST,
            "/api/auth/login",
            json!({ "username": DEMO_USERNAME, "password": DEMO_PASSWORD }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = json_body(response).await;
    assert_eq!(body["error"]["type"], "totp_required");

    // 4. 带上验证码即可登录
    let response = app
        .router()
        .oneshot(json_request(
            Method::POST,
            "/api/auth/login",
            json!({ "username": DEMO_USERNAME, "password": DEMO_PASSWORD, "otp": current_code(&secret) }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["account"]["totp_enabled"], true);

    // 5. 会话应记录实际使用的登录方式
    let devices = json_body(
        app.router()
            .oneshot(authorized(
                get("/api/devices"),
                body["token"].as_str().unwrap(),
            ))
            .await
            .unwrap(),
    )
    .await;
    let login_types: Vec<&str> = devices["devices"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["login_type"].as_str())
        .collect();
    assert!(login_types.contains(&"password+totp"));
    assert!(login_types.contains(&"password"));

    // 6. 关闭 OTP 需要验证码
    let response = app
        .router()
        .oneshot(authorized(
            json_request(Method::POST, "/api/otp/disable", json!({ "code": current_code(&secret) })),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let account = db::account::find_by_id(app.state.pool(), account_id)
        .await
        .unwrap()
        .unwrap();
    assert!(!account.totp_enabled);
    assert!(account.totp_secret.is_none(), "关闭后应清除密钥");
}

#[tokio::test]
async fn settings_are_seeded_then_sourced_from_database() {
    let app = TestApp::new().await;

    let seeded = aequi::service::settings::ensure(app.state.pool())
        .await
        .expect("载入设置失败");
    assert_eq!(seeded, Settings::default(), "首次启动应导入默认设置");

    let stored = db::settings::load_all(app.state.pool()).await.unwrap();
    assert_eq!(
        stored.len(),
        Settings::default().to_entries().len(),
        "所有设置项都应写入数据库"
    );

    // 只提交一个字段，其余保持不变
    let updated = aequi::service::settings::update(
        app.state.pool(),
        &seeded,
        SettingsPatch {
            token_ttl_hours: Some(12),
            ..Default::default()
        },
    )
    .await
    .expect("更新设置失败");
    assert_eq!(updated.token_ttl_hours, 12);
    assert_eq!(updated.listen, seeded.listen);

    // 重新载入（相当于重启）应当读到数据库里的值
    let reloaded = aequi::service::settings::ensure(app.state.pool())
        .await
        .unwrap();
    assert_eq!(reloaded.token_ttl_hours, 12, "设置应以数据库为准");
}

#[tokio::test]
async fn settings_api_requires_auth_and_reports_restart() {
    let app = TestApp::new().await;
    app.seed_account().await;

    let response = app.router().oneshot(get("/api/settings")).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let token = app.login().await;

    let body = json_body(
        app.router()
            .oneshot(authorized(get("/api/settings"), &token))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(body["token_ttl_hours"], 168);
    assert_eq!(body["restart_required"], false);

    // 令牌有效期即时生效，不需要重启
    let body = json_body(
        app.router()
            .oneshot(authorized(
                json_request(Method::PATCH, "/api/settings", json!({ "token_ttl_hours": 24 })),
                &token,
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(body["token_ttl_hours"], 24);
    assert_eq!(body["restart_required"], false);

    // 监听地址需要重启才生效
    let body = json_body(
        app.router()
            .oneshot(authorized(
                json_request(Method::PATCH, "/api/settings", json!({ "listen": "0.0.0.0:9000" })),
                &token,
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(body["listen"], "0.0.0.0:9000");
    assert_eq!(body["restart_required"], true);

    // 非法值被拒绝
    let response = app
        .router()
        .oneshot(authorized(
            json_request(Method::PATCH, "/api/settings", json!({ "token_ttl_hours": 0 })),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn token_ttl_setting_governs_new_sessions() {
    let app = TestApp::with_settings(Settings {
        token_ttl_hours: 1,
        ..Settings::default()
    })
    .await;
    app.seed_account().await;

    let token = app.login().await;

    let body = json_body(
        app.router()
            .oneshot(authorized(get("/api/devices"), &token))
            .await
            .unwrap(),
    )
    .await;

    let device = &body["devices"][0];
    let created = device["created_at"].as_i64().unwrap();
    let expires = device["expires_at"].as_i64().unwrap();

    assert_eq!(expires - created, 3600, "会话有效期应取数据库中的设置");
}

#[tokio::test]
async fn deleted_session_is_gone_from_the_database() {
    let app = TestApp::new().await;
    app.seed_account().await;

    let secret = app.login().await;
    let hash = token::hash(&secret);
    let now = time::now_secs();

    let session = db::session::find_active_by_token(app.state.pool(), &hash, now)
        .await
        .unwrap()
        .expect("刚登录的会话应当有效");

    assert!(
        db::session::delete(app.state.pool(), &session.id, session.account_id)
            .await
            .unwrap(),
        "首次删除应当生效"
    );

    // 退出即删除：记录不该以任何形式留在库里
    assert!(
        db::session::find_by_id(app.state.pool(), &session.id)
            .await
            .unwrap()
            .is_none(),
        "退出的会话应当被删除，而不是打标记留着"
    );
    assert!(
        db::session::find_active_by_token(app.state.pool(), &hash, now)
            .await
            .unwrap()
            .is_none(),
        "已退出的会话不应被按令牌查到"
    );
    assert!(
        !db::session::touch(app.state.pool(), &session.id, now)
            .await
            .unwrap(),
        "已退出的会话不应被刷新"
    );

    // 令牌立即失效
    let response = app
        .router()
        .oneshot(authorized(get("/api/auth/me"), &secret))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn expired_session_is_ignored_and_purged() {
    let app = TestApp::new().await;
    app.seed_account().await;

    let secret = app.login().await;
    let hash = token::hash(&secret);
    let now = time::now_secs();

    let session = db::session::find_active_by_token(app.state.pool(), &hash, now)
        .await
        .unwrap()
        .unwrap();

    set_session_expiry(app.state.pool(), &session.id, now - 10).await;

    assert!(
        db::session::find_active_by_token(app.state.pool(), &hash, now)
            .await
            .unwrap()
            .is_none(),
        "已过期的会话不应被查到"
    );
    assert!(!db::session::touch(app.state.pool(), &session.id, now)
        .await
        .unwrap());

    let response = app
        .router()
        .oneshot(authorized(get("/api/auth/me"), &secret))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let removed = db::session::purge(app.state.pool(), now).await.unwrap();
    assert_eq!(removed, 1, "过期会话应被回收");
    assert!(
        db::session::find_by_id(app.state.pool(), &session.id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn purge_only_reclaims_expired_sessions() {
    let app = TestApp::new().await;
    app.seed_account().await;

    let expired_secret = app.login().await;
    let active_secret = app.login().await;
    let now = time::now_secs();

    let expired =
        db::session::find_active_by_token(app.state.pool(), &token::hash(&expired_secret), now)
            .await
            .unwrap()
            .unwrap();
    let active =
        db::session::find_active_by_token(app.state.pool(), &token::hash(&active_secret), now)
            .await
            .unwrap()
            .unwrap();

    // 把其中一个的过期时间拨到过去
    set_session_expiry(app.state.pool(), &expired.id, now - 10).await;

    let removed = db::session::purge(app.state.pool(), now).await.unwrap();
    assert_eq!(removed, 1, "只应回收过期的那条");
    assert!(
        db::session::find_by_id(app.state.pool(), &expired.id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        db::session::find_by_id(app.state.pool(), &active.id)
            .await
            .unwrap()
            .is_some(),
        "在线会话不应被动到"
    );
}

#[tokio::test]
async fn device_list_shows_token_prefix_and_drops_signed_out_devices() {
    let app = TestApp::new().await;
    app.seed_account().await;

    let first = app.login().await;
    let second = app.login().await;

    let list = json_body(
        app.router()
            .oneshot(authorized(get("/api/devices"), &second))
            .await
            .unwrap(),
    )
    .await;
    let devices = list["devices"].as_array().unwrap();
    assert_eq!(devices.len(), 2);

    // 每条都带令牌前缀，且前缀确实是各自令牌的开头
    let prefixes: Vec<&str> = devices
        .iter()
        .filter_map(|device| device["token_prefix"].as_str())
        .collect();
    assert_eq!(prefixes.len(), 2, "每条设备都应带令牌前缀");
    assert!(prefixes.contains(&&first[..8]), "应能认出第一台设备");
    assert!(prefixes.contains(&&second[..8]), "应能认出当前设备");

    // 退出之后，那台设备从列表里消失，而不是变成一条「已失效」
    let response = app
        .router()
        .oneshot(authorized(
            Request::builder()
                .method(Method::POST)
                .uri("/api/auth/logout")
                .body(Body::empty())
                .unwrap(),
            &second,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let list = json_body(
        app.router()
            .oneshot(authorized(get("/api/devices"), &first))
            .await
            .unwrap(),
    )
    .await;
    let devices = list["devices"].as_array().unwrap();
    assert_eq!(devices.len(), 1, "退出的设备不应再出现在列表里");
    assert_eq!(devices[0]["token_prefix"], &first[..8]);
}

#[tokio::test]
async fn oversized_user_agent_is_truncated() {
    let app = TestApp::new().await;
    app.seed_account().await;

    let mut request = Request::builder()
        .method(Method::POST)
        .uri("/api/auth/login")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::USER_AGENT, "u".repeat(4000))
        .body(Body::from(
            json!({ "username": DEMO_USERNAME, "password": DEMO_PASSWORD }).to_string(),
        ))
        .unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 43210))));

    let response = app.router().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = json_body(response).await;
    let secret = body["token"].as_str().unwrap();

    let devices = json_body(
        app.router()
            .oneshot(authorized(get("/api/devices"), secret))
            .await
            .unwrap(),
    )
    .await;

    let stored = devices["devices"][0]["user_agent"].as_str().unwrap();
    assert_eq!(stored.chars().count(), 512, "超长 User-Agent 应被截断");
}

/// 直接把会话的过期时间改到指定时刻，用于构造过期场景。
async fn set_session_expiry(pool: &sqlx::SqlitePool, id: &str, expires_at: i64) {
    sqlx::query("UPDATE sessions SET expires_at = ? WHERE id = ?")
        .bind(expires_at)
        .bind(id)
        .execute(pool)
        .await
        .expect("更新过期时间失败");
}

#[tokio::test]
async fn panel_rejects_path_traversal() {
    let app = TestApp::new().await;

    // %2f 会被解码成 /，拼出 ../Cargo.toml —— 必须被挡在资源服务之外
    let response = app
        .router()
        .oneshot(get("/web/..%2fCargo.toml"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND, "路径穿越应被拒绝");

    let response = app
        .router()
        .oneshot(get("/web/%2e%2e%2f%2e%2e%2fCargo.toml"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // 正常路径不受影响
    let response = app.router().oneshot(get("/web/login")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK, "SPA 回退应照常工作");
}

#[tokio::test]
async fn forwarded_headers_are_only_trusted_when_enabled() {
    // 默认关闭：转发头由客户端掌控，不能用来伪造登录 IP
    let app = TestApp::new().await;
    app.seed_account().await;
    let token = login_with_forwarded(&app, "203.0.113.9").await;
    assert_eq!(
        device_ip(&app, &token).await,
        "127.0.0.1",
        "默认不应采信 X-Forwarded-For"
    );

    // 显式开启后才采信
    let app = TestApp::with_settings(Settings {
        trust_proxy: true,
        ..Settings::default()
    })
    .await;
    app.seed_account().await;
    let token = login_with_forwarded(&app, "203.0.113.9").await;
    assert_eq!(
        device_ip(&app, &token).await,
        "203.0.113.9",
        "开启 trust_proxy 后应采信转发头"
    );
}

#[tokio::test]
async fn restart_flag_reflects_actual_change() {
    let app = TestApp::new().await;
    app.seed_account().await;
    let token = app.login().await;
    let listen = app.state.settings().listen.to_string();

    // 提交与当前相同的值 → 不需要重启
    let body = json_body(
        app.router()
            .oneshot(authorized(
                json_request(Method::PATCH, "/api/settings", json!({ "listen": listen })),
                &token,
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(body["restart_required"], false, "同值提交不应要求重启");

    // 真正改动 → 需要重启
    let body = json_body(
        app.router()
            .oneshot(authorized(
                json_request(Method::PATCH, "/api/settings", json!({ "listen": "0.0.0.0:9999" })),
                &token,
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(body["restart_required"], true);
}

/// 带自定义转发头登录，返回令牌。
async fn login_with_forwarded(app: &TestApp, forwarded_for: &str) -> String {
    let request = with_connect_info(
        Request::builder()
            .method(Method::POST)
            .uri("/api/auth/login")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::USER_AGENT, "aequi-tests")
            .header("x-forwarded-for", forwarded_for)
            .body(Body::from(
                json!({ "username": DEMO_USERNAME, "password": DEMO_PASSWORD }).to_string(),
            ))
            .unwrap(),
    );

    let response = app.router().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK, "登录应当成功");
    json_body(response).await["token"]
        .as_str()
        .unwrap()
        .to_string()
}

/// 取当前设备记录里的 IP。
async fn device_ip(app: &TestApp, token: &str) -> String {
    let body = json_body(
        app.router()
            .oneshot(authorized(get("/api/devices"), token))
            .await
            .unwrap(),
    )
    .await;

    body["devices"][0]["ip"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn password_change_verifies_current_and_revokes_other_devices() {
    let app = TestApp::new().await;
    app.seed_account().await;

    // 两台设备同时在线
    let first = app.login().await;
    let second = app.login().await;

    // 当前密码不对 → 拒绝
    let response = app
        .router()
        .oneshot(authorized(
            json_request(
                Method::POST,
                "/api/auth/password",
                json!({
                    "current_password": "not-the-password",
                    "new_password": "new-secret",
                    "confirm_password": "new-secret",
                }),
            ),
            &first,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // 两次输入不一致 → 400（前端已比对，后端不依赖它）
    let response = app
        .router()
        .oneshot(authorized(
            json_request(
                Method::POST,
                "/api/auth/password",
                json!({
                    "current_password": DEMO_PASSWORD,
                    "new_password": "new-secret",
                    "confirm_password": "new-secret-typo",
                }),
            ),
            &first,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // 与当前密码相同 → 拒绝
    let response = app
        .router()
        .oneshot(authorized(
            json_request(
                Method::POST,
                "/api/auth/password",
                json!({
                    "current_password": DEMO_PASSWORD,
                    "new_password": DEMO_PASSWORD,
                    "confirm_password": DEMO_PASSWORD,
                }),
            ),
            &first,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // 正常修改
    let body = json_body(
        app.router()
            .oneshot(authorized(
                json_request(
                    Method::POST,
                    "/api/auth/password",
                    json!({
                        "current_password": DEMO_PASSWORD,
                        "new_password": "new-secret",
                        "confirm_password": "new-secret",
                    }),
                ),
                &first,
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(body["revoked_devices"], 1, "应撤销其它设备");

    // 发起修改的设备保持登录
    let response = app
        .router()
        .oneshot(authorized(get("/api/auth/me"), &first))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // 另一台设备被要求重新登录
    let response = app
        .router()
        .oneshot(authorized(get("/api/auth/me"), &second))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // 旧密码失效、新密码可用
    let response = app
        .router()
        .oneshot(json_request(
            Method::POST,
            "/api/auth/login",
            json!({ "username": DEMO_USERNAME, "password": DEMO_PASSWORD }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let response = app
        .router()
        .oneshot(json_request(
            Method::POST,
            "/api/auth/login",
            json!({ "username": DEMO_USERNAME, "password": "new-secret" }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn cli_reset_generates_a_fresh_random_password() {
    let app = TestApp::new().await;
    let account_id = app.seed_account().await;

    // 重置不接受外部密码，直接生成一个新的——与首次初始化同一口径
    let password = aequi::service::auth::reset_password(app.state.pool(), account_id)
        .await
        .expect("重置应当成功");

    assert_eq!(password.len(), 24, "应与初始化一致：24 位");
    assert!(
        password.chars().all(|c| c.is_ascii_alphanumeric()),
        "应为字母加数字"
    );

    // 新密码可用
    let response = app
        .router()
        .oneshot(json_request(
            Method::POST,
            "/api/auth/login",
            json!({ "username": DEMO_USERNAME, "password": password }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "重置后的新密码应当可用");

    // 旧密码立即失效
    let response = app
        .router()
        .oneshot(json_request(
            Method::POST,
            "/api/auth/login",
            json!({ "username": DEMO_USERNAME, "password": DEMO_PASSWORD }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // 每次重置都是新密码
    let again = aequi::service::auth::reset_password(app.state.pool(), account_id)
        .await
        .expect("再次重置应当成功");
    assert_ne!(password, again, "两次重置不应得到同一个密码");
}

#[tokio::test]
async fn username_change_requires_password_and_takes_effect() {
    let app = TestApp::new().await;
    app.seed_account().await;
    let token = app.login().await;

    // 当前密码错误 → 拒绝
    let response = app
        .router()
        .oneshot(authorized(
            json_request(
                Method::POST,
                "/api/auth/username",
                json!({ "current_password": "not-the-password", "username": "renamed" }),
            ),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // 空用户名 → 拒绝
    let response = app
        .router()
        .oneshot(authorized(
            json_request(
                Method::POST,
                "/api/auth/username",
                json!({ "current_password": DEMO_PASSWORD, "username": "   " }),
            ),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // 与当前相同 → 拒绝
    let response = app
        .router()
        .oneshot(authorized(
            json_request(
                Method::POST,
                "/api/auth/username",
                json!({ "current_password": DEMO_PASSWORD, "username": DEMO_USERNAME }),
            ),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // 正常修改：首尾空白应被去掉
    let body = json_body(
        app.router()
            .oneshot(authorized(
                json_request(
                    Method::POST,
                    "/api/auth/username",
                    json!({ "current_password": DEMO_PASSWORD, "username": "  renamed-user  " }),
                ),
                &token,
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(body["username"], "renamed-user");

    // 旧用户名登录失败，新用户名 + 原密码可用
    let response = app
        .router()
        .oneshot(json_request(
            Method::POST,
            "/api/auth/login",
            json!({ "username": DEMO_USERNAME, "password": DEMO_PASSWORD }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let response = app
        .router()
        .oneshot(json_request(
            Method::POST,
            "/api/auth/login",
            json!({ "username": "renamed-user", "password": DEMO_PASSWORD }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // 改名不踢设备，当前会话照常可用
    let response = app
        .router()
        .oneshot(authorized(get("/api/auth/me"), &token))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

/// 用与验证器 App 相同的方式算出当前验证码。
fn current_code(secret_base32: &str) -> String {
    use totp_rs::{Algorithm, Builder, Secret};

    let secret = Secret::try_from_base32(secret_base32).expect("密钥格式错误");
    Builder::new()
        .with_algorithm(Algorithm::SHA1)
        .with_secret(secret)
        .build_noncompliant()
        .generate_current()
        .to_string()
}
