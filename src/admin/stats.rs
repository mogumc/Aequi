use crate::state::{list_request_log_archives, MetricsWindow, RouterState};
use crate::util::{now_ms, query_get};
use hyper::{Body, Response};
use serde::Serialize;
use std::fmt::Write;
use std::sync::Arc;

use super::upstreams::{build_upstream_info, UpstreamInfo};

#[derive(Serialize)]
pub(crate) struct StatsSnapshot {
    pub(crate) ts_ms: u64,
    pub(crate) uptime_s: u64,

    pub(crate) max_retries: usize,
    pub(crate) retry_status_codes: Vec<u16>,

    pub(crate) requests_total: u64,
    pub(crate) requests_inflight: u64,
    pub(crate) upstream_selected_total: u64,

    pub(crate) responses_2xx: u64,
    pub(crate) responses_3xx: u64,
    pub(crate) responses_4xx: u64,
    pub(crate) responses_5xx: u64,

    pub(crate) queue_depth: u64,
    pub(crate) queue_timeout_total: u64,
    pub(crate) queue_enabled: bool,

    pub(crate) latency_avg_ms: f64,
    pub(crate) latency_max_ms: f64,
    pub(crate) latency_count: u64,

    pub(crate) prompt_tokens_total: u64,
    pub(crate) completion_tokens_total: u64,
    pub(crate) thought_tokens_total: u64,
    pub(crate) tokens_total: u64,

    pub(crate) upstreams: Vec<UpstreamInfo>,
}

pub(crate) fn build_snapshot(state: &RouterState) -> StatsSnapshot {
    let ts = now_ms();
    let uptime_s = (ts.saturating_sub(state.stats.started_at_ms)) / 1000;

    let latency_count = state
        .stats
        .latency_count
        .load(std::sync::atomic::Ordering::Relaxed);
    let latency_total = state
        .stats
        .latency_ns_total
        .load(std::sync::atomic::Ordering::Relaxed);
    let latency_max = state
        .stats
        .latency_ns_max
        .load(std::sync::atomic::Ordering::Relaxed);

    let latency_avg_ms = if latency_count == 0 {
        0.0
    } else {
        (latency_total as f64) / (latency_count as f64) / 1_000_000.0
    };
    let latency_max_ms = (latency_max as f64) / 1_000_000.0;

    let snap = state.snapshot.load_full();
    let global_max = state.key_config().max_concurrent_per_key;
    let ups: Vec<UpstreamInfo> = snap
        .upstreams
        .iter()
        .map(|u| build_upstream_info(u, global_max))
        .collect();
    let server = state.server_config();

    StatsSnapshot {
        ts_ms: ts,
        uptime_s,
        max_retries: state.max_retries(),
        retry_status_codes: state.retry_status_codes_sorted(),
        requests_total: state
            .stats
            .requests_total
            .load(std::sync::atomic::Ordering::Relaxed),
        requests_inflight: state
            .stats
            .requests_inflight
            .load(std::sync::atomic::Ordering::Relaxed),
        upstream_selected_total: state
            .stats
            .upstream_selected_total
            .load(std::sync::atomic::Ordering::Relaxed),
        responses_2xx: state
            .stats
            .responses_2xx
            .load(std::sync::atomic::Ordering::Relaxed),
        responses_3xx: state
            .stats
            .responses_3xx
            .load(std::sync::atomic::Ordering::Relaxed),
        responses_4xx: state
            .stats
            .responses_4xx
            .load(std::sync::atomic::Ordering::Relaxed),
        responses_5xx: state
            .stats
            .responses_5xx
            .load(std::sync::atomic::Ordering::Relaxed),
        queue_depth: state
            .stats
            .queue_depth
            .load(std::sync::atomic::Ordering::Relaxed),
        queue_timeout_total: state
            .stats
            .queue_timeout_total
            .load(std::sync::atomic::Ordering::Relaxed),
        queue_enabled: server.queue_enabled,
        latency_avg_ms,
        latency_max_ms,
        latency_count,
        prompt_tokens_total: state
            .stats
            .prompt_tokens_total
            .load(std::sync::atomic::Ordering::Relaxed),
        completion_tokens_total: state
            .stats
            .completion_tokens_total
            .load(std::sync::atomic::Ordering::Relaxed),
        thought_tokens_total: state
            .stats
            .thought_tokens_total
            .load(std::sync::atomic::Ordering::Relaxed),
        tokens_total: state
            .stats
            .tokens_total
            .load(std::sync::atomic::Ordering::Relaxed),
        upstreams: ups,
    }
}

