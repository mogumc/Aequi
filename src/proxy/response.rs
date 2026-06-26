use super::usage::{extract_nonstreaming_content, parse_sse_usage, usage_from_json_bytes};
use super::{RequestLogContext, RequestLifecycle, UsageTokens};
use crate::format::AuthStyle;
use crate::state::{
    sanitize_hop_headers, RequestLogEntry, RequestTiming, RouterState, HDR_AUTHORIZATION,
};
use crate::util::now_ms;
use flate2::{Decompress, FlushDecompress, Status};
use hyper::header::{ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE};
use hyper::{Body, Request, Response};
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio_stream::wrappers::ReceiverStream;

static REQUEST_LOG_ID: AtomicU64 = AtomicU64::new(1);

pub(super) fn record_request(
    state: &RouterState,
    ctx: &RequestLogContext,
    status: u16,
    resp_bytes: usize,
    usage: Option<UsageTokens>,
    upstream_error: Option<String>,
) {
    if !ctx.path.starts_with("/v1") {
        return;
    }
    let total_ms = ctx.start.elapsed().as_millis() as u64;
    let entry = RequestLogEntry {
        // ── Identity ──
        id: REQUEST_LOG_ID.fetch_add(1, Ordering::Relaxed),
        ts_ms: now_ms(),
        // ── Request ──
        client_ip: ctx.client_ip.clone(),
        method: ctx.method.clone(),
        path: ctx.path.clone(),
        model: ctx.model.clone(),
        upstream_id: ctx.upstream_id.clone(),
        is_stream: ctx.is_stream,
        billing_key: ctx.billing_key.clone(),
        // ── Response ──
        status,
        latency_ms: total_ms,
        timing: RequestTiming {
            queue_ms: ctx.queue_ms,
            upstream_ms: total_ms.saturating_sub(ctx.queue_ms),
            total_ms,
            attempts: 0,
        },
        // ── Size ──
        req_bytes: ctx.req_bytes,
        resp_bytes,
        // ── Usage ──
        prompt_tokens: usage.as_ref().map(|u| u.prompt),
        completion_tokens: usage.as_ref().map(|u| u.billing_completion()),
        thought_tokens: usage.as_ref().map(|u| u.thought),
        total_tokens: usage.map(|u| u.total),
        token_source: ctx.token_source.clone(),
        // ── Error ──
        error: upstream_error,
    };
    state.record_request(entry);
}

pub(super) fn logged_json_error(
    state: &RouterState,
    ctx: &RequestLogContext,
    status: http::StatusCode,
    message: &str,
    code: &str,
) -> Response<Body> {
    let resp = RouterState::json_error(status, message, code);
    logged_response(state, ctx, resp)
}

pub(super) fn logged_response(
    state: &RouterState,
    ctx: &RequestLogContext,
    resp: Response<Body>,
) -> Response<Body> {
    record_request(state, ctx, resp.status().as_u16(), 0, None, None);
    resp
}

