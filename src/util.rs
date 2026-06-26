use bytes::Bytes;
use hyper::header::CONTENT_TYPE;
use hyper::{Body, Response};
use std::time::{SystemTime, UNIX_EPOCH};

#[inline]
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[inline]
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ── Shared HTTP response builders ──

/// Standardized JSON error response.
/// Wire format: `{"error":{"message":"...","type":"proxy_error","param":null,"code":"..."}}`
pub fn json_error(status: http::StatusCode, message: &str, code: &str) -> Response<Body> {
    let body = serde_json::json!({
        "error": {
            "message": message,
            "type": "proxy_error",
            "param": null,
            "code": code
        }
    });
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap_or_else(|_| Response::new(Body::from("proxy_error")))
}

/// Standardized JSON success response with `cache-control: no-store`.
pub fn json_ok<T: ?Sized + serde::Serialize>(v: &T) -> Response<Body> {
    let body = match serde_json::to_vec(v) {
        Ok(b) => b,
        Err(_) => br#"{"error":"json"}"#.to_vec(),
    };
    Response::builder()
        .status(200)
        .header("content-type", "application/json")
        .header("cache-control", "no-store")
        .body(Body::from(body))
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "json_ok response builder failed");
            json_error(http::StatusCode::INTERNAL_SERVER_ERROR, "response_build", "internal_error")
        })
}

/// Read a hyper body with a byte limit. Returns error if the body exceeds `limit`.
pub async fn read_body_limit(
    mut req: hyper::Request<Body>,
    limit: usize,
) -> anyhow::Result<Bytes> {
    use hyper::body::HttpBody;
    let mut buf = Vec::new();
    while let Some(chunk) = req.body_mut().data().await {
        let chunk = chunk?;
        if buf.len() + chunk.len() > limit {
            anyhow::bail!("body too large (limit {} bytes)", limit);
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(buf))
}

/// Read raw body bytes with a byte limit. Unlike `read_body_limit` (which takes
/// a `Request`), this takes a bare `Body` — useful in proxy / response contexts.
pub async fn read_body_bytes(mut body: Body, limit: usize) -> anyhow::Result<Bytes> {
    use hyper::body::HttpBody;
    let mut buf = Vec::new();
    while let Some(chunk) = body.data().await {
        let chunk = chunk?;
        if buf.len() + chunk.len() > limit {
            anyhow::bail!("body too large (limit {} bytes)", limit);
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(buf))
}

// ── Async helpers ──

/// Collapse a `Result<Result<T, E>, JoinError>` from `spawn_blocking` into
/// `Result<T, (StatusCode, String)>` suitable for `?`-style error handling.
///
/// - Inner `Err(e)` (application error) → `(err_status, e.to_string())`
/// - Outer `Err` (JoinError, task panicked) → `(500, "spawn_blocking failed")`
pub fn spawn_result<T, E: std::fmt::Display>(
    result: Result<Result<T, E>, tokio::task::JoinError>,
    err_status: http::StatusCode,
) -> Result<T, (http::StatusCode, String)> {
    match result {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err((err_status, e.to_string())),
        Err(_) => Err((
            http::StatusCode::INTERNAL_SERVER_ERROR,
            "spawn_blocking failed".to_string(),
        )),
    }
}

// ── Query helpers ──

/// Very small query parser for `?a=b&c=d`.
/// Returns value for `key` if present. No percent-decoding (tokens are expected to be simple).
#[inline]
pub fn query_get<'a>(uri: &'a http::Uri, key: &'a str) -> Option<&'a str> {
    let q = uri.query()?;
    for part in q.split('&') {
        let mut it = part.splitn(2, '=');
        let k = it.next()?;
        if k == key {
            return it.next();
        }
    }
    None
}

// ── Token estimation: character-type weight heuristic ──
// Based on new-api's token_estimator.go. No external deps, no tokenizer file.
// Accuracy: ±15-30%. Suitable for fallback billing when upstream returns no usage.

struct TokenWeights {
    word: f64,
    number: f64,
    cjk: f64,
    symbol: f64,
    math_symbol: f64,
    url_delim: f64,
    at_sign: f64,
    emoji: f64,
    newline: f64,
    space: f64,
}

const OPENAI_WEIGHTS: TokenWeights = TokenWeights {
    word: 1.02,
    number: 1.55,
    cjk: 0.85,
    symbol: 0.4,
    math_symbol: 2.68,
    url_delim: 1.0,
    at_sign: 2.0,
    emoji: 2.12,
    newline: 0.5,
    space: 0.42,
};

#[derive(Clone, Copy, PartialEq)]
enum WordType {
    None,
    Latin,
    Number,
}

/// Estimate token count using OpenAI-compatible character-type weights.
/// Stateful grouping: consecutive letters/digits count as one token;
/// CJK/emoji/symbols count per character.
pub fn estimate_tokens(text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }
    let mut count: f64 = 0.0;
    let mut current = WordType::None;

    for ch in text.chars() {
        if ch.is_whitespace() {
            current = WordType::None;
            count += if ch == '\n' || ch == '\t' {
                OPENAI_WEIGHTS.newline
            } else {
                OPENAI_WEIGHTS.space
            };
            continue;
        }
        if is_cjk(ch) {
            current = WordType::None;
            count += OPENAI_WEIGHTS.cjk;
            continue;
        }
        if is_emoji(ch) {
            current = WordType::None;
            count += OPENAI_WEIGHTS.emoji;
            continue;
        }
        if ch.is_alphabetic() || ch.is_numeric() {
            let new_type = if ch.is_numeric() {
                WordType::Number
            } else {
                WordType::Latin
            };
            if current == WordType::None || current != new_type {
                count += if new_type == WordType::Number {
                    OPENAI_WEIGHTS.number
                } else {
                    OPENAI_WEIGHTS.word
                };
                current = new_type;
            }
            continue;
        }
        // Symbol class
        current = WordType::None;
        if is_math_symbol(ch) {
            count += OPENAI_WEIGHTS.math_symbol;
        } else if ch == '@' {
            count += OPENAI_WEIGHTS.at_sign;
        } else if is_url_delim(ch) {
            count += OPENAI_WEIGHTS.url_delim;
        } else {
            count += OPENAI_WEIGHTS.symbol;
        }
    }

    (count.ceil() as u64).max(1)
}

/// Accumulate visible + reasoning content from an SSE data chunk.
/// Models with CoT/thinking emit `reasoning_content` alongside `content`;
/// both are output tokens and must be counted for billing estimation.
pub fn extract_sse_content(chunk: &[u8], buf: &mut String) {
    // Zero-alloc fast path for valid UTF-8 (the normal case for SSE chunks).
    let text: std::borrow::Cow<str> = match std::str::from_utf8(chunk) {
        Ok(s) => std::borrow::Cow::Borrowed(s),
        Err(_) => String::from_utf8_lossy(chunk),
    };
    let mut extracted = 0usize;
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
            continue;
        };
        let Some(delta) = v
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("delta"))
        else {
            continue;
        };
        if let Some(s) = delta.get("reasoning_content").and_then(|v| v.as_str()) {
            buf.push_str(s);
            extracted += s.len();
        }
        if let Some(s) = delta.get("content").and_then(|v| v.as_str()) {
            buf.push_str(s);
            extracted += s.len();
        }
    }
    if extracted > 0 {
        tracing::debug!(extracted, "sse content extracted for fallback estimation");
    }
}

