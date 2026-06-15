use super::*;

use std::path::Path;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct RequestLogEntry {
    // ── Identity ──
    pub id: u64,
    pub ts_ms: u64,
    // ── Request ──
    pub client_ip: String,
    pub method: String,
    pub path: String,
    pub model: Option<String>,
    pub upstream_id: Option<String>,
    pub is_stream: Option<bool>,
    pub billing_key: Option<String>,
    // ── Response ──
    pub status: u16,
    pub latency_ms: u64,
    pub timing: RequestTiming,
    // ── Size ──
    pub req_bytes: usize,
    pub resp_bytes: usize,
    // ── Usage ──
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub thought_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub token_source: Option<String>,
    // ── Error ──
    pub error: Option<String>,
}

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct RequestTiming {
    pub queue_ms: u64,
    pub upstream_ms: u64,
    pub total_ms: u64,
    pub attempts: u32,
}

#[derive(Clone, serde::Serialize)]
pub struct MetricsBucket {
    pub ts_ms: u64,
    pub total: u64,
    pub success: u64,
    pub failure: u64,
    pub ignored: u64,
}

#[derive(Clone, Copy)]
pub enum MetricsWindow {
    OneMin,
    FiveMin,
    ThirtyMin,
    OneHour,
}

impl MetricsWindow {
    pub fn from_str(s: &str) -> Self {
        match s {
            "5min" | "5m" => MetricsWindow::FiveMin,
            "30min" | "30m" => MetricsWindow::ThirtyMin,
            "1h" | "hour" => MetricsWindow::OneHour,
            _ => MetricsWindow::OneMin,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            MetricsWindow::OneMin => "1min",
            MetricsWindow::FiveMin => "5min",
            MetricsWindow::ThirtyMin => "30min",
            MetricsWindow::OneHour => "1h",
        }
    }
}

pub struct RequestsLog {
    entries: Mutex<VecDeque<RequestLogEntry>>,
    metrics: Mutex<RequestMetrics>,
    cap: usize,
    tx: Option<mpsc::Sender<RequestLogEntry>>,
    broadcast_tx: broadcast::Sender<RequestLogEntry>,
    /// Set once when the file writer channel is detected as closed, to avoid logging the error on every request.
    writer_dead: AtomicBool,
}

impl RequestsLog {
    pub fn new(cap: usize, tx: Option<mpsc::Sender<RequestLogEntry>>) -> Self {
        let (broadcast_tx, _rx) = broadcast::channel(1024);
        Self {
            entries: Mutex::new(VecDeque::with_capacity(cap)),
            metrics: Mutex::new(RequestMetrics::new()),
            cap,
            tx,
            broadcast_tx,
            writer_dead: AtomicBool::new(false),
        }
    }

    pub fn record(&self, entry: RequestLogEntry) {
        let _ = self.broadcast_tx.send(entry.clone());
        if let Some(tx) = &self.tx {
            if let Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) = tx.try_send(entry.clone()) {
                if !self.writer_dead.swap(true, Ordering::Relaxed) {
                    tracing::error!("request log writer task has died — file logging disabled for remaining process lifetime");
                }
            }
        }
        self.push_entry(entry);
    }

    /// Load historical entries into memory only — no broadcast or file write.
    /// Used at startup to restore in-memory state without duplicating the log file.
    pub fn load_history<I: IntoIterator<Item = RequestLogEntry>>(&self, entries: I) {
        for entry in entries {
            self.push_entry(entry);
        }
    }

    fn push_entry(&self, entry: RequestLogEntry) {
        {
            let Ok(mut entries) = self.entries.lock() else { return };
            entries.push_back(entry.clone());
            while entries.len() > self.cap {
                entries.pop_front();
            }
        }
        {
            let Ok(mut metrics) = self.metrics.lock() else { return };
            metrics.update(&entry);
        }
    }

    pub fn recent(&self, limit: usize) -> Vec<RequestLogEntry> {
        let Ok(entries) = self.entries.lock() else { return vec![] };
        entries.iter().rev().take(limit).cloned().collect()
    }

    pub fn metrics_snapshot(&self, window: MetricsWindow) -> Vec<MetricsBucket> {
        let Ok(metrics) = self.metrics.lock() else { return vec![] };
        metrics.snapshot(window)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<RequestLogEntry> {
        self.broadcast_tx.subscribe()
    }
}

