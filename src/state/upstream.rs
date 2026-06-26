use super::*;

pub struct Upstream {
    pub id: Arc<str>,

    pub base_url: Arc<str>,
    pub base_scheme: Scheme,
    pub base_authority: Authority,
    pub base_path: Arc<str>,

    pub weight: usize,
    pub max_concurrent_per_key: u32,
    pub min_key_level: i32,
    pub format: UpstreamFormat,
    pub proxy: Option<String>,
    pub client: UpstreamClient,

    pub keys: ArcSwap<Vec<Arc<KeyState>>>,
    pub active_keys: ArcSwap<Vec<Arc<KeyState>>>,
    pub keys_update_lock: Mutex<()>,
    pub key_rr: AtomicUsize,
    pub models: ArcSwap<AHashSet<String>>,
    /// Incoming model name → upstream model name.
    pub model_map: ahash::AHashMap<String, String>,
    /// Reverse: upstream model name → incoming model name (for /v1/models listing).
    pub model_rmap: ahash::AHashMap<String, String>,
    /// Custom header overrides: header_name → value (Some = set/replace, None = delete).
    pub custom_headers: ahash::AHashMap<String, Option<String>>,

    pub stats: UpstreamStats,
}

/// Key status: active (in the rotation pool) or invalid (blacklisted).
pub const KEY_STATUS_ACTIVE: u8 = 0;
pub const KEY_STATUS_INVALID: u8 = 1;

pub struct KeyState {
    pub key: Arc<str>,
    pub auth_header: hyper::header::HeaderValue,
    pub failure_count: AtomicU32,
    pub status: AtomicU8,
    pub active_requests: AtomicU32,
    /// Unix ms timestamp when the 429 cooldown expires. 0 = not in cooldown.
    pub cooldown_until_ms: AtomicU64,
    pub latencies_ms: Mutex<VecDeque<u64>>,
}

impl RouterState {
    /// Handle key-level side effects of an upstream response status.
    /// Auth errors (401/403) count as key failures; 429 sets a short cooldown.
    /// Status counters are NOT incremented here — call `inc_upstream_status`
    /// separately once the final outcome is determined.
    #[inline]
    pub fn on_upstream_status(
        &self,
        sel: &Selected,
        status: http::StatusCode,
        retry_after_ms: Option<u64>,
    ) {
        // Only count auth errors as key failures (matches gpt-load behaviour).
        if status == http::StatusCode::UNAUTHORIZED || status == http::StatusCode::FORBIDDEN {
            self.handle_auth_failure(sel);
        } else if status == http::StatusCode::TOO_MANY_REQUESTS {
            let key_cfg = self.key_config();
            let configured = key_cfg.rate_limit_cooldown_ms;
            let cooldown_ms = retry_after_ms
                .or(if configured > 0 {
                    Some(configured)
                } else {
                    None
                })
                .unwrap_or(3000)
                .min(key_cfg.max_rate_limit_cooldown_ms.max(1));
            if cooldown_ms > 0 {
                let until = now_ms() + cooldown_ms;
                sel.key.cooldown_until_ms.store(until, Ordering::Relaxed);
            }
        }
    }

    /// Increment failure count; if threshold reached, mark key invalid and
    /// remove it from the upstream's active pool.
    fn handle_auth_failure(&self, sel: &Selected) {
        let threshold = self.key_config().blacklist_threshold;
        if threshold == 0 {
            return; // auto-blacklist disabled
        }

        let key = &sel.key;
        let new_count = key.failure_count.fetch_add(1, Ordering::Relaxed) + 1;
        if new_count >= threshold {
            key.status.store(KEY_STATUS_INVALID, Ordering::Relaxed);
            sel.upstream.rebuild_active_keys();
            tracing::info!(
                key = %key.key,
                upstream = %sel.upstream.id,
                failures = new_count,
                "key blacklisted after auth failures"
            );
        }
    }

    pub fn record_latency(&self, latency_ns: u64) {
        self.stats
            .latency_ns_total
            .fetch_add(latency_ns, Ordering::Relaxed);
        self.stats.latency_count.fetch_add(1, Ordering::Relaxed);

        // Update max with CAS loop.
        let mut cur = self.stats.latency_ns_max.load(Ordering::Relaxed);
        while latency_ns > cur {
            match self.stats.latency_ns_max.compare_exchange_weak(
                cur,
                latency_ns,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(v) => cur = v,
            }
        }
    }

