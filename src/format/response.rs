use crate::util::now_secs;
use crate::config::UpstreamFormat;
use bytes::Bytes;
use hyper::header::{CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE};
use hyper::{Body, Response};
use std::io;
use tokio_stream::wrappers::ReceiverStream;

pub(super) async fn adapt_response_inner(
    format: UpstreamFormat,
    up_resp: Response<Body>,
    stream_request: bool,
    model: Option<String>,
) -> (Response<Body>, Option<String>) {
    // Non-2xx: unified error handling for all formats.
    if !up_resp.status().is_success() {
        return handle_upstream_error(up_resp).await;
    }

    match format {
        UpstreamFormat::Openai => (up_resp, None),
        UpstreamFormat::Anthropic => {
            if stream_request {
                (transform_sse_response(up_resp, model, anthropic_sse_to_openai), None)
            } else {
                let resp = transform_json_response(up_resp, model, anthropic_json_to_openai).await;
                (resp, None)
            }
        }
        UpstreamFormat::Gemini => {
            if stream_request {
                (transform_sse_response(up_resp, model, gemini_sse_to_openai), None)
            } else {
                let resp = transform_json_response(up_resp, model, gemini_json_to_openai).await;
                (resp, None)
            }
        }
    }
}

/// Handle upstream non-2xx responses: read the error body for logging,
/// return a sanitized gateway error to the client.
/// The upstream's original error message is only recorded in the request log,
/// never exposed to the client (prevents sensitive data leakage from non-standard APIs).
async fn handle_upstream_error(up_resp: Response<Body>) -> (Response<Body>, Option<String>) {
    let (parts, body) = up_resp.into_parts();
    let body_bytes = hyper::body::to_bytes(body).await.unwrap_or_default();
    // Original error — logged only, never sent to client.
    let error_msg = extract_upstream_error(&body_bytes);
    let error_type = map_upstream_error_type(parts.status.as_u16());
    let error_body = serde_json::json!({
        "error": {
            "message": error_type,
            "code": parts.status.as_u16()
        }
    });
    let resp = Response::from_parts(parts, Body::from(error_body.to_string()));
    (resp, Some(error_msg))
}

/// Map upstream HTTP status to a unified error description.
/// Serves as both the client-facing message and error type — ensures consistent
/// output regardless of which upstream or error format produced the failure.
fn map_upstream_error_type(status: u16) -> &'static str {
    match status {
        400 => "upstream_invalid_request",
        401 | 403 => "upstream_authentication_failed",
        404 => "upstream_resource_not_found",
        429 => "upstream_rate_limit_exceeded",
        500..=599 => "upstream_server_error",
        _ => "upstream_error",
    }
}

async fn transform_json_response(
    up_resp: Response<Body>,
    model: Option<String>,
    f: fn(&serde_json::Value, Option<String>) -> serde_json::Value,
) -> Response<Body> {
    let (mut parts, body) = up_resp.into_parts();
    let body = match hyper::body::to_bytes(body).await {
        Ok(body) => body,
        Err(_) => {
            parts.status = http::StatusCode::BAD_GATEWAY;
            parts.headers.remove(CONTENT_LENGTH);
            parts.headers.remove(CONTENT_ENCODING);
            parts.headers.insert(
                CONTENT_TYPE,
                http::HeaderValue::from_static("application/json"),
            );
            return Response::from_parts(
                parts,
                Body::from(r#"{"error":{"message":"failed to read upstream response","type":"upstream_error"}}"#),
            );
        }
    };
    // Non-2xx is handled by handle_upstream_error before reaching here.
    parts.headers.remove(CONTENT_LENGTH);
    parts.headers.remove(CONTENT_ENCODING);
    parts.headers.insert(
        CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    let value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "upstream JSON parse failed, returning raw body");
            return Response::from_parts(parts, Body::from(body));
        }
    };
    let out = f(&value, model);
    Response::from_parts(parts, Body::from(out.to_string()))
}

