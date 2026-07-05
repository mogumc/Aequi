use super::response::{
    build_upstream_request, logged_json_error, logged_response, parse_retry_after_ms,
    proxy_upstream_response, should_retry_status,
};
use super::usage::{ensure_stream_usage, parse_request_json};
use super::{RequestLogContext, RequestLifecycle};
use crate::billing::ReserveResult;
use crate::config::UpstreamFormat;
use crate::format;
use crate::state::{KeyGuard, RouterState, Selected};
use crate::util::now_ms;
use hyper::{Body, Request, Response};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;
use tracing::Instrument;

enum AttemptResult {
    /// Request completed (success or final failure) — return response to client.
    Success(Response<Body>),
    /// Retry with a different key/upstream.
    Retry(Selected),
}

/// Release the pre-deducted billing reservation if one was taken.
fn release_if_reserved(state: &RouterState, billing_key: &str, billing_reserved: &mut bool) {
    if *billing_reserved {
        let _ = state.billing.release_reservation(billing_key);
    }
}

/// Try to find an alternative upstream+key for retry, excluding the current pair.
fn try_retry_alternative(
    state: &Arc<RouterState>,
    model: &str,
    billing_key: &str,
    sel: &Selected,
) -> Option<Selected> {
    let level = state.store.get_key_level(billing_key);
    state.select_for_model_excluding(model, level, Some((&sel.upstream.id, &sel.key.key)))
}

/// Decide whether to retry on an alternative upstream. Logs the decision.
/// Returns `Some(new_sel)` if retry is possible, `None` if the caller should return the error.
fn try_retry_or_give_up(
    state: &Arc<RouterState>,
    model: &str,
    sel: &Selected,
    billing_key: &str,
    reason: &str,
) -> Option<Selected> {
    let new_sel = try_retry_alternative(state, model, billing_key, sel)?;
    tracing::debug!(
        reason,
        old_upstream = %sel.upstream.id,
        new_upstream = %new_sel.upstream.id,
        "retrying with alternative upstream"
    );
    Some(new_sel)
}

/// When retry is not possible: release the billing reservation and wrap the error as an AttemptResult.
fn give_up_with_error(
    state: &Arc<RouterState>,
    billing_key: &str,
    billing_reserved: &mut bool,
    log_ctx: &RequestLogContext,
    error: Response<Body>,
) -> AttemptResult {
    release_if_reserved(state, billing_key, billing_reserved);
    AttemptResult::Success(logged_response(state, log_ctx, error))
}

