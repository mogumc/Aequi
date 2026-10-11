//! 认证编排：首次初始化、登录、登出与会话校验。

use sqlx::SqlitePool;

use crate::config::{non_empty_env, Settings, ADMIN_PASSWORD_ENV, ADMIN_USERNAME_ENV};
use crate::db;
use crate::error::{Error, Result};
use crate::model::{Account, NewSession, Session};
use crate::service::{password, token, totp};
use crate::utils::{random, time};

/// 初始账号用户名长度。
const INITIAL_USERNAME_LEN: usize = 10;
/// 初始账号密码长度。
const INITIAL_PASSWORD_LEN: usize = 24;
/// 会话里记录的 IP 长度上限。
const MAX_IP_LEN: usize = 64;
/// 会话里记录的 User-Agent 长度上限。UA 可以任意长，不截断会把表撑大。
const MAX_USER_AGENT_LEN: usize = 512;
/// 会话标识的最大尝试次数，用于处理随机串碰撞这类极小概率事件。
const SESSION_ID_ATTEMPTS: usize = 3;

/// 首次创建数据库时生成的初始凭据。
#[derive(Debug, Clone)]
pub struct InitialCredentials {
    pub username: String,
    pub password: String,
    /// 是否来自环境变量。来自环境变量时不回显密码。
    pub from_env: bool,
}

/// 初始化账号。
///
/// 仅在 `accounts` 表为空时执行。是否接受环境变量由 `is_first_run` 决定：
/// 只有首次启动才读 `AEQUI_ADMIN_USERNAME` / `AEQUI_ADMIN_PASSWORD`，
/// 数据库一旦存在，后续启动完全忽略外部输入，一律随机生成。
pub async fn initialize_account(
    pool: &SqlitePool,
    is_first_run: bool,
) -> Result<Option<InitialCredentials>> {
    if db::account::count(pool).await? > 0 {
        return Ok(None);
    }

    let credentials = if is_first_run {
        match env_credentials() {
            Some((username, password)) => InitialCredentials {
                username,
                password,
                from_env: true,
            },
            None => random_credentials(),
        }
    } else {
        tracing::warn!("数据库已存在但账号表为空，随机生成凭据并忽略环境变量");
        random_credentials()
    };

    let password_hash = password::hash(credentials.password.clone()).await?;
    db::account::insert(
        pool,
        &credentials.username,
        &password_hash,
        time::now_secs(),
    )
    .await?;

    Ok(Some(credentials))
}

/// 随机生成初始凭据（字母 + 数字）。
fn random_credentials() -> InitialCredentials {
    InitialCredentials {
        username: random::string(INITIAL_USERNAME_LEN),
        password: random::string(INITIAL_PASSWORD_LEN),
        from_env: false,
    }
}

/// 截断过长的记录值，空串按「没有」处理。
fn truncate(value: Option<&str>, max_chars: usize) -> Option<String> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    Some(value.chars().take(max_chars).collect())
}

/// 从环境变量读取初始凭据。两项都齐全才算有效。
fn env_credentials() -> Option<(String, String)> {
    let username = non_empty_env(ADMIN_USERNAME_ENV)?;
    let password = non_empty_env(ADMIN_PASSWORD_ENV)?;
    Some((username, password))
}

/// 登录请求。
#[derive(Debug)]
pub struct LoginRequest<'a> {
    pub username: &'a str,
    pub password: &'a str,
    /// 账号启用 OTP 时必须提供。
    pub totp_code: Option<&'a str>,
    pub ip: Option<&'a str>,
    pub user_agent: Option<&'a str>,
}

/// 登录成功的结果。
#[derive(Debug)]
pub struct LoginOutcome {
    /// 明文令牌，只在此处出现一次。
    pub token: String,
    pub session: Session,
    pub account: Account,
}

