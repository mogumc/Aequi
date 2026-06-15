use super::*;

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

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
            match tx.try_send(entry.clone()) {
                Ok(()) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    if !self.writer_dead.swap(true, Ordering::Relaxed) {
                        tracing::error!("request log writer task has died — file logging disabled for remaining process lifetime");
                    }
                }
                Err(tokio::sync::mpsc::error::TrySendError::Full(e)) => {
                    // Channel saturated — retry asynchronously to avoid losing this entry to disk.
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        if tx.send(e).await.is_err() {
                            tracing::error!("request log writer channel closed during retry");
                        }
                    });
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

/// Format a Unix timestamp as "YYYY-MM-DD" in UTC.
fn format_utc_date(secs: u64) -> String {
    let days = secs / 86400;
    // Howard Hinnant's civil_from_days algorithm (public domain).
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// List archive files (`requests-YYYY-MM-DD.jsonl`) in `dir`, sorted oldest-first.
pub fn list_request_log_archives(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut archives: Vec<(String, PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("requests-")
                && name.ends_with(".jsonl")
                && name.len() == "requests-YYYY-MM-DD.jsonl".len()
            {
                let date = name["requests-".len()..name.len() - ".jsonl".len()].to_string();
                // Basic date format validation.
                if date.len() == 10
                    && date.as_bytes()[4] == b'-'
                    && date.as_bytes()[7] == b'-'
                    && date.chars().all(|c| c.is_ascii_digit() || c == '-')
                {
                    Some((date, e.path()))
                } else {
                    None
                }
            } else {
                None
            }
        })
        .collect();
    archives.sort_by(|a, b| a.0.cmp(&b.0));
    archives
}

pub(super) fn start_request_log_writer(path: PathBuf) -> Option<mpsc::Sender<RequestLogEntry>> {
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
        // Option<File> tracks ownership across rotation: take() releases the handle
        // for rename, then Some(new_file) replaces it. The borrow checker requires
        // this because drop(file) invalidates the binding until reassignment.
        let mut file: Option<tokio::fs::File> = Some(tokio::fs::File::from_std(std_file));

        let mut pending = 0usize;
        // Periodic flush safety net — ensures unflushed data doesn't linger during low traffic.
        let mut tick = tokio::time::interval(Duration::from_secs(1));

        // Track current date for daily rotation.
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut current_date = format_utc_date(now_secs);

        loop {
            // Check if the UTC day has changed — rotate if so.
            let new_secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let new_date = format_utc_date(new_secs);
            if new_date != current_date {
                // Flush and release the current file handle.
                if let Some(f) = file.as_mut() {
                    let _ = f.flush().await;
                }
                let old_file = file.take();
                drop(old_file);

                // Skip rotation for empty files — avoids creating zero-byte archives
                // when no requests were received all day.
                let file_size = tokio::fs::metadata(&path)
                    .await
                    .map(|m| m.len())
                    .unwrap_or(0);

                if file_size > 0 {
                    let archive_path = path.with_file_name(format!("requests-{current_date}.jsonl"));
                    match tokio::fs::rename(&path, &archive_path).await {
                        Ok(()) => {
                            tracing::info!(
                                archive = %archive_path.display(),
                                "rotated request log to archive"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                src = %path.display(),
                                dst = %archive_path.display(),
                                error = %e,
                                "failed to rotate request log"
                            );
                        }
                    }
                }

                // Always open a (new or existing) file for the new day.
                // If rename failed the old file remains; append mode preserves its content.
                match tokio::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .await
                {
                    Ok(f) => file = Some(f),
                    Err(e) => {
                        tracing::error!(
                            path = %path.display(),
                            error = %e,
                            "failed to open new request log after rotation — file logging disabled"
                        );
                        break;
                    }
                }

                current_date = new_date;
                pending = 0;
            }

            tokio::select! {
                entry = rx.recv() => {
                    let Some(entry) = entry else { break; };
                    if let Some(f) = file.as_mut() {
                        if let Ok(line) = serde_json::to_string(&entry) {
                            if f.write_all(line.as_bytes()).await.is_ok() {
                                let _ = f.write_all(b"\n").await;
                                pending += 1;
                            }
                        }
                        // Flush in batches of 256 to balance throughput and durability.
                        if pending >= 256 {
                            let _ = f.flush().await;
                            pending = 0;
                        }
                    }
                }
                _ = tick.tick() => {
                    if pending > 0 {
                        if let Some(f) = file.as_mut() {
                            let _ = f.flush().await;
                        }
                        pending = 0;
                    }
                }
            }
        }

        if let Some(f) = file.as_mut() {
            let _ = f.flush().await;
        }
    });

    Some(tx)
}

/// Delete archive files older than `retention_days`. Archives are named `requests-YYYY-MM-DD.jsonl`,
/// so we compare the date in the filename against the cutoff — no need to read file contents.
/// Returns (files_kept, files_removed).
pub(super) async fn cleanup_request_log(
    dir: &Path,
    retention_days: u64,
) -> (usize, usize) {
    if retention_days == 0 {
        return (0, 0);
    }

    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let cutoff_date = format_utc_date(now_secs.saturating_sub(retention_days * 86_400));

    let archives = list_request_log_archives(dir);
    let mut kept = 0usize;
    let mut removed = 0usize;

    for (date, path) in &archives {
        if date.as_str() < cutoff_date.as_str() {
            match tokio::fs::remove_file(path).await {
                Ok(()) => {
                    tracing::info!(file = %path.display(), "deleted expired request log archive");
                    removed += 1;
                }
                Err(e) => {
                    tracing::warn!(file = %path.display(), error = %e, "failed to delete expired request log archive");
                    kept += 1;
                }
            }
        } else {
            kept += 1;
        }
    }

    (kept, removed)
}

/// Sleep until the next occurrence of the given UTC time-of-day (seconds since midnight).
pub(super) async fn sleep_until_utc(target_secs: u64) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
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

/// Spawn a task that deletes expired request log archives daily at 03:00 UTC.
pub fn spawn_request_log_cleanup(dir: PathBuf, retention_days: u64) {
    if retention_days == 0 {
        return;
    }
    tokio::spawn(async move {
        loop {
            sleep_until_utc(3 * 3600).await;

            let (kept, removed) = cleanup_request_log(&dir, retention_days).await;
            tracing::info!(
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
