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
    // Original error -- logged only, never sent to client.
    let error_msg = extract_upstream_error(&body_bytes);
    tracing::debug!(
        status = parts.status.as_u16(),
        body = %String::from_utf8_lossy(&body_bytes),
        "upstream error response"
    );
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
/// Serves as both the client-facing message and error type -- ensures consistent
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
    // Caller (adapt_response_inner) guarantees 2xx -- non-2xx is handled by handle_upstream_error.
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

// ─────────────────────────────────────────────────────────────
// Anthropic response converters (unchanged)
// ─────────────────────────────────────────────────────────────

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

// ─────────────────────────────────────────────────────────────
// Gemini Interactions API response converters
// ─────────────────────────────────────────────────────────────

/// Extract usage from Gemini Interactions API response.
/// Handles both top-level usage (non-streaming) and nested interaction.usage (streaming).
fn gemini_usage(v: &serde_json::Value) -> (u64, u64, u64, u64) {
    // Non-streaming: usage at top level
    // Streaming: usage inside interaction object
    let usage = v
        .get("usage")
        .or_else(|| v.get("interaction").and_then(|i| i.get("usage")));

    let input = usage
        .and_then(|u| u.get("total_input_tokens"))
        .and_then(|n| n.as_u64())
        .unwrap_or(0);
    let output = usage
        .and_then(|u| u.get("total_output_tokens"))
        .and_then(|n| n.as_u64())
        .unwrap_or(0);
    let thought = usage
        .and_then(|u| u.get("total_thought_tokens"))
        .and_then(|n| n.as_u64())
        .unwrap_or(0);
    let total = usage
        .and_then(|u| u.get("total_tokens"))
        .and_then(|n| n.as_u64())
        .unwrap_or(input + output + thought);
    (input, output, thought, total)
}

/// Determine finish_reason for Gemini interactions:
/// - function_call steps → "tool_calls"
/// - completed status → "stop"
/// - anything else → "content_filter" (safety block, cancelled, etc.)
fn gemini_finish_reason(has_function_calls: bool, status: &str) -> &'static str {
    if has_function_calls {
        "tool_calls"
    } else {
        match status {
            "completed" => "stop",
            _ => "content_filter",
        }
    }
}

/// Convert Gemini Interactions API JSON response to OpenAI format.
fn gemini_json_to_openai(v: &serde_json::Value, model: Option<String>) -> serde_json::Value {
    let model = model.unwrap_or_default();
    let id = v
        .get("id")
        .and_then(|s| s.as_str())
        .unwrap_or("chatcmpl-gemini");

    let steps = v.get("steps").and_then(|s| s.as_array());

    // Extract text content from model_output steps
    let mut content_parts = Vec::new();
    // Extract tool_calls from function_call steps
    let mut tool_calls = Vec::new();

    if let Some(steps) = steps {
        for step in steps {
            let step_type = step.get("type").and_then(|t| t.as_str()).unwrap_or("");
            match step_type {
                "model_output" => {
                    if let Some(content) = step.get("content").and_then(|c| c.as_array()) {
                        for block in content {
                            if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                    content_parts.push(text.to_string());
                                }
                            }
                        }
                    }
                }
                "function_call" => {
                    let tc_id = step.get("id").and_then(|i| i.as_str()).unwrap_or("");
                    let name = step.get("name").and_then(|n| n.as_str()).unwrap_or("");
                    let args = step.get("arguments").cloned().unwrap_or(serde_json::json!({}));
                    let args_str = serde_json::to_string(&args).unwrap_or_default();
                    tool_calls.push(serde_json::json!({
                        "id": tc_id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": args_str
                        }
                    }));
                }
                _ => {}
            }
        }
    }

    let content = content_parts.join("");

    let has_function_calls = !tool_calls.is_empty();
    let status = v
        .get("status")
        .and_then(|s| s.as_str())
        .unwrap_or("completed");
    let finish_reason = gemini_finish_reason(has_function_calls, status);

    let (prompt, completion, thought, total) = gemini_usage(v);

    let mut resp = chat_completion_json(id, &model, content, prompt, completion);
    // Override finish_reason
    resp["choices"][0]["finish_reason"] = serde_json::json!(finish_reason);
    // Add tool_calls if present
    if !tool_calls.is_empty() {
        resp["choices"][0]["message"]["tool_calls"] = serde_json::Value::Array(tool_calls);
    }
    // Add thought_tokens and total_tokens to usage
    resp["usage"]["thought_tokens"] = serde_json::json!(thought);
    resp["usage"]["total_tokens"] = serde_json::json!(total);

    resp
}