    /// Helper to produce standardized JSON error responses.
    pub fn json_error(status: http::StatusCode, message: &str, code: &str) -> Response<Body> {
        crate::util::json_error(status, message, code)
    }

    /// Spawn a background task that periodically re-validates invalid keys.
    /// Invalid keys are tested against their upstream; if the upstream responds
    /// with anything other than 401/403, the key is restored to active.
    pub fn start_revalidation(self: &Arc<RouterState>) {
        let state = Arc::clone(self);

        tokio::spawn(async move {
            // Wait a bit before first run to let the system stabilise.
            tokio::time::sleep(Duration::from_secs(10)).await;

            loop {
                let key_cfg = state.key_config();
                let interval_secs = key_cfg.revalidation_interval_secs.max(60);
                let timeout_secs = key_cfg.revalidation_timeout_secs.max(5);
                tracing::debug!("revalidation: checking invalid keys...");
                let start = now_ms();

                let snap = state.snapshot.load_full();
                let mut total_checked = 0u32;
                let mut total_restored = 0u32;

                for upstream in snap.upstreams.iter() {
                    let invalid_keys: Vec<Arc<KeyState>> = upstream
                        .keys
                        .load_full()
                        .iter()
                        .filter(|k| !k.is_active())
                        .cloned()
                        .collect();

                    if invalid_keys.is_empty() {
                        continue;
                    }

                    for key in invalid_keys {
                        total_checked += 1;
                        match validate_key(upstream, &key, Duration::from_secs(timeout_secs)).await
                        {
                            Ok(true) => {
                                key.failure_count.store(0, Ordering::Relaxed);
                                key.status.store(KEY_STATUS_ACTIVE, Ordering::Relaxed);
                                upstream.rebuild_active_keys();
                                state.queue_notify.notify_waiters();
                                total_restored += 1;
                                tracing::info!(
                                    key = %key.key,
                                    upstream = %upstream.id,
                                    "revalidation: key restored"
                                );
                            }
                            Ok(false) => {
                                tracing::debug!(
                                    key = %key.key,
                                    upstream = %upstream.id,
                                    "revalidation: key still invalid"
                                );
                            }
                            Err(e) => {
                                tracing::debug!(
                                    key = %key.key,
                                    upstream = %upstream.id,
                                    error = %e,
                                    "revalidation: request failed"
                                );
                            }
                        }
                    }
                }

                let elapsed_ms = now_ms().saturating_sub(start);
                if total_checked > 0 {
                    tracing::info!(
                        checked = total_checked,
                        restored = total_restored,
                        elapsed_ms,
                        "revalidation cycle complete"
                    );
                }

                tokio::time::sleep(Duration::from_secs(interval_secs)).await;
            }
        });
    }
}

/// Test a single key by calling the upstream's /v1/models endpoint.
/// Returns Ok(true) if the key is valid (not 401/403), Ok(false) if invalid.
pub async fn validate_key(
    upstream: &Upstream,
    key: &KeyState,
    timeout: Duration,
) -> anyhow::Result<bool> {
    use hyper::body::HttpBody;

    let pq: http::uri::PathAndQuery = match upstream.format {
        UpstreamFormat::Openai | UpstreamFormat::Anthropic => {
            http::uri::PathAndQuery::from_static("/v1/models")
        }
        UpstreamFormat::Gemini => {
            let path = format!("/v1beta/models?key={}", url_encode(key.key.as_ref()));
            path.parse()?
        }
    };
    let uri = upstream.build_uri(&pq)?;
    let mut req = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .body(Body::empty())?;
    insert_key_headers(req.headers_mut(), upstream.format, key)?;

    let mut resp = match tokio::time::timeout(timeout, upstream.client.request(req)).await {
        Ok(Ok(resp)) => resp,
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => anyhow::bail!("validation request timeout"),
    };

    let status = resp.status();
    // Drain the body to allow connection reuse.
    while let Some(Ok(_)) = resp.data().await {}

    // 401/403 = key is still invalid. Anything else = key is probably fine.
    Ok(status != http::StatusCode::UNAUTHORIZED && status != http::StatusCode::FORBIDDEN)
}

