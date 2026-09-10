//! JSONL 请求日志主通道（§5.4）：日志切 + 攒批写入 + 保留期清理。
//!
//! 请求日志是**追加密集型**数据（1.7k RPS ≈ 每天百万行），JSONL 顺序追加
//! O(1) 无事务无 WAL —— 这是 D2 把主通道定为 JSONL 的核心判断。
//! 沿用旧版判定"算法正确"的逻辑：UTC 日切、空文件不归档、按文件名清理。

use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// 单条日志行（schema v1，§6.6 `RequestFinished` 的 JSONL 投影）。
pub type LogLine = serde_json::Value;

const FLUSH_BATCH: usize = 256;

struct Inner {
    writer: std::io::BufWriter<std::fs::File>,
    /// 当前文件对应的 UTC 日（YYYYMMDD）。
    day: u32,
    buf_count: usize,
}

/// 日志 sink。`send` 克隆给热路径；actor 攒批落盘。
#[derive(Clone)]
pub struct JsonlSink {
    dir: PathBuf,
    tx: mpsc::UnboundedSender<LogLine>,
    inner: std::sync::Arc<Mutex<Inner>>,
}

impl JsonlSink {
    pub fn new(dir: impl Into<PathBuf>) -> std::io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        let day = utc_day(crate::core::config::now_ms());
        let file = open_day_file(&dir, day)?;
        let inner = std::sync::Arc::new(Mutex::new(Inner {
            writer: std::io::BufWriter::with_capacity(64 * 1024, file),
            day,
            buf_count: 0,
        }));
        let (tx, mut rx) = mpsc::unbounded_channel::<LogLine>();

        // actor：攒 FLUSH_BATCH 行或 1s tick 兜底 flush（§5.4）
        let sink_self = Self { dir: dir.clone(), tx: tx.clone(), inner: inner.clone() };
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                tokio::select! {
                    line = rx.recv() => {
                        match line {
                            Some(line) => {
                                if let Err(e) = sink_self.append_line(line) {
                                    warn!(%e, "jsonl append failed");
                                }
                            }
                            None => break,
                        }
                    }
                    _ = tick.tick() => {
                        if let Err(e) = sink_self.flush() {
                            warn!(%e, "jsonl flush failed");
                        }
                    }
                }
            }
            let _ = sink_self.flush();
            debug!("jsonl sink exited");
        });

        Ok(Self { dir, tx, inner })
    }

    /// 热路径提交（O(1)，永不阻塞）。
    pub fn send(&self, line: LogLine) {
        let _ = self.tx.send(line);
    }

    fn append_line(&self, line: LogLine) -> std::io::Result<()> {
        let now = crate::core::config::now_ms();
        let day = utc_day(now);
        let mut g = self.inner.lock().map_err(|_| std::io::Error::other("poisoned"))?;
        // UTC 日切（§5.4）：flush 后换文件；空文件不归档
        if g.day != day {
            g.writer.flush()?;
            let new_file = open_day_file(&self.dir, day)?;
            g.writer = std::io::BufWriter::with_capacity(64 * 1024, new_file);
            g.day = day;
            g.buf_count = 0;
        }
        serde_json::to_writer(&mut g.writer, &line).map_err(std::io::Error::other)?;
        g.writer.write_all(b"\n")?;
        g.buf_count += 1;
        if g.buf_count >= FLUSH_BATCH {
            g.writer.flush()?;
            g.buf_count = 0;
        }
        Ok(())
    }

    fn flush(&self) -> std::io::Result<()> {
        let mut g = self.inner.lock().map_err(|_| std::io::Error::other("poisoned"))?;
        if g.buf_count > 0 {
            g.writer.flush()?;
            g.buf_count = 0;
        }
        Ok(())
    }

    /// 保留期清理（§5.4）：删除 `date < today - retention_days` 的归档文件。
    /// 按文件名比较，不读内容，O(文件数)。每日 03:00 UTC 由 runtime/tasks 调度。
    pub fn cleanup(&self, retention_days: u32) -> std::io::Result<usize> {
        let today = utc_day(crate::core::config::now_ms());
        let cutoff = today.saturating_sub(retention_days * 10_000);
        let mut removed = 0;
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name_s = name.to_string_lossy();
            // requests-YYYY-MM-DD.jsonl
            let Some(date_part) = name_s
                .strip_prefix("requests-")
                .and_then(|s| s.strip_suffix(".jsonl"))
            else {
                continue;
            };
            let day = day_compact(date_part);
            if let Some(day) = day {
                if day < cutoff {
                    // 删除前确认非当前文件
                    if day != utc_day(crate::core::config::now_ms()) {
                        std::fs::remove_file(entry.path())?;
                        removed += 1;
                    }
                }
            }
        }
        Ok(removed)
    }

    /// 当前活跃文件路径（供测试）。
    pub fn active_path(&self) -> PathBuf {
        let day = utc_day(crate::core::config::now_ms());
        day_file_name(&self.dir, day)
    }
}