pub(crate) async fn api_stats_snapshot(state: Arc<RouterState>) -> Response<Body> {
    let snap = build_snapshot(&state);
    super::json_ok(&snap)
}

pub(crate) async fn api_requests(state: Arc<RouterState>, uri: &http::Uri) -> Response<Body> {
    let limit: usize = query_get(uri, "limit")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(200)
        .clamp(1, 5000);
    let list = state.recent_requests(limit);
    super::json_ok(&serde_json::json!({
        "now_ms": now_ms(),
        "count": list.len(),
        "requests": list
    }))
}

/// Read historical requests from the JSONL file, newest first.
/// ?limit=N  (default 100, max 5000)
/// ?before=ts_ms  (optional, only return entries before this timestamp)
pub(crate) async fn api_requests_history(state: Arc<RouterState>, uri: &http::Uri) -> Response<Body> {
    let limit: usize = query_get(uri, "limit")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(100)
        .clamp(1, 5000);
    let before: Option<u64> = query_get(uri, "before").and_then(|s| s.parse().ok());

    let path = state.requests_log_dir.clone();
    let items = match tokio::task::spawn_blocking(move || {
        read_request_log_reverse(&path, limit, before)
    }).await {
        Ok(items) => items,
        Err(e) => {
            tracing::error!(error = %e, "request log reader task panicked");
            vec![]
        }
    };

    super::json_ok(&serde_json::json!({
        "now_ms": now_ms(),
        "count": items.len(),
        "source": "file",
        "requests": items
    }))
}

pub(crate) async fn api_metrics(state: Arc<RouterState>, uri: &http::Uri) -> Response<Body> {
    let window = query_get(uri, "window").unwrap_or("1min");
    let win = MetricsWindow::from_str(window);
    let buckets = state.metrics_snapshot(win);
    super::json_ok(&serde_json::json!({
        "window": win.as_str(),
        "now_ms": now_ms(),
        "buckets": buckets
    }))
}

/// Prometheus metrics endpoint.
/// Returns metrics in Prometheus text exposition format (OpenMetrics compatible).
pub async fn prometheus_metrics(state: Arc<RouterState>) -> Response<Body> {
    let snap = state.snapshot.load_full();
    let now = now_ms();
    let uptime_s = (now.saturating_sub(state.stats.started_at_ms)) / 1000;
    let mut buf = String::with_capacity(4096);

    write_prometheus_global(&mut buf, &state, uptime_s);
    write_prometheus_upstreams(&mut buf, snap.upstreams.as_slice(), now);
    write_prometheus_keys(&mut buf, snap.upstreams.as_slice(), now);

    Response::builder()
        .status(200)
        .header("content-type", "text/plain; version=0.0.4; charset=utf-8")
        .body(Body::from(buf))
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "prometheus response builder failed");
            crate::util::json_error(http::StatusCode::INTERNAL_SERVER_ERROR, "response_build", "internal_error")
        })
}

