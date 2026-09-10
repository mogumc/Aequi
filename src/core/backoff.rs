//! 退避与熔断（§6.2，D12）：密钥级指数退避 + 上游级半开熔断。
//!
//! 退避：`base × factor^(streak-1)`，`Retry-After` 只作下界且受 cap；
//! 抖动默认 full（消除同步惊群）。双计数（rl/err）取较大者。
//! 计数与冷却**不持久化**（重启即清，§6.2.3）；持久化的只有
//! `SuspendedAuth` / `QuotaExhausted` / `enabled`。

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JitterMode {
    /// `uniform(0, dur_cap)` —— 标准选择：消除同步、期望等待最短。
    Full,
    /// `min(max_ms, uniform(base, prev × 3))` —— 上游对突发敏感时更平滑。
    Decorrelated,
    /// 无抖动 —— 仅测试复现。
    None,
}

/// 密钥级退避策略（§6.2.8）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BackoffPolicy {
    pub base_ms: u64,
    pub factor: f64,
    pub max_ms: u64,
    pub jitter: JitterMode,
    pub respect_retry_after: bool,
    pub retry_after_cap_ms: u64,
    /// 触发冷却的状态码（默认 [429, 500, 502, 503, 504]），另含连接错误与超时。
    pub on_status: Vec<u16>,
    /// 任一 2xx 清零双计数。
    pub success_reset: bool,
}

impl Default for BackoffPolicy {
    fn default() -> Self {
        Self {
            base_ms: 1000,
            factor: 2.0,
            max_ms: 60_000,
            jitter: JitterMode::Full,
            respect_retry_after: true,
            retry_after_cap_ms: 300_000,
            on_status: vec![429, 500, 502, 503, 504],
            success_reset: true,
        }
    }
}

/// 双计数（§6.2.3）。运行态，不持久化。
#[derive(Clone, Copy, Debug, Default)]
pub struct KeyBackoff {
    pub rl_streak: u32,
    pub err_streak: u32,
    /// 上一次 decorrelated 抖动的时长（仅 Decorrelated 模式使用）。
    pub prev_dur_ms: u64,
}

impl KeyBackoff {
    /// 记录一次状态码结果；返回是否应进入冷却。
    #[must_use]
    pub fn on_status(&mut self, policy: &BackoffPolicy, status: u16) -> bool {
        if policy.success_reset && (200..300).contains(&status) {
            self.reset();
            return false;
        }
        if status == 429 {
            self.rl_streak = self.rl_streak.saturating_add(1);
            return true;
        }
        if policy.on_status.contains(&status) {
            self.err_streak = self.err_streak.saturating_add(1);
            return true;
        }
        false
    }

    /// 记录连接错误 / 超时。
    pub fn on_network_error(&mut self) {
        self.err_streak = self.err_streak.saturating_add(1);
    }

    pub fn reset(&mut self) {
        self.rl_streak = 0;
        self.err_streak = 0;
        self.prev_dur_ms = 0;
    }

    /// 计算冷却时长（§6.2.1/§6.2.2，纯函数；抖动随机量由 `rng01 ∈ [0,1)` 外部注入以便测试）。
    ///
    /// 双计数各算 dur，**取较大者**；`Retry-After` 只作下界，受 `retry_after_cap_ms` 约束。
    #[must_use]
    pub fn cooldown_ms(&self, policy: &BackoffPolicy, retry_after_ms: Option<u64>, rng01: f64) -> u64 {
        let raw = |streak: u32| -> u64 {
            if streak == 0 {
                return 0;
            }
            let n = streak.saturating_sub(1).min(62) as f64;
            (policy.base_ms as f64 * policy.factor.powf(n)) as u64
        };
        let rl = raw(self.rl_streak);
        let err = raw(self.err_streak);
        let mut dur = rl.max(err);
        if dur == 0 {
            return 0;
        }

        // Retry-After 作为下界（受 cap）
        if policy.respect_retry_after {
            if let Some(ra) = retry_after_ms {
                let ra = ra.min(policy.retry_after_cap_ms);
                dur = dur.max(ra);
            }
        }

        // max_ms 截断
        let dur = dur.min(policy.max_ms);

        // 抖动
        let jittered = match policy.jitter {
            JitterMode::None => dur,
            JitterMode::Full => {
                // uniform(0, dur) —— 全抖动
                ((dur as f64) * rng01.clamp(0.0, 1.0)) as u64
            }
            JitterMode::Decorrelated => {
                let prev = if self.prev_dur_ms == 0 { policy.base_ms } else { self.prev_dur_ms };
                let lo = policy.base_ms as f64;
                let hi = ((prev as f64) * 3.0).min(policy.max_ms as f64);
                let span = hi - lo;
                (lo + span * rng01.clamp(0.0, 1.0)) as u64
            }
        };
        jittered.min(policy.max_ms)
    }
}

