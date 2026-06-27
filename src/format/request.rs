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


/// Parse a data URI "data:<mime>;base64,<data>" → (mime, data).
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

/// Convert a single OpenAI content part to Gemini Interactions API content items.
/// Returns a Vec because a single part may produce multiple items (text + binary).
fn openai_part_to_gemini(part: &serde_json::Value) -> Vec<serde_json::Value> {
    let part_type = part.get("type").and_then(|t| t.as_str()).unwrap_or("");

    // Text part
    if part_type.is_empty() || part_type == "text" {
        if let Some(text) = part.get("text").and_then(|t| t.as_str())
            .or_else(|| part.get("content").and_then(|t| t.as_str()))
        {
            if !text.is_empty() {
                return vec![serde_json::json!({"type": "text", "text": text})];
            }
        }
        return vec![];
    }

    // Binary attachment (image_url / input_audio / file)
    if let Some(att) = extract_binary_attachment(part, "gemini") {
        let gemini_type = gemini_inline_type(&att.mime, part_type);
        return vec![serde_json::json!({
            "type": gemini_type,
            "data": att.data,
            "mime_type": att.mime,
        })];
    }

    vec![]
}

/// Map MIME type (and OpenAI part_type fallback) to Gemini content type name.
fn gemini_inline_type(mime: &str, part_type: &str) -> &'static str {
    if mime.starts_with("image/") {
        "image"
    } else if mime.starts_with("audio/") {
        "audio"
    } else if mime.starts_with("video/") {
        "video"
    } else if part_type == "image_url" {
        "image"
    } else if part_type == "input_audio" {
        "audio"
    } else {
        "document"
    }
}