impl KeyState {
    pub fn is_active(&self) -> bool {
        self.status.load(Ordering::Relaxed) == KEY_STATUS_ACTIVE
    }

    pub fn record_latency_ms(&self, latency_ms: u64) {
        let Ok(mut latencies) = self.latencies_ms.lock() else {
            return;
        };
        latencies.push_back(latency_ms);
        while latencies.len() > 256 {
            latencies.pop_front();
        }
    }

    pub fn latency_percentiles(&self) -> (Option<u64>, Option<u64>, Option<u64>) {
        let Ok(latencies) = self.latencies_ms.lock() else {
            return (None, None, None);
        };
        if latencies.is_empty() {
            return (None, None, None);
        }
        let mut values: Vec<u64> = latencies.iter().copied().collect();
        values.sort_unstable();
        (
            Some(percentile(&values, 50)),
            Some(percentile(&values, 90)),
            Some(percentile(&values, 99)),
        )
    }
}

impl Upstream {
    /// Select an active key via atomic round-robin, skipping keys at their
    /// concurrency limit and keys in 429 cooldown. Returns None if no active
    /// keys available.
    pub(super) fn select_key(&self, max_concurrent: u32, exclude_key: Option<&str>) -> Option<Arc<KeyState>> {
        let keys = self.active_keys.load_full();
        let n = keys.len();
        if n == 0 {
            return None;
        }
        let now = now_ms();
        let start = self.key_rr.fetch_add(1, Ordering::Relaxed);
        for i in 0..n {
            let idx = (start + i) % n;
            let k = &keys[idx];
            // Skip excluded key (for retries).
            if exclude_key.is_some_and(|excluded| excluded == k.key.as_ref()) {
                continue;
            }
            let until = k.cooldown_until_ms.load(Ordering::Relaxed);
            if until > 0 && now < until {
                continue;
            }
            // Clear expired cooldown (no-op if already 0).
            if until > 0 && now >= until {
                k.cooldown_until_ms.store(0, Ordering::Relaxed);
            }
            // Skip keys at concurrency limit.
            if max_concurrent > 0 && k.active_requests.load(Ordering::Relaxed) >= max_concurrent {
                continue;
            }
            return Some(k.clone());
        }
        // All keys at limit, in cooldown, or excluded.
        None
    }

    /// Rebuild the active_keys list from the full keys list based on current status.
    pub fn rebuild_active_keys(&self) {
        let all = self.keys.load_full();
        let active: Vec<Arc<KeyState>> = all.iter().filter(|k| k.is_active()).cloned().collect();
        self.active_keys.store(Arc::new(active));
    }

    /// Generic key mutation: iterate all keys, apply `action` to those matching `predicate`,
    /// rebuild active list if any were mutated. Returns count mutated.
    fn mutate_keys(
        &self,
        predicate: impl Fn(&KeyState) -> bool,
        action: impl Fn(&KeyState),
    ) -> usize {
        let all = self.keys.load_full();
        let mut n = 0;
        for k in all.iter() {
            if predicate(k) {
                action(k);
                n += 1;
            }
        }
        if n > 0 {
            self.rebuild_active_keys();
        }
        n
    }

    /// Restore specified keys to active. Returns count restored.
    pub fn restore_keys(&self, set: &AHashSet<String>) -> usize {
        self.mutate_keys(
            |k| set.contains(k.key.as_ref()),
            |k| {
                k.failure_count.store(0, Ordering::Relaxed);
                k.status.store(KEY_STATUS_ACTIVE, Ordering::Relaxed);
            },
        )
    }

    /// Restore all invalid keys. Returns count restored.
    pub fn restore_all_keys(&self) -> usize {
        self.mutate_keys(
            |k| !k.is_active(),
            |k| {
                k.failure_count.store(0, Ordering::Relaxed);
                k.status.store(KEY_STATUS_ACTIVE, Ordering::Relaxed);
            },
        )
    }

