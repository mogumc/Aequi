use super::UsageTokens;
use crate::state::{RouterState, HDR_AUTHORIZATION};
use hyper::header::CONTENT_TYPE;
use hyper::{Body, Response};

pub(super) fn extract_api_key(headers: &hyper::HeaderMap) -> Option<String> {
    // Validate key chars inline (hot path, no allocation for invalid keys).
    // Allows dots for Gemini keys, but rejects consecutive dots (path traversal).
    fn key_ok(k: &str) -> bool {
        !k.is_empty()
            && !k.contains("..")
            && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
    }
    if let Some(h) = headers.get("x-api-key") {
        if let Ok(s) = h.to_str() {
            let key = s.trim();
            if key_ok(key) {
                return Some(key.to_string());
            }
        }
    }
    if let Some(h) = headers.get(HDR_AUTHORIZATION) {
        if let Ok(s) = h.to_str() {
            let raw = s.trim();
            if raw.is_empty() {
                return None;
            }
            let key = raw
                .strip_prefix("Bearer ")
                .or_else(|| raw.strip_prefix("bearer "))
                .unwrap_or(raw)
                .trim();
            if key_ok(key) {
                return Some(key.to_string());
            }
        }
    }
    None
}

pub(super) fn models_list(state: &RouterState, billing_key: &str) -> (Response<Body>, usize) {
    let routes = state.get_model_routes();
    let snap = state.snapshot.load_full();
    let user_level = state.store.get_key_level(billing_key);

    // Collect upstream IDs accessible at the user's key level.
    let accessible: Vec<&str> = snap
        .upstreams
        .iter()
        .filter(|u| user_level == -1 || u.min_key_level <= user_level)
        .map(|u| u.id.as_ref())
        .collect();

    // Filter models to only those with at least one accessible upstream.
    let mut models: Vec<String> = routes
        .models
        .iter()
        .filter(|(_model, upstreams)| upstreams.iter().any(|uid| accessible.contains(&uid.as_str())))
        .map(|(model, _)| model.clone())
        .collect();
    models.sort();

    // Apply reverse model mapping for each model across accessible upstreams only.
    let mut models: Vec<String> = models
        .into_iter()
        .map(|m| {
            for u in snap.upstreams.iter() {
                if !accessible.contains(&u.id.as_ref()) {
                    continue;
                }
                if let Some(user_name) = u.model_rmap.get(&m) {
                    return user_name.clone();
                }
            }
            m
        })
        .collect();
    models.sort();
    models.dedup();

    let data: Vec<serde_json::Value> = models
        .iter()
        .map(|id| serde_json::json!({ "id": id, "object": "model" }))
        .collect();

    let body = serde_json::json!({
        "object": "list",
        "data": data
    });

    let body_str = body.to_string();
    let resp = Response::builder()
        .status(http::StatusCode::OK)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(body_str.clone()))
        .unwrap_or_else(|_| {
            RouterState::json_error(
                http::StatusCode::INTERNAL_SERVER_ERROR,
                "failed to build response",
                "response_build_error",
            )
        });
    (resp, body_str.len())
}

pub(super) fn parse_request_json(
    headers: &hyper::HeaderMap,
    body: &bytes::Bytes,
) -> Option<serde_json::Value> {
    if body.is_empty() {
        return None;
    }
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !content_type.is_empty() && !content_type.starts_with("application/json") {
        return None;
    }
    serde_json::from_slice(body).ok()
}

pub(super) fn ensure_stream_usage(v: &mut serde_json::Value) -> bool {
    let obj = match v.as_object_mut() {
        Some(obj) => obj,
        None => return false,
    };
    let stream = obj.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    if !stream {
        return false;
    }
    let opts = obj
        .entry("stream_options")
        .or_insert_with(|| serde_json::json!({}));
    let opts_obj = match opts.as_object_mut() {
        Some(obj) => obj,
        None => return false,
    };
    let entry = opts_obj
        .entry("include_usage")
        .or_insert(serde_json::Value::Bool(true));
    match entry.as_bool() {
        Some(true) => false,
        _ => {
            *entry = serde_json::Value::Bool(true);
            true
        }
    }
}

pub(super) fn usage_from_json_bytes(body: &[u8]) -> Option<UsageTokens> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    extract_usage_from_value(&v)
}

