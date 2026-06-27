use super::{AdaptedRequest, AuthStyle};
use bytes::Bytes;
use hyper::{Body, Method, Response};

fn format_error(status: http::StatusCode, message: &str, code: &str) -> Response<Body> {
    crate::util::json_error(status, message, code)
}

fn is_chat_completions_path(path: &str) -> bool {
    path == "/v1/chat/completions" || path == "/v1/chat/completions/"
}

fn copy_number(src: &serde_json::Value, dst: &mut serde_json::Value, src_key: &str, dst_key: &str) {
    if let Some(v) = src.get(src_key).and_then(|n| n.as_f64()) {
        dst[dst_key] = serde_json::json!(v);
    }
}

fn content_to_text(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter_map(|part| {
                part.get("text")
                    .and_then(|t| t.as_str())
                    .or_else(|| part.get("content").and_then(|t| t.as_str()))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Max decoded file size: 20 MB (matching OpenAI / Anthropic limits).
const MAX_DECODED_BYTES: usize = 20 * 1024 * 1024;

fn content_to_anthropic_blocks(content: &serde_json::Value) -> serde_json::Value {
    match content {
        serde_json::Value::Array(parts) => {
            let out: Vec<serde_json::Value> = parts
                .iter()
                .filter_map(|part| openai_part_to_anthropic(part))
                .collect();
            serde_json::Value::Array(out)
        }
        _ => {
            let text = content_to_text(content);
            if text.len() > MAX_DECODED_BYTES {
                tracing::warn!(text_len = text.len(), "anthropic: fallback text content dropped (exceeds {}MB)", MAX_DECODED_BYTES / 1024 / 1024);
                return serde_json::Value::Array(vec![]);
            }
            serde_json::json!([{"type": "text", "text": text}])
        },
    }
}

/// Binary data extracted from an OpenAI multimodal content part.
struct BinaryAttachment {
    mime: String,
    data: String,
}

/// Extract binary data from an OpenAI content part (image_url / input_audio / file).
/// Handles data URI parsing, size checks, and MIME inference.
/// `provider` is used for warn logs ("anthropic" or "gemini").
fn extract_binary_attachment(part: &serde_json::Value, provider: &str) -> Option<BinaryAttachment> {
    let part_type = part.get("type").and_then(|t| t.as_str()).unwrap_or("");

    match part_type {
        "image_url" => {
            let url = part.get("image_url").and_then(|u| u.get("url")).and_then(|u| u.as_str());
            let url = match url {
                Some(u) => u,
                None => return None,
            };
            match parse_data_uri(url, MAX_DECODED_BYTES) {
                Some((mime, data)) => Some(BinaryAttachment { mime, data }),
                None => {
                    tracing::warn!(%url, "{provider}: image_url dropped (data URI parse failed or exceeds {}MB)", MAX_DECODED_BYTES / 1024 / 1024);
                    None
                }
            }
        }
        "input_audio" => {
            let audio = part.get("input_audio")?;
            let data = audio.get("data").and_then(|d| d.as_str()).unwrap_or("");
            let format = audio.get("format").and_then(|f| f.as_str()).unwrap_or("wav");
            if data.len() > MAX_DECODED_BYTES {
                tracing::warn!(data_len = data.len(), format, "{provider}: input_audio dropped (exceeds {}MB)", MAX_DECODED_BYTES / 1024 / 1024);
                return None;
            }
            Some(BinaryAttachment { mime: format!("audio/{}", format), data: data.to_string() })
        }
        "file" => {
            let file = part.get("file")?;
            let file_data = file.get("file_data").and_then(|d| d.as_str()).unwrap_or("");
            let filename = file.get("filename").and_then(|f| f.as_str()).unwrap_or("");
            if file_data.len() > MAX_DECODED_BYTES {
                tracing::warn!(data_len = file_data.len(), %filename, "{provider}: file dropped (exceeds {}MB)", MAX_DECODED_BYTES / 1024 / 1024);
                return None;
            }
            Some(BinaryAttachment { mime: mime_from_filename(filename), data: file_data.to_string() })
        }
        _ => None,
    }
}

/// Convert a single OpenAI content part to an Anthropic block.
fn openai_part_to_anthropic(part: &serde_json::Value) -> Option<serde_json::Value> {
    let part_type = part.get("type").and_then(|t| t.as_str()).unwrap_or("");

    let text = part
        .get("text")
        .and_then(|t| t.as_str())
        .or_else(|| part.get("content").and_then(|t| t.as_str()));
    if let Some(s) = text {
        if part_type.is_empty() || part_type == "text" {
            return Some(serde_json::json!({"type": "text", "text": s}));
        }
    }

    if let Some(att) = extract_binary_attachment(part, "anthropic") {
        let block_type = if part_type == "image_url" { "image" } else { "document" };
        return Some(serde_json::json!({
            "type": block_type,
            "source": {"type": "base64", "media_type": att.mime, "data": att.data}
        }));
    }
    // If we got here and part_type was recognized (image/audio/file), the warn was logged by extract_binary_attachment.
    if matches!(part_type, "image_url" | "input_audio" | "file") {
        return None;
    }

    // Fallback: plain text
    if let Some(s) = text {
        if s.len() > MAX_DECODED_BYTES {
            tracing::warn!(text_len = s.len(), "anthropic: fallback text block dropped (exceeds {}MB)", MAX_DECODED_BYTES / 1024 / 1024);
            return None;
        }
        return Some(serde_json::json!({"type": "text", "text": s}));
    }

    None
}

/// Convert a single OpenAI content part to a Gemini Interactions API content block.
/// Returns blocks in the format: {type:"text",text:"..."}, {type:"image",data:"...",mime_type:"..."}, etc.
fn openai_part_to_gemini_interaction(part: &serde_json::Value) -> Option<serde_json::Value> {
    let part_type = part.get("type").and_then(|t| t.as_str()).unwrap_or("");

    // Text content
    let text = part
        .get("text")
        .and_then(|t| t.as_str())
        .or_else(|| part.get("content").and_then(|t| t.as_str()));
    if let Some(s) = text {
        if part_type.is_empty() || part_type == "text" {
            if s.len() > MAX_DECODED_BYTES {
                tracing::warn!(text_len = s.len(), "gemini: text content dropped (exceeds {}MB)", MAX_DECODED_BYTES / 1024 / 1024);
                return None;
            }
            return Some(serde_json::json!({"type": "text", "text": s}));
        }
    }

    // Binary attachments (image, audio, video, document)
    // Prefer mime-based type detection (reflects actual content), fall back to part_type.
    if let Some(att) = extract_binary_attachment(part, "gemini") {
        let block_type = if att.mime.starts_with("image/") {
            "image"
        } else if att.mime.starts_with("audio/") {
            "audio"
        } else if att.mime.starts_with("video/") {
            "video"
        } else if part_type == "image_url" {
            "image"
        } else if part_type == "input_audio" {
            "audio"
        } else {
            "document"
        };
        return Some(serde_json::json!({
            "type": block_type,
            "data": att.data,
            "mime_type": att.mime
        }));
    }
    if matches!(part_type, "image_url" | "input_audio" | "file") {
        return None;
    }

    None
}

/// Parse a data URI "data:<mime>;base64,<data>" -> (mime, data).
/// Returns None if the decoded data exceeds `max_bytes`.
fn parse_data_uri(uri: &str, max_bytes: usize) -> Option<(String, String)> {
    let stripped = uri.strip_prefix("data:")?;
    let (mime_and_encoding, data) = stripped.split_once(',')?;
    let mime = mime_and_encoding.trim_end_matches(";base64").trim_end_matches(";BASE64");
    if mime.is_empty() || data.is_empty() {
        return None;
    }
    if data.len() > max_bytes {
        return None;
    }
    Some((mime.to_string(), data.to_string()))
}

/// Map a filename extension to MIME type. Falls back to application/octet-stream.
fn mime_from_filename(filename: &str) -> String {
    let ext = filename.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "pdf" => "application/pdf".into(),
        "doc" => "application/msword".into(),
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document".into(),
        "xls" => "application/vnd.ms-excel".into(),
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet".into(),
        "ppt" => "application/vnd.ms-powerpoint".into(),
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation".into(),
        "txt" => "text/plain".into(),
        "csv" => "text/csv".into(),
        "html" | "htm" => "text/html".into(),
        "json" => "application/json".into(),
        "xml" => "application/xml".into(),
        "zip" => "application/zip".into(),
        "mp3" => "audio/mpeg".into(),
        "mp4" => "video/mp4".into(),
        "wav" => "audio/wav".into(),
        "ogg" => "audio/ogg".into(),
        "webm" => "video/webm".into(),
        "png" => "image/png".into(),
        "jpg" | "jpeg" => "image/jpeg".into(),
        "gif" => "image/gif".into(),
        "webp" => "image/webp".into(),
        "svg" => "image/svg+xml".into(),
        _ => "application/octet-stream".into(),
    }
}

// ─────────────────────────────────────────────────────────────
// Gemini Interactions API: helpers
// ─────────────────────────────────────────────────────────────

/// Strip "$schema" and "additionalProperties" from tool parameters.
/// Gemini doesn't support these JSON Schema fields.
fn strip_gemini_unsupported_schema(params: &serde_json::Value) -> serde_json::Value {
    match params {
        serde_json::Value::Object(map) => {
            let mut cleaned = serde_json::Map::new();
            for (k, v) in map {
                if matches!(k.as_str(), "$schema" | "additionalProperties") {
                    continue;
                }
                cleaned.insert(k.clone(), strip_gemini_unsupported_schema(v));
            }
            serde_json::Value::Object(cleaned)
        }
        _ => params.clone(),
    }
}

/// Unwrap double-encoded content from MCP tools.
/// If text starts with "[" or "{", try parsing as JSON.
/// If it's an OpenAI content array [{type:"text",text:"..."}], extract the actual text.
fn unwrap_mcp_content(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.starts_with('[') || trimmed.starts_with('{') {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if let Some(arr) = parsed.as_array() {
                let extracted: Vec<String> = arr
                    .iter()
                    .filter_map(|item| {
                        if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                            item.get("text").and_then(|t| t.as_str()).map(|s| s.to_string())
                        } else {
                            None
                        }
                    })
                    .collect();
                if !extracted.is_empty() {
                    return extracted.join("\n");
                }
            }
            // If it parsed as JSON but wasn't a content array, return original text
            return text.to_string();
        }
    }
    text.to_string()
}