/// 登录。
///
/// 未启用 OTP 时只校验密码；启用后必须额外提供一次性验证码，
/// 会话的 `login_type` 会如实记录实际使用的登录方式。
pub async fn login(
    pool: &SqlitePool,
    settings: &Settings,
    request: LoginRequest<'_>,
) -> Result<LoginOutcome> {
    let account = db::account::find_by_username(pool, request.username)
        .await?
        .ok_or(Error::InvalidCredentials)?;

    if !password::verify(request.password.to_string(), account.password_hash.clone()).await? {
        return Err(Error::InvalidCredentials);
    }

    let login_type = if account.totp_enabled {
        let secret = account
            .totp_secret
            .as_deref()
            .ok_or_else(|| Error::Internal(anyhow::anyhow!("账号已启用 OTP 但缺少密钥")))?;
        let code = request.totp_code.ok_or(Error::TotpRequired)?;

        if !totp::verify(secret, code)? {
            return Err(Error::InvalidTotp);
        }
        "password+totp"
    } else {
        "password"
    };

    let now = time::now_secs();
    let expires_at = now + settings.token_ttl_hours * 3600;
    let ip = truncate(request.ip, MAX_IP_LEN);
    let user_agent = truncate(request.user_agent, MAX_USER_AGENT_LEN);

    // 会话标识是随机串，理论上可能撞车。撞上就换一组重来，
    // 不把这种极小概率事件当成服务故障抛给用户。
    let mut issued = None;
    for _ in 0..SESSION_ID_ATTEMPTS {
        let secret_token = token::generate();
        let session_id = token::generate_session_id();
        let token_hash = token::hash(&secret_token);

        let result = db::session::insert(
            pool,
            &NewSession {
                id: &session_id,
                account_id: account.id,
                token_hash: &token_hash,
                login_type,
                ip: ip.as_deref(),
                user_agent: user_agent.as_deref(),
                created_at: now,
                expires_at,
            },
        )
        .await;

        match result {
            Ok(()) => {
                issued = Some((secret_token, session_id));
                break;
            }
            Err(error) if db::is_unique_violation(&error) => {
                tracing::warn!("会话标识与已有记录冲突，重新生成");
            }
            Err(error) => return Err(error.into()),
        }
    }

    let (secret_token, session_id) =
        issued.ok_or_else(|| Error::Internal(anyhow::anyhow!("多次生成会话标识均冲突")))?;

    let session = db::session::find_by_id(pool, &session_id)
        .await?
        .ok_or_else(|| Error::Internal(anyhow::anyhow!("会话写入后未能回读")))?;

    Ok(LoginOutcome {
        token: secret_token,
        session,
        account,
    })
}

/// 校验令牌并返回对应的账号与会话。
///
/// 会顺带刷新会话的最近使用时间。
pub async fn authenticate(pool: &SqlitePool, secret_token: &str) -> Result<(Account, Session)> {
    let token_hash = token::hash(secret_token);
    let now = time::now_secs();

    // 有效性条件写在查询里：取不到就是失效，不依赖上层记得再判断一次。
    let session = db::session::find_active_by_token(pool, &token_hash, now)
        .await?
        .ok_or_else(|| Error::Unauthorized("令牌无效或会话已失效".to_string()))?;

    // 查到之后会话仍可能被并发撤销，刷新失败就按失效处理——
    // 否则一个已被撤销的会话还能靠不断请求来「保活」。
    if !db::session::touch(pool, &session.id, now).await? {
        return Err(Error::Unauthorized("该会话已失效".to_string()));
    }

    let account = db::account::find_by_id(pool, session.account_id)
        .await?
        .ok_or_else(|| Error::Unauthorized("账号不存在".to_string()))?;

    Ok((account, session))
}

/// 退出当前会话。
pub async fn logout(pool: &SqlitePool, session_id: &str, account_id: i64) -> Result<()> {
    db::session::revoke(pool, session_id, account_id, time::now_secs()).await?;
    Ok(())
}

/// 取当前账号。系统只维护一个账号，供 CLI 这类没有会话上下文的入口使用。
pub async fn sole_account(pool: &SqlitePool) -> Result<Account> {
    db::account::first(pool)
        .await?
        .ok_or_else(|| Error::NotFound("账号尚未初始化，请先启动一次服务".to_string()))
}

