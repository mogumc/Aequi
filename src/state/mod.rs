mod upstream;
mod requests;
mod routes;

pub use upstream::*;
pub use requests::*;
pub use routes::*;

use crate::billing::BillingStore;
use crate::config::{Config, KeyConfig, ServerConfig, UpstreamConfig, UpstreamFormat};
use crate::storage::KeyStore;
use crate::upstream_client::UpstreamClient;
use crate::util::now_ms;
use ahash::{AHashMap, AHashSet};
use arc_swap::ArcSwap;
use http::uri::{Authority, Scheme};
use hyper::client::HttpConnector;
use hyper::header::{
    HeaderName, CONNECTION, HOST, PROXY_AUTHENTICATE, PROXY_AUTHORIZATION, TE, TRAILER,
    TRANSFER_ENCODING, UPGRADE,
};
use hyper::{Body, Client, Method, Request, Response, Uri};
use hyper_rustls::HttpsConnectorBuilder;
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::{broadcast, mpsc, Notify, Semaphore, SemaphorePermit};

pub const HDR_AUTHORIZATION: HeaderName = hyper::header::AUTHORIZATION;

pub struct RouterState {
    pub runtime: ArcSwap<RuntimeConfig>,

    pub request_timeout: Duration,
    pub max_retries: usize,
    pub retry_status_codes: Arc<AHashSet<u16>>,
    pub key_config: KeyConfig,

    pub admin_tokens: Arc<AHashSet<String>>,
    pub usage_inject_upstreams: Option<Arc<AHashSet<String>>>,

    pub store: Arc<KeyStore>,
    pub billing: Arc<BillingStore>,
    pub model_routes_path: PathBuf,
    pub model_costs_path: PathBuf,
    pub upstreams_path: PathBuf,
    pub requests_log_dir: PathBuf,

    pub snapshot: ArcSwap<RouterSnapshot>,
    pub sched_rr: Arc<AtomicUsize>,

    pub client: Client<hyper_rustls::HttpsConnector<HttpConnector>, Body>,

    pub stats: Arc<Stats>,
    pub requests: Arc<RequestsLog>,
    pub admin_write_lock: Arc<Mutex<()>>,
    pub shutting_down: AtomicBool,
    pub queue_notify: Notify,
    pub queue_slots: Semaphore,
    pub config_path: Option<PathBuf>,
    pub listen_addr: String,
    pub worker_threads: Option<usize>,
    pub data_dir: PathBuf,
}

#[derive(Clone)]
pub struct RuntimeConfig {
    pub request_timeout: Duration,
    pub max_retries: usize,
    pub retry_status_codes: Arc<AHashSet<u16>>,
    pub key_config: KeyConfig,
    pub admin_tokens: Arc<AHashSet<String>>,
    pub usage_inject_upstreams: Option<Arc<AHashSet<String>>>,
    pub model_costs: ahash::AHashMap<String, crate::config::ModelCost>,
    pub server: ServerConfig,
    pub preview: serde_json::Value,
}

pub struct QueueGuard<'a> {
    state: &'a RouterState,
    _permit: SemaphorePermit<'a>,
}

impl Drop for QueueGuard<'_> {
    fn drop(&mut self) {
        self.state.stats.queue_depth.fetch_sub(1, Ordering::Relaxed);
    }
}

/// RAII guard: decrements a key's `active_requests` on drop.
pub struct KeyGuard {
    key: Arc<KeyState>,
}

impl KeyGuard {
    pub fn acquire(key: Arc<KeyState>) -> Self {
        key.active_requests.fetch_add(1, Ordering::Relaxed);
        Self { key }
    }
}

impl Drop for KeyGuard {
    fn drop(&mut self) {
        self.key.active_requests.fetch_sub(1, Ordering::Relaxed);
    }
}

pub struct RouterSnapshot {
    pub upstreams: Vec<Arc<Upstream>>,
    pub upstream_index: AHashMap<String, usize>,
    /// Weighted RR schedule. Contains indices into `upstreams`.
    pub schedule: Vec<usize>,
}