pub struct RequestMetrics {
    m1: VecDeque<MetricsBucket>,
    m5: VecDeque<MetricsBucket>,
    m30: VecDeque<MetricsBucket>,
    h1: VecDeque<MetricsBucket>,
}

impl RequestMetrics {
    pub fn new() -> Self {
        Self {
            m1: VecDeque::new(),
            m5: VecDeque::new(),
            m30: VecDeque::new(),
            h1: VecDeque::new(),
        }
    }

    pub fn update(&mut self, entry: &RequestLogEntry) {
        let (success, failure, ignored) = classify_status(entry.status);
        let ts_ms = entry.ts_ms;

        update_bucket(&mut self.m1, ts_ms, 60_000, 60, success, failure, ignored);
        update_bucket(&mut self.m5, ts_ms, 300_000, 60, success, failure, ignored);
        update_bucket(&mut self.m30, ts_ms, 1_800_000, 48, success, failure, ignored);
        update_bucket(&mut self.h1, ts_ms, 3_600_000, 24, success, failure, ignored);
    }

    pub fn snapshot(&self, window: MetricsWindow) -> Vec<MetricsBucket> {
        match window {
            MetricsWindow::OneMin => self.m1.iter().cloned().collect(),
            MetricsWindow::FiveMin => self.m5.iter().cloned().collect(),
            MetricsWindow::ThirtyMin => self.m30.iter().cloned().collect(),
            MetricsWindow::OneHour => self.h1.iter().cloned().collect(),
        }
    }
}

pub(super) fn classify_status(status: u16) -> (u64, u64, u64) {
    if (200..300).contains(&status) {
        (1, 0, 0)
    } else if status == 404 {
        (0, 0, 1)
    } else {
        (0, 1, 0)
    }
}

pub(super) fn update_bucket(
    buckets: &mut VecDeque<MetricsBucket>,
    ts_ms: u64,
    step_ms: u64,
    cap: usize,
    success: u64,
    failure: u64,
    ignored: u64,
) {
    let bucket_start = ts_ms - (ts_ms % step_ms);

    if buckets.is_empty() {
        buckets.push_back(MetricsBucket {
            ts_ms: bucket_start,
            total: 0,
            success: 0,
            failure: 0,
            ignored: 0,
        });
    } else if let Some(last) = buckets.back() {
        let last_start = last.ts_ms;
        if bucket_start > last_start {
            let mut next_start = last_start.saturating_add(step_ms);
            while next_start <= bucket_start {
                buckets.push_back(MetricsBucket {
                    ts_ms: next_start,
                    total: 0,
                    success: 0,
                    failure: 0,
                    ignored: 0,
                });
                next_start = next_start.saturating_add(step_ms);
            }
        }
    }

    if let Some(last) = buckets.back_mut() {
        last.total += 1;
        last.success += success;
        last.failure += failure;
        last.ignored += ignored;
    }

    while buckets.len() > cap {
        buckets.pop_front();
    }
}

pub(super) fn start_request_log_writer(path: PathBuf, pause: Arc<AtomicBool>) -> Option<mpsc::Sender<RequestLogEntry>> {
    // Open the file synchronously so we can detect failure immediately.
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) => {
            tracing::error!(path = %path.display(), error = %e, "request log file open failed — file logging disabled");
            return None;
        }
    };

    let (tx, mut rx) = mpsc::channel::<RequestLogEntry>(2048);
    let std_file = file;

    tokio::spawn(async move {
        let mut file = tokio::fs::File::from_std(std_file);

        let mut pending = 0usize;
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut pause_buf: Vec<RequestLogEntry> = Vec::new();
        let mut was_paused = false;

        loop {
            tokio::select! {
                entry = rx.recv() => {
                    let Some(entry) = entry else { break; };
                    if pause.load(Ordering::Relaxed) {
                        pause_buf.push(entry);
                        continue;
                    }
                    if let Ok(line) = serde_json::to_string(&entry) {
                        if file.write_all(line.as_bytes()).await.is_ok() {
                            let _ = file.write_all(b"\n").await;
                            pending += 1;
                        }
                    }
                    if pending >= 256 {
                        let _ = file.flush().await;
                        pending = 0;
                    }
                }
                _ = tick.tick() => {
                    let paused = pause.load(Ordering::Relaxed);
                    if paused {
                        was_paused = true;
                    } else {
                        // Reopen file after cleanup may have renamed it.
                        if was_paused {
                            was_paused = false;
                            let _ = file.flush().await;
                            match tokio::fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(&path)
                                .await
                            {
                                Ok(f) => file = f,
                                Err(e) => tracing::warn!(
                                    path = %path.display(), error = %e,
                                    "request log reopen failed after cleanup"
                                ),
                            }
                        }
                        // Drain any buffered entries in FIFO order.
                        for entry in pause_buf.drain(..) {
                            if let Ok(line) = serde_json::to_string(&entry) {
                                if file.write_all(line.as_bytes()).await.is_ok() {
                                    let _ = file.write_all(b"\n").await;
                                    pending += 1;
                                }
                            }
                            if pending >= 256 {
                                let _ = file.flush().await;
                                pending = 0;
                            }
                        }
                        if pending > 0 {
                            let _ = file.flush().await;
                            pending = 0;
                        }
                    }
                }
            }
        }

        let _ = file.flush().await;
    });

    Some(tx)
}

