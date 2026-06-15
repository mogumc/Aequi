use super::forward::forward;
use super::response::{logged_json_error, record_request};
use super::usage::{extract_api_key, models_list};
use super::{RequestLogContext, RequestLifecycle};
use crate::admin;
use crate::state::RouterState;
use hyper::header::ORIGIN;
use hyper::server::conn::AddrStream;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response, Server};
use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use tracing::Instrument;

// ── HTTP server ───────────────────────────────────────────────────────

pub async fn serve_http<F>(
    addr: SocketAddr,
    state: Arc<RouterState>,
    shutdown: F,
) -> anyhow::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let make_svc = make_service_fn(move |conn: &AddrStream| {
        let state = state.clone();
        let remote_addr = conn.remote_addr();
        async move {
            Ok::<_, Infallible>(service_fn(move |req| {
                let state = state.clone();
                async move { Ok::<_, Infallible>(handle(req, state, remote_addr).await) }
            }))
        }
    });

    let server = Server::bind(&addr)
        .tcp_nodelay(true)
        .serve(make_svc)
        .with_graceful_shutdown(shutdown);

    server.await?;
    Ok(())
}

async fn handle(
    req: Request<Body>,
    state: Arc<RouterState>,
    client_addr: SocketAddr,
) -> Response<Body> {
    let origin = req.headers().get(ORIGIN).cloned();
    if req.method() == hyper::Method::OPTIONS && origin.is_some() {
        return cors_preflight(&state, origin.as_ref());
    }

    let resp = handle_inner(req, state.clone(), client_addr).await;
    add_cors_headers(resp, &state, origin.as_ref())
}

async fn handle_inner(
    req: Request<Body>,
    state: Arc<RouterState>,
    client_addr: SocketAddr,
) -> Response<Body> {
    let path = req.uri().path().to_string();
    let method = req.method().clone();
    let client_ip = resolve_client_ip(req.headers(), client_addr);

    let span = tracing::info_span!(
        "proxy.request",
        http.method = %method,
        http.url = %path,
        net.peer.ip = %client_ip,
    );

    async move {
    // Health check.
    if method == hyper::Method::GET && path == "/health" {
        return check_health(&state);
    }

    // Root redirect to admin web UI.
    if (method == hyper::Method::GET || method == hyper::Method::HEAD) && (path == "/" || path == "") {
        return Response::builder()
            .status(http::StatusCode::FOUND)
            .header(http::header::LOCATION, "/web/")
            .body(Body::empty())
            .unwrap_or_else(|e| {
                tracing::error!(error = %e, "redirect response builder failed");
                crate::util::json_error(http::StatusCode::INTERNAL_SERVER_ERROR, "response_build", "internal_error")
            });
    }

    // Prometheus metrics.
    if method == hyper::Method::GET && path == "/metrics" {
        return admin::prometheus_metrics(state).await;
    }

    // Admin UI/API.
    if path.starts_with("/admin") || path.starts_with("/web") {
        return crate::route::handle_admin(req, state).await;
    }

    if state.is_shutting_down() {
        return RouterState::json_error(
            http::StatusCode::SERVICE_UNAVAILABLE,
            "server is shutting down",
            "shutting_down",
        );
    }

    let start = Instant::now();
    let mut base_log_ctx = RequestLogContext::new(
        start,
        client_ip.clone(),
        method.to_string(),
        path.clone(),
        None,
        None,
        0,
        None,
        0,
    );

    // Authenticate (proxy token → API key → balance).
    let billing_key = match authenticate_request(&state, &req, &base_log_ctx) {
        Ok(key) => key,
        Err(resp) => return resp,
    };
    base_log_ctx.billing_key = Some(billing_key.clone());

    // Stats: request start. The guard is moved into proxied response streams
    // so streaming requests stay inflight until the body finishes or is dropped.
    let _lifecycle = RequestLifecycle::start(state.clone());

    let resp =
        if req.method() == hyper::Method::GET && (path == "/v1/models" || path == "/v1/models/") {
            let (resp, resp_bytes) = models_list(&state, &billing_key);
            record_request(
                &state,
                &base_log_ctx,
                resp.status().as_u16(),
                resp_bytes,
                None,
                None,
            );
            resp
        } else {
            forward(
                req,
                state.clone(),
                _lifecycle,
                start,
                client_ip,
                method,
                path,
                billing_key,
            )
            .await
        };

    resp
    }
    .instrument(span)
    .await
}

// ── Route helpers ─────────────────────────────────────────────────────