impl Clone for RouterState {
    fn clone(&self) -> Self {
        RouterState {
            runtime: ArcSwap::from(self.runtime.load_full()),
            request_timeout: self.request_timeout,
            max_retries: self.max_retries,
            retry_status_codes: self.retry_status_codes.clone(),
            key_config: self.key_config.clone(),
            admin_tokens: self.admin_tokens.clone(),
            usage_inject_upstreams: self.usage_inject_upstreams.clone(),
            store: self.store.clone(),
            billing: self.billing.clone(),
            model_routes_path: self.model_routes_path.clone(),
            model_costs_path: self.model_costs_path.clone(),
            upstreams_path: self.upstreams_path.clone(),
            requests_log_dir: self.requests_log_dir.clone(),
            snapshot: ArcSwap::from(self.snapshot.load_full()),
            sched_rr: Arc::new(AtomicUsize::new(
                self.sched_rr.load(std::sync::atomic::Ordering::Relaxed),
            )),
            client: self.client.clone(),
            stats: self.stats.clone(),
            requests: self.requests.clone(),
            admin_write_lock: self.admin_write_lock.clone(),
            shutting_down: AtomicBool::new(self.shutting_down.load(Ordering::Relaxed)),
            queue_notify: Notify::new(),
            queue_slots: Semaphore::new(self.server_config().queue_max_depth),
            config_path: self.config_path.clone(),
            listen_addr: self.listen_addr.clone(),
            worker_threads: self.worker_threads,
            data_dir: self.data_dir.clone(),
        }
    }
}


#[derive(Clone)]
pub struct Selected {
    pub upstream: Arc<Upstream>,
    pub key: Arc<KeyState>,
}

/// Global stats (cheap atomics only).
pub struct Stats {
    pub started_at_ms: u64,

    pub requests_total: AtomicU64,
    pub requests_inflight: AtomicU64,

    pub upstream_selected_total: AtomicU64,

    pub responses_2xx: AtomicU64,
    pub responses_3xx: AtomicU64,
    pub responses_4xx: AtomicU64,
    pub responses_5xx: AtomicU64,

    pub prompt_tokens_total: AtomicU64,
    pub completion_tokens_total: AtomicU64,
    pub thought_tokens_total: AtomicU64,
    pub tokens_total: AtomicU64,

    pub queue_depth: AtomicU64,
    pub queue_timeout_total: AtomicU64,

    pub latency_ns_total: AtomicU64,
    pub latency_count: AtomicU64,
    pub latency_ns_max: AtomicU64,
}

pub struct UpstreamStats {
    pub selected_total: AtomicU64,
    pub responses_2xx: AtomicU64,
    pub responses_3xx: AtomicU64,
    pub responses_4xx: AtomicU64,
    pub responses_5xx: AtomicU64,
}

impl Default for UpstreamStats {
    fn default() -> Self {
        Self {
            selected_total: AtomicU64::new(0),
            responses_2xx: AtomicU64::new(0),
            responses_3xx: AtomicU64::new(0),
            responses_4xx: AtomicU64::new(0),
            responses_5xx: AtomicU64::new(0),
        }
    }
}

impl Stats {
    pub fn new() -> Self {
        let now = now_ms();
        Self {
            started_at_ms: now,
            requests_total: AtomicU64::new(0),
            requests_inflight: AtomicU64::new(0),
            upstream_selected_total: AtomicU64::new(0),
            responses_2xx: AtomicU64::new(0),
            responses_3xx: AtomicU64::new(0),
            responses_4xx: AtomicU64::new(0),
            responses_5xx: AtomicU64::new(0),
            prompt_tokens_total: AtomicU64::new(0),
            completion_tokens_total: AtomicU64::new(0),
            thought_tokens_total: AtomicU64::new(0),
            tokens_total: AtomicU64::new(0),
            queue_depth: AtomicU64::new(0),
            queue_timeout_total: AtomicU64::new(0),
            latency_ns_total: AtomicU64::new(0),
            latency_count: AtomicU64::new(0),
            latency_ns_max: AtomicU64::new(0),
        }
    }
}

