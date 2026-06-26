use crate::admin;
use crate::state::RouterState;
use bytes::Bytes;
use hyper::{Body, Method, Request, Response};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use tokio_stream::wrappers::ReceiverStream;

// Embedded static files from dist/
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "src/static/dist"]
struct Asset;

/// Get content-type and cache-control for a file path.
fn mime_and_cache(path: &str) -> (&'static str, &'static str) {
    match path.rsplit('.').next() {
        Some("html") => ("text/html; charset=utf-8", "no-store"),
        Some("js") => ("application/javascript; charset=utf-8", "max-age=31536000, immutable"),
        Some("css") => ("text/css; charset=utf-8", "max-age=31536000, immutable"),
        Some("woff") => ("font/woff", "max-age=31536000, immutable"),
        Some("woff2") => ("font/woff2", "max-age=31536000, immutable"),
        Some("ttf") => ("font/ttf", "max-age=31536000, immutable"),
        Some("otf") => ("font/otf", "max-age=31536000, immutable"),
        Some("json") => ("application/json", "no-store"),
        Some("png") => ("image/png", "max-age=31536000, immutable"),
        Some("jpg") | Some("jpeg") => ("image/jpeg", "max-age=31536000, immutable"),
        Some("gif") => ("image/gif", "max-age=31536000, immutable"),
        Some("svg") => ("image/svg+xml", "max-age=31536000, immutable"),
        Some("ico") => ("image/x-icon", "max-age=31536000, immutable"),
        Some("webp") => ("image/webp", "max-age=31536000, immutable"),
        Some("avif") => ("image/avif", "max-age=31536000, immutable"),
        Some("wasm") => ("application/wasm", "max-age=31536000, immutable"),
        Some("map") => ("application/json", "max-age=31536000, immutable"),
        _ => ("application/octet-stream", "max-age=31536000, immutable"),
    }
}

/// Main entry point for admin routes (web UI + API).
pub async fn handle_admin(req: Request<Body>, state: Arc<RouterState>) -> Response<Body> {
    let path = req.uri().path();

    // Serve static files from /web/ (no auth — SPA handles token input).
    if req.method() == Method::GET && path == "/web" {
        return Response::builder()
            .status(http::StatusCode::MOVED_PERMANENTLY)
            .header(http::header::LOCATION, "/web/")
            .body(Body::empty())
            .unwrap_or_else(|e| {
                tracing::error!(error = %e, "redirect response builder failed");
                RouterState::json_error(http::StatusCode::INTERNAL_SERVER_ERROR, "response_build", "internal_error")
            });
    }
    let is_web = req.method() == Method::GET
        && (path == "/web/" || path.starts_with("/web/"));
    if is_web {
        let file = path.strip_prefix("/web").unwrap_or("");
        let file = file.strip_prefix('/').unwrap_or("");
        let file = if file.is_empty() { "index.html" } else { file };

        if let Some(asset) = Asset::get(file) {
            let (content_type, cache_control) = mime_and_cache(file);
            return Response::builder()
                .status(200)
                .header("content-type", content_type)
                .header("cache-control", cache_control)
                .body(Body::from(asset.data.as_ref().to_vec()))
                .unwrap_or_else(|e| {
                    tracing::error!(error = %e, "static file response builder failed");
                    RouterState::json_error(http::StatusCode::INTERNAL_SERVER_ERROR, "response_build", "internal_error")
                });
        }

        // File not found in dist
        return Response::builder()
            .status(404)
            .header("content-type", "text/plain; charset=utf-8")
            .body(Body::from("404 Not Found"))
            .unwrap_or_else(|e| {
                tracing::error!(error = %e, "404 response builder failed");
                RouterState::json_error(http::StatusCode::INTERNAL_SERVER_ERROR, "response_build", "internal_error")
            });
    }

    // API
    if path.starts_with("/admin/api/") {
        return handle_api(req, state).await;
    }

    Response::builder()
        .status(404)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Body::from("not found"))
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "404 fallback builder failed");
            RouterState::json_error(http::StatusCode::INTERNAL_SERVER_ERROR, "response_build", "internal_error")
        })
}