/// Health-check endpoint: returns upstream/key counts and inflight requests.
fn check_health(state: &Arc<RouterState>) -> Response<Body> {
    let snap = state.snapshot.load_full();
    let mut total_keys = 0usize;
    let mut active_keys = 0usize;
    for u in snap.upstreams.iter() {
        let keys = u.keys.load_full();
        total_keys += keys.len();
        active_keys += keys.iter().filter(|k| k.is_active()).count();
    }
    let inflight = state
        .stats
        .requests_inflight
        .load(std::sync::atomic::Ordering::Relaxed);
    let body = serde_json::json!({
        "status": "ok",
        "upstreams": snap.upstreams.len(),
        "keys_total": total_keys,
        "keys_active": active_keys,
        "requests_inflight": inflight,
    });
    Response::builder()
        .status(200)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "health check response builder failed");
            crate::util::json_error(http::StatusCode::INTERNAL_SERVER_ERROR, "response_build", "internal_error")
        })
}

/// Authenticate a proxy request: proxy token → API key → balance check.
/// Returns the billing key on success, or an error response.
fn authenticate_request(
    state: &Arc<RouterState>,
    req: &Request<Body>,
    ctx: &RequestLogContext,
) -> Result<String, Response<Body>> {
    let billing_key = match extract_api_key(req.headers()) {
        Some(key) => key,
        None => {
            return Err(logged_json_error(
                state,
                ctx,
                http::StatusCode::UNAUTHORIZED,
                "missing api key",
                "api_key_required",
            ));
        }
    };

    let balance = match state.billing.get_balance(&billing_key) {
        Some(b) => b,
        None => {
            return Err(logged_json_error(
                state,
                ctx,
                http::StatusCode::UNAUTHORIZED,
                "invalid api key",
                "api_key_invalid",
            ));
        }
    };

    if balance <= 0 && balance != -1 {
        return Err(logged_json_error(
            state,
            ctx,
            http::StatusCode::UNAUTHORIZED,
            "insufficient balance",
            "balance_insufficient",
        ));
    }

    Ok(billing_key)
}

/// Resolve the real client IP from headers set by reverse proxies (nginx, etc.).
///
/// Checks `X-Forwarded-For` (takes the leftmost non-trusted IP),
/// then `X-Real-IP`, falling back to the TCP peer address.
fn resolve_client_ip(req: &hyper::HeaderMap, peer: SocketAddr) -> String {
    // X-Forwarded-For: client, proxy1, proxy2, ...
    if let Some(xff) = req
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
    {
        // Take the leftmost address (original client).
        let first = xff.split(',').next().unwrap_or("").trim();
        if !first.is_empty() {
            return first.to_string();
        }
    }

    // X-Real-IP (nginx single-IP alternative).
    if let Some(xri) = req
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
    {
        return xri.trim().to_string();
    }

    // Fall back to TCP peer address.
    peer.ip().to_string()
}

fn cors_preflight(state: &RouterState, origin: Option<&http::HeaderValue>) -> Response<Body> {
    let resp = Response::builder()
        .status(http::StatusCode::NO_CONTENT)
        .body(Body::empty())
        .unwrap_or_else(|_| Response::new(Body::empty()));
    add_cors_headers(resp, state, origin)
}

fn add_cors_headers(
    mut resp: Response<Body>,
    state: &RouterState,
    origin: Option<&http::HeaderValue>,
) -> Response<Body> {
    if let Some(value) = allowed_cors_origin(state, origin) {
        let headers = resp.headers_mut();
        headers.insert("access-control-allow-origin", value);
        headers.insert(
            "access-control-allow-methods",
            http::HeaderValue::from_static("GET,POST,PUT,DELETE,OPTIONS"),
        );
        headers.insert(
            "access-control-allow-headers",
            http::HeaderValue::from_static(
                "authorization,content-type,x-api-key,x-admin-token",
            ),
        );
        headers.insert(
            "access-control-expose-headers",
            http::HeaderValue::from_static("retry-after,content-type"),
        );
        headers.insert(
            "access-control-max-age",
            http::HeaderValue::from_static("600"),
        );
    }
    resp
}

fn allowed_cors_origin(
    state: &RouterState,
    origin: Option<&http::HeaderValue>,
) -> Option<http::HeaderValue> {
    let origin = origin?;
    let cfg = state.server_config();
    if cfg.cors_origins.iter().any(|o| o == "*") {
        return Some(http::HeaderValue::from_static("*"));
    }
    let origin_str = origin.to_str().ok()?;
    if cfg.cors_origins.iter().any(|allowed| allowed == origin_str) {
        Some(origin.clone())
    } else {
        None
    }
}