    /// Invalidate specified keys. Returns count invalidated.
    pub fn invalidate_keys(&self, set: &AHashSet<String>) -> usize {
        self.mutate_keys(
            |k| set.contains(k.key.as_ref()),
            |k| {
                k.status.store(KEY_STATUS_INVALID, Ordering::Relaxed);
            },
        )
    }

    /// Builds an absolute URI to upstream by combining base scheme+authority and request path/query.
    pub fn build_uri(&self, path_and_query: &http::uri::PathAndQuery) -> anyhow::Result<Uri> {
        if self.base_path.is_empty() || self.base_path.as_ref() == "/" {
            let mut parts = http::uri::Parts::default();
            parts.scheme = Some(self.base_scheme.clone());
            parts.authority = Some(self.base_authority.clone());
            parts.path_and_query = Some(path_and_query.clone());
            Ok(Uri::from_parts(parts)?)
        } else {
            let pq = path_and_query.as_str();
            let (path, query) = match pq.split_once('?') {
                Some((p, q)) => (p, Some(q)),
                None => (pq, None),
            };

            let mut joined = String::with_capacity(self.base_path.len() + path.len() + 8);
            joined.push_str(self.base_path.as_ref());
            if !joined.ends_with('/') {
                joined.push('/');
            }
            joined.push_str(path.trim_start_matches('/'));
            if let Some(q) = query {
                joined.push('?');
                joined.push_str(q);
            }

            let joined_pq: http::uri::PathAndQuery = joined.parse()?;
            let mut parts = http::uri::Parts::default();
            parts.scheme = Some(self.base_scheme.clone());
            parts.authority = Some(self.base_authority.clone());
            parts.path_and_query = Some(joined_pq);
            Ok(Uri::from_parts(parts)?)
        }
    }

    pub fn keys_len(&self) -> usize {
        self.keys.load().len()
    }
}

/// Remove hop-by-hop headers that should not be forwarded.
#[inline]
pub fn sanitize_hop_headers(headers: &mut hyper::HeaderMap) {
    headers.remove(CONNECTION);
    headers.remove(HOST);
    headers.remove("proxy-connection");
    headers.remove(PROXY_AUTHENTICATE);
    headers.remove(PROXY_AUTHORIZATION);
    headers.remove(TE);
    headers.remove(TRAILER);
    headers.remove(TRANSFER_ENCODING);
    headers.remove(UPGRADE);
    headers.remove("x-proxy-token");
    headers.remove("x-admin-token");
}

pub(super) fn parse_upstream(u: UpstreamConfig, weight: usize) -> anyhow::Result<Arc<Upstream>> {
    let name_for_err = u.id.clone();

    let base: Uri = u.base_url.parse()?;
    let format = u
        .format
        .unwrap_or_else(|| UpstreamFormat::detect(&u.base_url));
    let proxy = u.proxy.clone().filter(|p| !p.trim().is_empty());
    let client = UpstreamClient::new(proxy.as_deref())?;

    let scheme = base
        .scheme()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("upstream {}: base_url missing scheme", name_for_err))?;
    let authority = base
        .authority()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("upstream {}: base_url missing authority", name_for_err))?;

    let base_path = base.path().trim_end_matches('/').to_string();
    let base_path = if base_path == "/" {
        String::new()
    } else {
        base_path
    };

    let mut model_map = ahash::AHashMap::new();
    let mut model_rmap = ahash::AHashMap::new();
    for (k, v) in u.model_map.iter() {
        model_map.insert(k.clone(), v.clone());
        model_rmap.insert(v.clone(), k.clone());
    }

    let mut custom_headers = ahash::AHashMap::new();
    for (k, v) in u.custom_headers.iter() {
        custom_headers.insert(k.clone(), v.clone());
    }

    let upstream = Upstream {
        id: Arc::<str>::from(u.id),
        base_url: Arc::<str>::from(u.base_url.clone()),
        base_scheme: scheme,
        base_authority: authority,
        base_path: Arc::<str>::from(base_path),
        weight,
        max_concurrent_per_key: u.max_concurrent_per_key.unwrap_or(0),
        min_key_level: u.min_key_level,
        format,
        proxy,
        client,
        keys: ArcSwap::from_pointee(Vec::new()),
        active_keys: ArcSwap::from_pointee(Vec::new()),
        keys_update_lock: Mutex::new(()),
        key_rr: AtomicUsize::new(0),
        models: ArcSwap::from_pointee(AHashSet::new()),
        model_map,
        model_rmap,
        custom_headers,
        stats: UpstreamStats::default(),
    };

    Ok(Arc::new(upstream))
}