/// Convert OpenAI content field (string or array of parts) to Gemini content items.
/// Handles multimodal content: text, images, audio, files → inline data.
fn content_to_gemini_items(content: &serde_json::Value) -> Vec<serde_json::Value> {
    match content {
        serde_json::Value::String(s) => {
            if s.is_empty() {
                vec![]
            } else {
                vec![serde_json::json!({"type": "text", "text": s})]
            }
        }
        serde_json::Value::Array(parts) => {
            parts.iter().flat_map(openai_part_to_gemini).collect()
        }
        _ => vec![],
    }
}

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
    if let Some(input_messages) = v.get("messages").and_then(|m| m.as_array()) {
        for msg in input_messages {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            let content = msg
                .get("content")
                .cloned()
                .unwrap_or(serde_json::Value::Null);

            // Tool result → function_result step.
            if role == "tool" {
                let tool_call_id = msg.get("tool_call_id").and_then(|id| id.as_str()).unwrap_or("");
                let name = msg.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let result_text = content_to_text(&content);
                let result_value: serde_json::Value = serde_json::from_str(&result_text)
                    .unwrap_or_else(|_| serde_json::Value::String(result_text));
                input.push(serde_json::json!({
                    "type": "function_result",
                    "call_id": tool_call_id,
                    "name": name,
                    "result": result_value,
                }));
                continue;
            }

            if role == "system" {
                let text = content_to_text(&content);
                if !text.is_empty() {
                    system_parts.push(text);
                }
                continue;
            }

            // For assistant messages, handle both text content and tool_calls.
            if role == "assistant" {
                if let Some(tool_calls) = msg.get("tool_calls").and_then(|tc| tc.as_array()) {
                    // Emit text content as model_output if present.
                    let items = content_to_gemini_items(&content);
                    if !items.is_empty() {
                        input.push(serde_json::json!({
                            "type": "model_output",
                            "content": items,
                        }));
                    }
                    // Emit each tool call as a function_call step.
                    for tc in tool_calls {
                        let id = tc.get("id").and_then(|id| id.as_str()).unwrap_or("");
                        let function = tc.get("function");
                        let name = function.and_then(|f| f.get("name")).and_then(|n| n.as_str()).unwrap_or("");
                        let args_str = function.and_then(|f| f.get("arguments")).and_then(|a| a.as_str()).unwrap_or("{}");
                        let args: serde_json::Value = serde_json::from_str(args_str)
                            .unwrap_or_else(|_| serde_json::json!({}));
                        input.push(serde_json::json!({
                            "type": "function_call",
                            "id": id,
                            "name": name,
                            "arguments": args,
                        }));
                    }
                    continue;
                }
            }

            // Regular user/assistant message — convert content to Gemini items.
            let items = content_to_gemini_items(&content);
            if items.is_empty() {
                continue;
            }
            let step_type = if role == "assistant" { "model_output" } else { "user_input" };
            input.push(serde_json::json!({
                "type": step_type,
                "content": items,
            }));
        }
    }

    // Build Interactions API request body.
    let mut out = serde_json::json!({
        "model": model,
        "input": input,
        "store": false,
    });
    if !system_parts.is_empty() {
        out["system_instruction"] = serde_json::Value::String(system_parts.join("\n\n"));
    }
    if stream {
        out["stream"] = serde_json::Value::Bool(true);
    }

    // Convert OpenAI tools → Gemini function_declarations.
    if let Some(tools) = v.get("tools").and_then(|t| t.as_array()) {
        let declarations: Vec<serde_json::Value> = tools
            .iter()
            .filter_map(|t| t.get("function"))
            .map(|f| {
                serde_json::json!({
                    "name": f.get("name"),
                    "description": f.get("description"),
                    "parameters": f.get("parameters").cloned().unwrap_or(serde_json::json!({"type":"object","properties":{}})),
                })
            })
            .collect();
        if !declarations.is_empty() {
            out["tools"] = serde_json::json!([{
                "function_declarations": declarations,
            }]);
        }
    }

    // Convert OpenAI tool_choice → Gemini tool_config.
    if let Some(tc) = v.get("tool_choice") {
        let mode = match tc.as_str() {
            Some("auto") => "AUTO",
            Some("required") | Some("any") => "ANY",
            Some("none") => "NONE",
            _ if tc.is_object() => "ANY",
            _ => return Err(format_error(
                http::StatusCode::BAD_REQUEST,
                "invalid tool_choice for gemini format",
                "invalid_tool_choice",
            )),
        };
        out["tool_config"] = serde_json::json!({
            "function_calling_config": {"mode": mode}
        });
    }

    // generation_config with snake_case field names (Interactions API).
    let mut generation = serde_json::Map::new();
    if let Some(n) = v
        .get("max_tokens")
        .or_else(|| v.get("max_completion_tokens"))
        .and_then(|n| n.as_u64())
    {
        generation.insert("max_output_tokens".to_string(), serde_json::json!(n));
    }
    if let Some(n) = v.get("temperature").and_then(|n| n.as_f64()) {
        generation.insert("temperature".to_string(), serde_json::json!(n));
    }
    if let Some(n) = v.get("top_p").and_then(|n| n.as_f64()) {
        generation.insert("top_p".to_string(), serde_json::json!(n));
    }
    if let Some(stop) = v.get("stop") {
        let stops = match stop {
            serde_json::Value::Array(a) => a.clone(),
            serde_json::Value::String(_) => vec![stop.clone()],
            _ => Vec::new(),
        };
        if !stops.is_empty() {
            generation.insert("stop_sequences".to_string(), serde_json::Value::Array(stops));
        }
    }
    if !generation.is_empty() {
        out["generation_config"] = serde_json::Value::Object(generation);
    }

    let path_and_query = http::uri::PathAndQuery::from_static("/v1beta/interactions");

    // Api-Revision header to opt into the latest Interactions API schema.
    let extra_headers = vec![(
        hyper::header::HeaderName::from_static("api-revision"),
        hyper::header::HeaderValue::from_static("2026-05-20"),
    )];

    Ok(AdaptedRequest {
        method: Method::POST,
        path_and_query,
        body: Bytes::from(out.to_string()),
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
    fn gemini_request_uses_interactions_api() {
        let body = Bytes::from_static(
            br#"{"model":"gemini-3.5-flash","messages":[{"role":"system","content":"you are helpful"},{"role":"user","content":"hi"}],"stream":false}"#,
        );
        let adapted = adapt_request(
            UpstreamFormat::Gemini,
            &"/v1/chat/completions".parse().unwrap(),
            &Method::POST,
            &body,
            "gemini-3.5-flash",
        )
        .unwrap();
        // Path should be /v1beta/interactions with no key in query string
        assert_eq!(adapted.path_and_query.as_str(), "/v1beta/interactions");
        // No alt=sse for Interactions API
        assert!(!adapted.path_and_query.as_str().contains("key="));
        let v: serde_json::Value = serde_json::from_slice(&adapted.body).unwrap();
        assert_eq!(v["model"], "gemini-3.5-flash");
        // Input uses Step objects: {type: "user_input", content: [{type: "text", text: "..."}]}
        assert_eq!(v["input"][0]["type"], "user_input");
        assert_eq!(v["input"][0]["content"][0]["type"], "text");
        assert_eq!(v["input"][0]["content"][0]["text"], "hi");
        assert!(v["input"][0].get("role").is_none(), "Step objects must not have 'role' field");
        assert!(v["input"][0].get("parts").is_none(), "Interactions API must not use 'parts' field");
        assert_eq!(v["system_instruction"], "you are helpful");
        assert_eq!(v["store"], false);
        // Api-Revision header
        assert!(adapted.extra_headers.iter().any(|(n, v)| n.as_str() == "api-revision" && v == "2026-05-20"));
        // Auth via GeminiKey
        assert!(matches!(adapted.auth_style, AuthStyle::GeminiKey));
    }

    /// Multi-turn: assistant messages become "model_output" steps.
    #[test]
    fn gemini_multiturn_uses_step_objects() {
        let body = Bytes::from_static(
            br#"{"model":"gemini-3.5-flash","messages":[{"role":"user","content":"hello"},{"role":"assistant","content":"hi"},{"role":"user","content":"again"}],"stream":false}"#,
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
        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["type"], "user_input");
        assert_eq!(input[0]["content"][0]["text"], "hello");
        assert_eq!(input[1]["type"], "model_output");
        assert_eq!(input[1]["content"][0]["text"], "hi");
        assert_eq!(input[2]["type"], "user_input");
        assert_eq!(input[2]["content"][0]["text"], "again");
    }

    /// Gemini: image_url with data URI → inline image content.
    #[test]
    fn gemini_multimodal_image() {
        let body = Bytes::from_static(
            br#"{"model":"gemini-3.5-flash","messages":[{"role":"user","content":[{"type":"text","text":"What is this?"},{"type":"image_url","image_url":{"url":"data:image/png;base64,iVBOR"}}]}],"stream":false}"#,
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
        // Text part preserved
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "What is this?");
        // Image part converted to Gemini inline format
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["mime_type"], "image/png");
        assert_eq!(content[1]["data"], "iVBOR");
    }

    /// Gemini: pure image (no text) should NOT be skipped.
    #[test]
    fn gemini_multimodal_image_only_not_skipped() {
        let body = Bytes::from_static(
            br#"{"model":"gemini-3.5-flash","messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"data:image/jpeg;base64,/9j/4AAQ"}}]}],"stream":false}"#,
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
        assert_eq!(input.len(), 1, "pure image message must not be skipped");
        assert_eq!(input[0]["type"], "user_input");
        let content = input[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "image");
        assert_eq!(content[0]["mime_type"], "image/jpeg");
    }

    /// Gemini: input_audio → inline audio content.
    #[test]
    fn gemini_multimodal_audio() {
        let body = Bytes::from_static(
            br#"{"model":"gemini-3.5-flash","messages":[{"role":"user","content":[{"type":"input_audio","input_audio":{"data":"SUQz","format":"wav"}}]}],"stream":false}"#,
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
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "audio");
        assert_eq!(content[0]["mime_type"], "audio/wav");
        assert_eq!(content[0]["data"], "SUQz");
    }

    /// Gemini: file with file_data → inline document content.
    #[test]
    fn gemini_multimodal_file() {
        let body = Bytes::from_static(
            br#"{"model":"gemini-3.5-flash","messages":[{"role":"user","content":[{"type":"text","text":"Summarize"},{"type":"file","file":{"file_data":"JVBERi0=","filename":"report.pdf"}}]}],"stream":false}"#,
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
        assert_eq!(content[0]["text"], "Summarize");
        assert_eq!(content[1]["type"], "document");
        assert_eq!(content[1]["mime_type"], "application/pdf");
        assert_eq!(content[1]["data"], "JVBERi0=");
    }

    /// Gemini: OpenAI tools → Gemini function_declarations.
    #[test]
    fn gemini_tools_conversion() {
        let body = Bytes::from_static(
            br#"{"model":"gemini-3.5-flash","messages":[{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"get_weather","description":"Get weather","parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}}],"stream":false}"#,
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
        let decls = tools[0]["function_declarations"].as_array().unwrap();
        assert_eq!(decls.len(), 1);
        assert_eq!(decls[0]["name"], "get_weather");
        assert_eq!(decls[0]["description"], "Get weather");
        assert_eq!(decls[0]["parameters"]["required"][0], "city");
    }

    /// Gemini: tool_choice auto/required/none → tool_config mode.
    #[test]
    fn gemini_tool_choice_mapping() {
        for (choice, expected_mode) in [("auto", "AUTO"), ("required", "ANY"), ("none", "NONE")] {
            let body_str = format!(
                r#"{{"model":"gemini-3.5-flash","messages":[{{"role":"user","content":"hi"}}],"tool_choice":"{}","stream":false}}"#,
                choice
            );
            let adapted = adapt_request(
                UpstreamFormat::Gemini,
                &"/v1/chat/completions".parse().unwrap(),
                &Method::POST,
                &Bytes::from(body_str),
                "gemini-3.5-flash",
            )
            .unwrap();
            let v: serde_json::Value = serde_json::from_slice(&adapted.body).unwrap();
            assert_eq!(
                v["tool_config"]["function_calling_config"]["mode"],
                expected_mode,
                "tool_choice={} should map to {}",
                choice,
                expected_mode
            );
        }
    }

    /// Gemini: tool role message → function_result step.
    #[test]
    fn gemini_tool_message_to_function_result() {
        let body = Bytes::from_static(
            br#"{"model":"gemini-3.5-flash","messages":[{"role":"user","content":"weather?"},{"role":"tool","tool_call_id":"call_1","name":"get_weather","content":"{\"temp\":22}"}],"stream":false}"#,
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
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["type"], "user_input");
        assert_eq!(input[1]["type"], "function_result");
        assert_eq!(input[1]["call_id"], "call_1");
        assert_eq!(input[1]["name"], "get_weather");
        assert_eq!(input[1]["result"]["temp"], 22);
    }

    /// Gemini: assistant with tool_calls → function_call steps.
    #[test]
    fn gemini_assistant_tool_calls_to_function_call() {
        let body = Bytes::from_static(
            br#"{"model":"gemini-3.5-flash","messages":[{"role":"user","content":"weather?"},{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Beijing\"}"}}]}],"stream":false}"#,
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
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["type"], "user_input");
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["id"], "call_1");
        assert_eq!(input[1]["name"], "get_weather");
        assert_eq!(input[1]["arguments"]["city"], "Beijing");
    }
}
