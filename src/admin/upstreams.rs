use super::try_parse;
use crate::config::{UpstreamConfig, UpstreamFormat};
use crate::state::RouterState;
use crate::util::{now_ms, query_get};
use hyper::{Body, Method, Request, Response};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Look up upstream by id. Returns error response if not found.
pub(super) fn get_upstream(
    state: &RouterState,
    id: &str,
) -> Result<(usize, Arc<crate::state::Upstream>), Response<Body>> {
    state.upstream_by_id(id).ok_or_else(|| {
        RouterState::json_error(
            http::StatusCode::NOT_FOUND,
            "unknown upstream id",
            "not_found",
        )
    })
}

pub(crate) async fn handle_upstream_subroutes(
    req: Request<Body>,
    state: Arc<RouterState>,
    rest: &str,
) -> Response<Body> {
    // rest like "{id}" / "{id}/keys" / "{id}/models/refresh"
    let mut parts = rest.split('/');
    let upstream_id = match parts.next() {
        Some(s) if !s.is_empty() => s,
        _ => {
            return RouterState::json_error(
                http::StatusCode::BAD_REQUEST,
                "missing upstream id",
                "bad_request",
            )
        }
    };
    let sub = parts.next().unwrap_or("");

    if sub.is_empty() {
        match *req.method() {
            Method::PUT => return api_update_upstream(req, state, upstream_id).await,
            Method::DELETE => return api_delete_upstream(req, state, upstream_id).await,
            _ => return super::method_not_allowed(),
        }
    }

    if sub == "models" {
        let action = parts.next().unwrap_or("");
        if action == "refresh" {
            if *req.method() == Method::POST {
                return super::api_refresh_models(state, upstream_id).await;
            }
            return super::method_not_allowed();
        }
        return RouterState::json_error(
            http::StatusCode::NOT_FOUND,
            "not found",
            "not_found",
        );
    }

    if sub != "keys" {
        return RouterState::json_error(
            http::StatusCode::NOT_FOUND,
            "not found",
            "not_found",
        );
    }

    // Third segment selects a key sub-action: "" (CRUD), "release", "test", or "ban".
    let action = parts.next().unwrap_or("");
    match action {
        "" => match *req.method() {
            Method::POST => super::keys::api_add_keys(req, state, upstream_id).await,
            Method::PUT => super::keys::api_replace_keys(req, state, upstream_id).await,
            Method::DELETE => super::keys::api_delete_keys(req, state, upstream_id).await,
            Method::GET => super::keys::api_list_keys(state, upstream_id, req.uri()).await,
            _ => super::method_not_allowed(),
        },
        "release" => match *req.method() {
            Method::POST => super::keys::api_release_keys(req, state, upstream_id).await,
            _ => super::method_not_allowed(),
        },
        "test" => match *req.method() {
            Method::POST => super::keys::api_test_key(req, state, upstream_id).await,
            _ => super::method_not_allowed(),
        },
        "invalidate" | "ban" => match *req.method() {
            Method::POST => super::keys::api_invalidate_keys(req, state, upstream_id).await,
            _ => super::method_not_allowed(),
        },
        "export" => match *req.method() {
            Method::GET => super::keys::api_export_keys(state, upstream_id).await,
            _ => super::method_not_allowed(),
        },
        _ => RouterState::json_error(
            http::StatusCode::NOT_FOUND,
            "not found",
            "not_found",
        ),
    }
}

#[derive(Deserialize)]
struct UpstreamBody {
    #[serde(default)]
    id: Option<String>,
    base_url: String,
    weight: Option<usize>,
    max_concurrent_per_key: Option<u32>,
    format: Option<UpstreamFormat>,
    proxy: Option<String>,
    #[serde(default)]
    model_map: Option<std::collections::HashMap<String, String>>,
    #[serde(default)]
    min_key_level: Option<i32>,
    #[serde(default)]
    custom_headers: Option<std::collections::HashMap<String, Option<String>>>,
}

impl UpstreamBody {
    /// Validate common fields and build an UpstreamConfig.
    /// Returns a structured error Response on validation failure.
    fn into_upstream_config(
        self,
        id: String,
    ) -> Result<UpstreamConfig, Response<Body>> {
        if self.base_url.trim().is_empty() {
            return Err(RouterState::json_error(
                http::StatusCode::BAD_REQUEST,
                "missing base_url",
                "bad_request",
            ));
        }
        if self.weight.unwrap_or(1) > 10_000 {
            return Err(RouterState::json_error(
                http::StatusCode::BAD_REQUEST,
                "weight must be ≤ 10000",
                "bad_request",
            ));
        }
        if self.max_concurrent_per_key.unwrap_or(0) > 256 {
            return Err(RouterState::json_error(
                http::StatusCode::BAD_REQUEST,
                "max_concurrent_per_key must be ≤ 256",
                "bad_request",
            ));
        }
        let min_level = self.min_key_level.unwrap_or(0);
        if min_level < 0 && min_level != -1 {
            return Err(RouterState::json_error(
                http::StatusCode::BAD_REQUEST,
                "min_key_level must be >= 0 or -1",
                "bad_request",
            ));
        }
        Ok(UpstreamConfig {
            id,
            base_url: self.base_url.trim().to_string(),
            weight: self.weight,
            max_concurrent_per_key: self.max_concurrent_per_key,
            format: self.format,
            proxy: self.proxy.filter(|p| !p.trim().is_empty()),
            model_map: self.model_map.unwrap_or_default(),
            min_key_level: min_level,
            custom_headers: self.custom_headers.unwrap_or_default(),
        })
    }
}