async fn execute_attempt(
    state: &Arc<RouterState>,
    sel: &Selected,
    upstream: &Arc<crate::state::Upstream>,
    original_pq: &http::uri::PathAndQuery,
    out_method: &hyper::Method,
    version: http::Version,
    headers: &hyper::HeaderMap,
    body_bytes: &bytes::Bytes,
    injected: bool,
    billing_reserved: &mut bool,
    billing_key: &str,
    log_ctx: &RequestLogContext,
    stream_request: bool,
    lifecycle: &mut Option<RequestLifecycle>,
) -> AttemptResult {
    let attempt_start = Instant::now();

    let model = log_ctx.model.as_deref().unwrap_or_default();
    // Use the original client-requested model name for upstream selection during retries,
    // so that model_map rewriting doesn't cause matching the wrong upstream.
    let selection_model = log_ctx.original_model.as_deref().unwrap_or(model);
    let adapted = match format::adapt_request(
        upstream.format,
        original_pq,
        out_method,
        body_bytes,
        model,
    ) {
        Ok(adapted) => adapted,
        Err(resp) => {
            release_if_reserved(state, billing_key, billing_reserved);
            return AttemptResult::Success(logged_response(state, log_ctx, resp));
        }
    };

    let uri = match upstream.build_uri(&adapted.path_and_query) {
        Ok(u) => u,
        Err(_) => {
            release_if_reserved(state, billing_key, billing_reserved);
            return AttemptResult::Success(RouterState::json_error(
                http::StatusCode::BAD_GATEWAY,
                "invalid upstream URI",
                "invalid_upstream_uri",
            ));
        }
    };

    let out_req = match build_upstream_request(
        adapted.method,
        uri,
        version,
        headers,
        adapted.body,
        sel,
        injected,
        adapted.auth_style,
        &adapted.extra_headers,
    ) {
        Ok(req) => req,
        Err(resp) => {
            release_if_reserved(state, billing_key, billing_reserved);
            return AttemptResult::Success(logged_response(state, log_ctx, resp));
        }
    };

    if !*billing_reserved {
        match state.billing.reserve_request(billing_key) {
            ReserveResult::Reserved => {
                *billing_reserved = true;
            }
            ReserveResult::Insufficient => {
                return AttemptResult::Success(logged_json_error(
                    state,
                    log_ctx,
                    http::StatusCode::UNAUTHORIZED,
                    "insufficient balance",
                    "balance_insufficient",
                ));
            }
            ReserveResult::Missing => {
                return AttemptResult::Success(logged_json_error(
                    state,
                    log_ctx,
                    http::StatusCode::UNAUTHORIZED,
                    "invalid api key",
                    "api_key_invalid",
                ));
            }
        }
    }

    let http_span = tracing::info_span!(
        "proxy.upstream_http",
        upstream.id = %sel.upstream.id,
        upstream.base_url = %upstream.base_url,
        model = %log_ctx.model.as_deref().unwrap_or_default(),
    );

    let res = tokio::time::timeout(state.request_timeout(), upstream.client.request(out_req))
        .instrument(http_span)
        .await;

    match res {
        Ok(Ok(up_resp)) => {
            let upstream_ms = attempt_start.elapsed().as_millis() as u64;
            sel.key.record_latency_ms(upstream_ms);
            let status = up_resp.status();
            let retry_after_ms = if status == http::StatusCode::TOO_MANY_REQUESTS {
                parse_retry_after_ms(up_resp.headers().get(http::header::RETRY_AFTER))
            } else {
                None
            };
            state.on_upstream_status(sel, status, retry_after_ms);

            if should_retry_status(state, status) {
                if let Some(new_sel) = try_retry_or_give_up(state, selection_model, sel, billing_key, &status.to_string()) {
                    drop(up_resp);
                    return AttemptResult::Retry(new_sel);
                }
            }

            // Final outcome — count this status exactly once.
            state.inc_upstream_status(upstream, status);

            let (up_resp, upstream_error) = format::adapt_response(
                upstream.format,
                up_resp,
                stream_request,
                log_ctx.model.clone(),
            )
            .await;

            AttemptResult::Success(proxy_upstream_response(
                up_resp,
                state.clone(),
                log_ctx.clone(),
                stream_request,
                Some(billing_key.to_string()),
                lifecycle.take(),
                upstream_error,
            )
            .await)
        }
        Ok(Err(e)) => {
            sel.key
                .record_latency_ms(attempt_start.elapsed().as_millis() as u64);
            tracing::debug!(
                upstream = %sel.upstream.id,
                error = %e,
                "upstream request failed"
            );

            if let Some(new_sel) = try_retry_or_give_up(state, selection_model, sel, billing_key, "network_error") {
                return AttemptResult::Retry(new_sel);
            }

            // Final outcome — network error counts as 502.
            state.inc_upstream_status(upstream, http::StatusCode::BAD_GATEWAY);
            give_up_with_error(
                state, billing_key, billing_reserved, log_ctx,
                RouterState::json_error(
                    http::StatusCode::BAD_GATEWAY,
                    "upstream request failed",
                    "upstream_error",
                ),
            )
        }
        Err(_) => {
            sel.key
                .record_latency_ms(attempt_start.elapsed().as_millis() as u64);

            if let Some(new_sel) = try_retry_or_give_up(state, selection_model, sel, billing_key, "timeout") {
                return AttemptResult::Retry(new_sel);
            }

            // Final outcome — timeout counts as 504.
            state.inc_upstream_status(upstream, http::StatusCode::GATEWAY_TIMEOUT);
            give_up_with_error(
                state, billing_key, billing_reserved, log_ctx,
                RouterState::json_error(
                    http::StatusCode::GATEWAY_TIMEOUT,
                    "upstream request timeout",
                    "upstream_timeout",
                ),
            )
        }
    }
}

async fn read_request_body(body: Body) -> Result<bytes::Bytes, Response<Body>> {
    const MAX_BYTES: usize = 16 * 1024 * 1024;
    crate::util::read_body_bytes(body, MAX_BYTES).await.map_err(|e| {
        let msg = e.to_string();
        if msg.contains("too large") {
            RouterState::json_error(
                http::StatusCode::PAYLOAD_TOO_LARGE,
                "request body too large",
                "body_too_large",
            )
        } else {
            RouterState::json_error(
                http::StatusCode::BAD_GATEWAY,
                "failed to read request body",
                "body_read_error",
            )
        }
    })
}

/// OpenAI-compatible API paths that are allowed to be proxied to upstream.
/// Any /v1/* path NOT in this list is rejected to avoid unbilled upstream charges.
const ALLOWED_API_PATHS: &[&str] = &["/v1/chat/completions"];

fn is_allowed_api_path(path: &str) -> bool {
    if !path.starts_with("/v1/") {
        return true; // non-API paths pass through (admin, web, health, etc.)
    }
    let normalized = path.trim_end_matches('/');
    ALLOWED_API_PATHS.iter().any(|p| *p == normalized)
}