/// Extract message content from a chat completions JSON request body for token estimation.
/// Returns the message text concatenated — mirrors `extract_sse_content` / `extract_nonstreaming_content`
/// on the output side, so input and output token estimations use comparable text (not raw JSON).
///
/// Design note: deliberately skips `image_url` / multimodal non-text parts.
/// Following new-api's philosophy: fallback estimation sacrifices multimodal precision
/// for performance and simplicity. Accurate image token counting requires upstream `usage`,
/// and fallback exists precisely for when upstream doesn't provide it.
pub fn extract_request_content(request_body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(request_body).ok()?;
    let messages = v.get("messages")?.as_array()?;
    let mut buf = String::new();
    for msg in messages {
        let Some(content) = msg.get("content") else { continue };
        match content {
            serde_json::Value::String(s) => {
                if !buf.is_empty() {
                    buf.push('\n');
                }
                buf.push_str(s);
            }
            serde_json::Value::Array(parts) => {
                for part in parts {
                    if let Some("text") = part.get("type").and_then(|t| t.as_str()) {
                        if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                            if !buf.is_empty() {
                                buf.push('\n');
                            }
                            buf.push_str(text);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if buf.is_empty() { None } else { Some(buf) }
}

fn is_cjk(ch: char) -> bool {
    matches!(
        ch as u32,
        0x4E00..=0x9FFF
            | 0x3400..=0x4DBF
            | 0x20000..=0x2A6DF
            | 0x3040..=0x30FF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
    )
}

fn is_emoji(ch: char) -> bool {
    matches!(
        ch as u32,
        0x1F300..=0x1F9FF
            | 0x2600..=0x26FF
            | 0x2700..=0x27BF
            | 0x1FA00..=0x1FAFF
    )
}

fn is_math_symbol(ch: char) -> bool {
    matches!(
        ch as u32,
        0x2200..=0x22FF | 0x2A00..=0x2AFF | 0x1D400..=0x1D7FF
    ) || matches!(
        ch,
        '\u{2211}' | '\u{222B}' | '\u{2202}' | '\u{221A}' | '\u{221E}' | '\u{2248}' | '\u{2260}' | '\u{2264}' | '\u{2265}' | '\u{00B1}' | '\u{00D7}' | '\u{00F7}'
    )
}

fn is_url_delim(ch: char) -> bool {
    matches!(ch, '/' | ':' | '?' | '&' | '=' | '#' | ';' | '%')
}

// ── Key validation ──

/// Validate key string: only [A-Za-z0-9._-] allowed.
/// Consecutive dots (..) are rejected to prevent path traversal attacks.
pub fn validate_key_chars(key: &str) -> Result<(), String> {
    if key.is_empty() {
        return Err("key must not be empty".to_string());
    }
    if key.contains("..") {
        return Err("key contains invalid consecutive dots (path traversal protection)".to_string());
    }
    if !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.') {
        return Err(format!(
            "key contains invalid character '{}' (only A-Za-z0-9 . _ - allowed)",
            key.chars().find(|c| !c.is_ascii_alphanumeric() && *c != '_' && *c != '-' && *c != '.').unwrap_or(' ')
        ));
    }
    Ok(())
}