impl RuntimeConfig {
    pub fn from_config(cfg: &Config) -> Self {
        let retry_status_codes = cfg
            .retry_status_codes
            .clone()
            .unwrap_or_else(|| vec![429])
            .into_iter()
            .collect::<AHashSet<u16>>();

        let mut admin_set = AHashSet::with_capacity(cfg.admin_tokens.len().max(1));
        for t in cfg.admin_tokens.iter() {
            if !t.is_empty() {
                admin_set.insert(t.clone());
            }
        }

        let usage_inject_upstreams = cfg.usage_inject_upstreams.clone().and_then(|v| {
            let mut set = AHashSet::with_capacity(v.len().max(1));
            for id in v {
                let id = id.trim();
                if !id.is_empty() {
                    set.insert(id.to_string());
                }
            }
            if set.is_empty() {
                None
            } else {
                Some(Arc::new(set))
            }
        });

        let model_costs = ahash::AHashMap::new();

        Self {
            request_timeout: Duration::from_millis(cfg.request_timeout_ms),
            max_retries: cfg.max_retries.unwrap_or(5),
            retry_status_codes: Arc::new(retry_status_codes),
            key_config: cfg.key.clone(),
            admin_tokens: Arc::new(admin_set),
            usage_inject_upstreams,
            model_costs,
            server: cfg.server.clone(),
            preview: config_preview(cfg),
        }
    }
}