/// Clean old request log entries from the JSONL file.
/// Uses temp-file + rename to avoid data loss from concurrent writes.
/// Returns (entries_kept, entries_removed).
pub(super) async fn cleanup_request_log(
    path: &Path,
    retention_days: u64,
    pause: &AtomicBool,
) -> (usize, usize) {
    if retention_days == 0 {
        return (0, 0);
    }
    let cutoff_ms = now_ms().saturating_sub(retention_days * 86_400_000);

    // Pause log writer to prevent concurrent writes.
    pause.store(true, Ordering::Relaxed);
    // Brief wait for in-flight writes to finish (writer flushes every 1s).
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let content = match tokio::fs::read_to_string(path).await {
        Ok(c) => c,
        Err(_) => {
            pause.store(false, Ordering::Relaxed);
            return (0, 0);
        }
    };

    let mut kept = 0usize;
    let mut removed = 0usize;
    let mut new_content = String::with_capacity(content.len());

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(ts) = v.get("ts_ms").and_then(|t| t.as_u64()) {
                if ts < cutoff_ms {
                    removed += 1;
                    continue;
                }
            }
        }
        new_content.push_str(line);
        new_content.push('\n');
        kept += 1;
    }

    if removed > 0 {
        let tmp_path = path.with_extension("jsonl.tmp");
        let result = async {
            tokio::fs::write(&tmp_path, &new_content).await?;
            tokio::fs::rename(&tmp_path, path).await?;
            Ok::<_, std::io::Error>(())
        }.await;

        if let Err(e) = result {
            tracing::warn!(path = %path.display(), error = %e, "request log cleanup write failed");
            pause.store(false, Ordering::Relaxed);
            return (0, 0);
        }
    }

    pause.store(false, Ordering::Relaxed);
    (kept, removed)
}

/// Sleep until the next occurrence of the given UTC time-of-day (seconds since midnight).
pub(super) async fn sleep_until_utc(target_secs: u64) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let day_secs: u64 = 86_400;
    let today_target = (now.as_secs() / day_secs) * day_secs + target_secs;
    let next_target = if today_target > now.as_secs() {
        today_target
    } else {
        today_target + day_secs
    };
    tokio::time::sleep(Duration::from_secs(next_target.saturating_sub(now.as_secs()))).await;
}

/// Spawn a task that cleans old request log entries once daily at a fixed time (03:00 UTC).
pub fn spawn_request_log_cleanup(path: PathBuf, retention_days: u64, pause: Arc<AtomicBool>) {
    if retention_days == 0 {
        return;
    }
    tokio::spawn(async move {
        loop {
            sleep_until_utc(3 * 3600).await;

            let (kept, removed) = cleanup_request_log(&path, retention_days, &pause).await;
            tracing::info!(
                path = %path.display(),
                kept,
                removed,
                retention_days,
                "request log cleanup: {kept} kept, {removed} removed (>{retention_days}d)"
            );
        }
    });
}

/// Spawn a task that checks monthly key usage reset daily at 03:05 UTC.
pub fn spawn_monthly_reset_check(store: Arc<crate::storage::KeyStore>) {
    tokio::spawn(async move {
        loop {
            sleep_until_utc(3 * 3600 + 300).await;

            if let Err(e) = store.check_monthly_reset() {
                tracing::warn!("monthly usage reset failed: {e}");
            }
        }
    });
}