pub(super) fn build_upstream_request(
    method: hyper::Method,
    uri: http::Uri,
    version: http::Version,
    headers: &hyper::HeaderMap,
    body_bytes: bytes::Bytes,
    sel: &crate::state::Selected,
    injected: bool,
    auth_style: AuthStyle,
    extra_headers: &[(hyper::header::HeaderName, hyper::header::HeaderValue)],
) -> Result<Request<Body>, Response<Body>> {
    let mut builder = hyper::Request::builder()
        .method(method)
        .uri(uri)
        .version(version);

    for (name, value) in headers.iter() {
        builder = builder.header(name.clone(), value.clone());
    }

    let mut out_req = builder.body(Body::from(body_bytes.clone())).map_err(|_| {
        RouterState::json_error(
            http::StatusCode::BAD_GATEWAY,
            "failed to build request",
            "request_build_error",
        )
    })?;

    sanitize_hop_headers(out_req.headers_mut());
    // Strip compression headers so upstream (especially Cloudflare) does not
    // compress the response. aequi reads the full body to extract token
    // usage; decompressing gzip/brotli adds complexity and is unnecessary for
    // API proxy traffic where response bodies are small.
    out_req.headers_mut().remove(ACCEPT_ENCODING);
    out_req.headers_mut().remove(HDR_AUTHORIZATION);
    out_req.headers_mut().remove("x-api-key");
    out_req.headers_mut().remove("anthropic-version");
    out_req.headers_mut().remove("x-goog-api-key");
    match auth_style {
        AuthStyle::OpenAiBearer => {
            out_req
                .headers_mut()
                .insert(HDR_AUTHORIZATION, sel.key.auth_header.clone());
        }
        AuthStyle::AnthropicKey => {
            let key = http::HeaderValue::from_str(sel.key.key.as_ref()).map_err(|_| {
                RouterState::json_error(
                    http::StatusCode::BAD_GATEWAY,
                    "invalid upstream key header",
                    "request_build_error",
                )
            })?;
            out_req.headers_mut().insert("x-api-key", key);
            out_req.headers_mut().insert(
                "anthropic-version",
                http::HeaderValue::from_static("2023-06-01"),
            );
        }
        AuthStyle::GeminiKey => {
            let key = http::HeaderValue::from_str(sel.key.key.as_ref()).map_err(|_| {
                RouterState::json_error(
                    http::StatusCode::BAD_GATEWAY,
                    "invalid upstream key header",
                    "request_build_error",
                )
            })?;
            out_req.headers_mut().insert("x-goog-api-key", key);
        }
    }
    // Apply custom header overrides (skip protected auth headers).
    if !sel.upstream.custom_headers.is_empty() {
        for (name, val) in sel.upstream.custom_headers.iter() {
            let lower = name.to_ascii_lowercase();
            if lower == "authorization" || lower == "x-api-key" || lower == "anthropic-version" || lower == "x-goog-api-key" {
                continue;
            }
            match val {
                Some(v) => {
                    if let (Ok(hn), Ok(hv)) = (
                        http::header::HeaderName::from_bytes(lower.as_bytes()),
                        http::HeaderValue::from_str(v),
                    ) {
                        out_req.headers_mut().insert(hn, hv);
                    }
                }
                None => {
                    if let Ok(hn) = http::header::HeaderName::from_bytes(lower.as_bytes()) {
                        out_req.headers_mut().remove(hn);
                    }
                }
            }
        }
    }
    // Apply format-specific extra headers (e.g. Api-Revision for Gemini Interactions API).
    for (name, value) in extra_headers {
        out_req.headers_mut().insert(name.clone(), value.clone());
    }
    if injected || !body_bytes.is_empty() {
        out_req.headers_mut().remove(CONTENT_LENGTH);
        if let Ok(v) = http::HeaderValue::from_str(&body_bytes.len().to_string()) {
            out_req.headers_mut().insert(CONTENT_LENGTH, v);
        }
    }

    Ok(out_req)
}

pub(super) fn should_retry_status(state: &RouterState, status: http::StatusCode) -> bool {
    status == http::StatusCode::UNAUTHORIZED
        || status == http::StatusCode::FORBIDDEN
        || state.should_retry_status(status)
}