fn transform_sse_response(
    up_resp: Response<Body>,
    model: Option<String>,
    f: fn(&serde_json::Value, Option<&str>) -> Vec<serde_json::Value>,
) -> Response<Body> {
    let (mut parts, body) = up_resp.into_parts();
    // Caller (adapt_response_inner) guarantees 2xx — non-2xx is handled by handle_upstream_error.
    parts.headers.remove(CONTENT_LENGTH);
    parts.headers.remove(CONTENT_ENCODING);
    parts.headers.insert(
        CONTENT_TYPE,
        http::HeaderValue::from_static("text/event-stream"),
    );
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, io::Error>>(32);
    tokio::spawn(async move {
        use hyper::body::HttpBody;
        let mut body = body;
        let mut buf = String::new();
        while let Some(chunk) = body.data().await {
            let Ok(chunk) = chunk else {
                break;
            };
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buf.find('\n') {
                let line = buf[..pos].trim_end_matches('\r').to_string();
                buf.drain(..=pos);
                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data.is_empty() || data == "[DONE]" {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
                    continue;
                };
                for chunk in f(&value, model.as_deref()) {
                    let msg = format!("data: {}\n\n", chunk);
                    if tx.send(Ok(Bytes::from(msg))).await.is_err() {
                        return;
                    }
                }
            }
            if buf.len() > 1024 * 1024 {
                buf.clear();
            }
        }
        let _ = tx.send(Ok(Bytes::from_static(b"data: [DONE]\n\n"))).await;
    });
    Response::from_parts(parts, Body::wrap_stream(ReceiverStream::new(rx)))
}

fn anthropic_json_to_openai(v: &serde_json::Value, model: Option<String>) -> serde_json::Value {
    let id = v
        .get("id")
        .and_then(|s| s.as_str())
        .unwrap_or("chatcmpl-anthropic");
    let model = v
        .get("model")
        .and_then(|s| s.as_str())
        .map(|s| s.to_string())
        .or(model)
        .unwrap_or_default();
    let content = v
        .get("content")
        .and_then(|c| c.as_array())
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    let input = v
        .get("usage")
        .and_then(|u| u.get("input_tokens"))
        .and_then(|n| n.as_u64())
        .unwrap_or(0);
    let output = v
        .get("usage")
        .and_then(|u| u.get("output_tokens"))
        .and_then(|n| n.as_u64())
        .unwrap_or(0);
    chat_completion_json(id, &model, content, input, output)
}

fn gemini_json_to_openai(v: &serde_json::Value, model: Option<String>) -> serde_json::Value {
    let model = model.unwrap_or_default();
    let candidate = v
        .get("candidates")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let content = candidate
        .get("content")
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.as_array())
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    let prompt = v
        .get("usageMetadata")
        .and_then(|u| u.get("promptTokenCount"))
        .and_then(|n| n.as_u64())
        .unwrap_or(0);
    let candidates = v
        .get("usageMetadata")
        .and_then(|u| u.get("candidatesTokenCount"))
        .and_then(|n| n.as_u64())
        .unwrap_or(0);
    let thought = v
        .get("usageMetadata")
        .and_then(|u| u.get("thoughtsTokenCount"))
        .and_then(|n| n.as_u64())
        .unwrap_or(0);
    // completion_tokens = candidates only (visible output).
    // thought_tokens tracked separately; billing layer sums them via UsageTokens::billing_completion().
    let completion = candidates;
    let total = v
        .get("usageMetadata")
        .and_then(|u| u.get("totalTokenCount"))
        .and_then(|n| n.as_u64())
        .unwrap_or(prompt + completion + thought);
    let mut resp = chat_completion_json("chatcmpl-gemini", &model, content, prompt, completion);
    resp["usage"]["thought_tokens"] = serde_json::json!(thought);
    resp["usage"]["total_tokens"] = serde_json::json!(total);
    resp
}

fn chat_completion_json(
    id: &str,
    model: &str,
    content: String,
    prompt_tokens: u64,
    completion_tokens: u64,
) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "object": "chat.completion",
        "created": now_secs(),
        "model": model,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content},
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": prompt_tokens + completion_tokens
        }
    })
}