pub(crate) async fn api_add_upstream(req: Request<Body>, state: Arc<RouterState>) -> Response<Body> {
    let input: UpstreamBody = try_parse!(super::parse_json_body(req).await);
    if input.id.as_deref().unwrap_or("").trim().is_empty() {
        return RouterState::json_error(http::StatusCode::BAD_REQUEST, "missing id", "bad_request");
    }
    let id = input.id.as_deref().unwrap_or("").trim().to_string();
    let cfg = match input.into_upstream_config(id) {
        Ok(cfg) => cfg,
        Err(resp) => return resp,
    };
    let state2 = state.clone();
    let res = tokio::task::spawn_blocking(move || state2.add_upstream(cfg)).await;
    match crate::util::spawn_result(res, http::StatusCode::BAD_REQUEST) {
        Ok(_) => super::json_ok(&serde_json::json!({
            "ok": true,
            "upstreams": state.snapshot.load_full().upstreams.len(),
        })),
        Err((status, msg)) => RouterState::json_error(status, &msg, if status.as_u16() >= 500 { "internal_error" } else { "bad_request" }),
    }
}

async fn api_update_upstream(
    req: Request<Body>,
    state: Arc<RouterState>,
    upstream_id: &str,
) -> Response<Body> {
    let input: UpstreamBody = try_parse!(super::parse_json_body(req).await);
    let id = upstream_id.to_string();
    let cfg = match input.into_upstream_config(id.clone()) {
        Ok(cfg) => cfg,
        Err(resp) => return resp,
    };
    let state2 = state.clone();
    let res = tokio::task::spawn_blocking(move || state2.update_upstream(&id, cfg)).await;
    match crate::util::spawn_result(res, http::StatusCode::BAD_REQUEST) {
        Ok(_) => super::json_ok(&serde_json::json!({"ok": true})),
        Err((status, msg)) => RouterState::json_error(status, &msg, if status.as_u16() >= 500 { "internal_error" } else { "bad_request" }),
    }
}

async fn api_delete_upstream(
    req: Request<Body>,
    state: Arc<RouterState>,
    upstream_id: &str,
) -> Response<Body> {
    let delete_keys = query_get(req.uri(), "delete_keys")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let state2 = state.clone();
    let id = upstream_id.to_string();
    let res = tokio::task::spawn_blocking(move || state2.delete_upstream(&id, delete_keys)).await;
    match crate::util::spawn_result(res, http::StatusCode::BAD_REQUEST) {
        Ok(_) => super::json_ok(&serde_json::json!({"ok": true, "delete_keys": delete_keys})),
        Err((status, msg)) => RouterState::json_error(status, &msg, if status.as_u16() >= 500 { "internal_error" } else { "bad_request" }),
    }
}

#[derive(Serialize)]
pub(crate) struct UpstreamInfo {
    id: String,
    base_url: String,
    format: String,
    proxy: Option<String>,
    weight: usize,
    max_concurrent_per_key: u32,
    min_key_level: i32,
    custom_headers: std::collections::HashMap<String, Option<String>>,
    model_map: std::collections::HashMap<String, String>,
    keys_total: usize,
    keys_active: usize,
    keys_invalid: usize,
    keys_cooldown: usize,

    selected_total: u64,

    responses_2xx: u64,
    responses_3xx: u64,
    responses_4xx: u64,
    responses_5xx: u64,
}

pub(crate) fn build_upstream_info(u: &crate::state::Upstream, global_max: u32) -> UpstreamInfo {
    let keys_arc = u.keys.load_full();
    let total = keys_arc.len();
    let invalid = keys_arc.iter().filter(|k| !k.is_active()).count();
    let now = now_ms();
    let cooldown = keys_arc
        .iter()
        .filter(|k| {
            let until = k.cooldown_until_ms.load(std::sync::atomic::Ordering::Relaxed);
            k.is_active() && until > 0 && now < until
        })
        .count();
    let effective_max = if u.max_concurrent_per_key > 0 {
        u.max_concurrent_per_key
    } else {
        global_max
    };
    UpstreamInfo {
        id: u.id.to_string(),
        base_url: u.base_url.to_string(),
        format: u.format.as_str().to_string(),
        proxy: u.proxy.clone(),
        weight: u.weight,
        max_concurrent_per_key: effective_max,
        min_key_level: u.min_key_level,
        custom_headers: u.custom_headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        model_map: u.model_map.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        keys_total: total,
        keys_active: total.saturating_sub(invalid),
        keys_invalid: invalid,
        keys_cooldown: cooldown,
        selected_total: u
            .stats
            .selected_total
            .load(std::sync::atomic::Ordering::Relaxed),
        responses_2xx: u
            .stats
            .responses_2xx
            .load(std::sync::atomic::Ordering::Relaxed),
        responses_3xx: u
            .stats
            .responses_3xx
            .load(std::sync::atomic::Ordering::Relaxed),
        responses_4xx: u
            .stats
            .responses_4xx
            .load(std::sync::atomic::Ordering::Relaxed),
        responses_5xx: u
            .stats
            .responses_5xx
            .load(std::sync::atomic::Ordering::Relaxed),
    }
}

pub(crate) async fn api_list_upstreams(state: Arc<RouterState>) -> Response<Body> {
    let snap = state.snapshot.load_full();
    let global_max = state.key_config().max_concurrent_per_key;

    let ups: Vec<UpstreamInfo> = snap
        .upstreams
        .iter()
        .map(|u| build_upstream_info(u, global_max))
        .collect();
    super::json_ok(&ups)
}