/// Convert Gemini Interactions API SSE events to OpenAI streaming chunks.
/// Events: step.start, step.delta, step.stop, interaction.completed
fn gemini_sse_to_openai(v: &serde_json::Value, model: Option<&str>) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    let model_str = model.unwrap_or("");
    let event_type = v
        .get("event_type")
        .and_then(|s| s.as_str())
        .unwrap_or("");

    match event_type {
        "step.start" => {
            if let Some(step) = v.get("step") {
                let step_type = step.get("type").and_then(|t| t.as_str()).unwrap_or("");
                if step_type == "function_call" {
                    let id = step.get("id").and_then(|i| i.as_str()).unwrap_or("");
                    let name = step.get("name").and_then(|n| n.as_str()).unwrap_or("");
                    out.push(chat_chunk_json(
                        "chatcmpl-gemini",
                        model_str,
                        serde_json::json!({
                            "tool_calls": [{
                                "index": 0,
                                "id": id,
                                "type": "function",
                                "function": {
                                    "name": name,
                                    "arguments": ""
                                }
                            }]
                        }),
                        None,
                        None,
                    ));
                }
                // model_output step.start: no chunk needed (content comes via deltas)
            }
        }
        "step.delta" => {
            if let Some(delta) = v.get("delta") {
                let delta_type = delta.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match delta_type {
                    "text" => {
                        let text = delta.get("text").and_then(|t| t.as_str()).unwrap_or("");
                        if !text.is_empty() {
                            out.push(chat_chunk_json(
                                "chatcmpl-gemini",
                                model_str,
                                serde_json::json!({"content": text}),
                                None,
                                None,
                            ));
                        }
                    }
                    "arguments_delta" => {
                        let args =
                            delta.get("arguments").and_then(|a| a.as_str()).unwrap_or("");
                        out.push(chat_chunk_json(
                            "chatcmpl-gemini",
                            model_str,
                            serde_json::json!({
                                "tool_calls": [{
                                    "index": 0,
                                    "function": {
                                        "arguments": args
                                    }
                                }]
                            }),
                            None,
                            None,
                        ));
                    }
                    // Skip thought, image, and other delta types
                    _ => {}
                }
            }
        }
        "step.stop" => {
            // No output needed; finish_reason comes from interaction.completed
        }
        "interaction.completed" => {
            // Check if any function_call step exists → "tool_calls" finish_reason
            let has_function_calls = v
                .get("interaction")
                .and_then(|i| i.get("steps"))
                .and_then(|s| s.as_array())
                .map(|steps| {
                    steps.iter().any(|s| {
                        s.get("type")
                            .and_then(|t| t.as_str())
                            == Some("function_call")
                    })
                })
                .unwrap_or(false);

            let status = v
                .get("interaction")
                .and_then(|i| i.get("status"))
                .and_then(|s| s.as_str())
                .unwrap_or("completed");
            let finish_reason = gemini_finish_reason(has_function_calls, status);

            // Emit finish_reason chunk
            out.push(chat_chunk_json(
                "chatcmpl-gemini",
                model_str,
                serde_json::json!({}),
                Some(finish_reason),
                None,
            ));

            // Emit usage chunk
            let (input, output, thought, total) = gemini_usage(v);
            out.push(chat_chunk_json(
                "chatcmpl-gemini",
                model_str,
                serde_json::json!({}),
                None,
                Some(serde_json::json!({
                    "prompt_tokens": input,
                    "completion_tokens": output,
                    "thought_tokens": thought,
                    "total_tokens": total
                })),
            ));
        }
        // Skip interaction.created and any other unknown events
        _ => {}
    }

    out
}

/// Extract a human-readable error message from an upstream error response.
/// Handles both plain JSON and SSE format (Gemini streaming errors arrive as SSE).
fn extract_upstream_error(body: &[u8]) -> String {
    // Try plain JSON first (covers non-streaming errors for all formats).
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) {
        if let Some(msg) = extract_error_message(&v) {
            return msg;
        }
    }

    // SSE fallback: scan for data: lines and extract error messages.
    // Gemini streaming errors arrive as: event: error\ndata: {"error":{...}}\n\n
    let text = String::from_utf8_lossy(body);
    let mut found_error = false;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
            continue;
        };
        if let Some(msg) = extract_error_message(&v) {
            return msg;
        }
        if v.get("error").is_some() {
            found_error = true;
        }
    }

    if found_error {
        return "upstream error (details unavailable)".to_string();
    }

    // No structured error found in either format — raw text fallback.
    String::from_utf8_lossy(body).into_owned()
}