/// 上游级半开熔断状态机（§6.2.4）。
///
/// 熔断作用于**上游**而非密钥；阈值按窗口内**不同密钥**失败数
/// （单密钥故障不触发，防误开）。
#[derive(Clone, Debug)]
pub struct BreakerState {
    pub enabled: bool,
    pub error_threshold: u32,
    pub window_ms: u64,
    pub open_ms: u64,
    pub max_open_ms: u64,
    pub half_open_probes: u32,

    // 运行态
    state: Phase,
    /// 当前 Open 结束时刻（单调 ms）。
    open_until_ms: u64,
    /// 当前 open_ms（连续失败翻倍，受 max_open_ms 截断）。
    cur_open_ms: u64,
    /// 窗口起点（单调 ms）。
    window_start_ms: u64,
    /// 窗口内失败的不同密钥 id（去重）。
    failed_keys: std::collections::HashSet<u64>,
    /// 半开期已放行数。
    half_open_used: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Closed,
    Open,
    HalfOpen,
}

impl BreakerState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            enabled: true,
            error_threshold: 3,
            window_ms: 10_000,
            open_ms: 10_000,
            max_open_ms: 120_000,
            half_open_probes: 1,
            state: Phase::Closed,
            open_until_ms: 0,
            cur_open_ms: 10_000,
            window_start_ms: 0,
            failed_keys: std::collections::HashSet::new(),
            half_open_used: 0,
        }
    }

    #[must_use]
    pub const fn phase(&self) -> Phase {
        self.state
    }

    /// 本请求是否允许通过（`now_ms` 为单调时钟毫秒）。
    #[must_use]
    pub fn allows(&mut self, now_ms: u64) -> bool {
        if !self.enabled {
            return true;
        }
        match self.state {
            Phase::Closed => true,
            Phase::Open => {
                if now_ms >= self.open_until_ms {
                    self.state = Phase::HalfOpen;
                    self.half_open_used = 0;
                    true
                } else {
                    false
                }
            }
            Phase::HalfOpen => {
                if self.half_open_used < self.half_open_probes {
                    self.half_open_used += 1;
                    true
                } else {
                    false
                }
            }
        }
    }

    /// 记录一次失败（按不同密钥去重，§6.2.4）。返回是否状态发生变化（供事件）。
    #[must_use]
    pub fn on_failure(&mut self, key_id: u64, now_ms: u64) -> Option<(Phase, Phase)> {
        if !self.enabled {
            return None;
        }
        // 窗口滚动
        if now_ms.saturating_sub(self.window_start_ms) > self.window_ms {
            self.window_start_ms = now_ms;
            self.failed_keys.clear();
        }
        self.failed_keys.insert(key_id);

        let from = self.state;
        match self.state {
            Phase::Closed => {
                if self.failed_keys.len() as u32 >= self.error_threshold {
                    self.open(now_ms);
                    return Some((from, Phase::Open));
                }
            }
            Phase::HalfOpen => {
                // 半开探测失败 → 续开并翻倍 open_ms
                self.cur_open_ms = (self.cur_open_ms.saturating_mul(2)).min(self.max_open_ms);
                self.open(now_ms);
                return Some((from, Phase::Open));
            }
            Phase::Open => {}
        }
        None
    }

    /// 探测请求成功 → 闭合（清零失败集合）。
    #[must_use]
    pub fn on_success(&mut self, now_ms: u64) -> Option<(Phase, Phase)> {
        if !self.enabled || self.state == Phase::Closed {
            return None;
        }
        let from = self.state;
        self.state = Phase::Closed;
        self.failed_keys.clear();
        self.cur_open_ms = self.open_ms;
        self.window_start_ms = now_ms;
        self.half_open_used = 0;
        Some((from, Phase::Closed))
    }

    fn open(&mut self, now_ms: u64) {
        self.state = Phase::Open;
        self.open_until_ms = now_ms.saturating_add(self.cur_open_ms);
        self.failed_keys.clear();
    }
}

impl Default for BreakerState {
    fn default() -> Self {
        Self::new()
    }
}