/// 重置密码：随机生成一个写入，并把明文返回给调用方打印一次。
///
/// 刻意**不接受外部传入的密码**。这样口令只有两个来源——初始化时的随机生成，
/// 或者后台里带着当前密码的修改。任何入口都塞不进一个来历不明的口令，
/// 也就不存在「多个地方都能改数据」的口子。
pub async fn reset_password(pool: &SqlitePool, account_id: i64) -> Result<String> {
    let new_password = random::string(INITIAL_PASSWORD_LEN);
    let hash = password::hash(new_password.clone()).await?;

    db::account::update_password(pool, account_id, &hash, time::now_secs()).await?;

    tracing::info!(account_id, "已通过命令行重置密码");
    Ok(new_password)
}

/// 修改密码，返回被撤销的其它设备数。
///
/// 需要先验证当前密码，并撤销除本机以外的会话——多数人改密码正是因为怀疑泄露，
/// 只换口令却把旧会话留着，等于没改。
pub async fn change_password(
    pool: &SqlitePool,
    account: &Account,
    current_password: &str,
    new_password: &str,
    current_session_id: &str,
) -> Result<u64> {
    let matched =
        password::verify(current_password.to_string(), account.password_hash.clone()).await?;

    if !matched {
        return Err(Error::InvalidCredentials);
    }

    ensure_password_usable(new_password)?;

    if new_password == current_password {
        return Err(Error::BadRequest("新密码不能与当前密码相同".to_string()));
    }

    let hash = password::hash(new_password.to_string()).await?;
    let now = time::now_secs();

    // 换口令与踢设备必须一起生效。只改密码却没踢掉旧会话，恰恰是
    // 「怀疑泄露才来改密码」这个场景最不能接受的半成品状态；
    // 两次写操作之间任何一次失败都会留下这种状态，所以放进同一个事务。
    let mut tx = pool.begin().await?;

    db::account::update_password(&mut *tx, account.id, &hash, now).await?;
    let revoked = db::session::revoke_others(&mut *tx, account.id, current_session_id, now).await?;

    tx.commit().await?;

    tracing::info!(account = %account.username, revoked, "密码已更新，其它设备已退出");
    Ok(revoked)
}

/// 新密码的最低要求：不为难用户，但也不接受空密码。
fn ensure_password_usable(password: &str) -> Result<()> {
    if password.is_empty() {
        return Err(Error::BadRequest("密码不能为空".to_string()));
    }
    Ok(())
}

/// 用户名的长度上限。
const MAX_USERNAME_LEN: usize = 64;

/// 修改用户名，返回更新后的账号。
///
/// 同样要求当前密码：光有会话不等于就是账号主人，而改名会把原主人挡在门外，
/// 属于能被用来锁账号的操作。
pub async fn change_username(
    pool: &SqlitePool,
    account: &Account,
    current_password: &str,
    new_username: &str,
) -> Result<Account> {
    let matched =
        password::verify(current_password.to_string(), account.password_hash.clone()).await?;

    if !matched {
        return Err(Error::InvalidCredentials);
    }

    let new_username = validate_username(new_username)?;

    if new_username == account.username {
        return Err(Error::BadRequest("新用户名与当前用户名相同".to_string()));
    }

    db::account::update_username(pool, account.id, new_username, time::now_secs()).await?;

    tracing::info!(old = %account.username, new = new_username, "用户名已更新");

    db::account::find_by_id(pool, account.id)
        .await?
        .ok_or_else(|| Error::Internal(anyhow::anyhow!("账号在更新后消失")))
}

/// 用户名的基本校验：非空、去掉首尾空白、长度可控。
fn validate_username(username: &str) -> Result<&str> {
    let username = username.trim();

    if username.is_empty() {
        return Err(Error::BadRequest("用户名不能为空".to_string()));
    }

    if username.chars().count() > MAX_USERNAME_LEN {
        return Err(Error::BadRequest(format!(
            "用户名不能超过 {MAX_USERNAME_LEN} 个字符"
        )));
    }

    Ok(username)
}