/// Extract all output content from a non-streaming JSON response body.
/// Captures both `reasoning_content` (CoT/thinking models) and `content` (visible output)
/// so the token estimator counts the full output, not just the visible portion.
pub(super) fn extract_nonstreaming_content(body: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let message = v["choices"].get(0)?.get("message")?;
    let mut text = String::new();
    if let Some(s) = message.get("reasoning_content").and_then(|c| c.as_str()) {
        text.push_str(s);
    }
    if let Some(s) = message.get("content").and_then(|c| c.as_str()) {
        text.push_str(s);
    }
    if text.is_empty() { None } else { Some(text) }
}

fn extract_usage_from_value(v: &serde_json::Value) -> Option<UsageTokens> {
    let usage = v.get("usage")?;
    let prompt = usage.get("prompt_tokens").and_then(|v| v.as_u64());
    let completion = usage.get("completion_tokens").and_then(|v| v.as_u64());
    let thought = usage.get("thought_tokens").and_then(|v| v.as_u64());
    let total = usage
        .get("total_tokens")
        .and_then(|v| v.as_u64())
        .or_else(|| match (prompt, completion) {
            (Some(p), Some(c)) => Some(p + c),
            _ => None,
        });

    if prompt.is_none() && completion.is_none() && total.is_none() {
        return None;
    }

    Some(UsageTokens {
        prompt: prompt.unwrap_or(0),
        completion: completion.unwrap_or(0),
        thought: thought.unwrap_or(0),
        total: total.unwrap_or(0),
    })
}

pub(super) fn parse_sse_usage(buf: &mut String, chunk: &[u8]) -> Option<UsageTokens> {
    let mut found = None;
    let text = String::from_utf8_lossy(chunk);
    buf.push_str(&text);

    while let Some(pos) = buf.find('\n') {
        let line = buf[..pos].trim_end_matches('\r').to_string();
        buf.drain(..=pos);
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data == "[DONE]" {
            continue;
        }
        if !data.contains("\"usage\"") {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(data) {
            if let Some(u) = extract_usage_from_value(&v) {
                tracing::debug!(
                    prompt = u.prompt,
                    completion = u.completion,
                    thought = u.thought,
                    total = u.total,
                    raw_usage = %v.get("usage").map(|u| u.to_string()).unwrap_or_default(),
                    "sse usage found"
                );
                found = Some(u);
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    #[test]
    fn retry_after_seconds_parses_to_millis() {
        let value = HeaderValue::from_static("5");
        assert_eq!(crate::proxy::response::parse_retry_after_ms(Some(&value)), Some(5000));
    }

    #[test]
    fn retry_after_past_date_is_zero() {
        let value = HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT");
        assert_eq!(crate::proxy::response::parse_retry_after_ms(Some(&value)), Some(0));
    }

    /// Test: extract usage from a standard OpenAI chat completion response (non-streaming).
    #[test]
    fn extract_usage_from_openai_response() {
        let body = br#"{
            "id": "chatcmpl-xxx",
            "object": "chat.completion",
            "created": 1234567890,
            "model": "gpt-4o",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "Hello"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30}
        }"#;
        let usage = usage_from_json_bytes(body).expect("should extract usage from standard OpenAI response");
        assert_eq!(usage.prompt, 10);
        assert_eq!(usage.completion, 20);
        assert_eq!(usage.total, 30);
    }

    /// Edge case: OpenAI response where usage fields are 0
    #[test]
    fn openai_zero_token_response() {
        let body = br#"{
            "id": "chatcmpl-xxx",
            "object": "chat.completion",
            "created": 1234567890,
            "model": "gpt-4o",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": ""}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}
        }"#;
        let usage = usage_from_json_bytes(body).expect("should extract usage even with zero tokens");
        assert_eq!(usage.prompt, 0);
        assert_eq!(usage.completion, 0);
        assert_eq!(usage.total, 0);
    }

    /// Edge case: response without usage field at all.
    #[test]
    fn response_without_usage_field_returns_none() {
        let body = br#"{
            "id": "chatcmpl-xxx",
            "object": "chat.completion",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "Hi"}, "finish_reason": "stop"}]
        }"#;
        assert!(usage_from_json_bytes(body).is_none(), "should return None when no usage field");
    }

    /// Test: extract usage from image generation response format (no total_tokens).
    #[test]
    fn extract_usage_from_image_generation_response() {
        let body = br#"{
            "data": [{"b64_json": "base64...", "revised_prompt": "A cat"}],
            "usage": {"prompt_tokens": 50, "completion_tokens": 300}
        }"#;
        let usage = usage_from_json_bytes(body)
            .expect("should extract usage from image generation response");
        assert_eq!(usage.prompt, 50);
        assert_eq!(usage.completion, 300);
        assert_eq!(usage.total, 350, "total should fallback to prompt+completion");
    }
}