impl RouterState {
    pub fn new(cfg: Config, config_path: Option<PathBuf>) -> anyhow::Result<Self> {
        let mut runtime = Arc::new(RuntimeConfig::from_config(&cfg));
        let queue_max_depth = runtime.server.queue_max_depth;
        let request_timeout = runtime.request_timeout;
        let max_retries = runtime.max_retries;
        let retry_status_codes = runtime.retry_status_codes.clone();
        let admin_tokens = runtime.admin_tokens.clone();
        let usage_inject_upstreams = runtime.usage_inject_upstreams.clone();

        // Storage
        let data_dir: PathBuf = cfg.data_dir.clone();
        let store = Arc::new(KeyStore::open(&data_dir)?);
        let billing = Arc::new(BillingStore::new(&store)?);
        let model_routes_path = data_dir.join("models_routes.json");
        let model_costs_path = data_dir.join("models_costs.json");
        let upstreams_path = data_dir.join("upstreams.json");
        let requests_log_dir = data_dir.clone();
        let requests_log_path = requests_log_dir.join("requests.jsonl");
        let log_tx = start_request_log_writer(requests_log_path.clone());
        let requests = Arc::new(RequestsLog::new(5000, log_tx));

        // Load up to 5000 recent entries from current + archive files into memory.
        {
            let mut all_entries: Vec<RequestLogEntry> = Vec::new();

            // Current file (today's entries).
            if let Ok(content) = std::fs::read_to_string(&requests_log_path) {
                let entries: Vec<RequestLogEntry> = content
                    .lines()
                    .rev()
                    .filter_map(|line| serde_json::from_str(line.trim()).ok())
                    .collect();
                all_entries.extend(entries.into_iter().rev());
            }

            // Recent archive files (newest first), until we reach the 5000 cap.
            if all_entries.len() < 5000 {
                let archives = list_request_log_archives(&requests_log_dir);
                for (_date, path) in archives.iter().rev() {
                    if all_entries.len() >= 5000 {
                        break;
                    }
                    if let Ok(content) = std::fs::read_to_string(path) {
                        let entries: Vec<RequestLogEntry> = content
                            .lines()
                            .rev()
                            .take(5000 - all_entries.len())
                            .filter_map(|line| serde_json::from_str(line.trim()).ok())
                            .collect();
                        all_entries.extend(entries.into_iter().rev());
                    }
                }
            }

            // Ignore insertion order from files — sort by timestamp so
            // the deque is truly chronological. id is not monotonic
            // across restarts (process-static counter resets to 1), so
            // it cannot serve as a recency signal.
            all_entries.sort_unstable_by_key(|e| e.ts_ms);
            let loaded = all_entries.len().min(5000);
            // Keep the most recent 5000 in chronological order; push_entry
            // appends to the back, so recent() naturally returns the latest.
            if all_entries.len() > 5000 {
                all_entries = all_entries.split_off(all_entries.len() - 5000);
            }
            requests.load_history(all_entries);
            tracing::info!(dir = %requests_log_dir.display(), loaded, "loaded historical request logs");
        }

        // Check monthly usage reset.
        if let Err(e) = store.check_monthly_reset() {
            tracing::warn!("monthly usage reset check failed: {e}");
        }
        spawn_monthly_reset_check(store.clone());

        let retention_days = runtime.server.request_log_retention_days;
        spawn_request_log_cleanup(data_dir.clone(), retention_days);

        let mut upstream_configs: Vec<UpstreamConfig> = Vec::new();
        if let Ok(list) = load_upstreams_override(&upstreams_path) {
            upstream_configs = list;
        } else if upstreams_path.exists() {
            tracing::warn!(
                path = %upstreams_path.display(),
                "failed to load upstreams file"
            );
        }

        let snapshot = build_snapshot_from_configs(&upstream_configs, &store)?;

        // HTTPS (and HTTP) connector.
        let https = HttpsConnectorBuilder::new()
            .with_native_roots()
            .https_or_http()
            .enable_http1()
            .build();

        let client = Client::builder()
            .pool_idle_timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(64)
            .build::<_, Body>(https);

        if let Ok(routes) = load_model_routes(&model_routes_path) {
            apply_loaded_routes(&routes, &snapshot.upstreams, &snapshot.upstream_index);
        } else if model_routes_path.exists() {
            tracing::warn!(
                path = %model_routes_path.display(),
                "failed to load model routes file"
            );
        }

        // Load persisted model costs (set via admin API).
        if let Ok(data) = std::fs::read_to_string(&model_costs_path) {
            if let Ok(costs) = serde_json::from_str::<std::collections::HashMap<String, crate::config::ModelCost>>(&data) {
                let mut rt = (*runtime).clone();
                rt.model_costs = costs.into_iter().collect();
                runtime = Arc::new(rt);
                tracing::info!(path = %model_costs_path.display(), "loaded persisted model costs");
            }
        } else if model_costs_path.exists() {
            tracing::warn!(path = %model_costs_path.display(), "failed to read model costs file");
        }

        let stats = Arc::new(Stats::new());

        // Restore global token counters from persistent storage (survives restart, resets monthly).
        let (p, c, t, tot) = store.load_global_tokens();
        if p > 0 || c > 0 || t > 0 || tot > 0 {
            stats.prompt_tokens_total.store(p, Ordering::Relaxed);
            stats.completion_tokens_total.store(c, Ordering::Relaxed);
            stats.thought_tokens_total.store(t, Ordering::Relaxed);
            stats.tokens_total.store(tot, Ordering::Relaxed);
            tracing::info!(prompt = p, completion = c, thought = t, total = tot, "restored global token counters");
        }

        Ok(Self {
            runtime: ArcSwap::from(runtime),
            request_timeout,
            max_retries,
            retry_status_codes,
            key_config: cfg.key.clone(),
            admin_tokens,
            usage_inject_upstreams,
            store,
            billing,
            model_routes_path,
            model_costs_path,
            upstreams_path,
            requests_log_dir,
            snapshot: ArcSwap::from(Arc::new(snapshot)),
            sched_rr: Arc::new(AtomicUsize::new(0)),
            client,
            stats,
            requests,
            admin_write_lock: Arc::new(Mutex::new(())),
            shutting_down: AtomicBool::new(false),
            queue_notify: Notify::new(),
            queue_slots: Semaphore::new(queue_max_depth),
            config_path,
            listen_addr: cfg.listen_addr.clone(),
            worker_threads: cfg.worker_threads,
            data_dir,
        })
    }

    #[inline]
    pub fn authorize_admin_header(&self, req: &Request<Body>) -> bool {
        let Some(h) = req.headers().get("x-admin-token") else {
            return false;
        };
        match h.to_str() {
            Ok(s) => self.runtime.load().admin_tokens.contains(s),
            Err(_) => false,
        }
    }

