//! 首页看板接口（`GET /api/stats/dashboard`）的口径。
//!
//! 以前首页拉 500 条详细日志自己数，于是：窗口只有 500 条（不是「当日」）、
//! 错误提示只在那 500 条里找、每小时柱状图按「现在往前推 11 小时」而不是整点。
//! 这个接口把口径搬到库里，这里逐条钉住。
//!
//! 断言四件事——
//! 1) 「当日」按本地时区当天 0 点切，昨天的记录不进今天的数；
//! 2) 小时图固定 24 个整点桶、等间距、跨度 24 小时，没流量的小时补 0；
//! 3) 最近错误只取 3 条、新的在前、带模型与详细原因；
//! 4) 额度预警只算启用的有限额度密钥，余额低的排前面。
//!
//! 所有数据都是测试构造的假记录，不涉及任何真实服务。

use chrono::{Local, TimeZone};
use sqlx::SqlitePool;

use aequi::db;
use aequi::model::NewProxyLog;
use aequi::utils::time;

const HOUR: i64 = 3_600;

/// 一张只有数据面表的内存库。
async fn test_pool() -> SqlitePool {
    let pool = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("打开内存库");
    db::migrate_data(&pool).await.expect("建数据面表失败");
    pool
}

/// 本地时区里「今天」的 0 点。
///
/// 与被测代码同口径（都按系统本地时区切天），否则测试自己和实现各算各的。
fn today_start() -> i64 {
    let now = time::now_secs();
    Local
        .timestamp_opt(now, 0)
        .single()
        .and_then(|datetime| datetime.date_naive().and_hms_opt(0, 0, 0))
        .and_then(|naive| Local.from_local_datetime(&naive).single())
        .map(|datetime| datetime.timestamp())
        .unwrap_or_else(|| now - now.rem_euclid(86_400))
}

fn log_at(created_at: i64, status: &str, model: &str, error: Option<&str>) -> NewProxyLog {
    NewProxyLog {
        access_key_id: "key-1".to_string(),
        block_id: Some("demo-block".to_string()),
        upstream_key_id: Some("upstream-key-1".to_string()),
        model: model.to_string(),
        ip: Some("127.0.0.1".to_string()),
        user_agent: Some("mock-agent".to_string()),
        status: status.to_string(),
        error_detail: error.map(str::to_string),
        duration_ms: 12,
        prompt_tokens: 10,
        completion_tokens: 5,
        cache_tokens: 0,
        estimated: false,
        stream: false,
        credits_scaled: 0,
        created_at,
    }
}

async fn insert_log(pool: &SqlitePool, log: NewProxyLog) {
    db::proxy_log::insert(pool, &log).await.expect("写日志失败");
}

/// 建一把访问密钥。
async fn insert_key(
    pool: &SqlitePool,
    id: &str,
    name: &str,
    credits_scaled: i64,
    unlimited: bool,
    enabled: bool,
) {
    // 密钥值有唯一约束，拿 id 派生一个，免得几把钥匙撞在同一串上。
    let secret = format!("mock-secret-{id}");
    db::keys::insert(
        pool,
        &db::keys::NewApiKey {
            id,
            name,
            secret: &secret,
            key_prefix: "mock",
            credits_scaled,
            unlimited,
            enabled,
            allowed_models: &[],
            created_at: 0,
        },
    )
    .await
    .expect("插入密钥失败");
}

/// 「当日」按本地 0 点切：昨天的记录不算今天，但可能落在 24 小时窗口里。
#[tokio::test]
async fn today_counts_start_at_local_midnight() {
    let pool = test_pool().await;
    let today = today_start();

    insert_log(&pool, log_at(today + 10, "success", "demo-model", None)).await;
    insert_log(&pool, log_at(today + 20, "success", "demo-model", None)).await;
    insert_log(&pool, log_at(today + 30, "error", "demo-model", Some("上游返回 502"))).await;
    // 昨天：不进「今日」，但两分钟内仍是 24 小时窗口的一部分。
    insert_log(&pool, log_at(today - 60, "success", "demo-model", None)).await;
    insert_log(&pool, log_at(today - 59, "error", "demo-model", Some("上游返回 500"))).await;

    let view = aequi::service::stats::dashboard(&pool)
        .await
        .expect("统计失败");

    assert_eq!(view.today.requests, 3, "昨天那两条不算今天");
    assert_eq!(view.today.success, 2);
    assert_eq!(view.today.failed, 1);

    let hourly_requests: i64 = view.hourly.iter().map(|point| point.requests).sum();
    assert_eq!(hourly_requests, 5, "24 小时窗口包含昨天那两条");
    let hourly_failed: i64 = view.hourly.iter().map(|point| point.failed).sum();
    assert_eq!(hourly_failed, 2);
}

