//! OTP（TOTP）绑定与开关。
//!
//! 默认关闭。开启分两步：先取回密钥并扫码，再用一次正确的验证码确认，
//! 避免用户扫错码后把自己锁在外面。

use serde::Serialize;
use sqlx::SqlitePool;

use crate::db;
use crate::error::{Error, Result};
use crate::model::Account;
use crate::service::totp;
use crate::utils::time;

/// 绑定流程返回的信息。
#[derive(Debug, Clone, Serialize)]
pub struct OtpSetup {
    /// base32 密钥，供用户手动录入。
    pub secret: String,
    /// otpauth 地址，前端可据此渲染二维码。
    pub otpauth_url: String,
}

/// OTP 当前状态。
#[derive(Debug, Clone, Serialize)]
pub struct OtpStatus {
    /// 是否已启用。
    pub enabled: bool,
    /// 是否处于「已取回密钥但尚未确认」的中间状态。
    pub pending: bool,
}

/// 读取 OTP 状态。
pub fn status(account: &Account) -> OtpStatus {
    OtpStatus {
        enabled: account.totp_enabled,
        pending: !account.totp_enabled && account.totp_secret.is_some(),
    }
}

/// 开始绑定：生成密钥并落库，此时仍处于未启用状态。
pub async fn begin_setup(pool: &SqlitePool, account: &Account) -> Result<OtpSetup> {
    if account.totp_enabled {
        return Err(Error::BadRequest("OTP 已经处于启用状态".to_string()));
    }

    let secret = totp::generate_secret();
    let otpauth_url = totp::otpauth_url(&secret, &account.username)?;

    db::account::set_totp(pool, account.id, false, Some(&secret), time::now_secs()).await?;

    Ok(OtpSetup {
        secret,
        otpauth_url,
    })
}

/// 确认绑定：验证码正确才真正启用。
pub async fn enable(pool: &SqlitePool, account: &Account, code: &str) -> Result<()> {
    if account.totp_enabled {
        return Err(Error::BadRequest("OTP 已经处于启用状态".to_string()));
    }

    let secret = account
        .totp_secret
        .as_deref()
        .ok_or_else(|| Error::BadRequest("尚未开始绑定，请先获取密钥".to_string()))?;

    if !totp::verify(secret, code)? {
        return Err(Error::InvalidTotp);
    }

    db::account::set_totp(pool, account.id, true, Some(secret), time::now_secs()).await?;
    Ok(())
}

/// 关闭 OTP。已启用时必须提供一次正确的验证码。
pub async fn disable(pool: &SqlitePool, account: &Account, code: Option<&str>) -> Result<()> {
    if account.totp_enabled {
        let secret = account
            .totp_secret
            .as_deref()
            .ok_or_else(|| Error::Internal(anyhow::anyhow!("账号已启用 OTP 但缺少密钥")))?;
        let code = code.ok_or(Error::InvalidTotp)?;

        if !totp::verify(secret, code)? {
            return Err(Error::InvalidTotp);
        }
    }

    // 一并清掉密钥，避免残留的绑定信息被复用。
    db::account::set_totp(pool, account.id, false, None, time::now_secs()).await?;
    Ok(())
}
