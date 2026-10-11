//! TOTP 动态验证码（RFC 6238）。
//!
//! 采用 SHA-1 + 6 位 + 30 秒步长，这是各家验证器 App 兼容性最好的组合。
//! 是否启用由账号自身决定，默认关闭。

use totp_rs::{Algorithm, Builder, Secret, Totp};

use crate::error::{Error, Result};

/// 验证器 App 中显示的服务名。
const ISSUER: &str = "Aequi";

/// 生成一个新的 base32 密钥。
pub fn generate_secret() -> String {
    Secret::generate().to_base32()
}

/// 构造 TOTP 实例。
fn build(secret_base32: &str, account_name: &str) -> Result<Totp> {
    let secret = Secret::try_from_base32(secret_base32)
        .map_err(|e| Error::Internal(anyhow::anyhow!("TOTP 密钥格式非法: {e}")))?;

    Builder::new()
        .with_algorithm(Algorithm::SHA1)
        .with_secret(secret)
        .with_account_name(account_name)
        .with_issuer(Some(ISSUER))
        .build()
        .map_err(|e| Error::Internal(anyhow::anyhow!("构造 TOTP 失败: {e}")))
}

/// 生成供验证器 App 扫码的 otpauth 地址。
pub fn otpauth_url(secret_base32: &str, account_name: &str) -> Result<String> {
    build(secret_base32, account_name)?
        .to_url()
        .map_err(|e| Error::Internal(anyhow::anyhow!("生成 otpauth 地址失败: {e}")))
}

/// 校验一个动态验证码。
pub fn verify(secret_base32: &str, code: &str) -> Result<bool> {
    let totp = build(secret_base32, "")?;
    Ok(totp.check_current(code.trim()).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_is_base32_and_unique() {
        let secret = generate_secret();
        assert!(!secret.is_empty());
        assert!(secret.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(secret, generate_secret());
    }

    #[test]
    fn current_code_verifies() {
        let secret = generate_secret();
        let totp = build(&secret, "demo").unwrap();
        let code = totp.generate_current().to_string();
        assert!(verify(&secret, &code).unwrap());
    }

    #[test]
    fn wrong_code_is_rejected() {
        let secret = generate_secret();
        assert!(!verify(&secret, "000000").unwrap());
    }

    #[test]
    fn otpauth_url_contains_issuer() {
        let secret = generate_secret();
        let url = otpauth_url(&secret, "demo").unwrap();
        assert!(url.starts_with("otpauth://totp/"));
        assert!(url.contains(ISSUER));
    }
}