pub fn build_key_states(keys: Vec<String>) -> anyhow::Result<Arc<Vec<Arc<KeyState>>>> {
    let mut out: Vec<Arc<KeyState>> = Vec::with_capacity(keys.len());
    for k in keys {
        let k = k.trim();
        if k.is_empty() {
            continue;
        }
        if let Err(e) = crate::util::validate_key_chars(k) {
            tracing::warn!(key = %k, error = %e, "key rejected");
            continue;
        }
        let key_arc: Arc<str> = Arc::<str>::from(k.to_string());
        let auth_header = hyper::header::HeaderValue::from_str(&format!("Bearer {}", key_arc))
            .map_err(|_| anyhow::anyhow!("invalid key (cannot be used in HTTP header)"))?;
        out.push(Arc::new(KeyState {
            key: key_arc,
            auth_header,
            failure_count: AtomicU32::new(0),
            status: AtomicU8::new(KEY_STATUS_ACTIVE),
            active_requests: AtomicU32::new(0),
            cooldown_until_ms: AtomicU64::new(0),
            latencies_ms: Mutex::new(VecDeque::with_capacity(256)),
        }));
    }
    Ok(Arc::new(out))
}

pub(super) fn parse_models_response(format: UpstreamFormat, body: &[u8]) -> anyhow::Result<AHashSet<String>> {
    let v: serde_json::Value = serde_json::from_slice(body)?;
    let mut out: AHashSet<String> = AHashSet::new();
    match format {
        UpstreamFormat::Openai | UpstreamFormat::Anthropic => {
            let data = v
                .get("data")
                .and_then(|d| d.as_array())
                .ok_or_else(|| anyhow::anyhow!("missing data array in models response"))?;
            out.reserve(data.len());
            for item in data {
                if let Some(id) = item.get("id").and_then(|s| s.as_str()) {
                    out.insert(id.to_string());
                }
            }
        }
        UpstreamFormat::Gemini => {
            let data = v
                .get("models")
                .and_then(|d| d.as_array())
                .ok_or_else(|| anyhow::anyhow!("missing models array in gemini response"))?;
            out.reserve(data.len());
            for item in data {
                if let Some(name) = item.get("name").and_then(|s| s.as_str()) {
                    out.insert(name.trim_start_matches("models/").to_string());
                }
            }
        }
    }
    Ok(out)
}

pub(super) fn insert_key_headers(
    headers: &mut hyper::HeaderMap,
    format: UpstreamFormat,
    key: &KeyState,
) -> anyhow::Result<()> {
    match format {
        UpstreamFormat::Openai => {
            headers.insert(HDR_AUTHORIZATION, key.auth_header.clone());
        }
        UpstreamFormat::Anthropic => {
            let value = hyper::header::HeaderValue::from_str(key.key.as_ref())?;
            headers.insert("x-api-key", value);
            headers.insert(
                "anthropic-version",
                hyper::header::HeaderValue::from_static("2023-06-01"),
            );
        }
        UpstreamFormat::Gemini => {}
    }
    Ok(())
}

pub(super) fn percentile(sorted: &[u64], pct: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len().saturating_sub(1)) * pct) / 100;
    sorted[idx]
}

pub(super) fn url_encode(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}

pub fn validate_keys(keys: &[String]) -> anyhow::Result<()> {
    let mut valid_count = 0usize;
    for k in keys {
        let k = k.trim();
        if k.is_empty() {
            continue;
        }
        valid_count += 1;
        hyper::header::HeaderValue::from_str(&format!("Bearer {}", k))
            .map_err(|_| anyhow::anyhow!("invalid key (cannot be used in HTTP header)"))?;
    }
    if valid_count == 0 {
        anyhow::bail!("no keys provided");
    }
    Ok(())
}