pub(super) async fn forward(
    req: Request<Body>,
    state: Arc<RouterState>,
    lifecycle: RequestLifecycle,
    start: Instant,
    client_ip: String,
    method: hyper::Method,
    path: String,
    billing_key: String,
) -> Response<Body> {
    // Reject unknown /v1/* paths — they would be proxied but NOT billed.
    if !is_allowed_api_path(&path) {
        return RouterState::json_error(
            http::StatusCode::NOT_FOUND,
            "endpoint not supported",
            "unsupported_endpoint",
        );
    }

    let mut lifecycle = Some(lifecycle);

    let (parts, body) = req.into_parts();

    // Extract URI and method early (before moving parts)
    let original_pq = parts
        .uri
        .path_and_query()
        .cloned()
        .unwrap_or_else(|| http::uri::PathAndQuery::from_static("/"));
    let path_model = original_pq
        .path()
        .strip_prefix("/v1/models/")
        .and_then(|s| {
            if s.is_empty() {
                None
            } else {
                Some(s.to_string())
            }
        });
    let out_method = parts.method.clone();
    let version = parts.version;
    let headers = parts.headers.clone();

    // Read body into bytes for potential retries.
    let body_bytes = match read_request_body(body).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let req_bytes = body_bytes.len();
    // Kept in-memory for token estimation only — NOT persisted to the request log.
    let request_body = String::from_utf8(body_bytes.to_vec())
        .ok()
        .filter(|s| s.len() <= 16 * 1024);

    let mut req_json = parse_request_json(&headers, &body_bytes);
    let mut model = req_json
        .as_ref()
        .and_then(|v| v.get("model"))
        .and_then(|m| m.as_str())
        .map(|s| s.to_string());
    if model.is_none() {
        model = path_model;
    }

    let stream_request = req_json
        .as_ref()
        .and_then(|v| v.get("stream"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let is_chat_completions = path == "/v1/chat/completions" || path == "/v1/chat/completions/";

    let mut log_ctx = RequestLogContext::new(
        start,
        client_ip,
        method.to_string(),
        path,
        model.clone(),
        None,
        req_bytes,
        request_body,
        0,
    );
    log_ctx.is_stream = Some(stream_request);
    log_ctx.billing_key = Some(billing_key.clone());

    let Some(mut model) = model else {
        return logged_json_error(
            &state,
            &log_ctx,
            http::StatusCode::BAD_REQUEST,
            "missing model",
            "model_required",
        );
    };

    let mut queue_wait_ms = 0u64;
    let mut sel = if !state.model_exists(&model) {
        return logged_json_error(
            &state,
            &log_ctx,
            http::StatusCode::NOT_FOUND,
            "model not found",
            "model_not_found",
        );
    } else if let Some(sel) = state.select_for_model(&model, state.store.get_key_level(&billing_key)) {
        sel
    } else {
        match wait_for_selection(&state, &model, state.store.get_key_level(&billing_key)).await {
            Ok((sel, waited)) => {
                queue_wait_ms = waited;
                sel
            }
            Err(resp) => return logged_response(&state, &log_ctx, resp),
        }
    };
    log_ctx.queue_ms = queue_wait_ms;

    // Apply model mapping: incoming name → upstream internal name.
    // Save original name for cost calculation (map key is user-facing model).
    let mut body_bytes = body_bytes;
    let billing_model = model.clone();
    if let Some(mapped) = sel.upstream.model_map.get(&model) {
        // Preserve the original model name for upstream selection during retries.
        log_ctx.original_model = Some(model.clone());
        let mapped = mapped.clone();
        if let Some(ref mut json) = req_json {
            json["model"] = serde_json::Value::String(mapped.clone());
            // Re-serialize body_bytes so the upstream receives the mapped model name.
            body_bytes = bytes::Bytes::from(serde_json::to_vec(json).unwrap_or_else(|_| body_bytes.to_vec()));
        }
        log_ctx.model = Some(mapped.clone());
        model = mapped;
    }
    log_ctx.billing_model = Some(billing_model);

    let mut injected = false;
    if stream_request
        && is_chat_completions
        && sel.upstream.format == UpstreamFormat::Openai
        && state.should_inject_usage(sel.upstream.id.as_ref())
    {
        if let Some(ref mut json) = req_json {
            if ensure_stream_usage(json) {
                if let Ok(encoded) = serde_json::to_vec(json) {
                    body_bytes = bytes::Bytes::from(encoded);
                    injected = true;
                }
            }
        }
    }

    // Retry policy from config.
    let max_retries = state.max_retries();

    let forward_span = tracing::info_span!(
        "proxy.forward",
        model = %model,
        stream = stream_request,
    );

    let state_c = state.clone();
    let mut log_ctx_c = log_ctx.clone();

    async move {
    let mut retry_count = 0;
    let mut billing_reserved = false;

    loop {
        log_ctx_c.upstream_id = Some(sel.upstream.id.to_string());
        let upstream = &sel.upstream;

        // Track per-key concurrency for this attempt. Guard decrements on drop.
        let _key_guard = KeyGuard::acquire(sel.key.clone());

        let result = execute_attempt(
            &state_c,
            &sel,
            upstream,
            &original_pq,
            &out_method,
            version,
            &headers,
            &body_bytes,
            injected,
            &mut billing_reserved,
            &billing_key,
            &log_ctx_c,
            stream_request,
            &mut lifecycle,
        )
        .await;

        // _key_guard dropped here → fetch_sub, then notify
        drop(_key_guard);
        state_c.notify_capacity();

        match result {
            AttemptResult::Success(resp) => return resp,
            AttemptResult::Retry(new_sel) => {
                retry_count += 1;
                if retry_count > max_retries {
                    // Last attempt's status was not counted (deferred for retry).
                    // Count it now as 502 since we are giving up.
                    state_c.inc_upstream_status(&sel.upstream, http::StatusCode::BAD_GATEWAY);
                    release_if_reserved(&state, &billing_key, &mut billing_reserved);
                    return logged_json_error(
                        &state,
                        &log_ctx,
                        http::StatusCode::BAD_GATEWAY,
                        "max retries exceeded",
                        "max_retries",
                    );
                }
                sel = new_sel;
                continue;
            }
        }
    }
    }
    .instrument(forward_span)
    .await
}

async fn wait_for_selection(
    state: &Arc<RouterState>,
    model: &str,
    billing_key_level: i32,
) -> Result<(Selected, u64), Response<Body>> {
    let server = state.server_config();
    if !server.queue_enabled {
        return Err(RouterState::json_error(
            http::StatusCode::SERVICE_UNAVAILABLE,
            "no available upstream keys for model",
            "model_unavailable",
        ));
    }

    let Some(_guard) = state.queue_enter() else {
        return Err(queue_rejected_response(server.queue_timeout_ms));
    };

    let start = Instant::now();
    let timeout = tokio::time::sleep(std::time::Duration::from_millis(server.queue_timeout_ms));
    tokio::pin!(timeout);

    loop {
        if state.is_shutting_down() {
            return Err(RouterState::json_error(
                http::StatusCode::SERVICE_UNAVAILABLE,
                "server is shutting down",
                "shutting_down",
            ));
        }
        if let Some(sel) = state.select_for_model(model, billing_key_level) {
            return Ok((sel, start.elapsed().as_millis() as u64));
        }

        if let Some(delay) = next_cooldown_delay(state, model) {
            let cooldown = tokio::time::sleep(delay);
            tokio::pin!(cooldown);
            tokio::select! {
                _ = &mut timeout => {
                    state.stats.queue_timeout_total.fetch_add(1, Ordering::Relaxed);
                    return Err(queue_rejected_response(server.queue_timeout_ms));
                }
                _ = state.queue_notify.notified() => {}
                _ = &mut cooldown => {}
            }
        } else {
            tokio::select! {
                _ = &mut timeout => {
                    state.stats.queue_timeout_total.fetch_add(1, Ordering::Relaxed);
                    return Err(queue_rejected_response(server.queue_timeout_ms));
                }
                _ = state.queue_notify.notified() => {}
            }
        }
    }
}

fn next_cooldown_delay(state: &RouterState, model: &str) -> Option<std::time::Duration> {
    let now = now_ms();
    let snap = state.snapshot.load_full();
    let mut next_until: Option<u64> = None;
    for upstream in snap.upstreams.iter() {
        if !upstream.models.load().contains(model) && !upstream.model_map.contains_key(model) {
            continue;
        }
        let keys = upstream.active_keys.load_full();
        for key in keys.iter() {
            let until = key.cooldown_until_ms.load(Ordering::Relaxed);
            if until > now {
                next_until = Some(next_until.map_or(until, |cur| cur.min(until)));
            }
        }
    }
    next_until.map(|until| {
        std::time::Duration::from_millis(until.saturating_sub(now).max(1))
    })
}

fn queue_rejected_response(queue_timeout_ms: u64) -> Response<Body> {
    let retry_after_secs = ((queue_timeout_ms.max(1000) + 999) / 1000).max(1);
    let mut resp = RouterState::json_error(
        http::StatusCode::TOO_MANY_REQUESTS,
        "request queue full or timed out",
        "queue_unavailable",
    );
    if let Ok(value) = http::HeaderValue::from_str(&retry_after_secs.to_string()) {
        resp.headers_mut().insert(http::header::RETRY_AFTER, value);
    }
    resp
}
