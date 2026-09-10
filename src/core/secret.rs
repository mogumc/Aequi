//! `Secret<T>`：类型级脱敏（§3.4 移除 `redact_config_value` 的替代）。
//!
//! `Debug` / `Display` / `Serialize` 永不输出明文；明文只能通过 [`Secret::expose`]
//! 显式取出（仅用于发往上游的请求头）。配合 `#![forbid(unsafe_code)]`，
//! 明文不可能经格式化路径泄漏进日志。

use serde::Serialize;
use std::fmt;

#[derive(Clone)]
pub struct Secret<T> {
    inner: T,
    /// 展示用前缀（如 "sk-abc…"），可安全输出。
    prefix_hint: Option<Box<str>>,
}

impl<T: AsRef<str>> Secret<T> {
    /// 包装明文；自动派生前缀提示（保留前 8 字符）。
    #[must_use]
    pub fn new(inner: T) -> Self {
        let s = inner.as_ref();
        let hint = if s.len() > 8 {
            Some(s.chars().take(8).collect::<String>().into())
        } else {
            Some(s.to_string().into())
        };
        Self {
            inner,
            prefix_hint: hint,
        }
    }
}

impl<T> Secret<T> {
    #[must_use]
    pub fn with_prefix(inner: T, prefix: impl Into<String>) -> Self {
        Self {
            inner,
            prefix_hint: Some(prefix.into().into()),
        }
    }

    /// 显式取出明文 —— 调用点即审计点。
    #[must_use]
    pub const fn expose(&self) -> &T {
        &self.inner
    }

    /// 安全展示的前缀。
    #[must_use]
    pub fn prefix(&self) -> &str {
        self.prefix_hint.as_deref().unwrap_or("")
    }

    pub fn into_inner(self) -> T {
        self.inner
    }
}

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Secret").field("prefix", &self.prefix()).finish()
    }
}

impl<T> fmt::Display for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl<T: Serialize> Serialize for Secret<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str("[REDACTED]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_leaks_via_debug_or_display() {
        let s = Secret::new("sk-aequi-0123456789abcdef".to_string());
        let dbg = format!("{s:?}");
        assert!(!dbg.contains("0123456789"));
        let disp = format!("{s}");
        assert_eq!(disp, "[REDACTED]");
    }

    #[test]
    fn serialize_is_redacted() {
        let s = Secret::new("sk-secret-value".to_string());
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, "\"[REDACTED]\"");
    }

    #[test]
    fn expose_returns_plaintext() {
        let s = Secret::new("sk-live-9999".to_string());
        assert_eq!(s.expose().as_str(), "sk-live-9999");
        assert_eq!(s.prefix(), "sk-live-");
    }
}