/// Global metrics: uptime, requests, inflight, queue, responses, errors, latency, selection.
fn write_prometheus_global(buf: &mut String, state: &RouterState, uptime_s: u64) {
    // Uptime
    let _ = writeln!(buf, "# HELP gptload_uptime_seconds Uptime in seconds");
    let _ = writeln!(buf, "# TYPE gptload_uptime_seconds gauge");
    let _ = writeln!(buf, "gptload_uptime_seconds {}", uptime_s);

    // Requests total
    let _ = writeln!(buf, "# HELP gptload_requests_total Total number of requests");
    let _ = writeln!(buf, "# TYPE gptload_requests_total counter");
    let _ = writeln!(
        buf,
        "gptload_requests_total {}",
        state.stats.requests_total.load(std::sync::atomic::Ordering::Relaxed)
    );

    // Requests inflight
    let _ = writeln!(buf, "# HELP gptload_requests_inflight Currently inflight requests");
    let _ = writeln!(buf, "# TYPE gptload_requests_inflight gauge");
    let _ = writeln!(
        buf,
        "gptload_requests_inflight {}",
        state.stats.requests_inflight.load(std::sync::atomic::Ordering::Relaxed)
    );

    // Queue
    let _ = writeln!(buf, "# HELP gptload_queue_depth Currently queued requests");
    let _ = writeln!(buf, "# TYPE gptload_queue_depth gauge");
    let _ = writeln!(
        buf,
        "gptload_queue_depth {}",
        state.stats.queue_depth.load(std::sync::atomic::Ordering::Relaxed)
    );
    let _ = writeln!(buf, "# HELP gptload_queue_timeout_total Queue timeout/full rejections");
    let _ = writeln!(buf, "# TYPE gptload_queue_timeout_total counter");
    let _ = writeln!(
        buf,
        "gptload_queue_timeout_total {}",
        state.stats.queue_timeout_total.load(std::sync::atomic::Ordering::Relaxed)
    );

    // Response status codes (global)
    let _ = writeln!(buf, "# HELP gptload_responses_total Total responses by status class");
    let _ = writeln!(buf, "# TYPE gptload_responses_total counter");
    let _ = writeln!(
        buf,
        "gptload_responses_total{{status_class=\"2xx\"}} {}",
        state.stats.responses_2xx.load(std::sync::atomic::Ordering::Relaxed)
    );
    let _ = writeln!(
        buf,
        "gptload_responses_total{{status_class=\"3xx\"}} {}",
        state.stats.responses_3xx.load(std::sync::atomic::Ordering::Relaxed)
    );
    let _ = writeln!(
        buf,
        "gptload_responses_total{{status_class=\"4xx\"}} {}",
        state.stats.responses_4xx.load(std::sync::atomic::Ordering::Relaxed)
    );
    let _ = writeln!(
        buf,
        "gptload_responses_total{{status_class=\"5xx\"}} {}",
        state.stats.responses_5xx.load(std::sync::atomic::Ordering::Relaxed)
    );

    // Latency
    let latency_count = state.stats.latency_count.load(std::sync::atomic::Ordering::Relaxed);
    let latency_total_ns = state.stats.latency_ns_total.load(std::sync::atomic::Ordering::Relaxed);
    let latency_max_ns = state.stats.latency_ns_max.load(std::sync::atomic::Ordering::Relaxed);

    let _ = writeln!(buf, "# HELP gptload_request_duration_seconds Request latency");
    let _ = writeln!(buf, "# TYPE gptload_request_duration_seconds summary");
    if latency_count > 0 {
        let avg_s = (latency_total_ns as f64) / (latency_count as f64) / 1_000_000_000.0;
        let _ = writeln!(
            buf,
            "gptload_request_duration_seconds{{quantile=\"avg\"}} {:.6}",
            avg_s
        );
    }
    let _ = writeln!(
        buf,
        "gptload_request_duration_seconds{{quantile=\"max\"}} {:.6}",
        (latency_max_ns as f64) / 1_000_000_000.0
    );
    let _ = writeln!(buf, "gptload_request_duration_count {}", latency_count);
    let _ = writeln!(
        buf,
        "gptload_request_duration_sum_seconds {:.6}",
        (latency_total_ns as f64) / 1_000_000_000.0
    );

    // Upstream selection
    let _ = writeln!(buf, "# HELP gptload_upstream_selected_total Total upstream selections");
    let _ = writeln!(buf, "# TYPE gptload_upstream_selected_total counter");
    let _ = writeln!(
        buf,
        "gptload_upstream_selected_total {}",
        state.stats.upstream_selected_total.load(std::sync::atomic::Ordering::Relaxed)
    );
}