fn utc_day(now_ms: u64) -> u32 {
    let secs = now_ms / 1000;
    let days = secs / 86_400;
    // days since epoch → YYYYMMDD（civil from days，Howard Hinnant）
    let z = i64::try_from(days).unwrap_or(0) + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let year = y + i64::from(m <= 2);
    let month = i64::from(m <= 2) * 12 + m;
    let day = doy - (153 * mp + 2) / 5 + 1;
    format!("{year:04}{month:02}{day:02}").parse().unwrap_or(19700101)
}

fn day_compact(date: &str) -> Option<u32> {
    let parts: Vec<&str> = date.split('-').collect();
    if parts.len() != 3 {
        return None;
    }
    Some(format!("{:04}{:02}{:02}", parts[0].parse::<u32>().ok()?, parts[1].parse::<u32>().ok()?, parts[2].parse::<u32>().ok()?)
        .parse()
        .ok()?)
}

fn day_file_name(dir: &Path, day: u32) -> PathBuf {
    let s = format!("{day:08}");
    dir.join(format!(
        "requests-{}-{}-{}.jsonl",
        &s[..4],
        &s[4..6],
        &s[6..8]
    ))
}

fn open_day_file(dir: &Path, day: u32) -> std::io::Result<std::fs::File> {
    let path = day_file_name(dir, day);
    std::fs::OpenOptions::new().create(true).append(true).open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn writes_and_rotates() {
        let dir = tempfile::tempdir().unwrap();
        let sink = JsonlSink::new(dir.path()).unwrap();

        sink.send(json!({"kind": "request_finished", "model": "gpt-4o"}));
        sink.send(json!({"kind": "request_finished", "model": "o3"}));
        // 等待 actor 消费（unbounded channel + select 循环）
        for _ in 0..50 {
            if sink.flush().is_ok() {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            let n = std::fs::read_to_string(sink.active_path())
                .map(|c| c.lines().count())
                .unwrap_or(0);
            if n >= 2 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        sink.flush().unwrap();

        let content = std::fs::read_to_string(sink.active_path()).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "两行均落盘");
        let parsed: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed["model"], "gpt-4o");
    }

    #[test]
    fn cleanup_removes_expired_only() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter(); // JsonlSink::new 会 spawn actor，需要 runtime context
        let dir = tempfile::tempdir().unwrap();
        // 手造文件：一个过期、一个保留、一个当前
        std::fs::write(dir.path().join("requests-2000-01-01.jsonl"), "x").unwrap();
        std::fs::write(dir.path().join("requests-2099-01-01.jsonl"), "x").unwrap();
        let sink = JsonlSink::new(dir.path()).unwrap();
        let current_name = sink.active_path().file_name().unwrap().to_string_lossy().to_string();
        std::fs::write(dir.path().join(&current_name), "x").unwrap();

        let removed = sink.cleanup(30).unwrap();
        assert_eq!(removed, 1, "仅删 2000-01-01");
        assert!(dir.path().join("requests-2099-01-01.jsonl").exists());
        assert!(dir.path().join(&current_name).exists());
    }

    #[test]
    fn utc_day_math() {
        // 2026-09-10 00:00 UTC = 1786224000s → day 20260910
        let ms = 1_786_224_000_000u64;
        assert_eq!(utc_day(ms), 20260910);
        assert_eq!(utc_day(0), 19700101);
    }
}
