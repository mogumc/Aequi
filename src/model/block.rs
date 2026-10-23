//! 上游块与密钥。
//!
//! 「块」是上游的调度单位：一个块**只使用一种 API 格式**，块内配多个密钥，
//! 密钥的增删、暂停、测试、恢复、退避、停用都以块为范围进行。

use serde::{Deserialize, Serialize};

/// 上游 API 格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApiFormat {
    Openai,
    Google,
    Anthropic,
}

impl ApiFormat {
    /// 存库用的标识。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Openai => "openai",
            Self::Google => "google",
            Self::Anthropic => "anthropic",
        }
    }

    /// 从库里的标识解析。
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "openai" => Some(Self::Openai),
            "google" => Some(Self::Google),
            "anthropic" => Some(Self::Anthropic),
            _ => None,
        }
    }
}

/// 模型条目：本块对外接受的名字、转发给上游时替换成的名字，以及计费倍率。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    /// 对外的模型名，也就是用户请求里写的那个。
    pub name: String,
    /// 转发给上游时使用的名字。留空表示同名转发。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forward: Option<String>,
    /// 计费倍率，万分比整数（10000 = 1.0 倍）。
    ///
    /// 倍率跟着模型走，而模型是块内配置的——同一个模型名在不同块里
    /// 可以有不同的价。旧数据缺这个字段时按 1.0 处理。
    #[serde(default = "default_multiplier_bp")]
    pub multiplier_bp: i64,
    /// 额外请求体：只允许**管理端**在这里配置，下游请求体里的同名键一律丢弃。
    ///
    /// 按**上游格式**书写（块绑定的是哪家就写哪家），原样并入发往上游的
    /// 请求体顶层，用于网关没暴露的参数（`top_k`、`seed`、Google 的
    /// `generationConfig` 等）。`model` / 消息内容 / 工具 / `stream*`
    /// 这些由网关掌管的键不允许出现，`validate_models` 会拒。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_body: Option<serde_json::Value>,
}

/// 未配置倍率时的默认值：1.0 倍。
fn default_multiplier_bp() -> i64 {
    crate::model::credits::DEFAULT_MULTIPLIER_BP
}

impl ModelEntry {
    /// 转发时实际使用的模型名。
    pub fn target(&self) -> &str {
        self.forward.as_deref().unwrap_or(&self.name)
    }
}

/// 一个自定义请求头。
///
/// 有些上游（自建网关、中转站、企业版接口）会要求额外的头才能用：
/// 指定版本的 `anthropic-version`、租户/组织的 `OpenAI-Organization`、
/// 网关自己的路由头，或者把认证整条换成只认某种写法的 `Authorization`。
/// 这些都不该逼着用户去改代码，所以按块配置。
///
/// 校验都在写入口（`service::upstream::normalize_headers`）做一次：头名与头值必须
/// 能构成合法的 HTTP 头、同名不许配两遍、`content-length` 这类由 HTTP 栈自己管的
/// 头不许配。这里只是记录口径，判断不在这儿。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeaderEntry {
    pub name: String,
    pub value: String,
}

/// 一个上游块。
#[derive(Debug, Clone)]
pub struct UpstreamBlock {
    pub id: String,
    pub name: String,
    pub api_format: ApiFormat,
    /// 调度权重：同一个模型挂在多个块上时，按平滑加权轮询分配流量。
    pub weight: u32,
    /// 同时发往该上游的请求数上限。0 = 不限制。
    ///
    /// 按块配置：每个块的出口与配额彼此独立，上限也该各自权衡。
    /// 达到上限后新请求在本块内排队（FIFO），等太久会明确失败，
    /// 不会把客户端无限期挂住。
    pub max_concurrency: u32,
    pub base_url: String,
    /// 该块出网使用的代理，例如 `http://127.0.0.1:7890`。
    pub proxy: Option<String>,
    /// 本块接受并转发的模型。
    pub models: Vec<ModelEntry>,
    /// 每次发往该上游都额外带上的请求头。空数组表示不加。
    ///
    /// 生效范围是整块：对话、生图、密钥探测与拉取模型走同一个出口，
    /// 所以「探测通过」与「真实请求能通」始终是同一个条件。
    pub headers: Vec<HeaderEntry>,
    pub enabled: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

impl UpstreamBlock {
    /// 是否接受该模型。
    pub fn accepts(&self, model: &str) -> bool {
        self.models.iter().any(|entry| entry.name == model)
    }