    #[inline]
    pub fn should_inject_usage(&self, upstream_id: &str) -> bool {
        self.runtime
            .load()
            .usage_inject_upstreams
            .as_ref()
            .map(|set| set.contains(upstream_id))
            .unwrap_or(false)
    }

    #[inline]
    pub fn should_retry_status(&self, status: http::StatusCode) -> bool {
        self.runtime
            .load()
            .retry_status_codes
            .contains(&status.as_u16())
    }

    pub fn retry_status_codes_sorted(&self) -> Vec<u16> {
        let rt = self.runtime.load();
        let mut v: Vec<u16> = rt.retry_status_codes.iter().copied().collect();
        v.sort_unstable();
        v
    }

    #[inline]
    pub fn record_request(&self, entry: RequestLogEntry) {
        self.requests.record(entry);
    }

    pub fn recent_requests(&self, limit: usize) -> Vec<RequestLogEntry> {
        self.requests.recent(limit)
    }

    pub fn subscribe_requests(&self) -> broadcast::Receiver<RequestLogEntry> {
        self.requests.subscribe()
    }

    pub fn metrics_snapshot(&self, window: MetricsWindow) -> Vec<MetricsBucket> {
        self.requests.metrics_snapshot(window)
    }

    pub fn server_config(&self) -> ServerConfig {
        self.runtime.load().server.clone()
    }

    pub fn request_timeout(&self) -> Duration {
        self.runtime.load().request_timeout
    }

    pub fn max_retries(&self) -> usize {
        self.runtime.load().max_retries
    }

    pub fn key_config(&self) -> KeyConfig {
        self.runtime.load().key_config.clone()
    }

    pub fn config_preview(&self) -> serde_json::Value {
        self.runtime.load().preview.clone()
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Relaxed)
    }

    pub fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::Relaxed);
        self.queue_notify.notify_waiters();
    }

    pub fn notify_capacity(&self) {
        self.queue_notify.notify_one();
    }

    pub fn queue_enter(&self) -> Option<QueueGuard<'_>> {
        let permit = self.queue_slots.try_acquire().ok()?;
        self.stats.queue_depth.fetch_add(1, Ordering::Relaxed);
        Some(QueueGuard {
            state: self,
            _permit: permit,
        })
    }

    pub fn upstream_by_id(&self, id: &str) -> Option<(usize, Arc<Upstream>)> {
        let snap = self.snapshot.load_full();
        let idx = *snap.upstream_index.get(id)?;
        Some((idx, snap.upstreams[idx].clone()))
    }

    /// Select an upstream + key that supports the given model.
    /// `billing_key_level`: the request user's billing key permission level.
    /// -1 = admin (no restriction). Returns None if no upstream+key is eligible.
    pub fn select_for_model(&self, model: &str, billing_key_level: i32) -> Option<Selected> {
        self.select_for_model_excluding(model, billing_key_level, None)
    }

    pub fn select_for_model_excluding(
        &self,
        model: &str,
        billing_key_level: i32,
        exclude: Option<(&str, &str)>,
    ) -> Option<Selected> {
        if self.is_shutting_down() {
            return None;
        }
        let snap = self.snapshot.load_full();
        let sched_len = snap.schedule.len();
        if sched_len == 0 {
            return None;
        }

        let global_max = self.key_config().max_concurrent_per_key;

        for _ in 0..sched_len {
            let rr = self.sched_rr.as_ref().fetch_add(1, Ordering::Relaxed);
            let u_idx = snap.schedule[rr % sched_len];
            let u = &snap.upstreams[u_idx];

            if !u.models.load().contains(model) && !u.model_map.contains_key(model) {
                continue;
            }

            // Billing-key level gate: user must have level >= upstream min.
            if billing_key_level != -1 && billing_key_level < u.min_key_level {
                continue;
            }

            // Per-upstream override, fallback to global default.
            let max = if u.max_concurrent_per_key > 0 {
                u.max_concurrent_per_key
            } else {
                global_max
            };

            let excluded_key = exclude
                .and_then(|(upstream_id, key)| (upstream_id == u.id.as_ref()).then_some(key));

            if let Some(k) = u.select_key(max, excluded_key) {
                self.stats
                    .upstream_selected_total
                    .fetch_add(1, Ordering::Relaxed);
                u.stats.selected_total.fetch_add(1, Ordering::Relaxed);
                return Some(Selected {
                    upstream: u.clone(),
                    key: k,
                });
            }
        }

        None
    }

    pub fn model_exists(&self, model: &str) -> bool {
        let snap = self.snapshot.load_full();
        snap.upstreams
            .iter()
            .any(|u| u.models.load().contains(model) || u.model_map.contains_key(model))
    }

    pub fn any_models_loaded(&self) -> bool {
        let snap = self.snapshot.load_full();
        snap.upstreams.iter().any(|u| !u.models.load().is_empty())
    }
}