/// Extract error.message (or error as string) from a JSON value.
fn extract_error_message(v: &serde_json::Value) -> Option<String> {
    v.get("error")
        .and_then(|e| {
            e.get("message")
                .and_then(|m| m.as_str())
                .map(|s| s.to_string())
                .or_else(|| e.as_str().map(|s| s.to_string()))
        })
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

    // ── Anthropic response tests (unchanged) ──

    /// Round-trip: Anthropic JSON -> OpenAI format -> serialize -> parse -> verify usage fields.
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

    // ── Gemini Interactions API response tests ──

    /// Non-streaming: Gemini interactions response with usage -> OpenAI format.
    #[test]
    fn gemini_interactions_json_usage_conversion() {
        let gemini_resp = serde_json::json!({
            "id": "int_abc123",
            "steps": [
                {"type": "model_output", "content": [{"type": "text", "text": "Hello from Gemini"}]}
            ],
            "usage": {
                "total_input_tokens": 15,
                "total_output_tokens": 25,
                "total_thought_tokens": 5,
                "total_tokens": 45
            },
            "status": "completed"
        });

        let converted = gemini_json_to_openai(&gemini_resp, Some("gemini-3.5-flash".to_string()));

        assert_eq!(converted["id"], "int_abc123");
        assert_eq!(converted["choices"][0]["message"]["content"], "Hello from Gemini");
        assert_eq!(converted["choices"][0]["finish_reason"], "stop");
        assert_eq!(converted["usage"]["prompt_tokens"], 15);
        assert_eq!(converted["usage"]["completion_tokens"], 25);
        assert_eq!(converted["usage"]["thought_tokens"], 5);
        assert_eq!(converted["usage"]["total_tokens"], 45);
    }

    /// Non-streaming: function_call steps produce tool_calls in OpenAI format.
    #[test]
    fn gemini_interactions_json_function_calls() {
        let gemini_resp = serde_json::json!({
            "id": "int_xyz",
            "steps": [
                {"type": "model_output", "content": [{"type": "text", "text": "Let me check."}]},
                {"type": "function_call", "id": "call_123", "name": "get_weather", "arguments": {"city": "Paris"}}
            ],
            "usage": {
                "total_input_tokens": 10,
                "total_output_tokens": 20,
                "total_thought_tokens": 0,
                "total_tokens": 30
            },
            "status": "completed"
        });

        let converted = gemini_json_to_openai(&gemini_resp, Some("gemini-3.5-flash".to_string()));

        assert_eq!(converted["choices"][0]["finish_reason"], "tool_calls");
        let tool_calls = converted["choices"][0]["message"]["tool_calls"]
            .as_array()
            .unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0]["id"], "call_123");
        assert_eq!(tool_calls[0]["type"], "function");
        assert_eq!(tool_calls[0]["function"]["name"], "get_weather");
        // arguments should be JSON-stringified
        let args: serde_json::Value =
            serde_json::from_str(tool_calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["city"], "Paris");
    }

    /// Streaming: step.start for function_call emits tool_calls chunk.
    #[test]
    fn gemini_stream_step_start_function_call() {
        let event = serde_json::json!({
            "index": 0,
            "step": {"type": "function_call", "id": "call_123", "name": "get_weather", "arguments": {}},
            "event_type": "step.start"
        });

        let chunks = gemini_sse_to_openai(&event, Some("gemini-3.5-flash"));

        assert_eq!(chunks.len(), 1);
        let delta = &chunks[0]["choices"][0]["delta"];
        assert!(delta["tool_calls"].is_array());
        let tc = &delta["tool_calls"][0];
        assert_eq!(tc["id"], "call_123");
        assert_eq!(tc["function"]["name"], "get_weather");
        assert_eq!(tc["function"]["arguments"], "");
        // No finish_reason on step.start
        assert!(chunks[0]["choices"][0]["finish_reason"].is_null());
    }

    /// Streaming: step.delta with text emits content chunk.
    #[test]
    fn gemini_stream_step_delta_text() {
        let event = serde_json::json!({
            "index": 0,
            "delta": {"type": "text", "text": "Hello"},
            "event_type": "step.delta"
        });

        let chunks = gemini_sse_to_openai(&event, Some("gemini-3.5-flash"));

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0]["choices"][0]["delta"]["content"], "Hello");
    }

    /// Streaming: step.delta with arguments_delta emits tool_calls chunk.
    #[test]
    fn gemini_stream_step_delta_arguments() {
        let event = serde_json::json!({
            "index": 0,
            "delta": {"type": "arguments_delta", "arguments": "{\"city\":"},
            "event_type": "step.delta"
        });

        let chunks = gemini_sse_to_openai(&event, Some("gemini-3.5-flash"));

        assert_eq!(chunks.len(), 1);
        let tc = &chunks[0]["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(tc["function"]["arguments"], "{\"city\":");
    }

    /// Streaming: interaction.completed emits finish_reason + usage chunks.
    #[test]
    fn gemini_stream_interaction_completed() {
        let event = serde_json::json!({
            "interaction": {
                "status": "completed",
                "usage": {
                    "total_input_tokens": 10,
                    "total_output_tokens": 5,
                    "total_thought_tokens": 0,
                    "total_tokens": 15
                }
            },
            "event_type": "interaction.completed"
        });

        let chunks = gemini_sse_to_openai(&event, Some("gemini-3.5-flash"));

        assert_eq!(chunks.len(), 2);
        // First chunk: finish_reason
        assert_eq!(chunks[0]["choices"][0]["finish_reason"], "stop");
        // Second chunk: usage
        assert_eq!(chunks[1]["usage"]["prompt_tokens"], 10);
        assert_eq!(chunks[1]["usage"]["completion_tokens"], 5);
        assert_eq!(chunks[1]["usage"]["thought_tokens"], 0);
        assert_eq!(chunks[1]["usage"]["total_tokens"], 15);
    }

    /// Streaming: thought deltas are skipped.
    #[test]
    fn gemini_stream_thought_delta_skipped() {
        let event = serde_json::json!({
            "index": 0,
            "delta": {"type": "thought", "thought": "I'm thinking..."},
            "event_type": "step.delta"
        });

        let chunks = gemini_sse_to_openai(&event, Some("gemini-3.5-flash"));
        assert!(chunks.is_empty());
    }

    /// Error extraction: extracts message from error JSON.
    #[test]
    fn gemini_error_extraction() {
        let body = br#"{"error": {"message": "Invalid API key", "code": 401}}"#;
        let msg = extract_upstream_error(body);
        assert_eq!(msg, "Invalid API key");

        // Non-JSON body
        let body2 = b"Internal Server Error";
        let msg2 = extract_upstream_error(body2);
        assert_eq!(msg2, "Internal Server Error");

        // Missing message field
        let body3 = br#"{"error": "something went wrong"}"#;
        let msg3 = extract_upstream_error(body3);
        assert_eq!(msg3, "something went wrong");
    }

    /// Non-streaming: safety-blocked response produces content_filter finish_reason.
    #[test]
    fn gemini_interactions_safety_blocked() {
        let gemini_resp = serde_json::json!({
            "id": "int_blocked",
            "steps": [],
            "usage": {
                "total_input_tokens": 8,
                "total_output_tokens": 0,
                "total_tokens": 8
            },
            "status": "blocked"
        });

        let converted = gemini_json_to_openai(&gemini_resp, Some("gemini-3.5-flash".to_string()));

        assert_eq!(converted["choices"][0]["finish_reason"], "content_filter");
        assert_eq!(converted["usage"]["prompt_tokens"], 8);
        assert_eq!(converted["usage"]["completion_tokens"], 0);
    }

    /// Streaming: step.stop produces no output.
    #[test]
    fn gemini_stream_step_stop_no_output() {
        let event = serde_json::json!({
            "index": 0,
            "event_type": "step.stop"
        });

        let chunks = gemini_sse_to_openai(&event, Some("gemini-3.5-flash"));
        assert!(chunks.is_empty());
    }

    /// Streaming: interaction.created produces no output.
    #[test]
    fn gemini_stream_interaction_created_no_output() {
        let event = serde_json::json!({
            "interaction": {"id": "int_xxx"},
            "event_type": "interaction.created"
        });

        let chunks = gemini_sse_to_openai(&event, Some("gemini-3.5-flash"));
        assert!(chunks.is_empty());
    }

    /// SSE error response (Gemini streaming error) → extract error.message, not the whole body.
    #[test]
    fn extract_upstream_error_sse_format() {
        let sse_body =
            b"event: error\ndata: {\"error\":{\"message\":\"Invalid API key\",\"code\":401}}\n\n";
        let msg = extract_upstream_error(sse_body);
        assert_eq!(msg, "Invalid API key");
    }

    /// SSE with multiple events — error buried among normal events.
    #[test]
    fn extract_upstream_error_sse_among_events() {
        let sse_body = b"event: interaction.created\ndata: {\"id\":\"int_123\"}\n\nevent: error\ndata: {\"error\":{\"message\":\"Rate limit exceeded\",\"code\":429}}\n\n";
        let msg = extract_upstream_error(sse_body);
        assert_eq!(msg, "Rate limit exceeded");
    }

    /// Plain JSON error body still works (non-streaming path).
    #[test]
    fn extract_upstream_error_json_format() {
        let json_body = br#"{"error":{"message":"Model not found","code":404}}"#;
        let msg = extract_upstream_error(json_body);
        assert_eq!(msg, "Model not found");
    }

    /// SSE with error object but no message field → fallback string.
    #[test]
    fn extract_upstream_error_sse_no_message() {
        let sse_body = b"event: error\ndata: {\"error\":{\"code\":500}}\n\n";
        let msg = extract_upstream_error(sse_body);
        assert_eq!(msg, "upstream error (details unavailable)");
    }
}
