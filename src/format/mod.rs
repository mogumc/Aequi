mod request;
mod response;

use crate::config::UpstreamFormat;
use bytes::Bytes;
use hyper::{Body, Method, Response};
use request::{adapt_request_inner, adapt_request_inner_gemini};
use response::adapt_response_inner;

pub enum AuthStyle {
    OpenAiBearer,
    AnthropicKey,
    None,
}

pub struct AdaptedRequest {
    pub method: Method,
    pub path_and_query: http::uri::PathAndQuery,
    pub body: Bytes,
    pub auth_style: AuthStyle,
}

pub fn adapt_request(
    format: UpstreamFormat,
    original_pq: &http::uri::PathAndQuery,
    method: &Method,
    body: &Bytes,
    model: &str,
    key: &str,
) -> Result<AdaptedRequest, Response<Body>> {
    match format {
        UpstreamFormat::Openai => Ok(AdaptedRequest {
            method: method.clone(),
            path_and_query: original_pq.clone(),
            body: body.clone(),
            auth_style: AuthStyle::OpenAiBearer,
        }),
        UpstreamFormat::Anthropic => adapt_request_inner(original_pq, body),
        UpstreamFormat::Gemini => {
            adapt_request_inner_gemini(original_pq, body, model, key)
        }
    }
}

pub async fn adapt_response(
    format: UpstreamFormat,
    up_resp: Response<Body>,
    stream_request: bool,
    model: Option<String>,
) -> (Response<Body>, Option<String>) {
    adapt_response_inner(format, up_resp, stream_request, model).await
}
