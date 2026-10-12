//! 密码哈希与校验（Argon2id）。
//!
//! Argon2 是计算密集型的，直接放进异步上下文会占住 worker 线程，
//! 因此对外只提供 `spawn_blocking` 包装后的异步版本。

use argon2::Argon2;
use argon2::password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash};

use crate::error::{Error, Result};

/// 计算密码的 PHC 字符串（含随机盐与参数）。
fn hash_blocking(password: &str) -> Result<String> {
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|hash| hash.to_string())
        .map_err(|e| Error::Internal(anyhow::anyhow!("密码哈希失败: {e}")))
}

/// 校验密码。
fn verify_blocking(password: &str, phc: &str) -> Result<bool> {
    let parsed = match PasswordHash::new(phc) {
        Ok(parsed) => parsed,
        // 存量哈希格式非法，视为校验不通过而不是服务错误。
        Err(_) => return Ok(false),
    };

    Ok(Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok())
}

/// 异步计算密码哈希。
pub async fn hash(password: String) -> Result<String> {
    tokio::task::spawn_blocking(move || hash_blocking(&password))
        .await
        .map_err(|e| Error::Internal(anyhow::anyhow!("密码哈希任务异常: {e}")))?
}

/// 异步校验密码。
pub async fn verify(password: String, phc: String) -> Result<bool> {
    tokio::task::spawn_blocking(move || verify_blocking(&password, &phc))
        .await
        .map_err(|e| Error::Internal(anyhow::anyhow!("密码校验任务异常: {e}")))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_then_verify_roundtrip() {
        let phc = hash_blocking("demo-password").unwrap();
        assert!(phc.starts_with("$argon2"));
        assert!(verify_blocking("demo-password", &phc).unwrap());
        assert!(!verify_blocking("wrong-password", &phc).unwrap());
    }

    #[test]
    fn same_password_gets_different_hashes() {
        let a = hash_blocking("demo-password").unwrap();
        let b = hash_blocking("demo-password").unwrap();
        assert_ne!(a, b, "每次哈希都应使用独立随机盐");
    }

    #[test]
    fn broken_hash_is_rejected_without_error() {
        assert!(!verify_blocking("demo-password", "not-a-phc-string").unwrap());
    }
}