async fn handle_api(req: Request<Body>, state: Arc<RouterState>) -> Response<Body> {
    let path = req.uri().path().to_string();
    let method = req.method().clone();

    // All admin API endpoints require admin token via X-Admin-Token header.
    let admin_ok = state.authorize_admin_header(&req);
    if !admin_ok {
        return RouterState::json_error(
            http::StatusCode::UNAUTHORIZED,
            "missing or invalid admin token",
            "admin_unauthorized",
        );
    }

    match (&method, path.as_str()) {
        (&Method::GET, "/admin/api/v1/stats/stream") => stats_stream(state).await,
        (&Method::GET, "/admin/api/v1/upstreams") => admin::api_list_upstreams(state).await,
        (&Method::POST, "/admin/api/v1/upstreams") => admin::api_add_upstream(req, state).await,
        (&Method::GET, "/admin/api/v1/stats") => admin::api_stats_snapshot(state).await,
        (&Method::GET, "/admin/api/v1/model-costs") => admin::api_get_model_costs(state).await,
        (&Method::POST, "/admin/api/v1/model-costs") => admin::api_set_model_costs(req, state).await,
        (&Method::POST, "/admin/api/v1/reload") => admin::api_reload_all(state).await,
        (&Method::GET, "/admin/api/v1/config") => admin::api_config_preview(state).await,
        (&Method::GET, "/admin/api/v1/models/routes") => admin::api_get_model_routes(state).await,
        (&Method::PUT, "/admin/api/v1/models/routes") => admin::api_put_model_routes(req, state).await,
        (&Method::GET, "/admin/api/v1/requests/stream") => requests_stream(state).await,
        (&Method::GET, "/admin/api/v1/requests") => admin::api_requests(state, req.uri()).await,
        (&Method::GET, "/admin/api/v1/requests/history") => admin::api_requests_history(state, req.uri()).await,
        (&Method::GET, "/admin/api/v1/metrics") => admin::api_metrics(state, req.uri()).await,
        (&Method::GET, "/admin/api/v1/billing/keys") => crate::billing::api_billing_list_keys(state).await,
        (&Method::GET, "/admin/api/v1/billing/overview") => crate::billing::api_billing_overview(state).await,
        (&Method::POST, "/admin/api/v1/billing/keys") => crate::billing::api_billing_create_key(req, state).await,
        _ => {
            // Dynamic routes:
            if let Some(rest) = path.strip_prefix("/admin/api/v1/billing/") {
                if rest != "keys" && rest != "overview" {
                    return crate::billing::handle_billing_key_subroutes(req, state, rest).await;
                }
            }
            if let Some(rest) = path.strip_prefix("/admin/api/v1/upstreams/") {
                return admin::handle_upstream_subroutes(req, state, rest).await;
            }
            RouterState::json_error(
                http::StatusCode::NOT_FOUND,
                "not found",
                "not_found",
            )
        }
    }
}

async fn stats_stream(state: Arc<RouterState>) -> Response<Body> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(32);
    let state2 = state.clone();

    tokio::spawn(async move {
        // Sliding window RPM: track request totals over 3 minutes at 2-second intervals.
        // This avoids the old approach (2s delta × 30) which always reads 0 under low traffic.
        const WINDOW_SECS: u64 = 180; // 3-minute sliding window
        const TICK_MS: u64 = 2_000;
        let max_samples = (WINDOW_SECS * 1_000 / TICK_MS) as usize; // 90
        let mut window: VecDeque<u64> = VecDeque::with_capacity(max_samples + 1);

        loop {
            let snap = admin::build_snapshot(&state2);
            let total = snap.requests_total;

            window.push_back(total);
            if window.len() > max_samples {
                window.pop_front();
            }

            // Rolling RPM = requests_in_window / window_duration_in_minutes
            // Need ≥2 samples to compute a meaningful rate.
            let rpm = if window.len() >= 2 {
                let oldest = *window.front().unwrap();
                let newest = *window.back().unwrap();
                let intervals = (window.len() - 1) as u64;
                let window_secs = intervals * (TICK_MS / 1_000);
                let reqs = newest.saturating_sub(oldest);
                reqs * 60 / window_secs.max(1)
            } else {
                0
            };

            let mut v = serde_json::to_value(&snap)
                .unwrap_or(serde_json::json!({"error":"snapshot_failed"}));
            if let serde_json::Value::Object(ref mut m) = v {
                m.insert("rpm".into(), serde_json::json!(rpm));
            }
            let s = match serde_json::to_string(&v) {
                Ok(s) => s,
                Err(_) => String::from(r#"{"error":"json"}"#),
            };
            let msg = format!("data: {}\n\n", s);

            if tx.send(Ok(Bytes::from(msg))).await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });

    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        .body(Body::wrap_stream(ReceiverStream::new(rx)))
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "stats SSE response builder failed");
            RouterState::json_error(http::StatusCode::INTERNAL_SERVER_ERROR, "response_build", "internal_error")
        })
}

async fn requests_stream(state: Arc<RouterState>) -> Response<Body> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(128);
    let mut sub = state.subscribe_requests();

    tokio::spawn(async move {
        loop {
            match sub.recv().await {
                Ok(entry) => {
                    let payload = serde_json::to_string(&entry)
                        .unwrap_or_else(|_| String::from(r#"{"error":"json"}"#));
                    let msg = format!("data: {}\n\n", payload);
                    if tx.send(Ok(Bytes::from(msg))).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        .body(Body::wrap_stream(ReceiverStream::new(rx)))
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "requests SSE response builder failed");
            RouterState::json_error(http::StatusCode::INTERNAL_SERVER_ERROR, "response_build", "internal_error")
        })
}