    /// 转发时使用的模型名。不在列表里时原样返回，由上游去判断。
    pub fn target_model(&self, model: &str) -> String {
        self.models
            .iter()
            .find(|entry| entry.name == model)
            .map(|entry| entry.target().to_string())
            .unwrap_or_else(|| model.to_string())
    }

    /// 该模型在本块的计费倍率（万分比）。
    ///
    /// 块里没列这个模型时返回默认倍率——能走到计费说明请求已经被本块接下了，
    /// 此时缺一条倍率配置不该让请求失败。
    pub fn multiplier_bp(&self, model: &str) -> i64 {
        self.models
            .iter()
            .find(|entry| entry.name == model)
            .map(|entry| entry.multiplier_bp)
            .unwrap_or(crate::model::credits::DEFAULT_MULTIPLIER_BP)
    }

    /// 该模型在本块的额外请求体（管理端配置）。
    ///
    /// 只从块配置里取——下游请求体里的同名键在入口解析层就被丢弃，
    /// 这里不做任何合并判断，交给 `UpstreamRequest::apply_extra_body`。
    pub fn extra_body(&self, model: &str) -> Option<&serde_json::Value> {
        self.models
            .iter()
            .find(|entry| entry.name == model)
            .and_then(|entry| entry.extra_body.as_ref())
    }
}

/// 密钥状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyStatus {
    /// 可用。
    Active,
    /// 被用户手动暂停。
    Paused,
    /// 因连续报错被自动停用，需要人工确认后恢复。
    Disabled,
}

impl KeyStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Disabled => "disabled",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "active" => Some(Self::Active),
            "paused" => Some(Self::Paused),
            "disabled" => Some(Self::Disabled),
            _ => None,
        }
    }
}