fn anthropic_sse_to_openai(v: &serde_json::Value, model: Option<&str>) -> Vec<serde_json::Value> {
    let ty = v.get("type").and_then(|s| s.as_str()).unwrap_or("");
    match ty {
        "message_start" => {
            let id = v
                .get("message")
                .and_then(|m| m.get("id"))
                .and_then(|s| s.as_str())
                .unwrap_or("chatcmpl-anthropic");
            let model_str = v
                .get("message")
                .and_then(|m| m.get("model"))
                .and_then(|s| s.as_str())
                .unwrap_or(model.unwrap_or(""));
            let mut out = vec![chat_chunk_json(
                id,
                model_str,
                serde_json::json!({"role": "assistant", "content": ""}),
                None,
                None,
            )];
            // Emit input_tokens as a usage-bearing chunk so the billing layer
            // can merge prompt_tokens (here) with completion_tokens (from message_delta).
            if let Some(input) = v
                .get("message")
                .and_then(|m| m.get("usage"))
                .and_then(|u| u.get("input_tokens"))
                .and_then(|n| n.as_u64())
            {
                out.push(chat_chunk_json(
                    id,
                    model_str,
                    serde_json::json!({}),
                    None,
                    Some(serde_json::json!({
                        "prompt_tokens": input,
                        "completion_tokens": 0,
                        "total_tokens": input
                    })),
                ));
            }
            out
        }
        "content_block_delta" => {
            let text = v
                .get("delta")
                .and_then(|d| d.get("text"))
                .and_then(|s| s.as_str())
                .unwrap_or("");
            if text.is_empty() {
                Vec::new()
            } else {
                vec![chat_chunk_json(
                    "chatcmpl-anthropic",
                    model.unwrap_or(""),
                    serde_json::json!({"content": text}),
                    None,
                    None,
                )]
            }
        }
        "message_delta" => {
            let usage = v.get("usage").map(|u| {
                let output = u.get("output_tokens").and_then(|n| n.as_u64()).unwrap_or(0);
                serde_json::json!({
                    "prompt_tokens": 0,
                    "completion_tokens": output,
                    "total_tokens": output
                })
            });
            usage
                .map(|usage| {
                    vec![chat_chunk_json(
                        "chatcmpl-anthropic",
                        model.unwrap_or(""),
                        serde_json::json!({}),
                        None,
                        Some(usage),
                    )]
                })
                .unwrap_or_default()
        }
        "message_stop" => vec![chat_chunk_json(
            "chatcmpl-anthropic",
            model.unwrap_or(""),
            serde_json::json!({}),
            Some("stop"),
            None,
        )],
        _ => Vec::new(),
    }
}

fn gemini_sse_to_openai(v: &serde_json::Value, model: Option<&str>) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    if let Some(candidate) = v
        .get("candidates")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
    {
        let text = candidate
            .get("content")
            .and_then(|c| c.get("parts"))
            .and_then(|p| p.as_array())
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default();
        if !text.is_empty() {
            out.push(chat_chunk_json(
                "chatcmpl-gemini",
                model.unwrap_or(""),
                serde_json::json!({"content": text}),
                None,
                None,
            ));
        }
        if candidate.get("finishReason").is_some() {
            out.push(chat_chunk_json(
                "chatcmpl-gemini",
                model.unwrap_or(""),
                serde_json::json!({}),
                Some("stop"),
                None,
            ));
        }
    }
    if let Some(usage) = v.get("usageMetadata") {
        let prompt = usage
            .get("promptTokenCount")
            .and_then(|n| n.as_u64())
            .unwrap_or(0);
        let completion = usage
            .get("candidatesTokenCount")
            .and_then(|n| n.as_u64())
            .unwrap_or(0);
        let thought = usage
            .get("thoughtsTokenCount")
            .and_then(|n| n.as_u64())
            .unwrap_or(0);
        // Use Gemini's native totalTokenCount which includes thoughts.
        // Fallback to sum if missing.
        let total = usage
            .get("totalTokenCount")
            .and_then(|n| n.as_u64())
            .unwrap_or(prompt + completion + thought);
        out.push(chat_chunk_json(
            "chatcmpl-gemini",
            model.unwrap_or(""),
            serde_json::json!({}),
            None,
            Some(serde_json::json!({
                "prompt_tokens": prompt,
                "completion_tokens": completion,
                "thought_tokens": thought,
                "total_tokens": total
            })),
        ));
    }
    out
}

/// Extract a human-readable error message from an upstream error response.
fn extract_upstream_error(body: &[u8]) -> String {
    let v: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return String::from_utf8_lossy(body).into_owned(),
    };
    // Anthropic / Gemini / OpenAI all use error.message
    if let Some(msg) = v.get("error").and_then(|e| e.get("message")).and_then(|m| m.as_str()) {
        return msg.to_string();
    }
    // Generic fallback: error object as string
    v.get("error")
        .and_then(|e| e.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "unknown upstream error".to_string())
}

