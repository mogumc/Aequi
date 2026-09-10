//! 配置（§4.2 / §八 第 11 条）：`AppConfig` 不可变快照 + `RateTable` 独立热更新。
//!
//! 策略即数据：退避 / 探针 / 费率 / 分组全部落在 `AppConfig`（上层以 `ArcSwap` 持有），
//! 调参不发版。无哨兵值 —— 一切 `Unlimited` 用 `Option` 显式建模。

use crate::core::backoff::BackoffPolicy;
use crate::core::rating::Rate;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// 单调/墙钟毫秒（core 无 IO，时间由调用方注入；此处提供标准实现）。
#[must_use]
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 探针策略（§6.1.3 / §6.1.4）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProbePolicy {
    pub concurrency: u32,
    pub per_upstream: u32,
    pub budget_per_min: u32,
    pub startup_delay_ms: u64,
    pub timeout_ms: u64,
    /// `Unknown` 态是否可参与选路（默认 true，网络恢复后自动接单）。
    pub unverified_usable: bool,
    /// `Inconclusive` 连续超限 → 标记 `Unknown`（不自动降级）。
    pub inconclusive_limit: u32,
    /// 复核退避基数与上限（§6.1.4）。
    pub base_recheck_ms: u64,
    pub max_recheck_ms: u64,
}

impl Default for ProbePolicy {
    fn default() -> Self {
        Self {
            concurrency: 4,
            per_upstream: 2,
            budget_per_min: 10,
            startup_delay_ms: 10_000,
            timeout_ms: 5_000,
            unverified_usable: true,
            inconclusive_limit: 5,
            base_recheck_ms: 60_000,
            max_recheck_ms: 3_600_000,
        }
    }
}

/// 上游密钥额度策略（§6.5.1，D13）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QuotaPolicy {
    pub enabled: bool,
    pub default_safety_margin: f64,
}

impl Default for QuotaPolicy {
    fn default() -> Self {
        Self { enabled: true, default_safety_margin: 0.98 }
    }
}

/// 请求日志存储策略（§5.1）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogSinkPolicy {
    /// jsonl（默认）/ sqlite / both。
    pub sink: LogSinkKind,
    pub retention_days: u32,
    pub data_dir: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogSinkKind {
    Jsonl,
    Sqlite,
    Both,
}

impl Default for LogSinkPolicy {
    fn default() -> Self {
        Self {
            sink: LogSinkKind::Jsonl,
            retention_days: 30,
            data_dir: "data".into(),
        }
    }
}

/// 费率表（§6.5.2 / §6.5.3，D8）：按**模型 id** 统一配置，无分组倍率。
/// 随 `AppConfig` 热更新，不重启生效。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RateTable {
    /// display_model → Rate；未命中走 `default_rate`。
    pub rates: BTreeMap<String, Rate>,
    /// 未知模型的默认行（取代旧版硬编码 0.1 / 1.0）。
    pub default_rate: Option<Rate>,
}

impl RateTable {
    #[must_use]
    pub fn rate_for(&self, display_model: &str) -> Rate {
        self.rates
            .get(display_model)
            .cloned()
            .or_else(|| self.default_rate.clone())
            .unwrap_or_default()
    }
}

/// 访问密钥策略（§6.5.4）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KeyPolicy {
    /// 新访问密钥默认积分上限（µcredit）；`None` = 无限。
    pub default_credits_cap_micro: Option<i64>,
    /// 软阈值告警比例（默认 0.8）。
    pub quota_soft_threshold: f64,
    /// 新访问密钥默认分组。
    pub default_groups: Vec<String>,
}

impl Default for KeyPolicy {
    fn default() -> Self {
        Self {
            default_credits_cap_micro: None,
            quota_soft_threshold: 0.8,
            default_groups: vec![crate::core::group::GroupId::DEFAULT.to_string()],
        }
    }
}

/// 应用配置（不可变快照；热更新 = 整体替换，§4.2）。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub listen_addr: String,
    pub admin_tokens: Vec<String>,
    pub probe: ProbePolicy,
    pub quota: QuotaPolicy,
    pub backoff: BackoffPolicy,
    pub log: LogSinkPolicy,
    pub key: KeyPolicy,
    pub rate_table: RateTable,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            listen_addr: "127.0.0.1:8787".into(),
            admin_tokens: Vec::new(),
            probe: ProbePolicy::default(),
            quota: QuotaPolicy::default(),
            backoff: BackoffPolicy::default(),
            log: LogSinkPolicy::default(),
            key: KeyPolicy::default(),
            rate_table: RateTable::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_table_fallback() {
        let mut rt = RateTable::default();
        rt.rates.insert("gpt-4o".into(), Rate::new(2.0, 3.0, 3.0, 0.4));
        rt.default_rate = Some(Rate::new(9.0, 9.0, 9.0, 0.9));
        assert_eq!(rt.rate_for("gpt-4o").input, 2.0);
        assert_eq!(rt.rate_for("unknown-model").input, 9.0, "未知模型走默认行");
        assert_eq!(rt.rate_for("unknown-model").cache, 0.9);
    }

    #[test]
    fn config_roundtrip_toml() {
        let cfg = AppConfig::default();
        let s = toml::to_string(&cfg).unwrap();
        let back: AppConfig = toml::from_str(&s).unwrap();
        assert_eq!(back.probe.concurrency, 4);
        assert_eq!(back.backoff.base_ms, 1000);
        assert_eq!(back.rate_table.rates.len(), 0);
    }

    #[test]
    fn no_sentinel_values() {
        // §八 第 5 条：无限额用 None 表达，不存在 -1
        let cfg = AppConfig::default();
        assert_eq!(cfg.key.default_credits_cap_micro, None);
        let json = serde_json::to_string(&cfg.key).unwrap();
        assert!(!json.contains("-1"), "禁止哨兵值 -1 出现在配置序列化中");
    }
}
