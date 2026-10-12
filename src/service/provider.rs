//! 第三方登录的预留接口。
//!
//! 这里只定义契约与注册表，**未接入任何具体提供方**。
//! 后续新增登录方式时，实现 [`IdentityProvider`] 并在
//! [`ProviderRegistry`] 中注册即可，无需改动登录主流程。
//!
//! 会话表已预留 `login_type` 字段，第三方登录产生的会话会记录为
//! `provider:<id>`，与密码登录区分开。

use async_trait::async_trait;
use serde::Serialize;

use crate::error::Result;

/// 由外部提供方返回的身份信息。
#[derive(Debug, Clone)]
pub struct ExternalIdentity {
    /// 提供方内的唯一用户标识。
    pub subject: String,
    pub display_name: Option<String>,
    pub email: Option<String>,
}

/// 一种第三方登录方式。
#[async_trait]
pub trait IdentityProvider: Send + Sync {
    /// 唯一标识，例如 `github`。
    fn id(&self) -> &'static str;

    /// 展示名。
    fn display_name(&self) -> &'static str;

    /// 生成跳转到提供方的授权地址。
    fn authorize_url(&self, state: &str, redirect_uri: &str) -> String;

    /// 用回调携带的 code 换取外部身份。
    async fn exchange(&self, code: &str, redirect_uri: &str) -> Result<ExternalIdentity>;
}

/// 已注册的第三方登录方式。
#[derive(Default)]
pub struct ProviderRegistry {
    providers: Vec<Box<dyn IdentityProvider>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一种登录方式。
    pub fn register(&mut self, provider: Box<dyn IdentityProvider>) {
        self.providers.push(provider);
    }

    /// 是否尚未接入任何第三方登录。
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    /// 按标识查找。
    pub fn get(&self, id: &str) -> Option<&dyn IdentityProvider> {
        self.providers
            .iter()
            .find(|provider| provider.id() == id)
            .map(|provider| provider.as_ref())
    }

    /// 列出可用登录方式，供前端渲染。
    pub fn list(&self) -> Vec<ProviderInfo> {
        self.providers
            .iter()
            .map(|provider| ProviderInfo {
                id: provider.id().to_string(),
                display_name: provider.display_name().to_string(),
            })
            .collect()
    }
}

/// 登录方式的对外描述。
#[derive(Debug, Clone, Serialize)]
pub struct ProviderInfo {
    pub id: String,
    pub display_name: String,
}