/// Per-upstream metrics: keys, responses, errors, selection.
fn write_prometheus_upstreams(buf: &mut String, upstreams: &[Arc<crate::state::Upstream>], now: u64) {
    let _ = writeln!(buf, "# HELP gptload_upstream_responses_total Per-upstream responses by status class");
    let _ = writeln!(buf, "# TYPE gptload_upstream_responses_total counter");
    let _ = writeln!(buf, "# HELP gptload_upstream_errors_total Per-upstream errors by type");
    let _ = writeln!(buf, "# TYPE gptload_upstream_errors_total counter");
    let _ = writeln!(buf, "# HELP gptload_upstream_selected_total Per-upstream selection count");
    let _ = writeln!(buf, "# TYPE gptload_upstream_selected_total counter");
    let _ = writeln!(buf, "# HELP gptload_upstream_keys Total keys per upstream");
    let _ = writeln!(buf, "# TYPE gptload_upstream_keys gauge");
    let _ = writeln!(buf, "# HELP gptload_upstream_active_keys Active keys per upstream");
    let _ = writeln!(buf, "# TYPE gptload_upstream_active_keys gauge");
    let _ = writeln!(buf, "# HELP gptload_upstream_invalid_keys Invalid keys per upstream");
    let _ = writeln!(buf, "# TYPE gptload_upstream_invalid_keys gauge");
    let _ = writeln!(buf, "# HELP gptload_upstream_cooldown_keys Keys in 429 cooldown per upstream");
    let _ = writeln!(buf, "# TYPE gptload_upstream_cooldown_keys gauge");

    for u in upstreams {
        let id = u.id.as_ref();
        let keys_arc = u.keys.load_full();
        let total = keys_arc.len();
        let mut invalid = 0usize;
        let mut active = 0usize;
        let mut cooldown = 0usize;
        for k in keys_arc.iter() {
            if k.is_active() {
                active += 1;
                let until = k.cooldown_until_ms.load(std::sync::atomic::Ordering::Relaxed);
                if until > 0 && now < until {
                    cooldown += 1;
                }
            } else {
                invalid += 1;
            }
        }

        let _ = writeln!(buf, "gptload_upstream_keys{{upstream=\"{}\"}} {}", id, total);
        let _ = writeln!(buf, "gptload_upstream_active_keys{{upstream=\"{}\"}} {}", id, active);
        let _ = writeln!(buf, "gptload_upstream_invalid_keys{{upstream=\"{}\"}} {}", id, invalid);
        let _ = writeln!(buf, "gptload_upstream_cooldown_keys{{upstream=\"{}\"}} {}", id, cooldown);

        let sel = u.stats.selected_total.load(std::sync::atomic::Ordering::Relaxed);
        let _ = writeln!(buf, "gptload_upstream_selected_total{{upstream=\"{}\"}} {}", id, sel);

        let r2 = u.stats.responses_2xx.load(std::sync::atomic::Ordering::Relaxed);
        let r3 = u.stats.responses_3xx.load(std::sync::atomic::Ordering::Relaxed);
        let r4 = u.stats.responses_4xx.load(std::sync::atomic::Ordering::Relaxed);
        let r5 = u.stats.responses_5xx.load(std::sync::atomic::Ordering::Relaxed);
        let _ = writeln!(buf, "gptload_upstream_responses_total{{upstream=\"{}\",status_class=\"2xx\"}} {}", id, r2);
        let _ = writeln!(buf, "gptload_upstream_responses_total{{upstream=\"{}\",status_class=\"3xx\"}} {}", id, r3);
        let _ = writeln!(buf, "gptload_upstream_responses_total{{upstream=\"{}\",status_class=\"4xx\"}} {}", id, r4);
        let _ = writeln!(buf, "gptload_upstream_responses_total{{upstream=\"{}\",status_class=\"5xx\"}} {}", id, r5);
    }
}

/// Global key distribution: total, active, invalid, cooldown counts.
fn write_prometheus_keys(buf: &mut String, upstreams: &[Arc<crate::state::Upstream>], now: u64) {
    let mut total_keys = 0usize;
    let mut active_keys = 0usize;
    let mut cooldown_keys = 0usize;
    for u in upstreams {
        let keys = u.keys.load_full();
        total_keys += keys.len();
        for k in keys.iter() {
            if k.is_active() {
                active_keys += 1;
                let until = k.cooldown_until_ms.load(std::sync::atomic::Ordering::Relaxed);
                if until > 0 && now < until {
                    cooldown_keys += 1;
                }
            }
        }
    }
    let _ = writeln!(buf, "# HELP gptload_keys_total Total keys across all upstreams");
    let _ = writeln!(buf, "# TYPE gptload_keys_total gauge");
    let _ = writeln!(buf, "gptload_keys_total {}", total_keys);
    let _ = writeln!(buf, "# HELP gptload_keys_active Active keys across all upstreams");
    let _ = writeln!(buf, "# TYPE gptload_keys_active gauge");
    let _ = writeln!(buf, "gptload_keys_active {}", active_keys);
    let _ = writeln!(buf, "# HELP gptload_keys_invalid Invalid keys across all upstreams");
    let _ = writeln!(buf, "# TYPE gptload_keys_invalid gauge");
    let _ = writeln!(buf, "gptload_keys_invalid {}", total_keys.saturating_sub(active_keys));
    let _ = writeln!(buf, "# HELP gptload_keys_cooldown Keys in 429 cooldown");
    let _ = writeln!(buf, "# TYPE gptload_keys_cooldown gauge");
    let _ = writeln!(buf, "gptload_keys_cooldown {}", cooldown_keys);
}

