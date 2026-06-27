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
    f: fn(&serde_json::Value, Option<&str>, &str, bool) -> (Vec<serde_json::Value>, bool),
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
        let mut current_event = String::new();
        let mut had_function_call = false;
        while let Some(chunk) = body.data().await {
            let Ok(chunk) = chunk else {
                break;
            };
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buf.find('\n') {
                let line = buf[..pos].trim_end_matches('\r').to_string();
                buf.drain(..=pos);
                if let Some(event) = line.strip_prefix("event:") {
                    current_event = event.trim().to_string();
                    continue;
                }
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
                let (chunks, is_fc) = f(&value, model.as_deref(), &current_event, had_function_call);
                if is_fc {
                    had_function_call = true;
                }
                for chunk in chunks {
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

/// Extract (prompt, completion, thought, total) from a Gemini usage object.
/// Accepts both OpenAI-style names (prompt_tokens) and Gemini-style names (total_input_tokens).
fn extract_gemini_usage(usage: &serde_json::Value) -> (u64, u64, u64, u64) {
    let prompt = usage
        .get("prompt_tokens")
        .and_then(|n| n.as_u64())
        .or_else(|| usage.get("total_input_tokens").and_then(|n| n.as_u64()))
        .unwrap_or(0);
    let completion = usage
        .get("completion_tokens")
        .and_then(|n| n.as_u64())
        .or_else(|| usage.get("total_output_tokens").and_then(|n| n.as_u64()))
        .unwrap_or(0);
    let thought = usage
        .get("total_thought_tokens")
        .and_then(|n| n.as_u64())
        .unwrap_or(0);
    let total = usage
        .get("total_tokens")
        .and_then(|n| n.as_u64())
        .unwrap_or(prompt + completion + thought);
    (prompt, completion, thought, total)
}

fn gemini_json_to_openai(v: &serde_json::Value, model: Option<String>) -> serde_json::Value {
    let model = model.unwrap_or_default();

    let steps = v.get("steps").and_then(|s| s.as_array());

    // Extract text from steps[type="model_output"].content[*].text
    let content = steps
        .map(|steps| {
            steps
                .iter()
                .filter(|s| s.get("type").and_then(|t| t.as_str()) == Some("model_output"))
                .filter_map(|s| s.get("content").and_then(|c| c.as_array()))
                .flatten()
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();

    // Extract function_call steps → OpenAI tool_calls.
    let tool_calls: Vec<serde_json::Value> = steps
        .map(|steps| {
            steps
                .iter()
                .filter(|s| s.get("type").and_then(|t| t.as_str()) == Some("function_call"))
                .map(|s| {
                    let id = s.get("id").and_then(|id| id.as_str()).unwrap_or("");
                    let name = s.get("name").and_then(|n| n.as_str()).unwrap_or("");
                    let arguments = s.get("arguments")
                        .map(|a| if a.is_string() { a.clone() } else { serde_json::json!(a.to_string()) })
                        .unwrap_or(serde_json::json!("{}"));
                    serde_json::json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": arguments,
                        }
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    // Extract usage — API returns total_input_tokens / total_output_tokens / total_thought_tokens / total_tokens.
    let (prompt, completion, thought, total) = v
        .get("usage")
        .map(extract_gemini_usage)
        .unwrap_or((0, 0, 0, 0));

    let finish_reason = if !tool_calls.is_empty() {
        "tool_calls"
    } else {
        match v.get("status").and_then(|s| s.as_str()) {
            Some("completed") | None => "stop",
            _ => "content_filter",
        }
    };
    let mut message = serde_json::json!({
        "role": "assistant",
        "content": if content.is_empty() { serde_json::Value::Null } else { serde_json::Value::String(content) },
    });
    if !tool_calls.is_empty() {
        message["tool_calls"] = serde_json::Value::Array(tool_calls);
    }

    let mut resp = serde_json::json!({
        "id": "chatcmpl-gemini",
        "object": "chat.completion",
        "created": now_secs(),
        "model": model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason,
        }],
        "usage": {
            "prompt_tokens": prompt,
            "completion_tokens": completion,
            "total_tokens": prompt + completion,
        }
    });
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

fn anthropic_sse_to_openai(v: &serde_json::Value, model: Option<&str>, _event_type: &str, _had_fc: bool) -> (Vec<serde_json::Value>, bool) {
    let ty = v.get("type").and_then(|s| s.as_str()).unwrap_or("");
    let chunks = match ty {
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
    };
    (chunks, false)
}

fn gemini_sse_to_openai(v: &serde_json::Value, model: Option<&str>, event_type: &str, had_function_call: bool) -> (Vec<serde_json::Value>, bool) {
    match event_type {
        "step.start" => {
            let step_type = v.get("step").and_then(|s| s.get("type")).and_then(|t| t.as_str()).unwrap_or("");
            if step_type != "function_call" {
                return (Vec::new(), false);
            }
            let step = v.get("step");
            let id = step.and_then(|s| s.get("id")).and_then(|id| id.as_str()).unwrap_or("");
            let name = step.and_then(|s| s.get("name")).and_then(|n| n.as_str()).unwrap_or("");
            // Emit tool call with empty arguments — actual arguments arrive
            // incrementally via step.delta(type="arguments") partial_arguments.
            // This matches OpenAI streaming format where function.arguments
            // accumulates across chunks.
            let chunk = chat_chunk_json(
                "chatcmpl-gemini",
                model.unwrap_or(""),
                serde_json::json!({
                    "tool_calls": [{
                        "index": 0,
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": "",
                        }
                    }]
                }),
                None,
                None,
            );
            (vec![chunk], true)
        }
        "step.delta" => {
            let delta = v.get("delta");
            let delta_type = delta.and_then(|d| d.get("type")).and_then(|t| t.as_str()).unwrap_or("text");
            match delta_type {
                "text" => {
                    let text = delta
                        .and_then(|d| d.get("text"))
                        .and_then(|t| t.as_str())
                        .unwrap_or("");
                    if text.is_empty() {
                        return (Vec::new(), false);
                    }
                    (vec![chat_chunk_json(
                        "chatcmpl-gemini",
                        model.unwrap_or(""),
                        serde_json::json!({"content": text}),
                        None,
                        None,
                    )], false)
                }
                "arguments" => {
                    // Streaming function call argument increment.
                    // Forward partial_arguments as tool_calls delta so the
                    // client can accumulate them into the full arguments string.
                    let partial = delta
                        .and_then(|d| d.get("partial_arguments"))
                        .and_then(|s| s.as_str())
                        .unwrap_or("");
                    if partial.is_empty() {
                        return (Vec::new(), false);
                    }
                    (vec![chat_chunk_json(
                        "chatcmpl-gemini",
                        model.unwrap_or(""),
                        serde_json::json!({
                            "tool_calls": [{
                                "index": 0,
                                "function": {
                                    "arguments": partial,
                                }
                            }]
                        }),
                        None,
                        None,
                    )], false)
                }
                // thought, thought_signature, etc. — filtered out
                _ => (Vec::new(), false),
            }
        }
        "interaction.completed" => {
            // Completion event with usage.
            let interaction = v.get("interaction");
            let usage = interaction.and_then(|i| i.get("usage"));

            // finish_reason: if we had function calls, emit "tool_calls" here
            // (step.start no longer sends finish_reason to avoid premature termination).
            let finish_reason = if had_function_call {
                Some("tool_calls")
            } else {
                match interaction.and_then(|i| i.get("status")).and_then(|s| s.as_str()) {
                    Some("completed") | None => Some("stop"),
                    _ => Some("content_filter"),
                }
            };

            // Emit finish reason chunk.
            let mut out = vec![chat_chunk_json(
                "chatcmpl-gemini",
                model.unwrap_or(""),
                serde_json::json!({}),
                finish_reason,
                None,
            )];

            // Emit usage if present.
            if let Some(usage) = usage {
                let (prompt, completion, thought, total) = extract_gemini_usage(usage);
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
            (out, false)
        }
        // step.stop, interaction.created, interaction.in_progress — ignored
        _ => (Vec::new(), false),
    }
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

    /// Round-trip: Gemini Interactions API JSON → OpenAI format → serialize → parse → verify usage fields.
    /// API returns total_input_tokens / total_output_tokens / total_thought_tokens / total_tokens.
    #[test]
    fn gemini_json_to_openai_produces_correct_usage() {
        let gemini_resp = serde_json::json!({
            "id": "int_test123",
            "model": "gemini-3.5-flash",
            "object": "interaction",
            "status": "completed",
            "steps": [{
                "type": "model_output",
                "content": [{"type": "text", "text": "Hello from Gemini"}]
            }],
            "usage": {
                "total_input_tokens": 15,
                "total_output_tokens": 25,
                "total_thought_tokens": 5,
                "total_tokens": 45
            }
        });

        let converted = gemini_json_to_openai(&gemini_resp, Some("gemini-3.5-flash".to_string()));

        assert_eq!(converted["usage"]["prompt_tokens"], 15);
        assert_eq!(converted["usage"]["completion_tokens"], 25);
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

        // Verify content extraction from steps
        assert_eq!(
            parsed["choices"][0]["message"]["content"],
            "Hello from Gemini"
        );
        // Completed interaction without function_call → "stop"
        assert_eq!(parsed["choices"][0]["finish_reason"], "stop");
    }

    /// SSE step.delta: thought types filtered out, text and arguments forwarded.
    #[test]
    fn gemini_sse_filters_thought_deltas() {
        // text delta — should produce output
        let text_event = serde_json::json!({
            "delta": {"type": "text", "text": "hello"}
        });
        let (result, is_fc) = gemini_sse_to_openai(&text_event, Some("m"), "step.delta", false);
        assert_eq!(result.len(), 1);
        assert!(!is_fc);
        assert_eq!(result[0]["choices"][0]["delta"]["content"], "hello");

        // thought delta — must be filtered
        let thought_event = serde_json::json!({
            "delta": {"type": "thought", "text": "thinking..."}
        });
        let (result, _) = gemini_sse_to_openai(&thought_event, Some("m"), "step.delta", false);
        assert!(result.is_empty(), "thought deltas must not be forwarded");

        // no type field — defaults to text (backward compat)
        let no_type_event = serde_json::json!({
            "delta": {"text": "fallback"}
        });
        let (result, _) = gemini_sse_to_openai(&no_type_event, Some("m"), "step.delta", false);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["choices"][0]["delta"]["content"], "fallback");
    }

    /// SSE step.delta with type "arguments" → forward partial_arguments as tool_calls delta.
    #[test]
    fn gemini_sse_arguments_delta_forwarded() {
        let event = serde_json::json!({
            "delta": {"type": "arguments", "partial_arguments": "{\"city\":"}
        });
        let (result, is_fc) = gemini_sse_to_openai(&event, Some("m"), "step.delta", false);
        assert!(!is_fc);
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            "{\"city\":"
        );

        // empty partial_arguments → no output
        let empty_event = serde_json::json!({
            "delta": {"type": "arguments", "partial_arguments": ""}
        });
        let (result, _) = gemini_sse_to_openai(&empty_event, Some("m"), "step.delta", false);
        assert!(result.is_empty());
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

    /// SSE error response (Gemini streaming error) → extract error.message, not the whole body.
    #[test]
    fn extract_upstream_error_sse_format() {
        let sse_body = b"event: error\ndata: {\"error\":{\"message\":\"Invalid API key\",\"code\":401}}\n\n";
        let msg = extract_upstream_error(sse_body);
        assert_eq!(msg, "Invalid API key");
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

    /// JSON response with function_call step → OpenAI tool_calls format.
    #[test]
    fn gemini_json_function_call_to_tool_calls() {
        let gemini_resp = serde_json::json!({
            "steps": [
                {"type": "model_output", "content": [{"type": "text", "text": ""}]},
                {"type": "function_call", "id": "call_abc", "name": "get_weather", "arguments": {"city": "Beijing"}}
            ],
            "usage": {"total_input_tokens": 10, "total_output_tokens": 5, "total_tokens": 15}
        });
        let result = gemini_json_to_openai(&gemini_resp, Some("gemini-3.5-flash".to_string()));
        assert_eq!(result["choices"][0]["finish_reason"], "tool_calls");
        let tool_calls = result["choices"][0]["message"]["tool_calls"].as_array().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0]["id"], "call_abc");
        assert_eq!(tool_calls[0]["function"]["name"], "get_weather");
        // arguments should be a JSON string
        let args_str = tool_calls[0]["function"]["arguments"].as_str().unwrap();
        assert!(args_str.contains("Beijing"));
    }

    /// SSE step.start with function_call → tool_calls chunk with empty arguments.
    /// Actual arguments arrive incrementally via step.delta(type="arguments").
    #[test]
    fn gemini_sse_function_call_step_start() {
        let event = serde_json::json!({
            "step": {
                "type": "function_call",
                "id": "call_xyz",
                "name": "search",
                "arguments": {"query": "rust"}
            }
        });
        let (chunks, is_fc) = gemini_sse_to_openai(&event, Some("m"), "step.start", false);
        assert!(is_fc, "function_call should set is_fc=true");
        assert_eq!(chunks.len(), 1);
        // step.start emits empty arguments — actual args come via step.delta
        assert_eq!(chunks[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"], "");
        // finish_reason not set here — comes from interaction.completed
        assert!(chunks[0]["choices"][0]["finish_reason"].is_null());
        let tc = &chunks[0]["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(tc["id"], "call_xyz");
        assert_eq!(tc["function"]["name"], "search");
    }

    /// SSE interaction.completed after function_call → finish_reason is "tool_calls".
    #[test]
    fn gemini_sse_completed_after_function_call() {
        let event = serde_json::json!({
            "interaction": {"usage": {"total_input_tokens": 10, "total_output_tokens": 5, "total_tokens": 15}}
        });
        // had_function_call = true
        let (chunks, _) = gemini_sse_to_openai(&event, Some("m"), "interaction.completed", true);
        // finish_reason should be "tool_calls" (moved here from step.start)
        assert_eq!(
            chunks[0]["choices"][0]["finish_reason"].as_str(),
            Some("tool_calls")
        );
    }

    /// JSON: interaction status "failed" → finish_reason "content_filter".
    #[test]
    fn gemini_json_failed_status_content_filter() {
        let gemini_resp = serde_json::json!({
            "status": "failed",
            "steps": [],
            "usage": {"total_input_tokens": 8, "total_tokens": 8}
        });
        let result = gemini_json_to_openai(&gemini_resp, Some("gemini-3.5-flash".to_string()));
        assert_eq!(result["choices"][0]["finish_reason"], "content_filter");
    }

    /// SSE: interaction.completed with status "failed" → finish_reason "content_filter".
    #[test]
    fn gemini_sse_completed_failed_status() {
        let event = serde_json::json!({
            "interaction": {"status": "failed", "usage": {"total_tokens": 8}}
        });
        let (chunks, _) = gemini_sse_to_openai(&event, Some("m"), "interaction.completed", false);
        assert_eq!(chunks[0]["choices"][0]["finish_reason"], "content_filter");
    }

}