/// `Retry-After` 解析（§6.2.2）：支持 delta-seconds 与 HTTP-date。
///
/// 非法值 / 已过去的日期 → `None`。`now_unix_ms` 用于 date 比较。
#[must_use]
pub fn parse_retry_after(value: &str, now_unix_ms: u64) -> Option<u64> {
    let v = value.trim();
    if v.is_empty() {
        return None;
    }
    // delta-seconds
    if let Ok(secs) = v.parse::<u64>() {
        return Some(secs.saturating_mul(1000));
    }
    // HTTP-date (IMF-fixdate / RFC 850 / asctime) —— 解析常见 IMF-fixdate
    let parsed = parse_http_date_ms(v)?;
    if parsed <= now_unix_ms {
        return None; // 已过去 → 不使用
    }
    Some(parsed.saturating_sub(now_unix_ms))
}

/// 解析 `Sun, 06 Nov 1994 08:49:37 GMT` 形式 → unix ms。手写避免引入 chrono。
fn parse_http_date_ms(s: &str) -> Option<u64> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let s = s.trim();
    let rest = s.split_once(", ").map(|(_, r)| r).unwrap_or(s);
    let mut parts = rest.split_whitespace();
    let day_s = parts.next()?;
    let day: u64 = day_s.trim_end_matches(',').parse().ok()?;
    let mon_s = parts.next()?.to_lowercase();
    let month = MONTHS.iter().position(|m| *m == mon_s)? as u64 + 1;
    let year: u64 = parts.next()?.parse().ok()?;
    let time = parts.next()?;
    let mut tp = time.split(':');
    let hh: u64 = tp.next()?.parse().ok()?;
    let mm: u64 = tp.next()?.parse().ok()?;
    let ss: u64 = tp.next().unwrap_or("0").parse().ok()?;
    if !(1..=31).contains(&day) || month > 12 || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    // days from civil (Howard Hinnant 算法) → unix secs
    let y = i64::try_from(year).ok()?;
    let m = i64::try_from(month).ok()?;
    let d = i64::try_from(day).ok()?;
    let yy = if m <= 2 { y - 1 } else { y };
    let era = if yy >= 0 { yy } else { yy - 399 } / 400;
    let yoe = yy - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + i64::try_from(hh * 3600 + mm * 60 + ss).ok()?;
    u64::try_from(secs).ok().map(|s| s.saturating_mul(1000))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> BackoffPolicy {
        BackoffPolicy::default()
    }

    #[test]
    fn backoff_grows_exponentially_capped() {
        let p = policy();
        let mut b = KeyBackoff::default();
        b.on_status(&p, 429);
        assert_eq!(b.cooldown_ms(&p, None, 0.5), 1000, "streak=1 → base");
        b.on_status(&p, 429);
        assert_eq!(b.cooldown_ms(&p, None, 0.5), 2000, "streak=2 → base×2");
        b.on_status(&p, 429);
        assert_eq!(b.cooldown_ms(&p, None, 0.5), 4000);
        // 大 streak → cap
        for _ in 0..20 {
            b.on_status(&p, 429);
        }
        assert_eq!(b.cooldown_ms(&p, None, 0.5), p.max_ms, "受 max_ms 截断");
    }

    #[test]
    fn dual_counter_takes_larger() {
        // §6.2.7：5xx 与 429 交替，取两条计数较大者，不互相清零（rng01=1.0 → 抖动取满）
        let p = policy();
        let mut b = KeyBackoff::default();
        b.on_status(&p, 429); // rl=1
        b.on_status(&p, 500); // err=1
        b.on_status(&p, 500); // err=2
        // rl dur=1s, err dur=2s → 取 2s
        assert_eq!(b.cooldown_ms(&p, None, 1.0), 2000);
        // rl=1 仍在（未被 5xx 清零）
        b.on_status(&p, 429); // rl=2
        assert_eq!(b.cooldown_ms(&p, None, 1.0), 2000);
    }

    #[test]
    fn success_resets_both() {
        let p = policy();
        let mut b = KeyBackoff::default();
        b.on_status(&p, 429);
        b.on_status(&p, 500);
        b.on_status(&p, 200);
        assert_eq!((b.rl_streak, b.err_streak), (0, 0));
    }

    #[test]
    fn full_jitter_spreads() {
        let p = policy();
        let mut b = KeyBackoff::default();
        b.on_status(&p, 429);
        let a = b.cooldown_ms(&p, None, 0.0);
        let m = b.cooldown_ms(&p, None, 0.5);
        let z = b.cooldown_ms(&p, None, 1.0);
        assert_eq!(a, 0);
        assert_eq!(m, 500);
        assert_eq!(z, 1000);
    }

    #[test]
    fn retry_after_is_floor_and_capped() {
        let p = policy();
        let mut b = KeyBackoff::default();
        b.on_status(&p, 429);
        // 上界拉高：Retry-After 10s > dur 1s → 取 10s
        assert_eq!(b.cooldown_ms(&p, Some(10_000), 1.0), 10_000);
        // 超大 Retry-After → cap 到 300s
        assert_eq!(b.cooldown_ms(&p, Some(86_400_000), 1.0), p.retry_after_cap_ms);
    }

    #[test]
    fn parse_retry_after_forms() {
        let now = 1_700_000_000_000;
        assert_eq!(parse_retry_after("5", now), Some(5_000));
        assert_eq!(parse_retry_after(" 30 ", now), Some(30_000));
        assert_eq!(parse_retry_after("abc", now), None);
        assert_eq!(parse_retry_after("", now), None);
        // 过去日期 → None
        assert_eq!(parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT", now), None);
        // 未来日期 → 剩余毫秒
        let d = parse_retry_after("Sun, 06 Nov 2044 08:49:37 GMT", now).unwrap();
        assert!(d > 1_000_000_000);
    }

    #[test]
    fn breaker_opens_on_distinct_keys() {
        let mut br = BreakerState::new();
        let t0 = 1000;
        // 单密钥连续失败 5 次 —— 不触发（防一个坏密钥误熔断）
        for i in 0..5 {
            assert!(br.allows(t0 + i));
            assert!(br.on_failure(1, t0 + i).is_none());
        }
        // 第 2、3 个不同密钥失败 → threshold=3 → Open
        assert!(br.allows(t0 + 10));
        assert!(br.on_failure(2, t0 + 10).is_none());
        assert!(br.allows(t0 + 11));
        let ev = br.on_failure(3, t0 + 11);
        assert!(ev.is_some(), "达到 3 个不同密钥 → 打开");
        assert_eq!(ev.unwrap(), (Phase::Closed, Phase::Open));
        // Open 期拒绝
        assert!(!br.allows(t0 + 12));
    }

    #[test]
    fn breaker_half_open_cycle() {
        let mut br = BreakerState::new();
        let t0 = 1000;
        for k in 1..=3u64 {
            let _ = br.allows(t0);
            let _ = br.on_failure(k, t0);
        }
        assert_eq!(br.phase(), Phase::Open);
        // 到期 → HalfOpen
        let t1 = t0 + br.open_ms + 1;
        assert!(br.allows(t1), "到期后放行探测");
        assert_eq!(br.phase(), Phase::HalfOpen);
        // 半开探测成功 → Closed
        assert!(br.on_success(t1 + 1).is_some());
        assert_eq!(br.phase(), Phase::Closed);
    }

    #[test]
    fn breaker_half_open_failure_doubles_open() {
        let mut br = BreakerState::new();
        let t0 = 1000;
        for k in 1..=3u64 {
            let _ = br.allows(t0);
            let _ = br.on_failure(k, t0);
        }
        let first_open = br.cur_open_ms;
        let t1 = t0 + br.open_ms + 1;
        let _ = br.allows(t1); // 进入半开
        br.on_failure(9, t1 + 1);
        assert_eq!(br.phase(), Phase::Open);
        assert_eq!(br.cur_open_ms, first_open * 2, "半开失败 → open_ms 翻倍");
    }

    #[test]
    fn breaker_disabled_always_allows() {
        let mut br = BreakerState::new();
        br.enabled = false;
        for k in 1..=10u64 {
            let _ = br.on_failure(k, 0);
        }
        assert!(br.allows(1));
        assert_eq!(br.phase(), Phase::Closed);
    }

    #[test]
    fn disabled_key_state_orthogonal() {
        // enabled(人工) 与 health 正交的回归锚点：本模块只管退避/熔断，
        // 人工暂停由 upstream_key.enabled 表达，冷却不写入任何持久化字段。
        let p = policy();
        let mut b = KeyBackoff::default();
        b.on_status(&p, 429);
        b.reset();
        assert_eq!(b.cooldown_ms(&p, None, 1.0), 0, "重启即清空（有意为之）");
    }
}