/// Read historical requests from the log directory (current file + archives), newest first.
/// Archives are `requests-YYYY-MM-DD.jsonl` files; the current file is `requests.jsonl`.
/// Files are read in reverse-chronological order; within each file, entries are read from
/// the tail using 4 KB chunked reverse reads.
fn read_request_log_reverse(
    dir: &std::path::Path,
    limit: usize,
    before: Option<u64>,
) -> Vec<serde_json::Value> {
    let mut out: Vec<serde_json::Value> = Vec::with_capacity(limit);
    let mut cursor = before;

    // 1. Current file (today's entries, newest first).
    let current = dir.join("requests.jsonl");
    if current.exists() {
        let items = read_single_log_reverse(&current, limit - out.len(), cursor);
        if let Some(oldest) = items.last().and_then(|v| v.get("ts_ms").and_then(|t| t.as_u64())) {
            cursor = Some(oldest);
        }
        out.extend(items);
    }

    // 2. Archive files (newest first).
    if out.len() < limit {
        let archives = list_request_log_archives(dir);
        for (_date, path) in archives.iter().rev() {
            if out.len() >= limit {
                break;
            }
            let items = read_single_log_reverse(path, limit - out.len(), cursor);
            if let Some(oldest) = items.last().and_then(|v| v.get("ts_ms").and_then(|t| t.as_u64())) {
                cursor = Some(oldest);
            }
            out.extend(items);
        }
    }

    out
}

/// Reverse chunk reader for a single JSONL file. Reads from end to start in 4 KB chunks.
fn read_single_log_reverse(
    path: &std::path::Path,
    limit: usize,
    before: Option<u64>,
) -> Vec<serde_json::Value> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let file_size = match file.seek(SeekFrom::End(0)) {
        Ok(n) => n,
        Err(_) => return Vec::new(),
    };
    if file_size == 0 {
        return Vec::new();
    }

    let mut out: Vec<serde_json::Value> = Vec::with_capacity(limit);
    let mut leftover: Vec<u8> = Vec::new(); // bytes of a line that crosses chunk boundary
    let mut chunk = vec![0u8; 4096];
    let mut pos = file_size;

    while pos > 0 && out.len() < limit {
        let read_size = (pos as usize).min(chunk.len());
        pos -= read_size as u64;
        if file.seek(SeekFrom::Start(pos)).is_err() {
            break;
        }
        if file.read_exact(&mut chunk[..read_size]).is_err() {
            break;
        }

        // Prepend the current chunk to leftover to form complete lines at the boundary.
        let mut combined = chunk[..read_size].to_vec();
        combined.extend_from_slice(&leftover);
        leftover.clear();

        // Split into lines (keep empty slices to detect trailing newline).
        let mut lines: Vec<&[u8]> = combined.split(|&b| b == b'\n').collect();

        // If the file doesn't end with a newline, the first segment of the
        // first chunk is not really a line — it's the last incomplete line.
        if pos == 0 && lines.len() == 1 && !lines[0].is_empty() {
            // Single line at the very beginning, no trailing newline.
            if let Ok(text) = std::str::from_utf8(lines[0]) {
                let text = text.trim();
                if !text.is_empty() {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
                        if passes_before(&v, before) && out.len() < limit {
                            out.push(v);
                        }
                    }
                }
            }
            break;
        }

        // The first entry (after split) is the start of a line that was cut;
        // save it for the previous chunk. Process the rest in reverse.
        let carry = lines.remove(0);
        if !carry.is_empty() {
            leftover = carry.to_vec();
        } else {
            leftover.clear();
        }

        for raw in lines.iter().rev() {
            if out.len() >= limit {
                break;
            }
            if let Ok(text) = std::str::from_utf8(raw) {
                let text = text.trim();
                if text.is_empty() {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
                    if passes_before(&v, before) {
                        out.push(v);
                    }
                }
            }
        }
    }

    // Process the final leftover from the beginning of the file.
    if !leftover.is_empty() && out.len() < limit {
        if let Ok(text) = std::str::from_utf8(&leftover) {
            let text = text.trim();
            if !text.is_empty() {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
                    if passes_before(&v, before) {
                        out.push(v);
                    }
                }
            }
        }
    }

    out
}

fn passes_before(v: &serde_json::Value, before: Option<u64>) -> bool {
    match before {
        Some(b) => v.get("ts_ms").and_then(|t| t.as_u64()).map(|ts| ts < b).unwrap_or(true),
        None => true,
    }
}