/// Increment the appropriate status-code counter based on HTTP status class.
impl RouterState {
    /// Record the final outcome of a proxy request. Increments both global and
    /// per-upstream response counters. Timeouts and network errors should be
    /// reported here as 504 / 502 so they land in `responses_5xx`.
    #[inline]
    pub fn inc_upstream_status(
        &self,
        upstream: &crate::state::Upstream,
        status: http::StatusCode,
    ) {
        let bump = |r2: &AtomicU64, r3: &AtomicU64, r4: &AtomicU64, r5: &AtomicU64| {
            if status.is_success() {
                r2.fetch_add(1, Ordering::Relaxed);
            } else if status.is_redirection() {
                r3.fetch_add(1, Ordering::Relaxed);
            } else if status.is_client_error() {
                r4.fetch_add(1, Ordering::Relaxed);
            } else if status.is_server_error() {
                r5.fetch_add(1, Ordering::Relaxed);
            }
        };
        bump(
            &upstream.stats.responses_2xx,
            &upstream.stats.responses_3xx,
            &upstream.stats.responses_4xx,
            &upstream.stats.responses_5xx,
        );
        bump(
            &self.stats.responses_2xx,
            &self.stats.responses_3xx,
            &self.stats.responses_4xx,
            &self.stats.responses_5xx,
        );
    }
}

impl RouterState {
    /// Acquire admin write lock + upstream keys update lock in spawn_blocking, then run `f`.
    /// The upstream Arc is passed to the closure so it can mutate keys/rebuild_active_keys.
    /// Returns the nested result wrapped in tokio's JoinHandle result.
    pub async fn with_key_write_lock<T, F>(
        &self,
        upstream: &Arc<Upstream>,
        f: F,
    ) -> Result<anyhow::Result<T>, tokio::task::JoinError>
    where
        T: Send + 'static,
        F: FnOnce(Arc<Upstream>) -> anyhow::Result<T> + Send + 'static,
    {
        let admin_lock = self.admin_write_lock.clone();
        let u_lock = upstream.clone();
        let u_work = upstream.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<T> {
            let _ag = admin_lock
                .lock()
                .map_err(|_| anyhow::anyhow!("admin write lock poisoned"))?;
            let _kg = u_lock.keys_update_lock
                .lock()
                .map_err(|_| anyhow::anyhow!("key update lock poisoned"))?;
            f(u_work)
        })
        .await
    }

    /// Complete billing settlement for a request: settle if usage found, or release reservation.
    pub fn settle_billing(
        &self,
        billing_key: &str,
        usage: Option<(u64, u64, u64, u64)>, // (prompt, completion, thought, total)
        billing_model: &str,
        is_billable: bool,
        is_2xx: bool,
    ) {
        match usage {
            Some((prompt, completion, thought, total)) => {
                let model_costs = &self.runtime.load_full().model_costs;
                let bill_out = completion.saturating_add(thought);
                let cost = crate::billing::compute_credit_cost(
                    prompt, bill_out, billing_model, model_costs,
                );
                if self.billing.settle_reserved_usage(
                    billing_key, prompt, bill_out, billing_model, model_costs,
                ).is_none() {
                    tracing::error!(key = billing_key, "settle_reserved_usage failed: key not found");
                }
                let p = self.stats
                    .prompt_tokens_total
                    .fetch_add(prompt, Ordering::Relaxed) + prompt;
                let c = self.stats
                    .completion_tokens_total
                    .fetch_add(bill_out, Ordering::Relaxed) + bill_out;
                let t = self.stats
                    .thought_tokens_total
                    .fetch_add(thought, Ordering::Relaxed) + thought;
                let tot = self.stats
                    .tokens_total
                    .fetch_add(total, Ordering::Relaxed) + total;
                if let Err(e) = self.store.add_key_usage(billing_key, total, cost) {
                    tracing::error!(key = billing_key, error = %e, "add_key_usage failed");
                }
                // Persist global token counters (sled caches tree handles, flush is cheap).
                if let Err(e) = self.store.save_global_tokens(p, c, t, tot) {
                    tracing::error!(error = %e, "save_global_tokens failed");
                }
            }
            None if !is_billable || !is_2xx => {
                if self.billing.release_reservation(billing_key).is_none() {
                    tracing::error!(key = billing_key, "release_reservation failed: key not found");
                }
            }
            _ => {}
        }
    }
}