fn chat_chunk_json(
    id: &str,
    model: &str,
    delta: serde_json::Value,
    finish_reason: Option<&str>,
    usage: Option<serde_json::Value>,
) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": now_secs(),
        "model": model,
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": finish_reason
        }],
        "usage": usage
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip: Gemini JSON → OpenAI format → serialize → parse → verify usage fields.
    #[test]
    fn gemini_json_to_openai_produces_correct_usage() {
        let gemini_resp = serde_json::json!({
            "candidates": [{
                "content": {
                    "parts": [{"text": "Hello from Gemini"}],
                    "role": "model"
                },
                "finishReason": "STOP",
                "index": 0
            }],
            "usageMetadata": {
                "promptTokenCount": 15,
                "candidatesTokenCount": 25,
                "thoughtsTokenCount": 5,
                "totalTokenCount": 45
            },
            "modelVersion": "gemini-2.0-flash"
        });

        let converted = gemini_json_to_openai(&gemini_resp, Some("gemini-2.0-flash".to_string()));

        assert_eq!(converted["usage"]["prompt_tokens"], 15);
        assert_eq!(converted["usage"]["completion_tokens"], 25); // candidates only
        assert_eq!(converted["usage"]["thought_tokens"], 5);
        assert_eq!(converted["usage"]["total_tokens"], 45);

        // Serialize → parse back (simulates HTTP body round-trip)
        let serialized = converted.to_string();
        let parsed: serde_json::Value =
            serde_json::from_str(&serialized).expect("should parse serialized JSON");

        assert_eq!(parsed["usage"]["prompt_tokens"], 15);
        assert_eq!(parsed["usage"]["completion_tokens"], 25);
        assert_eq!(parsed["usage"]["thought_tokens"], 5);
        assert_eq!(parsed["usage"]["total_tokens"], 45);
    }

    /// Round-trip: Anthropic JSON → OpenAI format → serialize → parse → verify usage fields.
    #[test]
    fn anthropic_json_to_openai_produces_correct_usage() {
        let anthropic_resp = serde_json::json!({
            "id": "msg_xxx",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-20250514",
            "content": [{"type": "text", "text": "Hello from Claude"}],
            "usage": {
                "input_tokens": 12,
                "output_tokens": 34
            }
        });

        let converted =
            anthropic_json_to_openai(&anthropic_resp, Some("claude-sonnet-4-20250514".to_string()));

        assert_eq!(converted["usage"]["prompt_tokens"], 12);
        assert_eq!(converted["usage"]["completion_tokens"], 34);
        assert_eq!(converted["usage"]["total_tokens"], 46);

        let serialized = converted.to_string();
        let parsed: serde_json::Value =
            serde_json::from_str(&serialized).expect("should parse serialized JSON");

        assert_eq!(parsed["usage"]["prompt_tokens"], 12);
        assert_eq!(parsed["usage"]["completion_tokens"], 34);
        assert_eq!(parsed["usage"]["total_tokens"], 46);
    }

    /// Edge case: Gemini safety-blocked response still has usage metadata.
    #[test]
    fn gemini_safety_blocked_still_produces_usage() {
        let gemini_resp = serde_json::json!({
            "candidates": [{
                "finishReason": "SAFETY",
                "safetyRatings": [{"category": "HARM_CATEGORY_HARASSMENT", "probability": "HIGH"}]
            }],
            "usageMetadata": {
                "promptTokenCount": 8,
                "totalTokenCount": 8
            },
            "modelVersion": "gemini-2.0-flash"
        });

        let converted = gemini_json_to_openai(&gemini_resp, Some("gemini-2.0-flash".to_string()));

        let serialized = converted.to_string();
        let parsed: serde_json::Value =
            serde_json::from_str(&serialized).expect("should parse");

        assert_eq!(parsed["usage"]["prompt_tokens"], 8);
        assert_eq!(parsed["usage"]["completion_tokens"], 0); // no candidatesTokenCount
        assert_eq!(parsed["usage"]["total_tokens"], 8);
    }

    /// Edge case: Anthropic response with 0 tokens.
    #[test]
    fn anthropic_zero_tokens_produces_correct_usage() {
        let anthropic_resp = serde_json::json!({
            "id": "msg_xxx",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-20250514",
            "content": [{"type": "text", "text": ""}],
            "usage": {
                "input_tokens": 0,
                "output_tokens": 0
            }
        });

        let converted =
            anthropic_json_to_openai(&anthropic_resp, Some("claude-sonnet-4-20250514".to_string()));

        let serialized = converted.to_string();
        let parsed: serde_json::Value =
            serde_json::from_str(&serialized).expect("should parse");

        assert_eq!(parsed["usage"]["prompt_tokens"], 0);
        assert_eq!(parsed["usage"]["completion_tokens"], 0);
        assert_eq!(parsed["usage"]["total_tokens"], 0);
    }
}