/// 一条上游密钥。
#[derive(Debug, Clone)]
pub struct UpstreamKey {
    pub id: String,
    pub block_id: String,
    /// 明文保存：上游密钥需要能取回来发给上游，散列没有意义。
    pub secret: String,
    pub status: KeyStatus,
    /// 免于被自动停用。
    ///
    /// 连续失败只记录、不改状态；429 的退避照常生效
    /// （退避是几十秒级的临时避让，不是停用）。手动暂停/恢复不受影响。
    /// 管理员测试确认无效时仍会停用——那是带证据的人工动作，不是自动规则。
    pub never_disable: bool,
    pub weight: u32,
    /// 连续失败次数（401/403 与 429 累加，上游 5xx 不计），成功后清零。
    ///
    /// 401/403 的停用阈值与 429 的退避阶梯都读它。
    pub failure_count: u32,
    /// 退避截止时间（只有 429 会设）；在此之前不参与调度。
    pub cooldown_until: Option<i64>,
    pub last_error: Option<String>,
    pub last_used_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl UpstreamKey {
    /// 在给定时刻能否被选中。
    ///
    /// 暂停与停用都不参与调度；处于退避期内的同样跳过。
    pub fn is_selectable(&self, now: i64) -> bool {
        self.status == KeyStatus::Active
            && self
                .cooldown_until
                .map(|until| until <= now)
                .unwrap_or(true)
    }
}

/// 块的对外视图。
#[derive(Debug, Clone, Serialize)]
pub struct BlockView {
    pub id: String,
    pub name: String,
    pub api_format: ApiFormat,
    pub weight: u32,
    /// 同时发往该上游的请求数上限。0 = 不限制。
    pub max_concurrency: u32,
    pub base_url: String,
    pub proxy: Option<String>,
    pub models: Vec<ModelEntry>,
    /// 自定义请求头（明文——与上游密钥同一套取舍，管理员要能取回来核对）。
    pub headers: Vec<HeaderEntry>,
    pub enabled: bool,
    /// 密钥总数。
    pub key_count: usize,
    /// 当前可调度的密钥数。
    pub available_keys: usize,
    /// 正在发往上游的请求数（读取瞬间的快照）。
    ///
    /// 块不限制并发（`max_concurrency` 为 0）时为 `None`——没有闸门，
    /// 也就没有「在飞数」这个数。
    pub in_flight: Option<u32>,
    /// 排在并发闸门后的请求数（快照）。涨说明在飞数常年打满，
    /// 该考虑调大上限或加块了。
    pub queued: Option<u32>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 上游密钥的对外视图。
///
/// 明文返回——与访问密钥同一套取舍：管理员要能取回来核对。
#[derive(Debug, Clone, Serialize)]
pub struct UpstreamKeyView {
    pub id: String,
    pub block_id: String,
    pub secret: String,
    pub status: KeyStatus,
    /// 免于被自动停用（失败只记录，不改状态）。
    pub never_disable: bool,
    pub weight: u32,
    pub failure_count: u32,
    pub cooldown_until: Option<i64>,
    pub last_error: Option<String>,
    pub last_used_at: Option<i64>,
    pub created_at: i64,
}

/// 创建块的请求体。
#[derive(Debug, Deserialize)]
pub struct BlockCreate {
    /// 用户自己指定的标识。
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub api_format: ApiFormat,
    #[serde(default = "default_weight")]
    pub weight: u32,
    /// 同时发往该上游的请求数上限。缺省 0（不限制）。
    #[serde(default)]
    pub max_concurrency: u32,
    pub base_url: String,
    #[serde(default)]
    pub proxy: Option<String>,
    #[serde(default)]
    pub models: Vec<ModelEntry>,
    /// 自定义请求头，缺省为空。
    #[serde(default)]
    pub headers: Vec<HeaderEntry>,
}

/// 更新块的请求体。只提交需要改的字段。
#[derive(Debug, Default, Deserialize)]
pub struct BlockUpdate {
    pub name: Option<String>,
    pub api_format: Option<ApiFormat>,
    pub weight: Option<u32>,
    /// 并发上限；0 表示不限制。不传则保持原值。
    pub max_concurrency: Option<u32>,
    pub base_url: Option<String>,
    /// 显式传 `null` 表示清空代理，不传则保持原值。
    #[serde(default, deserialize_with = "double_option")]
    pub proxy: Option<Option<String>>,
    pub models: Option<Vec<ModelEntry>>,
    /// 整份替换：传 `[]` 表示清空，不传则保持原值。
    ///
    /// 不做「按名字逐条合并」——头是有序配置，合并规则只会让人猜不到结果。
    pub headers: Option<Vec<HeaderEntry>>,
    pub enabled: Option<bool>,
}

/// 区分「没传」与「传了 null」。
///
/// 代理这类可空字段需要它：不传是保持原值，传 null 才是清空。
fn double_option<'de, D>(deserializer: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer).map(Some)
}

/// 新增上游密钥的请求体。
#[derive(Debug, Deserialize)]
pub struct UpstreamKeyCreate {
    /// 一次性提交多个密钥。
    pub secrets: Vec<String>,
    #[serde(default = "default_weight")]
    pub weight: u32,
    /// 入库即免于被自动停用。缺省关闭。
    #[serde(default)]
    pub never_disable: bool,
}

fn default_weight() -> u32 {
    1
}

/// 批量操作的请求体。
#[derive(Debug, Deserialize)]
pub struct KeyIdsRequest {
    pub ids: Vec<String>,
}

/// 批量设置「永不暂停」的请求体。
#[derive(Debug, Deserialize)]
pub struct KeyNeverDisableRequest {
    pub ids: Vec<String>,
    /// 开启还是关闭；缺省开启——不带参数的请求就是「设上」。
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

/// 批量设置调度权重的请求体。
///
/// 权重就是块内平滑加权轮询的份额，0 会让密钥永远选不中，
/// 所以 `weight` 必填且由服务层挡住 0（要停用请用暂停）。
#[derive(Debug, Deserialize)]
pub struct KeyWeightRequest {
    pub ids: Vec<String>,
    pub weight: u32,
}

/// 块内单把密钥的测试结果。
#[derive(Debug, Clone, Serialize)]
pub struct KeyTestResult {
    pub key_id: String,
    /// 密钥前缀，便于辨认；不回传完整密钥。
    pub secret_prefix: String,
    /// `valid` | `invalid` | `transient`
    pub outcome: &'static str,
    pub detail: Option<String>,
}

/// 整块连通性测试的结果。
#[derive(Debug, Clone, Serialize)]
pub struct BlockTestResult {
    pub block_id: String,
    pub tested: usize,
    pub valid: usize,
    pub invalid: usize,
    pub transient: usize,
    pub results: Vec<KeyTestResult>,
}