#[cfg(unix)]
impl RouterState {
    pub fn apply_config_reload(&self, cfg: Config) -> anyhow::Result<()> {
        if cfg.listen_addr != self.listen_addr {
            tracing::warn!(
                old = %self.listen_addr,
                new = %cfg.listen_addr,
                "config: listen_addr changed, restart required"
            );
        }
        if cfg.worker_threads != self.worker_threads {
            tracing::warn!(
                old = ?self.worker_threads,
                new = ?cfg.worker_threads,
                "config: worker_threads changed, restart required"
            );
        }
        if cfg.data_dir != self.data_dir {
            tracing::warn!(
                old = %self.data_dir.display(),
                new = %cfg.data_dir.display(),
                "config: data_dir changed, restart required"
            );
        }

        let old = self.runtime.load_full();
        let mut runtime = RuntimeConfig::from_config(&cfg);
        runtime.model_costs = old.model_costs.clone(); // preserve admin-set model costs
        let runtime = Arc::new(runtime);
        if runtime.server.queue_max_depth > old.server.queue_max_depth {
            self.queue_slots
                .add_permits(runtime.server.queue_max_depth - old.server.queue_max_depth);
        } else if runtime.server.queue_max_depth < old.server.queue_max_depth {
            tracing::warn!(
                old = old.server.queue_max_depth,
                new = runtime.server.queue_max_depth,
                "config: queue_max_depth decrease applies after restart"
            );
        }
        log_runtime_config_changes(&old, &runtime);
        self.runtime.store(runtime);

        let _guard = self
            .admin_write_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("admin write lock poisoned"))?;
        self.queue_notify.notify_waiters();
        Ok(())
    }
}

#[cfg(unix)]
fn log_runtime_config_changes(old: &RuntimeConfig, new: &RuntimeConfig) {
    if old.request_timeout != new.request_timeout {
        tracing::info!(
            old_ms = old.request_timeout.as_millis() as u64,
            new_ms = new.request_timeout.as_millis() as u64,
            "config reloaded: request_timeout_ms"
        );
    }
    if old.max_retries != new.max_retries {
        tracing::info!(
            old = old.max_retries,
            new = new.max_retries,
            "config reloaded: max_retries"
        );
    }
    if old.server.cors_origins != new.server.cors_origins {
        tracing::info!("config reloaded: cors_origins");
    }
    if old.server.queue_enabled != new.server.queue_enabled
        || old.server.queue_max_depth != new.server.queue_max_depth
        || old.server.queue_timeout_ms != new.server.queue_timeout_ms
    {
        tracing::info!("config reloaded: queue settings");
    }
}

fn config_preview(cfg: &Config) -> serde_json::Value {
    let mut v = serde_json::to_value(cfg).unwrap_or_else(|_| serde_json::json!({}));
    redact_config_value(&mut v);
    v
}

fn redact_config_value(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            for (k, value) in map.iter_mut() {
                if k.contains("token") || k == "keys" {
                    *value = serde_json::json!("<redacted>");
                } else {
                    redact_config_value(value);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                redact_config_value(item);
            }
        }
        _ => {}
    }
}