pub(super) async fn proxy_upstream_response(
    up_resp: Response<Body>,
    state: Arc<RouterState>,
    log_ctx: RequestLogContext,
    stream_request: bool,
    billing_key: Option<String>,
    lifecycle: Option<RequestLifecycle>,
    upstream_error: Option<String>,
) -> Response<Body> {
    let (mut parts, body) = up_resp.into_parts();
    sanitize_hop_headers(&mut parts.headers);

    let status = parts.status;
    let content_type = parts
        .headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let content_encoding = parts
        .headers
        .get(CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    // ── Non-streaming: read body synchronously, extract usage, bill, return ──
    if !stream_request {
        let (resp_bytes, was_decompressed) = read_and_bill_body(
            body,
            &content_type,
            &content_encoding,
            status,
            &state,
            &log_ctx,
            billing_key.as_deref(),
            upstream_error,
        )
        .await;

        let body_len = resp_bytes.len();
        let body = Body::from(resp_bytes);
        if let Ok(v) = http::HeaderValue::from_str(&body_len.to_string()) {
            parts.headers.insert(CONTENT_LENGTH, v);
        }
        if was_decompressed {
            parts.headers.remove(CONTENT_ENCODING);
        }
        return Response::from_parts(parts, body);
    }

    // ── Streaming: spawn task to forward chunks and extract SSE usage ──
    // Strip Content-Length / Content-Encoding so the client receives
    // true chunked streaming, not a buffered response.
    parts.headers.remove(CONTENT_LENGTH);
    parts.headers.remove(CONTENT_ENCODING);

    let is_event_stream = content_type.starts_with("text/event-stream");
    let want_sse_usage = is_event_stream;

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, io::Error>>(32);

    // Clone before the move — watchdog needs copies.
    let state_w = state.clone();
    let log_ctx_w = log_ctx.clone();
    let billing_key_w = billing_key.clone();
    let upstream_error_w = upstream_error.clone();

    let bill_handle = tokio::spawn(async move {
        let _lifecycle = lifecycle;
        use hyper::body::HttpBody;
        const MAX_SSE_BUF_BYTES: usize = 2 * 1024 * 1024;

        let mut resp_bytes = 0usize;
        let mut usage: Option<UsageTokens> = None;
        let mut sse_buf = String::new();
        let mut content_buf = String::new(); // accumulated delta.content for fallback

        let mut body = body;
        while let Some(chunk) = body.data().await {
            match chunk {
                Ok(chunk) => {
                    resp_bytes = resp_bytes.saturating_add(chunk.len());
                    if tx.send(Ok(chunk.clone())).await.is_err() {
                        break;
                    }
                    if want_sse_usage {
                        if sse_buf.len().saturating_add(chunk.len()) <= MAX_SSE_BUF_BYTES {
                            if let Some(found) = parse_sse_usage(&mut sse_buf, &chunk) {
                                usage = Some(match usage {
                                    Some(prev) => UsageTokens {
                                        prompt: if found.prompt > 0 { found.prompt } else { prev.prompt },
                                        completion: if found.completion > 0 { found.completion } else { prev.completion },
                                        thought: if found.thought > 0 { found.thought } else { prev.thought },
                                        total: (found.prompt.max(prev.prompt) + found.completion.max(prev.completion) + found.thought.max(prev.thought))
                                            .max(found.total)
                                            .max(prev.total),
                                    },
                                    None => found,
                                });
                            }
                        }
                        crate::util::extract_sse_content(&chunk, &mut content_buf);
                    }
                }
                Err(_) => break,
            }
        }

        let mut token_source: Option<String> = None;

        // Determine effective usage: upstream actual, local estimate, or none.
        // Trigger fallback when usage is missing OR completion_tokens == 0
        // (low-quality upstreams may count prompt but not output tokens).
        let effective: Option<UsageTokens> = match usage {
            Some(u) if u.completion > 0 => {
                token_source = Some("upstream".into());
                Some(u)
            }
            _ => {
                let is_billable = log_ctx.is_billable();
                let is_2xx = status.is_success();
                if is_billable && is_2xx {
                    if let Some(est) = UsageTokens::estimate_fallback(
                        usage.as_ref(), &content_buf, log_ctx.request_body.as_deref(),
                    ) {
                        token_source = Some("estimated".into());
                        Some(est)
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
        };

        if let Some(key) = billing_key.as_deref() {
            let model = log_ctx.billing_model.as_deref().unwrap_or("");
            let is_billable = log_ctx.is_billable();
            let is_2xx = status.is_success();
            state.settle_billing(
                key,
                effective.as_ref().map(|u| (u.prompt, u.completion, u.thought, u.total)),
                model,
                is_billable,
                is_2xx,
            );
        }

        let mut ctx_with_source = log_ctx.clone();
        ctx_with_source.token_source = token_source;
        record_request(&state, &ctx_with_source, status.as_u16(), resp_bytes, effective, upstream_error);
    });

    // Watchdog: if the billing task panics, release the reservation so the
    // pre-deducted 1 µcredit is not permanently lost.
    tokio::spawn(async move {
        if let Err(e) = bill_handle.await {
            if e.is_panic() {
                if let Some(key) = billing_key_w.as_deref() {
                    let _ = state_w.billing.release_reservation(key);
                }
                record_request(&state_w, &log_ctx_w, status.as_u16(), 0, None, upstream_error_w);
            }
        }
    });

    Response::from_parts(parts, Body::wrap_stream(ReceiverStream::new(rx)))
}

/// Read non-streaming body, extract usage, bill.
/// Returns (body_bytes, was_decompressed). If gzip, body is decompressed.
async fn read_and_bill_body(
    body: Body,
    content_type: &str,
    content_encoding: &str,
    status: http::StatusCode,
    state: &Arc<RouterState>,
    log_ctx: &RequestLogContext,
    billing_key: Option<&str>,
    upstream_error: Option<String>,
) -> (Vec<u8>, bool) {
    use hyper::body::HttpBody;
    const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

    let mut raw = Vec::new();
    let mut body = body;
    let mut overflow = false;
    while let Some(chunk) = body.data().await {
        match chunk {
            Ok(chunk) => {
                if raw.len().saturating_add(chunk.len()) > MAX_BODY_BYTES {
                    overflow = true;
                    continue;
                }
                raw.extend_from_slice(&chunk);
            }
            Err(_) => break,
        }
    }

    let is_gzip = content_encoding.contains("gzip");

    // If gzip, decompress the response body for the client AND for usage parsing.
    let (out_bytes, was_decompressed) = if is_gzip {
        match decompress_gzip(&raw) {
            Ok(decompressed) => (decompressed, true),
            Err(e) => {
                tracing::warn!(error = %e, "gzip decompression failed, returning raw body");
                (raw, false)
            }
        }
    } else {
        (raw, false)
    };

    let resp_bytes = out_bytes.len();

    let is_billable = log_ctx.is_billable();
    let is_2xx = status.is_success();

    let usage = if is_billable && is_2xx && !overflow && content_type.starts_with("application/json") {
        if !out_bytes.is_empty() {
            match usage_from_json_bytes(&out_bytes) {
                Some(u) => {
                    tracing::debug!(
                        prompt = u.prompt,
                        completion = u.completion,
                        thought = u.thought,
                        total = u.total,
                        "non-streaming usage extracted"
                    );
                    Some(u)
                }
                None => {
                    tracing::warn!(
                        path = %log_ctx.path,
                        model = %log_ctx.model.as_deref().unwrap_or(""),
                        content_type = %content_type,
                        body_len = out_bytes.len(),
                        body_preview = %String::from_utf8_lossy(&out_bytes[..out_bytes.len().min(256)]),
                        "non-streaming usage NOT found in JSON body — upstream may not return usage in non-streaming mode"
                    );
                    None
                }
            }
        } else {
            None
        }
    } else {
        if is_billable && is_2xx && !overflow {
            tracing::debug!(
                path = %log_ctx.path,
                content_type = %content_type,
                content_encoding = %content_encoding,
                "skipping usage parse: content_type not application/json"
            );
        }
        None
    };

    let mut token_source: Option<String> = None;
    let mut final_usage: Option<UsageTokens> = None;

    // Accept upstream usage only if completion_tokens > 0.
    // completion==0 means the upstream didn't count output tokens (low-quality channel).
    if let Some(u) = usage {
        if u.completion > 0 {
            token_source = Some("upstream".into());
            final_usage = Some(u);
        }
    }

    // Fallback: estimate when upstream usage is missing or completion==0.
    if final_usage.is_none() && is_billable && is_2xx && !overflow {
        if let Some(text) = extract_nonstreaming_content(&out_bytes) {
            if let Some(est) = UsageTokens::estimate_fallback(
                usage.as_ref(), &text, log_ctx.request_body.as_deref(),
            ) {
                tracing::info!(
                    path = %log_ctx.path,
                    prompt = est.prompt,
                    completion_est = est.completion,
                    "non-streaming usage estimated (upstream completion=0 or no usage)"
                );
                token_source = Some("estimated".into());
                final_usage = Some(est);
            }
        }
    }

    let usage = final_usage;

    if let Some(key) = billing_key {
        let model = log_ctx.billing_model.as_deref().unwrap_or("");
        state.settle_billing(
            key,
            usage.as_ref().map(|u| (u.prompt, u.completion, u.thought, u.total)),
            model,
            is_billable,
            is_2xx,
        );
    }
    let mut ctx_with_source = log_ctx.clone();
    ctx_with_source.token_source = token_source;
    record_request(state, &ctx_with_source, status.as_u16(), resp_bytes, usage, upstream_error);

    (out_bytes, was_decompressed)
}

fn decompress_gzip(input: &[u8]) -> Result<Vec<u8>, io::Error> {
    let mut decoder = GzipDecoder::new();
    decoder.decompress_chunk(input)
}

pub(super) fn parse_retry_after_ms(value: Option<&http::HeaderValue>) -> Option<u64> {
    let raw = value?.to_str().ok()?.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(secs) = raw.parse::<u64>() {
        return Some(secs.saturating_mul(1000));
    }
    let dt = httpdate::parse_http_date(raw).ok()?;
    let now = std::time::SystemTime::now();
    let dur = dt.duration_since(now).unwrap_or_default();
    Some(dur.as_millis() as u64)
}

struct GzipDecoder {
    decompressor: Decompress,
}

impl GzipDecoder {
    fn new() -> Self {
        Self {
            decompressor: Decompress::new(true),
        }
    }

    fn decompress_chunk(&mut self, input: &[u8]) -> Result<Vec<u8>, io::Error> {
        let mut out = Vec::new();
        let mut offset = 0usize;
        while offset < input.len() {
            let mut buf = [0u8; 8192];
            let in_before = self.decompressor.total_in();
            let out_before = self.decompressor.total_out();
            let status = self
                .decompressor
                .decompress(&input[offset..], &mut buf, FlushDecompress::None)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let in_after = self.decompressor.total_in();
            let out_after = self.decompressor.total_out();
            let used_in = (in_after - in_before) as usize;
            let produced = (out_after - out_before) as usize;
            offset = offset.saturating_add(used_in);
            if produced > 0 {
                out.extend_from_slice(&buf[..produced]);
            }
            if status == Status::StreamEnd {
                break;
            }
            if used_in == 0 && produced == 0 {
                break;
            }
        }
        Ok(out)
    }
}