/// Convert OpenAI content (string or array) to Gemini Interactions API content blocks.
/// Returns vec of {type:"text",text:"..."}, {type:"image",data,mime_type}, etc.
fn content_to_gemini_interaction_content(content: &serde_json::Value) -> Vec<serde_json::Value> {
    match content {
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter_map(|part| openai_part_to_gemini_interaction(part))
            .collect(),
        serde_json::Value::String(s) => {
            if s.len() > MAX_DECODED_BYTES {
                tracing::warn!(text_len = s.len(), "gemini: text content dropped (exceeds {}MB)", MAX_DECODED_BYTES / 1024 / 1024);
                return vec![];
            }
            vec![serde_json::json!({"type": "text", "text": s})]
        }
        _ => {
            tracing::debug!("gemini: unhandled content type, dropping content");
            vec![]
        },
    }
}

// ─────────────────────────────────────────────────────────────
// Anthropic adapter (unchanged)
// ─────────────────────────────────────────────────────────────

pub(super) fn adapt_request_inner(
    original_pq: &http::uri::PathAndQuery,
    body: &Bytes,
) -> Result<AdaptedRequest, Response<Body>> {
    if !is_chat_completions_path(original_pq.path()) {
        return Err(format_error(
            http::StatusCode::BAD_REQUEST,
            "anthropic format only supports /v1/chat/completions",
            "unsupported_format_path",
        ));
    }
    let v: serde_json::Value = serde_json::from_slice(body).map_err(|_| {
        format_error(
            http::StatusCode::BAD_REQUEST,
            "request body must be valid json",
            "bad_request",
        )
    })?;
    let model = v.get("model").and_then(|m| m.as_str()).unwrap_or_default();
    let max_tokens = v
        .get("max_tokens")
        .or_else(|| v.get("max_completion_tokens"))
        .and_then(|n| n.as_u64())
        .unwrap_or(1024);
    let stream = v.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);

    let mut system_parts = Vec::new();
    let mut messages = Vec::new();
    if let Some(input_messages) = v.get("messages").and_then(|m| m.as_array()) {
        for msg in input_messages {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            let content = msg
                .get("content")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            if role == "system" {
                let text = content_to_text(&content);
                if !text.is_empty() {
                    system_parts.push(text);
                }
                continue;
            }
            let out_role = if role == "assistant" {
                "assistant"
            } else {
                "user"
            };
            messages.push(serde_json::json!({
                "role": out_role,
                "content": content_to_anthropic_blocks(&content),
            }));
        }
    }

    let mut out = serde_json::json!({
        "model": model,
        "max_tokens": max_tokens,
        "messages": messages,
        "stream": stream,
    });
    if !system_parts.is_empty() {
        out["system"] = serde_json::Value::String(system_parts.join("\n\n"));
    }
    // Pass through tools and tool_choice.
    if let Some(tools) = v.get("tools") {
        let anthropic_tools: Vec<serde_json::Value> = tools
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| t.get("function"))
                    .map(|f| {
                        serde_json::json!({
                            "name": f.get("name"),
                            "description": f.get("description"),
                            "input_schema": f.get("parameters").cloned().unwrap_or(serde_json::json!({})),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        if !anthropic_tools.is_empty() {
            out["tools"] = serde_json::Value::Array(anthropic_tools);
        }
    }
    if let Some(tc) = v.get("tool_choice") {
        if tc.as_str() == Some("auto") || tc.as_str() == Some("any") {
            out["tool_choice"] = serde_json::json!({"type": "auto"});
        } else if tc.as_str() == Some("required") {
            out["tool_choice"] = serde_json::json!({"type": "any"});
        } else if let Some(name) = tc.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()) {
            out["tool_choice"] = serde_json::json!({"type": "tool", "name": name});
        }
    }
    copy_number(&v, &mut out, "temperature", "temperature");
    copy_number(&v, &mut out, "top_p", "top_p");
    if let Some(stop) = v.get("stop") {
        out["stop_sequences"] = match stop {
            serde_json::Value::Array(_) => stop.clone(),
            serde_json::Value::String(_) => serde_json::json!([stop.clone()]),
            _ => serde_json::Value::Null,
        };
    }

    Ok(AdaptedRequest {
        method: Method::POST,
        path_and_query: http::uri::PathAndQuery::from_static("/v1/messages"),
        body: Bytes::from(out.to_string()),
        auth_style: AuthStyle::AnthropicKey,
        extra_headers: Vec::new(),
    })
}

// ─────────────────────────────────────────────────────────────
// Gemini Interactions API adapter
// ─────────────────────────────────────────────────────────────

pub(super) fn adapt_request_inner_gemini(
    original_pq: &http::uri::PathAndQuery,
    body: &Bytes,
    model: &str,
) -> Result<AdaptedRequest, Response<Body>> {
    if !is_chat_completions_path(original_pq.path()) {
        return Err(format_error(
            http::StatusCode::BAD_REQUEST,
            "gemini format only supports /v1/chat/completions",
            "unsupported_format_path",
        ));
    }
    let v: serde_json::Value = serde_json::from_slice(body).map_err(|_| {
        format_error(
            http::StatusCode::BAD_REQUEST,
            "request body must be valid json",
            "bad_request",
        )
    })?;
    let stream = v.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);

    let mut system_parts = Vec::new();
    let mut input = Vec::new();
    // Track call_id → function_name from assistant tool_calls for use in function_result
    let mut call_name_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    if let Some(input_messages) = v.get("messages").and_then(|m| m.as_array()) {
        for msg in input_messages {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            let content = msg
                .get("content")
                .cloned()
                .unwrap_or(serde_json::Value::Null);

            match role {
                "system" => {
                    let text = content_to_text(&content);
                    if !text.is_empty() {
                        system_parts.push(text);
                    }
                }
                "assistant" => {
                    // Two distinct cases:
                    // 1. Has tool_calls → ONLY record call_name_map for name lookup
                    //    (function_call and function_result are rejected by v1beta Interactions API)
                    // 2. No tool_calls → pure text response, preserve as model_output
                    if let Some(tool_calls) = msg.get("tool_calls").and_then(|t| t.as_array()) {
                        for tc in tool_calls {
                            let id = tc.get("id").and_then(|i| i.as_str()).unwrap_or("");
                            let func = tc.get("function");
                            let name = func
                                .and_then(|f| f.get("name"))
                                .and_then(|n| n.as_str())
                                .unwrap_or("");
                            if !id.is_empty() {
                                call_name_map.insert(id.to_string(), name.to_string());
                            }
                        }
                    } else {
                        let parts = content_to_gemini_interaction_content(&content);
                        if !parts.is_empty() {
                            input.push(serde_json::json!({
                                "type": "model_output",
                                "content": parts
                            }));
                        }
                    }
                }
                "tool" => {
                    let tool_msg_name = msg
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("");
                    let call_id = msg
                        .get("tool_call_id")
                        .and_then(|i| i.as_str())
                        .unwrap_or("");
                    // Prefer name from tool message, fall back to name from function_call
                    let name = if !tool_msg_name.is_empty() {
                        tool_msg_name
                    } else {
                        call_name_map.get(call_id).map(|s| s.as_str()).unwrap_or("")
                    };
                    let raw_text = content_to_text(&content);
                    let unwrapped = unwrap_mcp_content(&raw_text);
                    // Skip empty tool results — don't emit content-free steps
                    if unwrapped.is_empty() {
                        continue;
                    }
                    // Emit as user_input — v1beta Interactions API rejects function_result in input.
                    // Prefix with tool name for context.
                    let text = if name.is_empty() {
                        unwrapped
                    } else {
                        format!("[Tool {} result]\n{}", name, unwrapped)
                    };
                    input.push(serde_json::json!({
                        "type": "user_input",
                        "content": [{"type": "text", "text": text}]
                    }));
                }
                _ => {
                    // "user" or unknown role -> user_input
                    let parts = content_to_gemini_interaction_content(&content);
                    if !parts.is_empty() {
                        input.push(serde_json::json!({
                            "type": "user_input",
                            "content": parts
                        }));
                    }
                }
            }
        }
    }

    // Build output body
    let mut out = serde_json::json!({
        "model": model,
        "input": input,
        "store": false,
    });
    if stream {
        out["stream"] = serde_json::json!(true);
    }

    // System instruction as plain string
    if !system_parts.is_empty() {
        out["system_instruction"] = serde_json::Value::String(system_parts.join("\n\n"));
    }

    // Tools: flat array of {type:"function", name, description, parameters}
    if let Some(tools) = v.get("tools").and_then(|t| t.as_array()) {
        let gemini_tools: Vec<serde_json::Value> = tools
            .iter()
            .filter_map(|t| t.get("function"))
            .map(|f| {
                let name = f.get("name").cloned().unwrap_or(serde_json::json!(""));
                let description = f.get("description").cloned();
                let parameters = f
                    .get("parameters")
                    .map(|p| strip_gemini_unsupported_schema(p))
                    .unwrap_or(serde_json::json!({}));

                let mut tool = serde_json::json!({
                    "type": "function",
                    "name": name,
                    "parameters": parameters,
                });
                // Only include description if present and non-null
                if let Some(desc) = description {
                    if !desc.is_null() {
                        tool["description"] = desc;
                    }
                }
                tool
            })
            .collect();
        if !gemini_tools.is_empty() {
            out["tools"] = serde_json::Value::Array(gemini_tools);
        }
    }

    // Build generation_config: tool_choice (if present) + snake_case param fields
    let mut gen_config = serde_json::json!({});

    // tool_choice mapping: OpenAI -> Gemini
    if let Some(tc) = v.get("tool_choice") {
        let gemini_tc = match tc {
            serde_json::Value::String(s) => match s.as_str() {
                "auto" => "auto",
                "required" | "any" => "any",
                "none" => "none",
                invalid => {
                    return Err(format_error(
                        http::StatusCode::BAD_REQUEST,
                        &format!("invalid tool_choice '{}' for gemini format", invalid),
                        "invalid_tool_choice",
                    ))
                }
            },
            serde_json::Value::Object(_) => "any",
            _ => {
                return Err(format_error(
                    http::StatusCode::BAD_REQUEST,
                    "invalid tool_choice type for gemini format",
                    "invalid_tool_choice",
                ))
            }
        };
        gen_config["tool_choice"] = serde_json::json!(gemini_tc);
    }

    if let Some(n) = v
        .get("max_tokens")
        .or_else(|| v.get("max_completion_tokens"))
        .and_then(|n| n.as_u64())
    {
        gen_config["max_output_tokens"] = serde_json::json!(n);
    }
    if let Some(n) = v.get("temperature").and_then(|n| n.as_f64()) {
        gen_config["temperature"] = serde_json::json!(n);
    }
    if let Some(n) = v.get("top_p").and_then(|n| n.as_f64()) {
        gen_config["top_p"] = serde_json::json!(n);
    }
    if let Some(stop) = v.get("stop") {
        let stops = match stop {
            serde_json::Value::Array(a) => a.clone(),
            serde_json::Value::String(_) => vec![stop.clone()],
            _ => vec![],
        };
        if !stops.is_empty() {
            gen_config["stop_sequences"] = serde_json::Value::Array(stops);
        }
    }

    if gen_config.as_object().map_or(false, |m| !m.is_empty()) {
        out["generation_config"] = gen_config;
    }

    // Fixed path for Interactions API
    let path_and_query = http::uri::PathAndQuery::from_static("/v1beta/interactions");

    // Api-Revision header
    let extra_headers = vec![(
        hyper::header::HeaderName::from_static("api-revision"),
        hyper::header::HeaderValue::from_static("2026-05-20"),
    )];

    let body_bytes = Bytes::from(out.to_string());
    tracing::debug!(body = %String::from_utf8_lossy(&body_bytes), "gemini upstream request");

    Ok(AdaptedRequest {
        method: Method::POST,
        path_and_query,
        body: body_bytes,
        auth_style: AuthStyle::GeminiKey,
        extra_headers,
    })
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::config::UpstreamFormat;
    use bytes::Bytes;
    use hyper::Method;

    #[test]
    fn anthropic_request_moves_system_and_messages() {
        let body = Bytes::from_static(
            br#"{"model":"claude-3","messages":[{"role":"system","content":"sys"},{"role":"user","content":"hi"}],"max_tokens":7,"stream":true}"#,
        );
        let adapted = adapt_request(
            UpstreamFormat::Anthropic,
            &"/v1/chat/completions".parse().unwrap(),
            &Method::POST,
            &body,
            "claude-3",
        )
        .unwrap();
        assert_eq!(adapted.path_and_query.as_str(), "/v1/messages");
        let v: serde_json::Value = serde_json::from_slice(&adapted.body).unwrap();
        assert_eq!(v["system"], "sys");
        assert_eq!(v["messages"][0]["role"], "user");
        assert_eq!(v["max_tokens"], 7);
    }

    #[test]
    fn gemini_request_uses_interactions_path() {
        let body = Bytes::from_static(
            br#"{"model":"gemini-3.5-flash","messages":[{"role":"user","content":"hi"}],"stream":false}"#,
        );
        let adapted = adapt_request(
            UpstreamFormat::Gemini,
            &"/v1/chat/completions".parse().unwrap(),
            &Method::POST,
            &body,
            "gemini-3.5-flash",
        )
        .unwrap();
        assert_eq!(adapted.path_and_query.as_str(), "/v1beta/interactions");
        let v: serde_json::Value = serde_json::from_slice(&adapted.body).unwrap();
        assert_eq!(v["model"], "gemini-3.5-flash");
        assert_eq!(v["input"][0]["type"], "user_input");
        assert_eq!(v["store"], false);
    }

    // ── Gemini request tests ──

    #[test]
    fn gemini_system_instruction_extracted() {
        let body = Bytes::from_static(
            br#"{"model":"gemini-3.5-flash","messages":[{"role":"system","content":"You are helpful."},{"role":"user","content":"hi"}]}"#,
        );
        let adapted = adapt_request(
            UpstreamFormat::Gemini,
            &"/v1/chat/completions".parse().unwrap(),
            &Method::POST,
            &body,
            "gemini-3.5-flash",
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&adapted.body).unwrap();
        assert_eq!(v["system_instruction"], "You are helpful.");
        // System message should NOT appear in input
        assert_eq!(v["input"].as_array().unwrap().len(), 1);
        assert_eq!(v["input"][0]["type"], "user_input");
    }

    #[test]
    fn gemini_multi_turn_conversation() {
        let body = Bytes::from_static(
            br#"{"model":"gemini-3.5-flash","messages":[{"role":"user","content":"hi"},{"role":"assistant","content":"hello"},{"role":"user","content":"how are you?"}]}"#,
        );
        let adapted = adapt_request(
            UpstreamFormat::Gemini,
            &"/v1/chat/completions".parse().unwrap(),
            &Method::POST,
            &body,
            "gemini-3.5-flash",
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&adapted.body).unwrap();
        let input = v["input"].as_array().unwrap();
        // Assistant text is preserved as model_output for multi-turn context
        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["type"], "user_input");
        assert_eq!(input[1]["type"], "model_output");
        assert_eq!(input[2]["type"], "user_input");
    }

    #[test]
    fn gemini_multimodal_image() {
        let body = Bytes::from_static(
            br#"{"model":"gemini-3.5-flash","messages":[{"role":"user","content":[{"type":"text","text":"describe"},{"type":"image_url","image_url":{"url":"data:image/png;base64,abc123"}}]}]}"#,
        );
        let adapted = adapt_request(
            UpstreamFormat::Gemini,
            &"/v1/chat/completions".parse().unwrap(),
            &Method::POST,
            &body,
            "gemini-3.5-flash",
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&adapted.body).unwrap();
        let content = v["input"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "describe");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["data"], "abc123");
        assert_eq!(content[1]["mime_type"], "image/png");
    }

    #[test]
    fn gemini_tools_converted_flat_format() {
        let body = Bytes::from_static(
            br#"{"model":"gemini-3.5-flash","messages":[{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"get_weather","description":"Get weather","parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}}]}"#,
        );
        let adapted = adapt_request(
            UpstreamFormat::Gemini,
            &"/v1/chat/completions".parse().unwrap(),
            &Method::POST,
            &body,
            "gemini-3.5-flash",
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&adapted.body).unwrap();
        let tools = v["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["name"], "get_weather");
        assert_eq!(tools[0]["description"], "Get weather");
        assert_eq!(tools[0]["parameters"]["type"], "object");
        // Should NOT have nested function_declarations
        assert!(v["tools"][0]["function_declarations"].is_null());
    }

    #[test]
    fn gemini_tools_strip_schema_and_additional_properties() {
        let body = Bytes::from_static(
            br#"{"model":"gemini-3.5-flash","messages":[{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"test","parameters":{"$schema":"http://json-schema.org/draft-07/schema#","type":"object","properties":{"x":{"type":"number"}},"additionalProperties":false}}}]}"#,
        );
        let adapted = adapt_request(
            UpstreamFormat::Gemini,
            &"/v1/chat/completions".parse().unwrap(),
            &Method::POST,
            &body,
            "gemini-3.5-flash",
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&adapted.body).unwrap();
        let params = &v["tools"][0]["parameters"];
        assert!(params.get("$schema").is_none());
        assert!(params.get("additionalProperties").is_none());
        assert_eq!(params["type"], "object");
    }

    #[test]
    fn gemini_tool_choice_mapping() {
        // Test "auto"
        let body = Bytes::from_static(
            br#"{"model":"gemini-3.5-flash","messages":[{"role":"user","content":"hi"}],"tool_choice":"auto"}"#,
        );
        let adapted = adapt_request(
            UpstreamFormat::Gemini,
            &"/v1/chat/completions".parse().unwrap(),
            &Method::POST,
            &body,
            "gemini-3.5-flash",
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&adapted.body).unwrap();
        assert_eq!(v["generation_config"]["tool_choice"], "auto");

        // Test "required" -> "any"
        let body2 = Bytes::from_static(
            br#"{"model":"gemini-3.5-flash","messages":[{"role":"user","content":"hi"}],"tool_choice":"required"}"#,
        );
        let adapted2 = adapt_request(
            UpstreamFormat::Gemini,
            &"/v1/chat/completions".parse().unwrap(),
            &Method::POST,
            &body2,
            "gemini-3.5-flash",
        )
        .unwrap();
        let v2: serde_json::Value = serde_json::from_slice(&adapted2.body).unwrap();
        assert_eq!(v2["generation_config"]["tool_choice"], "any");

        // Test "none"
        let body3 = Bytes::from_static(
            br#"{"model":"gemini-3.5-flash","messages":[{"role":"user","content":"hi"}],"tool_choice":"none"}"#,
        );
        let adapted3 = adapt_request(
            UpstreamFormat::Gemini,
            &"/v1/chat/completions".parse().unwrap(),
            &Method::POST,
            &body3,
            "gemini-3.5-flash",
        )
        .unwrap();
        let v3: serde_json::Value = serde_json::from_slice(&adapted3.body).unwrap();
        assert_eq!(v3["generation_config"]["tool_choice"], "none");
    }

    #[test]
    fn gemini_tool_message_becomes_function_result() {
        let body = Bytes::from_static(
            br#"{"model":"gemini-3.5-flash","messages":[{"role":"user","content":"hi"},{"role":"assistant","tool_calls":[{"id":"call_123","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Paris\"}"}}]},{"role":"tool","tool_call_id":"call_123","name":"get_weather","content":"{\"temp\": 22}"}]}"#,
        );
        let adapted = adapt_request(
            UpstreamFormat::Gemini,
            &"/v1/chat/completions".parse().unwrap(),
            &Method::POST,
            &body,
            "gemini-3.5-flash",
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&adapted.body).unwrap();
        let input = v["input"].as_array().unwrap();
        // function_call and function_result are rejected by v1beta Interactions API;
        // tool result emitted as user_input with [Tool name result] prefix.
        assert_eq!(input.len(), 2, "user_input → user_input (tool result)");
        assert_eq!(input[0]["type"], "user_input");
        assert_eq!(input[1]["type"], "user_input");
        let tool_text = input[1]["content"][0]["text"].as_str().unwrap();
        assert!(tool_text.contains("[Tool get_weather result]"), "tool name prefix expected, got: {}", tool_text);
        assert!(tool_text.contains("{\"temp\": 22}"), "result text expected in: {}", tool_text);
    }

    #[test]
    fn gemini_assistant_tool_calls_become_function_calls() {
        let body = Bytes::from_static(
            br#"{"model":"gemini-3.5-flash","messages":[{"role":"user","content":"weather?"},{"role":"assistant","tool_calls":[{"id":"call_abc","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"London\"}"}}]}]}"#,
        );
        let adapted = adapt_request(
            UpstreamFormat::Gemini,
            &"/v1/chat/completions".parse().unwrap(),
            &Method::POST,
            &body,
            "gemini-3.5-flash",
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&adapted.body).unwrap();
        let input = v["input"].as_array().unwrap();
        // function_call is rejected by v1beta Interactions API; assistant with tool_calls
        // only records call_name_map, emits no step.
        assert_eq!(input.len(), 1, "only user_input remains");
        assert_eq!(input[0]["type"], "user_input");
    }

    /// Assistant with tool_calls must NOT emit model_output — even if content is non-empty.
    /// This is a function-calling turn; the preamble text should be dropped alongside function_call.
    #[test]
    fn gemini_assistant_with_tool_calls_no_model_output() {
        let body = Bytes::from_static(
            br#"{"model":"gemini-3.5-flash","messages":[{"role":"user","content":"fetch ip.sb"},{"role":"assistant","content":"","tool_calls":[{"id":"call_x","type":"function","function":{"name":"mcp__fetch__fetch","arguments":"{}"}}]},{"role":"tool","tool_call_id":"call_x","content":"{\"ip\":\"1.2.3.4\"}"}]}"#,
        );
        let adapted = adapt_request(
            UpstreamFormat::Gemini,
            &"/v1/chat/completions".parse().unwrap(),
            &Method::POST,
            &body,
            "gemini-3.5-flash",
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&adapted.body).unwrap();
        let input = v["input"].as_array().unwrap();
        // user_input → user_input (tool result as text), no model_output or function_call/function_result
        assert_eq!(input.len(), 2, "user_input → user_input (tool result)");
        assert_eq!(input[0]["type"], "user_input");
        assert_eq!(input[1]["type"], "user_input");
        let tool_text = input[1]["content"][0]["text"].as_str().unwrap();
        assert!(tool_text.contains("[Tool mcp__fetch__fetch result]"), "name inherited from call_name_map, got: {}", tool_text);
        assert!(tool_text.contains("1.2.3.4"), "result text expected in: {}", tool_text);
    }

    #[test]
    fn gemini_extra_headers_and_auth() {
        let body = Bytes::from_static(
            br#"{"model":"gemini-3.5-flash","messages":[{"role":"user","content":"hi"}]}"#,
        );
        let adapted = adapt_request(
            UpstreamFormat::Gemini,
            &"/v1/chat/completions".parse().unwrap(),
            &Method::POST,
            &body,
            "gemini-3.5-flash",
        )
        .unwrap();
        // Check extra headers contain Api-Revision
        assert_eq!(adapted.extra_headers.len(), 1);
        assert_eq!(adapted.extra_headers[0].0.as_str(), "api-revision");
        assert_eq!(adapted.extra_headers[0].1.to_str().unwrap(), "2026-05-20");
        // Check auth style
        assert!(matches!(adapted.auth_style, AuthStyle::GeminiKey));
    }

    /// Real-world MCP scenario: double-encoded function_result text,
    /// tool with description:null, $schema, additionalProperties:false.
    #[test]
    fn gemini_mcp_double_encoded_function_result() {
        // The tool result content is a JSON-stringified OpenAI content array (MCP double-encoding)
        let body = Bytes::from_static(
            br#"{"model":"gemini-3.1-flash-lite","messages":[{"role":"user","content":"fetch baidu"},{"role":"assistant","tool_calls":[{"id":"PHAqNGxV","type":"function","function":{"name":"mcp__fetch__fetch","arguments":"{\"url\":\"https://www.baidu.com\"}"}}]},{"role":"tool","tool_call_id":"PHAqNGxV","name":"mcp__fetch__fetch","content":"[{\"type\":\"text\",\"text\":\"Baidu homepage content here\"}]"}],"tools":[{"type":"function","function":{"name":"mcp__fetch__fetch","description":null,"parameters":{"type":"object","properties":{"url":{"type":"string"},"max_length":{"type":"number","default":5000},"raw":{"type":"boolean","default":false}},"required":["url"],"additionalProperties":false,"$schema":"http://json-schema.org/draft-07/schema#"}}}]}"#,
        );
        let adapted = adapt_request(
            UpstreamFormat::Gemini,
            &"/v1/chat/completions".parse().unwrap(),
            &Method::POST,
            &body,
            "gemini-3.1-flash-lite",
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&adapted.body).unwrap();

        // Step verification
        let input = v["input"].as_array().unwrap();
        // function_call/function_result rejected by v1beta; tool result as user_input
        assert_eq!(input.len(), 2, "user_input → user_input (tool result)");
        assert_eq!(input[0]["type"], "user_input");
        assert_eq!(input[1]["type"], "user_input");
        let tool_text = input[1]["content"][0]["text"].as_str().unwrap();
        assert!(tool_text.contains("[Tool mcp__fetch__fetch result]"), "name prefix expected, got: {}", tool_text);
        assert!(tool_text.contains("Baidu homepage content here"), "unwrapped text expected in: {}", tool_text);

        // Tools: should NOT have description:null, $schema, or additionalProperties
        let tools = v["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["name"], "mcp__fetch__fetch");
        assert!(tools[0].get("description").is_none(), "description:null must be stripped");
        let params = &tools[0]["parameters"];
        assert!(params.get("$schema").is_none(), "$schema must be stripped");
        assert!(params.get("additionalProperties").is_none(), "additionalProperties must be stripped");
        assert_eq!(params["properties"]["url"]["type"], "string");
    }

}