/// 小时图是 24 个等宽整点桶：缺的小时补 0，最后一个桶是当前这一小时。
#[tokio::test]
async fn hourly_buckets_are_whole_hours_and_gapless() {
    let pool = test_pool().await;
    let now = time::now_secs();
    let current_hour = now - now.rem_euclid(HOUR);

    // 只在这一个小时里放一条：其余 23 个桶必须补 0 而不是消失。
    insert_log(&pool, log_at(now, "success", "demo-model", None)).await;

    let view = aequi::service::stats::dashboard(&pool)
        .await
        .expect("统计失败");

    assert_eq!(view.hourly.len(), 24, "固定 24 个桶");
    assert_eq!(
        view.hourly.last().map(|point| point.hour),
        Some(current_hour),
        "最后一个桶是当前这一小时"
    );
    assert_eq!(
        view.hourly.first().map(|point| point.hour),
        Some(current_hour - 23 * HOUR),
        "第一个桶在 23 小时前"
    );
    for points in view.hourly.windows(2) {
        assert_eq!(points[1].hour - points[0].hour, HOUR, "桶必须等间距");
    }

    let filled: Vec<i64> = view
        .hourly
        .iter()
        .filter(|point| point.requests == 0)
        .map(|point| point.hour)
        .collect();
    assert_eq!(filled.len(), 23, "没有流量的小时补 0");
}

/// 最近错误：只要 3 条、新的在前、带模型与详细原因。
#[tokio::test]
async fn recent_errors_are_the_three_newest_failures() {
    let pool = test_pool().await;
    let today = today_start();

    insert_log(&pool, log_at(today + 1, "error", "demo-old", Some("最早的错误"))).await;
    insert_log(&pool, log_at(today + 2, "success", "demo-model", None)).await;
    insert_log(&pool, log_at(today + 3, "error", "demo-mid", Some("中间的错误"))).await;
    insert_log(&pool, log_at(today + 4, "error", "demo-new", Some("最新的错误"))).await;

    let view = aequi::service::stats::dashboard(&pool)
        .await
        .expect("统计失败");

    assert_eq!(view.recent_errors.len(), 3, "首页只放得下三条");
    assert_eq!(
        view.recent_errors
            .iter()
            .map(|error| error.model.as_str())
            .collect::<Vec<_>>(),
        vec!["demo-new", "demo-mid", "demo-old"],
        "新的在前，成功的那条不算错误"
    );
    assert_eq!(
        view.recent_errors[0].error_detail.as_deref(),
        Some("最新的错误"),
        "错误原因要带出来，首页直接显示它"
    );
    assert_eq!(
        view.recent_errors[0].block_id.as_deref(),
        Some("demo-block")
    );
}

/// 额度预警：只算启用的有限额度密钥，余额低的排前面。
#[tokio::test]
async fn low_credit_alert_skips_unlimited_and_disabled_keys() {
    let pool = test_pool().await;

    insert_key(&pool, "key-low", "低额", 10_000, false, true).await; // 1.0000
    insert_key(&pool, "key-lower", "更低", 45_000, false, true).await; // 4.5000
    insert_key(&pool, "key-rich", "充裕", 1_000_000, false, true).await; // 100.0000
    insert_key(&pool, "key-unlimited", "无限", 0, true, true).await;
    insert_key(&pool, "key-disabled", "停用", 0, false, false).await;

    let view = aequi::service::stats::dashboard(&pool)
        .await
        .expect("统计失败");

    assert_eq!(view.keys.total, 5);
    assert_eq!(view.keys.enabled, 4);

    assert_eq!(view.low_credits.count, 2, "无限额度与停用的都不提醒");
    assert_eq!(view.low_credits.threshold, "5.0000");
    assert_eq!(
        view.low_credits
            .keys
            .iter()
            .map(|key| (key.id.as_str(), key.credits.as_str()))
            .collect::<Vec<_>>(),
        vec![("key-low", "1.0000"), ("key-lower", "4.5000")],
        "余额最低的排最前面，详细金额一起给"
    );
}
