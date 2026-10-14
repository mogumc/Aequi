//! 上游块与密钥的管理与调度。
//!
//! 管理面（增删改查、暂停、恢复、测试）在这里；调度面只有两个入口——
//! 按模型挑块、块内挑密钥，都走平滑加权轮询。
//!
//! 密钥的生命周期是自治的：请求成功清零失败计数；可恢复的失败按连续
//! 次数指数退避；只有密钥被上游明确拒绝（401/403）累计到阈值才自动
//! 停用——限流、上游 5xx、网络抖动这类短期故障不该把好密钥永久禁掉。
//! `never_disable` 的密钥连自动停用也免掉。

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sqlx::SqlitePool;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::db;
use crate::error::{Error, Result};
use crate::model::{
    ApiKey, BlockCreate, BlockUpdate, BlockView, HeaderEntry, KeyStatus, ModelEntry, UpstreamBlock,
    UpstreamKey, UpstreamKeyCreate, UpstreamKeyView,
};
use crate::proxy;
use crate::proxy::adapter::{ProbeOutcome, adapter_for};
use crate::utils::{random, time};

/// 失败的分类，决定后续动作。
///
/// - [`Hard`](FailureKind::Hard)：密钥本身被上游拒绝（401/403）。换密钥也救不了，
///   累计到阈值就自动停用。
/// - [`RateLimited`](FailureKind::RateLimited)：被限流（429）。指数退避避让，永不停用。
/// - [`Transient`](FailureKind::Transient)：上游瞬时故障（5xx、网络、200 但结构不对）。
///   同样指数退避，但**不计入停用**——上游打个嗝不该把好密钥永久禁掉。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    Hard,
    RateLimited,
    Transient,
}

/// 连续硬失败（密钥被上游拒绝）达到该次数后自动停用密钥。
///
/// 只数 [`FailureKind::Hard`]；限流与瞬时故障走退避，不碰停用判定。
const FAILURE_THRESHOLD: u32 = 3;
/// 退避基础时长（秒）：第一次可恢复失败后休息这么久。
const BACKOFF_BASE_SECS: i64 = 30;
/// 退避时长上限（秒）：指数到顶就不再涨，免得一把密钥被雪藏太久。
const BACKOFF_MAX_SECS: i64 = 480;
/// 指数退避的最大移位：30s × 2^4 = 480s 到顶。
const BACKOFF_MAX_SHIFT: u32 = 4;
/// 排队等位的最长时间。
///
/// 等不到就明说「通道繁忙」，别把客户端无限期挂住——它那头的超时
/// 比我们更短，挂着只是让两边都说不清发生了什么。
pub const GATE_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
/// 块并发上限的防呆上限。
///
/// 单机网关到不了这个量级，纯粹挡住「多打一个零」这种配置事故。
const MAX_BLOCK_CONCURRENCY: u32 = 1024;
/// 发往上游的请求超时。
/// 整个请求（含读流）的总时长上限。思考型模型输出慢，给足余量。
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(120);
/// 连接池空闲超时。池里放太久的连接可能已被服务端静默关闭，
/// 复用到它就会表现为「响应读到一半流就没了」且概率性出现。
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// 这一次该退避多久：按连续失败次数指数增长，到顶封顶。
///
/// `failures` 是含本次在内的连续失败次数：第一次 30s，之后每多一次
/// 翻一倍，第五次起固定 480s。
fn backoff_secs(failures: u32) -> i64 {
    let shift = failures.saturating_sub(1).min(BACKOFF_MAX_SHIFT);
    (BACKOFF_BASE_SECS << shift).min(BACKOFF_MAX_SECS)
}

/// 上游的运行时状态：轮询累积值、每块的 HTTP 客户端与并发闸门。
#[derive(Default)]
pub struct UpstreamState {
    /// 平滑加权轮询的累积值。键是块或密钥的 id。
    currents: Mutex<HashMap<String, i64>>,
    /// 每块一个客户端，连接池跨请求复用；块配置变更时失效。
    clients: Mutex<HashMap<String, reqwest::Client>>,
    /// 被「不认识 stream_options」的上游拉黑的块：对这些块不再注入
    /// include_usage。进程内记录，重启后重新试探——降级是自动的。
    no_usage_inject: Mutex<HashMap<String, ()>>,
    /// 每块的并发闸门：键是块 id。
    ///
    /// 容量为 0（不限制）的块不建门。容量变化时换一个新门：已在
    /// 旧门后排队的请求按旧容量放行（上界就是旧容量，不会超额），
    /// 新请求立即按新容量走——调大立即生效，调小最多让老队列的人走完。
    gates: Mutex<HashMap<String, Arc<BlockGate>>>,
}

/// 一个块的并发闸门。
///
/// tokio 的信号量不暴露「有多少人在等」，而排队数正是运维最想看的数字
/// 之一——在飞数打满可能只是忙，排队数持续不降才是真堵了，所以自己数。
pub struct BlockGate {
    /// 建门时的容量，与块的 `max_concurrency` 一致。
    capacity: u32,
    semaphore: Arc<Semaphore>,
    /// 等位子的人数：进闸门前 +1，拿到位子或放弃后 -1。
    queued: Arc<AtomicUsize>,
}

impl BlockGate {
    fn new(capacity: u32) -> Arc<Self> {
        Arc::new(Self {
            capacity,
            semaphore: Arc::new(Semaphore::new(capacity as usize)),
            queued: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// 正在发往上游的请求数：容量扣掉剩余位子。
    pub fn in_flight(&self) -> u32 {
        self.capacity
            .saturating_sub(self.semaphore.available_permits() as u32)
    }

    /// 排在闸门后的请求数。
    pub fn queued(&self) -> u32 {
        self.queued.load(Ordering::Relaxed) as u32
    }

    /// 等一个位子，排队期间计数。
    ///
    /// 计数由 [`QueuedGuard`] 在丢弃时收回，所以**超时取消也安全**——
    /// 调用方（`proxy::acquire_gate` 的 `timeout`）丢下这个 future 时，
    /// 排队数照样减回去，不会虚高。
    ///
    /// 返回的位子与闸门同生命周期：调用方持有多久，在飞数就记多久。
    pub async fn acquire(&self) -> Result<OwnedSemaphorePermit> {
        let _queued = QueuedGuard::enter(&self.queued);
        self.semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|error| Error::Internal(anyhow::anyhow!("并发闸门异常: {error}")))
    }
}

/// 排队计数守卫：构造时 +1，丢弃时 -1（无论原因）。
struct QueuedGuard {
    counter: Arc<AtomicUsize>,
}

impl QueuedGuard {
    fn enter(counter: &Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self {
            counter: Arc::clone(counter),
        }
    }
}

impl Drop for QueuedGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::Relaxed);
    }
}

/// 块的实时并发负载快照。
#[derive(Debug, Clone, Copy)]
pub struct BlockLoad {
    /// 正在发往上游的请求数。
    pub in_flight: u32,
    /// 排在闸门后的请求数。
    pub queued: u32,
}

impl UpstreamState {
    /// 平滑加权轮询选一个候选。
    ///
    /// 每轮给每个候选加上自身权重，取累积值最大者并扣回总权重——
    /// 权重大的被选得频繁，但不会连续压同一个；同分时按传入顺序取先者，
    /// 这就是「权重 + 顺序」的全部含义。
    pub fn pick(&self, candidates: &[(String, u32)]) -> Option<String> {
        let total: i64 = candidates
            .iter()
            .map(|(_, weight)| i64::from(*weight))
            .sum();
        if total == 0 {
            return None;
        }

        let mut currents = self.currents.lock().ok()?;
        let mut best: Option<(String, i64)> = None;

        for (id, weight) in candidates {
            let value = currents.entry(id.clone()).or_insert(0);
            *value += i64::from(*weight);

            if best.as_ref().is_none_or(|(_, top)| *value > *top) {
                best = Some((id.clone(), *value));
            }
        }

        let (picked, _) = best?;
        if let Some(value) = currents.get_mut(&picked) {
            *value -= total;
        }

        Some(picked)
    }

    /// 候选消失时把它们的轮询状态一并清掉，免得越积越多。
    pub fn forget(&self, ids: &[String]) {
        if let Ok(mut currents) = self.currents.lock() {
            for id in ids {
                currents.remove(id);
            }
        }
    }

    /// 该块是否允许注入 `stream_options.include_usage`。
    pub fn usage_inject_allowed(&self, block_id: &str) -> bool {
        !self
            .no_usage_inject
            .lock()
            .is_ok_and(|set| set.contains_key(block_id))
    }

    /// 上游用 400 拒绝了注入字段：拉黑该块，之后剥离注入重试。
    pub fn disable_usage_inject(&self, block_id: &str) {
        if let Ok(mut set) = self.no_usage_inject.lock() {
            if set.insert(block_id.to_string(), ()).is_none() {
                tracing::warn!(
                    block = %block_id,
                    "上游不认识 stream_options，已停止对该块注入 include_usage"
                );
            }
        }
    }

    /// 取该块的并发闸门。块不限制并发（`max_concurrency` 为 0）时返回 `None`。
    ///
    /// 容量与缓存不一致就换一个新门（见结构体注释）：调大立即生效，
    /// 调小只影响后续请求。调用方拿到的是 `Arc`，队列里排着的人不受换门影响。
    pub fn gate(&self, block: &UpstreamBlock) -> Option<Arc<BlockGate>> {
        if block.max_concurrency == 0 {
            return None;
        }

        let mut gates = self.gates.lock().ok()?;
        let entry = gates
            .entry(block.id.clone())
            .or_insert_with(|| BlockGate::new(block.max_concurrency));
        if entry.capacity != block.max_concurrency {
            *entry = BlockGate::new(block.max_concurrency);
        }
        Some(Arc::clone(entry))
    }

    /// 该块的实时并发负载。没有闸门（不限制并发）时返回 `None`。
    ///
    /// 给管理面板看的快照：调用即建门（有限额的块迟早要有门），
    /// 所以有限额的块即使没有流量也报 `0 / 0`，不会忽有忽无。
    pub fn load(&self, block: &UpstreamBlock) -> Option<BlockLoad> {
        let gate = self.gate(block)?;
        Some(BlockLoad {
            in_flight: gate.in_flight(),
            queued: gate.queued(),
        })
    }

    /// 块删除时清掉它的闸门。
    ///
    /// 删除时仍在飞的请求持有旧信号量的 `Arc`，跑完才释放；重建同 id 的块
    /// 会拿到一个满容量的新门——最坏情况是「删了又在飞的块又原地重建」这个
    /// 窗口里新旧流量短暂叠加，上界是旧门的容量，随后自然收敛。
    pub fn invalidate_gate(&self, block_id: &str) {
        if let Ok(mut gates) = self.gates.lock() {
            gates.remove(block_id);
        }
    }

    /// 取该块的 HTTP 客户端（含代理配置），没有就建一个缓存起来。
    pub fn client(&self, block: &UpstreamBlock) -> Result<reqwest::Client> {
        // Client 内部是 Arc，clone 只是加引用计数。
        let cached = self
            .clients
            .lock()
            .ok()
            .and_then(|map| map.get(&block.id).cloned());
        if let Some(client) = cached {
            return Ok(client);
        }

        let mut builder = reqwest::Client::builder()
            .timeout(UPSTREAM_TIMEOUT)
            .pool_idle_timeout(POOL_IDLE_TIMEOUT);
        if let Some(proxy) = &block.proxy {
            let proxy = reqwest::Proxy::all(proxy)
                .map_err(|error| Error::UpstreamUnavailable(format!("代理配置无效: {error}")))?;
            builder = builder.proxy(proxy);
        }
        let client = builder
            .build()
            .map_err(|error| Error::UpstreamUnavailable(format!("HTTP 客户端构建失败: {error}")))?;

        if let Ok(mut map) = self.clients.lock() {
            map.insert(block.id.clone(), client.clone());
        }

        Ok(client)
    }

    /// 块的出口配置变了，缓存里的客户端随之作废。
    pub fn invalidate_client(&self, block_id: &str) {
        if let Ok(mut map) = self.clients.lock() {
            map.remove(block_id);
        }
    }
}

// ---------- 管理面 ----------

/// 块 id 的长度上限。
const MAX_BLOCK_ID_LEN: usize = 64;

/// 校验块的 id。
///
/// 它会被当作请求里模型名的前缀（`块id/模型名`），所以只允许能安全出现在
/// URL 与模型名里的字符——含 `/` 或空格会直接破坏那条解析规则。
fn validate_block_id(id: &str) -> Result<()> {
    if id.is_empty() {
        return Err(Error::BadRequest("块的 id 不能为空".to_string()));
    }
    if id.chars().count() > MAX_BLOCK_ID_LEN {
        return Err(Error::BadRequest(format!(
            "块的 id 不能超过 {MAX_BLOCK_ID_LEN} 个字符"
        )));
    }

    let ok = id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'));
    if !ok {
        return Err(Error::BadRequest(
            "块的 id 只能用小写字母、数字、点、下划线或连字符——它会作为模型名前缀使用".to_string(),
        ));
    }

    Ok(())
}

/// 校验块的模型条目。
///
/// 倍率是计费的乘数，负数会让计费方向反转（消耗变收入），必须在入口挡住。
fn validate_models(models: &[ModelEntry]) -> Result<()> {
    for entry in models {
        if entry.name.trim().is_empty() {
            return Err(Error::BadRequest("模型名不能为空".to_string()));
        }
        if entry.multiplier_bp < 0 {
            return Err(Error::BadRequest(format!(
                "模型 {} 的倍率不能为负",
                entry.name
            )));
        }
    }
    Ok(())
}

/// 校验块的同时发往上游的请求数上限。
///
/// 0 = 不限制（缺省）；上限只为防呆——「多打一个零」会让整台机器
/// 的出口连接全部押到这个块上，而这种错误在日志里看一个月也看不出来。
fn validate_concurrency(max_concurrency: u32) -> Result<()> {
    if max_concurrency > MAX_BLOCK_CONCURRENCY {
        return Err(Error::BadRequest(format!(
            "块的并发上限不能超过 {MAX_BLOCK_CONCURRENCY}（0 表示不限制）"
        )));
    }
    Ok(())
}

/// 自定义请求头的条数上限。
///
/// 只是防呆：正常上游要的是三两个头，32 已经远超需要，但能挡住
/// 「整坨 JSON 粘进来」这种配置事故——那会让每条请求都拖着几十个头出门。
const MAX_HEADERS: usize = 32;

/// 单个头值的字符数上限。同样是防呆，不是协议限制。
const MAX_HEADER_VALUE_LEN: usize = 4096;

/// 不允许配置的头。
///
/// 这些由 HTTP 栈自己管（请求体长度、连接语义、升级协商），配了只会让
/// 请求畸形、或者让上游行为变得无从解释——浏览器的 fetch 也禁掉同一批。
/// 其余一律放行：`host`、乃至整条 `authorization`（有的上游就是要这种写法）
/// 都是真实存在的需求，由配置者自己负责。
const FORBIDDEN_HEADERS: &[&str] = &[
    "content-length",
    "transfer-encoding",
    "connection",
    "upgrade",
    "expect",
    "te",
    "trailer",
    "keep-alive",
];

/// 校验并归一化自定义请求头。
///
/// 在保存前跑一次，脏数据不进库——而且**校验与发送用的是同一套 HTTP 头类型**
/// （[`reqwest::header`] 就是 http crate 的那一套），所以这里放行的东西
/// 发请求时一定塞得进去，不会出现「存下来了但发不出去」。
///
/// 归一化 = 名字与值都去掉首尾空白（粘贴进来的多半带着）；校验三条，
/// 都是「现在挡住、否则事后很难查」的：
///
/// - 名字与值必须能构成一个合法的 HTTP 头（`\r\n` 之类直接构成头注入）；
/// - 同一个名字不许配两遍：应用时按名字覆盖，配两遍就说不清哪条生效；
/// - 不许配由 HTTP 栈自己管的头（见 [`FORBIDDEN_HEADERS`]）。
fn normalize_headers(headers: Vec<HeaderEntry>) -> Result<Vec<HeaderEntry>> {
    if headers.len() > MAX_HEADERS {
        return Err(Error::BadRequest(format!(
            "自定义请求头最多 {MAX_HEADERS} 条"
        )));
    }

    let mut seen = std::collections::HashSet::with_capacity(headers.len());
    let mut normalized = Vec::with_capacity(headers.len());

    for entry in headers {
        let name = entry.name.trim().to_string();
        let value = entry.value.trim().to_string();

        if name.is_empty() {
            return Err(Error::BadRequest("请求头名不能为空".to_string()));
        }
        let parsed = reqwest::header::HeaderName::try_from(name.as_str())
            .map_err(|_| Error::BadRequest(format!("请求头名不合法: {name}")))?;
        if reqwest::header::HeaderValue::try_from(value.as_str()).is_err() {
            return Err(Error::BadRequest(format!(
                "请求头 {name} 的值不合法（不能含换行等控制字符）"
            )));
        }
        if value.chars().count() > MAX_HEADER_VALUE_LEN {
            return Err(Error::BadRequest(format!(
                "请求头 {name} 的值不能超过 {MAX_HEADER_VALUE_LEN} 个字符"
            )));
        }

        // 头名大小写不敏感，去重与禁用名单都按小写比对。
        let lower = parsed.as_str().to_string();
        if FORBIDDEN_HEADERS.contains(&lower.as_str()) {
            return Err(Error::BadRequest(format!(
                "请求头 {name} 由 HTTP 栈自己管理，不能配在块上"
            )));
        }
        if !seen.insert(lower) {
            return Err(Error::BadRequest(format!("请求头 {name} 配置了多次")));
        }

        normalized.push(HeaderEntry { name, value });
    }

    Ok(normalized)
}

/// 该密钥当前可用的模型名，含**短名**与**带块前缀的全名**两种写法。
///
/// 短名（`gpt-4o`）在该密钥**被允许的**那些块之间加权轮询；全名
/// （`block-a/gpt-4o`）把请求锁定到那个块。白名单为空即不限制。
///
/// 短名只在**至少有一个被允许的块**提供它时才出现——否则列出来也调不通。
pub async fn available_models(pool: &SqlitePool, key: &ApiKey) -> Result<Vec<String>> {
    let blocks = db::block::list_blocks(pool).await?;

    let mut short = std::collections::BTreeSet::new();
    let mut full = std::collections::BTreeSet::new();

    for block in blocks.iter().filter(|block| block.enabled) {
        for model in &block.models {
            if !key.allows_capability(&block.id, &model.name) {
                continue;
            }
            short.insert(model.name.clone());
            full.insert(format!("{}/{}", block.id, model.name));
        }
    }

    Ok(short.into_iter().chain(full).collect())
}

/// 密钥侧自助视图里的一条模型路由：某启用块内的某模型及其计费倍率。
///
/// 脱敏边界：只暴露调用时本来就公开的东西——块 id（调用前缀）、模型名、
/// 倍率。转发名发往上游才有意义，上游出口与密钥则完全不出这个视图。
#[derive(Debug, Clone, serde::Serialize)]
pub struct KeyModelRoute {
    pub block_id: String,
    pub model: String,
    /// 计费倍率，两位小数字符串（0.01 精度）。
    pub multiplier: String,
}

/// 该密钥可用的模型路由（块/模型 对），只含**启用块**，按模型名、
/// 再按块 id 排序。与 [`available_models`] 同一套白名单过滤口径
/// （[`crate::model::ApiKey::allows_capability`]），但保留结构化倍率——
/// `/v1/models` 那份是给 OpenAI SDK 的纯名字列表，这里才是给面板看的。
pub async fn key_model_routes(pool: &SqlitePool, key: &ApiKey) -> Result<Vec<KeyModelRoute>> {
    let blocks = db::block::list_blocks(pool).await?;
    let mut routes = Vec::new();

    for block in blocks.iter().filter(|block| block.enabled) {
        for model in &block.models {
            if !key.allows_capability(&block.id, &model.name) {
                continue;
            }
            routes.push(KeyModelRoute {
                block_id: block.id.clone(),
                model: model.name.clone(),
                multiplier: crate::model::credits::format_multiplier(model.multiplier_bp),
            });
        }
    }

    routes.sort_by(|a, b| a.model.cmp(&b.model).then_with(|| a.block_id.cmp(&b.block_id)));
    Ok(routes)
}

/// 用块自己的出口与密钥去上游拉一次可用模型列表。
///
/// 复用的就是密钥探测那条请求——它本来就是各家的「列模型」接口，
/// 所以探测与拉取共用一个出口，不会出现「探测通过但拉不到模型」的偏差。
pub async fn fetch_models(
    pool: &SqlitePool,
    state: &UpstreamState,
    id: &str,
) -> Result<Vec<String>> {
    let block = get_block(pool, id).await?;

    let keys = db::block::list_keys(pool, id).await?;
    let key = keys
        .iter()
        .find(|key| matches!(key.status, KeyStatus::Active))
        .ok_or_else(|| Error::BadRequest("该块还没有可用的上游密钥".to_string()))?;

    let adapter = adapter_for(block.api_format);
    let client = state.client(&block)?;
    let request = proxy::prepare_request(
        &client,
        &block,
        &adapter.build_probe(),
        adapter.auth_style(),
        &key.secret,
    )?;

    let response = request
        .send()
        .await
        .map_err(|error| Error::UpstreamUnavailable(format!("拉取模型失败: {error}")))?;

    let status = response.status();
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|error| Error::UpstreamUnavailable(format!("上游响应不是 JSON: {error}")))?;

    if !status.is_success() {
        let detail = body
            .pointer("/error/message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("上游拒绝了这次请求");
        return Err(Error::UpstreamUnavailable(format!(
            "拉取模型失败（HTTP {status}）：{detail}"
        )));
    }

    let mut names = adapter.parse_models(&body);
    names.retain(|name| !name.trim().is_empty());
    names.sort();
    names.dedup();

    if names.is_empty() {
        return Err(Error::UpstreamUnavailable(
            "上游没有返回任何模型".to_string(),
        ));
    }

    Ok(names)
}

/// 创建块。
pub async fn create_block(pool: &SqlitePool, payload: BlockCreate) -> Result<UpstreamBlock> {
    let id = payload.id.trim().to_string();
    validate_block_id(&id)?;
    if payload.base_url.trim().is_empty() {
        return Err(Error::BadRequest("base_url 不能为空".to_string()));
    }
    validate_models(&payload.models)?;
    validate_concurrency(payload.max_concurrency)?;
    let headers = normalize_headers(payload.headers)?;

    let now = time::now_secs();
    let block = UpstreamBlock {
        id,
        name: payload.name.trim().to_string(),
        api_format: payload.api_format,
        weight: payload.weight,
        max_concurrency: payload.max_concurrency,
        base_url: payload.base_url.trim().trim_end_matches('/').to_string(),
        proxy: payload.proxy,
        models: payload.models,
        headers,
        enabled: true,
        created_at: now,
        updated_at: now,
    };

    // 用户自己起的名字撞上了现有块，要给出可读的错误。
    let result = db::block::insert_block(pool, &block).await;
    if let Err(error) = &result {
        if db::block::is_unique_violation(error) {
            return Err(Error::BadRequest(format!("块 id `{}` 已存在", block.id)));
        }
    }
    result?;

    Ok(block)
}

/// 更新块。未提交的字段保持原值。
pub async fn update_block(
    pool: &SqlitePool,
    state: &UpstreamState,
    id: &str,
    patch: BlockUpdate,
) -> Result<UpstreamBlock> {
    let BlockUpdate {
        name,
        api_format,
        weight,
        max_concurrency,
        base_url,
        proxy,
        models,
        headers,
        enabled,
    } = patch;

    if let Some(models) = &models {
        validate_models(models)?;
    }
    if let Some(max_concurrency) = max_concurrency {
        validate_concurrency(max_concurrency)?;
    }
    // 整份替换：传 `[]` 就是清空，不做按名字的增量合并（那是猜）。
    let headers = match headers {
        Some(headers) => Some(normalize_headers(headers)?),
        None => None,
    };

    let current = get_block(pool, id).await?;
    let mut block = current;

    if let Some(name) = name {
        block.name = name.trim().to_string();
    }
    if let Some(api_format) = api_format {
        block.api_format = api_format;
    }
    if let Some(weight) = weight {
        block.weight = weight;
    }
    if let Some(max_concurrency) = max_concurrency {
        block.max_concurrency = max_concurrency;
    }
    if let Some(base_url) = base_url {
        block.base_url = base_url.trim().trim_end_matches('/').to_string();
    }
    if let Some(proxy) = proxy {
        block.proxy = proxy;
    }
    if let Some(models) = models {
        block.models = models;
    }
    if let Some(headers) = headers {
        block.headers = headers;
    }
    if let Some(enabled) = enabled {
        block.enabled = enabled;
    }
    block.updated_at = time::now_secs();

    db::block::update_block(pool, &block).await?;

    // 出口变了，缓存的客户端就该重建。
    state.invalidate_client(id);

    Ok(block)
}

/// 删除块及其全部密钥。
pub async fn delete_block(pool: &SqlitePool, state: &UpstreamState, id: &str) -> Result<()> {
    if !db::block::delete_block(pool, id).await? {
        return Err(Error::NotFound("块不存在".to_string()));
    }

    state.forget(&[id.to_string()]);
    state.invalidate_client(id);
    state.invalidate_gate(id);
    Ok(())
}

/// 按 ID 取块。
pub async fn get_block(pool: &SqlitePool, id: &str) -> Result<UpstreamBlock> {
    db::block::find_block(pool, id)
        .await?
        .ok_or_else(|| Error::NotFound("块不存在".to_string()))
}

/// 列出块，附带密钥统计与实时并发负载。
pub async fn list_blocks(pool: &SqlitePool, state: &UpstreamState) -> Result<Vec<BlockView>> {
    let blocks = db::block::list_blocks(pool).await?;

    let mut views = Vec::with_capacity(blocks.len());
    for block in &blocks {
        views.push(block_view_of(pool, state, block).await?);
    }

    Ok(views)
}

/// 单个块的视图。
pub async fn block_view(pool: &SqlitePool, state: &UpstreamState, id: &str) -> Result<BlockView> {
    let block = get_block(pool, id).await?;
    block_view_of(pool, state, &block).await
}

/// 由块记录生成视图，顺带统计密钥的可用情况与并发负载。
async fn block_view_of(
    pool: &SqlitePool,
    state: &UpstreamState,
    block: &UpstreamBlock,
) -> Result<BlockView> {
    let keys = db::block::list_keys(pool, &block.id).await?;
    let now = time::now_secs();
    let available = keys.iter().filter(|key| key.is_selectable(now)).count();
    let load = state.load(block);

    Ok(BlockView {
        id: block.id.clone(),
        name: block.name.clone(),
        api_format: block.api_format,
        weight: block.weight,
        max_concurrency: block.max_concurrency,
        base_url: block.base_url.clone(),
        proxy: block.proxy.clone(),
        models: block.models.clone(),
        headers: block.headers.clone(),
        enabled: block.enabled,
        key_count: keys.len(),
        available_keys: available,
        in_flight: load.map(|load| load.in_flight),
        queued: load.map(|load| load.queued),
        created_at: block.created_at,
        updated_at: block.updated_at,
    })
}

/// 给块添加密钥。批量提交，要么全进要么全不进。
/// 批量添加的结果：真正入库的密钥，与被跳过的重复数量。
pub struct AddKeysOutcome {
    pub keys: Vec<UpstreamKeyView>,
    /// 与块内已有密钥、或本批内彼此重复而被跳过的数量。
    pub skipped: usize,
}

/// 批量添加密钥。重复的（与块内已有、或本批内彼此重复）自动跳过，
/// 其余正常入库——批量导入几百把时不可能让人自己去把重复的挑出来。
pub async fn add_keys(
    pool: &SqlitePool,
    block_id: &str,
    payload: UpstreamKeyCreate,
) -> Result<AddKeysOutcome> {
    // 块不存在时先给出可读的错误，而不是等外键约束来报。
    if db::block::find_block(pool, block_id).await?.is_none() {
        return Err(Error::NotFound("块不存在".to_string()));
    }

    let existing = db::block::list_keys(pool, block_id).await?;
    let known: std::collections::HashSet<&str> =
        existing.iter().map(|key| key.secret.as_str()).collect();

    let now = time::now_secs();
    let mut entries = Vec::new();
    let mut skipped = 0usize;
    let mut batch: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for secret in &payload.secrets {
        let secret = secret.trim();
        if secret.is_empty() {
            skipped += 1;
            continue;
        }
        // 同块内重复的密钥会让调度对同一把加倍用力，所以宁可跳过也不入库。
        if known.contains(secret) || !batch.insert(secret) {
            skipped += 1;
            continue;
        }
        entries.push((random::string(16), secret.to_string()));
    }

    if entries.is_empty() {
        return Ok(AddKeysOutcome {
            keys: Vec::new(),
            skipped,
        });
    }

    let mut tx = pool.begin().await?;
    db::block::insert_keys(
        &mut *tx,
        &entries,
        block_id,
        payload.weight,
        payload.never_disable,
        now,
    )
    .await?;
    tx.commit().await?;

    let mut views = Vec::with_capacity(entries.len());
    for (id, _) in &entries {
        let key = db::block::find_key(pool, block_id, id)
            .await?
            .ok_or_else(|| Error::Internal(anyhow::anyhow!("密钥写入后读取失败")))?;
        views.push(view_key(&key));
    }

    Ok(AddKeysOutcome {
        keys: views,
        skipped,
    })
}

/// 删除一批密钥。
pub async fn delete_keys(
    pool: &SqlitePool,
    state: &UpstreamState,
    block_id: &str,
    ids: &[String],
) -> Result<u64> {
    let removed = db::block::delete_keys(pool, block_id, ids).await?;
    state.forget(ids);
    Ok(removed)
}

/// 暂停一批密钥（手动）。
pub async fn pause_keys(pool: &SqlitePool, block_id: &str, ids: &[String]) -> Result<()> {
    for id in ids {
        db::block::set_key_status(pool, block_id, id, KeyStatus::Paused, time::now_secs()).await?;
    }
    Ok(())
}

/// 恢复一批密钥：清掉停用与失败计数，重新参与调度。
pub async fn resume_keys(pool: &SqlitePool, block_id: &str, ids: &[String]) -> Result<()> {
    for id in ids {
        db::block::set_key_status(pool, block_id, id, KeyStatus::Active, time::now_secs()).await?;
    }
    Ok(())
}

/// 批量设置「永不暂停」：免于被自动停用。
///
/// 只叠加一个豁免，不改状态也不清计数——想让它重新上岗用 resume。
pub async fn set_key_never_disable(
    pool: &SqlitePool,
    block_id: &str,
    ids: &[String],
    enabled: bool,
) -> Result<()> {
    for id in ids {
        db::block::set_key_never_disable(pool, block_id, id, enabled, time::now_secs()).await?;
    }
    Ok(())
}

/// 测试一把密钥：发一个不消耗 token 的请求。
pub async fn test_key(
    pool: &SqlitePool,
    state: &UpstreamState,
    block_id: &str,
    key_id: &str,
) -> Result<ProbeOutcome> {
    let block = get_block(pool, block_id).await?;
    let key = db::block::find_key(pool, block_id, key_id)
        .await?
        .ok_or_else(|| Error::NotFound("密钥不存在".to_string()))?;

    let adapter = adapter_for(block.api_format);
    let probe = adapter.build_probe();
    let client = state.client(&block)?;

    let status = proxy::send(&client, &block, &probe, adapter.auth_style(), &key.secret).await;

    let outcome = match status {
        Ok((code, body)) => adapter.parse_probe(code, &body),
        Err(error) => ProbeOutcome::Transient(error.to_string()),
    };

    // 探测结果同步落库：测出坏密钥就不用等真实流量再踩一遍。
    match &outcome {
        ProbeOutcome::Valid => {
            db::block::mark_key_success(pool, &key.id, time::now_secs()).await?;
        }
        ProbeOutcome::Invalid(detail) => {
            // 手动测试是带证据的人工动作：即使密钥设了永不暂停，
            // 确认无效也停用——豁免免的是自动规则，不是这个。
            disable_key(pool, &key, key.failure_count + 1, detail).await?;
        }
        ProbeOutcome::Transient(_) => {}
    }

    Ok(outcome)
}

// ---------- 调度面 ----------

/// 按模型选出可用的块。
///
/// 候选是「启用且接受该模型」的块，平滑加权轮询决定这一次用谁。
/// 请求里模型名的解析结果。
#[derive(Debug, Clone)]
pub struct ResolvedTarget {
    /// 归一化后的模型名（已剥掉块前缀），也是发给上游的名字。
    pub model: String,
    /// 显式锁定的块 id；`None` 表示在所有提供该模型的块之间加权轮询。
    pub block_id: Option<String>,
}

impl ResolvedTarget {
    /// 没锁定块、模型名原样。
    pub fn plain(model: &str) -> Self {
        Self {
            model: model.to_string(),
            block_id: None,
        }
    }
}

/// 解析请求里的模型名，支持 `块id/模型名` 形式的显式命名空间。
///
/// **整名优先**：先拿整个字符串去匹配块里的模型——`anthropic/claude-3`
/// 这种本身带 `/` 的模型名不能被误拆成「块 anthropic 的模型 claude-3」。
/// 整名匹配不到，才尝试把它拆成前缀与模型名，前缀必须是存在的块 id。
///
/// 两级都匹配不到就报错，**不静默回退到轮询**：打错一个字就悄悄走别的块，
/// 账目和日志都会说不清。
pub async fn resolve_target(pool: &SqlitePool, requested: &str) -> Result<ResolvedTarget> {
    let blocks = db::block::list_blocks(pool).await?;
    let enabled: Vec<&UpstreamBlock> = blocks.iter().filter(|block| block.enabled).collect();

    if enabled.iter().any(|block| block.accepts(requested)) {
        return Ok(ResolvedTarget::plain(requested));
    }

    if let Some((prefix, rest)) = requested.split_once('/') {
        if let Some(block) = enabled.iter().find(|block| block.id == prefix) {
            if block.accepts(rest) {
                return Ok(ResolvedTarget {
                    model: rest.to_string(),
                    block_id: Some(prefix.to_string()),
                });
            }
            return Err(Error::NotFound(format!("块 {prefix} 没有提供模型 {rest}")));
        }
    }

    Err(Error::NotFound(format!(
        "没有可处理模型 {requested} 的上游"
    )))
}

/// 解析请求目标，并合入请求头里指定的块。
///
/// 模型名前缀与 `X-Aequi-Block` 是两种等价的指定方式；都给了且指向不同的块时
/// 直接报错——这种矛盾没法猜用户想要哪个，静默取其一只会让人以为设置生效了。
pub async fn resolve_with_header(
    pool: &SqlitePool,
    model: &str,
    header_block: Option<&str>,
) -> Result<ResolvedTarget> {
    let mut target = resolve_target(pool, model).await?;

    let Some(header_block) = header_block else {
        return Ok(target);
    };

    if let Some(from_prefix) = target.block_id.as_deref() {
        if from_prefix != header_block {
            return Err(Error::BadRequest(format!(
                "请求头指定的块 {header_block} 与模型名里的前缀 {from_prefix} 冲突"
            )));
        }
    }

    target.block_id = Some(header_block.to_string());
    Ok(target)
}

/// 该密钥请求某个模型时，实际允许使用的块。
///
/// 白名单的条目是**全名**（`块id/模型名`）。同一个模型名出现在两个块里时，
/// 那是两条彼此独立的授权——出口与价格都可能不同。所以「只允许了 a 的 gpt-4o」
/// 时，这里不会把 b 也算进来，短名请求自然也不会被轮询到 b。
///
/// 白名单为空表示不限制，返回全部可用的块。
pub async fn allowed_blocks(pool: &SqlitePool, model: &str, key: &ApiKey) -> Result<Vec<String>> {
    let blocks = db::block::list_blocks(pool).await?;

    Ok(blocks
        .iter()
        .filter(|block| block.enabled && block.accepts(model))
        .filter(|block| key.allows_capability(&block.id, model))
        .map(|block| block.id.clone())
        .collect())
}

/// 选一个能处理该模型的块。
///
/// `only` 非空时只在指定块里选——请求里带了块前缀或 `X-Aequi-Block` 头时为真。
/// 其余情况在**该密钥被允许的、能处理该模型的块**之间做平滑加权轮询；
/// 一个都没有就报错，绝不退到未授权的块上。
pub async fn select_block(
    pool: &SqlitePool,
    state: &UpstreamState,
    key: &ApiKey,
    model: &str,
    only: Option<&str>,
) -> Result<UpstreamBlock> {
    let blocks = db::block::list_blocks(pool).await?;

    let allowed = allowed_blocks(pool, model, key).await?;

    let candidates: Vec<(String, u32)> = blocks
        .iter()
        .filter(|block| block.enabled && block.accepts(model))
        .filter(|block| allowed.contains(&block.id))
        .filter(|block| only.is_none_or(|id| block.id == id))
        .map(|block| (block.id.clone(), block.weight))
        .collect();

    if candidates.is_empty() {
        return Err(match only {
            Some(id) if !allowed.iter().any(|item| item == id) => {
                Error::Forbidden(format!("该密钥无权使用模型 {model}（块 {id}）"))
            }
            Some(id) => Error::NotFound(format!("块 {id} 不提供模型 {model}")),
            None => Error::NotFound(format!("没有可处理模型 {model} 的上游")),
        });
    }

    let picked = state
        .pick(&candidates)
        .ok_or_else(|| Error::NotFound(format!("没有可处理模型 {model} 的上游")))?;

    blocks
        .into_iter()
        .find(|block| block.id == picked)
        .ok_or_else(|| Error::Internal(anyhow::anyhow!("被选中的块不在候选里")))
}

/// 在块内选出一把可用密钥。
///
/// 只考虑启用、未停用、不在退避期内的密钥；权重轮询之外再按创建顺序打散。
pub async fn select_key(
    pool: &SqlitePool,
    state: &UpstreamState,
    block: &UpstreamBlock,
) -> Result<UpstreamKey> {
    let keys = db::block::list_keys(pool, &block.id).await?;
    let now = time::now_secs();

    let candidates: Vec<(String, u32)> = keys
        .iter()
        .filter(|key| key.is_selectable(now))
        .map(|key| (key.id.clone(), key.weight))
        .collect();

    if candidates.is_empty() {
        return Err(Error::Forbidden("该上游没有可用密钥".to_string()));
    }

    let picked = state
        .pick(&candidates)
        .ok_or_else(|| Error::Forbidden("该上游没有可用密钥".to_string()))?;

    keys.into_iter()
        .find(|key| key.id == picked)
        .ok_or_else(|| Error::Internal(anyhow::anyhow!("被选中的密钥不在候选里")))
}

/// 一次成功：清零失败计数。
pub async fn report_success(pool: &SqlitePool, key_id: &str) {
    if let Err(error) = db::block::mark_key_success(pool, key_id, time::now_secs()).await {
        tracing::warn!(key_id, error = %error, "记录密钥成功状态失败");
    }
}

/// 一次失败：按类型决定退避与停用。
///
/// - 硬失败（密钥被上游拒绝）累计到阈值自动停用；`never_disable` 的密钥
///   除外——计数、留痕照旧，状态不动。
/// - 限流与瞬时故障按连续失败次数指数退避（30s 起步、480s 封顶），
///   **不停用**：上游打个嗝不该把好密钥永久禁掉，等管理员发现时
///   故障早就过去了。
/// - 三种都写 `last_error`：后台看得见最近一次失败的原因。
pub async fn report_failure(pool: &SqlitePool, key: &UpstreamKey, kind: FailureKind, detail: &str) {
    let failures = key.failure_count + 1;
    let now = time::now_secs();

    if kind == FailureKind::Hard && failures >= FAILURE_THRESHOLD {
        if key.never_disable {
            tracing::info!(
                key_id = %key.id,
                failures,
                "密钥连续被拒，但已设为永不暂停，仅记录不停用"
            );
        } else {
            // 留痕与停用一把做完：Disabled 是终态，停机原因要跟着落库，
            // 否则后台只看到「停了」，看不到「为什么停」。
            if let Err(error) = disable_key(pool, key, failures, detail).await {
                tracing::warn!(key_id = %key.id, error = %error, "停用密钥失败");
            } else {
                tracing::warn!(key_id = %key.id, failures, "密钥连续被拒，已自动停用");
            }
            return;
        }
    }

    let cooldown_until = match kind {
        // 硬失败不退避：它要么即将停用，要么该由人工看一眼，歇着没用。
        FailureKind::Hard => None,
        FailureKind::RateLimited | FailureKind::Transient => Some(now + backoff_secs(failures)),
    };

    let mut connection = match pool.acquire().await {
        Ok(connection) => connection,
        Err(error) => {
            tracing::warn!(key_id = %key.id, error = %error, "获取连接失败");
            return;
        }
    };

    if let Err(error) = db::block::update_key_runtime(
        connection.as_mut(),
        &key.id,
        failures,
        cooldown_until,
        Some(detail.to_string()),
        now,
    )
    .await
    {
        tracing::warn!(key_id = %key.id, error = %error, "记录密钥失败状态失败");
    }
}

/// 停用一把密钥，并把停机原因写进 `last_error`。
///
/// 连续失败达标与手动测试确认无效走的是同一条路：结果都是「这把钥匙
/// 不能用了」，后台要能看出是哪一次失败送它下线的。
async fn disable_key(
    pool: &SqlitePool,
    key: &UpstreamKey,
    failures: u32,
    detail: &str,
) -> Result<()> {
    let now = time::now_secs();

    let mut connection = pool.acquire().await?;
    // `set_key_status` 只在转 active 时清场，disabled 原样保留——所以这里
    // 写下去的计数与原因就是后台看到的最终值。
    db::block::update_key_runtime(
        connection.as_mut(),
        &key.id,
        failures,
        None,
        Some(detail.to_string()),
        now,
    )
    .await?;
    drop(connection);

    db::block::set_key_status(pool, &key.block_id, &key.id, KeyStatus::Disabled, now).await?;
    Ok(())
}

/// 整块连通性测试：逐把测试块内全部密钥。
///
/// 结果口径与单把测试一致（有效则清零失败计数、无效则停用），
/// 串行执行——块内密钥数量有限，并发打上游反而容易被限流。
pub async fn test_block(
    pool: &SqlitePool,
    state: &UpstreamState,
    block_id: &str,
) -> Result<crate::model::BlockTestResult> {
    let keys = db::block::list_keys(pool, block_id).await?;

    let mut results = Vec::with_capacity(keys.len());
    let (mut valid, mut invalid, mut transient) = (0, 0, 0);

    for key in &keys {
        let outcome = test_key(pool, state, block_id, &key.id).await?;

        let (label, detail) = match outcome {
            ProbeOutcome::Valid => {
                valid += 1;
                ("valid", None)
            }
            ProbeOutcome::Invalid(detail) => {
                invalid += 1;
                ("invalid", Some(detail))
            }
            ProbeOutcome::Transient(detail) => {
                transient += 1;
                ("transient", Some(detail))
            }
        };

        results.push(crate::model::KeyTestResult {
            key_id: key.id.clone(),
            // 只回前缀：整块测试的结果可能被贴进工单，完整密钥没必要外带。
            secret_prefix: key.secret.chars().take(6).collect(),
            outcome: label,
            detail,
        });
    }

    Ok(crate::model::BlockTestResult {
        block_id: block_id.to_string(),
        tested: results.len(),
        valid,
        invalid,
        transient,
        results,
    })
}

/// 密钥的对外视图。
fn view_key(key: &UpstreamKey) -> UpstreamKeyView {
    UpstreamKeyView {
        id: key.id.clone(),
        block_id: key.block_id.clone(),
        secret: key.secret.clone(),
        status: key.status,
        never_disable: key.never_disable,
        weight: key.weight,
        failure_count: key.failure_count,
        cooldown_until: key.cooldown_until,
        last_error: key.last_error.clone(),
        last_used_at: key.last_used_at,
        created_at: key.created_at,
    }
}

/// 列出某块的全部密钥。
pub async fn list_keys(pool: &SqlitePool, block_id: &str) -> Result<Vec<UpstreamKeyView>> {
    let keys = db::block::list_keys(pool, block_id).await?;
    Ok(keys.iter().map(view_key).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(name: &str, value: &str) -> HeaderEntry {
        HeaderEntry {
            name: name.to_string(),
            value: value.to_string(),
        }
    }

    #[test]
    fn headers_are_trimmed_and_kept_in_order() {
        // 粘贴进来的名字和值多半带着空白；顺手去掉，别让它变成上游看到的怪值。
        let normalized = normalize_headers(vec![
            header("  X-Tenant  ", "  demo-tenant  "),
            header("anthropic-version", "2099-01-01"),
        ])
        .expect("合法头应当通过");

        assert_eq!(normalized[0].name, "X-Tenant");
        assert_eq!(normalized[0].value, "demo-tenant");
        assert_eq!(normalized[1].name, "anthropic-version", "顺序即配置顺序");
    }

    #[test]
    fn duplicate_header_names_are_rejected() {
        // 头名大小写不敏感，`X-Tenant` 与 `x-tenant` 是同一个头。
        let error = normalize_headers(vec![header("X-Tenant", "a"), header("x-tenant", "b")])
            .expect_err("同名头必须被拒");
        assert!(
            error.to_string().contains("配置了多次"),
            "错误要说清是哪条、为什么: {error}"
        );
    }

    #[test]
    fn headers_owned_by_the_http_stack_are_rejected() {
        for name in ["Content-Length", "transfer-encoding", "Connection"] {
            let error = normalize_headers(vec![header(name, "1")]).expect_err("这类头必须被拒");
            assert!(
                error.to_string().contains("HTTP 栈"),
                "{name} 的错误文案要指出是 HTTP 栈管的: {error}"
            );
        }
    }

    #[test]
    fn headers_that_cannot_be_sent_are_rejected() {
        // 头注入就在这里挡住：值里的换行能凭空造出一个新头。
        assert!(
            normalize_headers(vec![header("X-Tenant", "bad\nvalue")]).is_err(),
            "值里带换行必须被拒"
        );
        assert!(
            normalize_headers(vec![header("", "x")]).is_err(),
            "空头名必须被拒"
        );
        assert!(
            normalize_headers(vec![header("X Bad", "x")]).is_err(),
            "头名里有空格必须被拒"
        );
    }

    #[test]
    fn too_many_headers_are_rejected() {
        let many = (0..=MAX_HEADERS)
            .map(|index| header(&format!("X-Demo-{index}"), "1"))
            .collect();
        assert!(
            normalize_headers(many).is_err(),
            "超过 {MAX_HEADERS} 条要挡住，免得整坨 JSON 粘进来"
        );
    }

    // ---------- 并发上限与闸门 ----------

    #[test]
    fn concurrency_limit_is_validated() {
        assert!(validate_concurrency(0).is_ok(), "0 = 不限制");
        assert!(validate_concurrency(MAX_BLOCK_CONCURRENCY).is_ok());
        let error =
            validate_concurrency(MAX_BLOCK_CONCURRENCY + 1).expect_err("超过防呆上限必须被拒");
        assert!(
            error.to_string().contains("并发上限"),
            "错误要说清超的是什么: {error}"
        );
    }

    /// 造一个只差运行时分数的块（测试不联网）。
    fn demo_block(max_concurrency: u32) -> UpstreamBlock {
        UpstreamBlock {
            id: "demo-block".to_string(),
            name: "demo".to_string(),
            api_format: crate::model::ApiFormat::Openai,
            weight: 1,
            max_concurrency,
            base_url: "https://demo-upstream.invalid".to_string(),
            proxy: None,
            models: Vec::new(),
            headers: Vec::new(),
            enabled: true,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[tokio::test]
    async fn gate_hands_out_permits_and_queues_when_full() {
        let state = UpstreamState::default();

        // 不限制的块没有闸门
        assert!(
            state.gate(&demo_block(0)).is_none(),
            "max_concurrency 为 0 时不该建闸门"
        );

        let gate = state.gate(&demo_block(2)).expect("配了上限就应当有闸门");
        assert_eq!(gate.in_flight(), 0);

        let first = gate.acquire().await.expect("拿第一个位子");
        let second = gate.acquire().await.expect("拿第二个位子");
        assert_eq!(gate.in_flight(), 2, "位子发完就该堵住");

        // 第三个等不到：50ms 超时（线上口径是 GATE_WAIT_TIMEOUT）。
        assert!(
            tokio::time::timeout(Duration::from_millis(50), gate.acquire())
                .await
                .is_err(),
            "闸门满了必须排队，而不是放行超额请求"
        );
        assert_eq!(gate.queued(), 0, "超时取消也要把排队数收回去，不能虚高");

        drop(first);
        assert_eq!(gate.in_flight(), 1, "释放一个还一个");
        drop(second);

        // 容量调小：换一个新门，新请求按新容量走（旧门上的排队者不受影响）。
        let smaller = state.gate(&demo_block(1)).expect("换门");
        assert_eq!(smaller.in_flight(), 0, "新门按新容量发放位子");
        assert_eq!(smaller.queued(), 0);

        // 删除后重建同 id 的块：拿到的是满容量的新门。
        state.invalidate_gate("demo-block");
        assert_eq!(
            state.gate(&demo_block(3)).expect("重建后的门").in_flight(),
            0
        );
    }

    #[tokio::test]
    async fn load_reports_in_flight_and_queued() {
        let state = UpstreamState::default();

        // 不限制并发的块没有负载可报：没有闸门，就没有这两个数。
        assert!(
            state.load(&demo_block(0)).is_none(),
            "max_concurrency 为 0 时不该报负载"
        );

        let gate = state.gate(&demo_block(1)).expect("配了上限就应当有闸门");
        let held = gate.acquire().await.expect("占住唯一的位子");

        // 第二个请求开始排队：在飞 1、排队 1。
        let waiter = tokio::spawn({
            let gate = Arc::clone(&gate);
            async move { gate.acquire().await }
        });
        // spawn 到真正 +1 之间有调度窗口，等它落秤。
        for _ in 0..100 {
            if gate.queued() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let load = state.load(&demo_block(1)).expect("有限额的块要报负载");
        assert_eq!(load.in_flight, 1, "在飞数要算上占着位子的请求");
        assert_eq!(load.queued, 1, "排队数要算上等位子的请求");

        // 放走在飞的请求，排队者补位：在飞仍是 1，排队归零。
        drop(held);
        // 位子一路持有到测试结束——在飞数要的正是「补位者还在飞」。
        let _permit = waiter
            .await
            .expect("排队任务不该 panic")
            .expect("位子终会等到");
        assert_eq!(gate.in_flight(), 1, "补位的请求接住在飞数里");
        assert_eq!(gate.queued(), 0, "等到位子就不再算排队");
    }

    // ---------- 密钥生命周期与退避 ----------

    /// 一张只有数据面表的内存库。
    async fn test_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("打开内存库");
        crate::db::migrate_data(&pool)
            .await
            .expect("建数据面表失败");
        pool
    }

    /// 建一个块并放一把密钥，返回读出来的密钥记录。
    async fn seed_key(pool: &SqlitePool, never_disable: bool) -> UpstreamKey {
        sqlx::query(
            "INSERT INTO upstream_blocks \
             (id, name, api_format, weight, max_concurrency, base_url, created_at, updated_at) \
             VALUES ('demo-block', 'demo', 'openai', 1, 0, 'https://demo-upstream.invalid', 1, 1)",
        )
        .execute(pool)
        .await
        .expect("建块失败");
        sqlx::query(
            "INSERT INTO upstream_keys \
             (id, block_id, secret, never_disable, created_at, updated_at) \
             VALUES ('key-1', 'demo-block', 'sk-demo', ?, 1, 1)",
        )
        .bind(i64::from(never_disable))
        .execute(pool)
        .await
        .expect("建密钥失败");

        db::block::find_key(pool, "demo-block", "key-1")
            .await
            .expect("读密钥失败")
            .expect("密钥应当存在")
    }

    /// 重新读库里那把密钥——`report_failure` 按传入记录的计数累加，
    /// 连续报告必须每轮换最新的记录，否则测的是「同一份旧数据加五次」。
    async fn reload(pool: &SqlitePool) -> UpstreamKey {
        db::block::find_key(pool, "demo-block", "key-1")
            .await
            .expect("读密钥失败")
            .expect("密钥应当存在")
    }

    #[test]
    fn backoff_doubles_each_failure_and_caps() {
        assert_eq!(backoff_secs(1), 30, "第一次失败休息基础时长");
        assert_eq!(backoff_secs(2), 60, "之后每多一次翻倍");
        assert_eq!(backoff_secs(3), 120);
        assert_eq!(backoff_secs(4), 240);
        assert_eq!(backoff_secs(5), 480, "到顶封顶");
        assert_eq!(backoff_secs(99), 480, "之后不再涨——雪藏太久等于停用");
    }

    #[tokio::test]
    async fn rate_limit_only_backs_off_never_disables() {
        let pool = test_pool().await;
        let mut key = seed_key(&pool, false).await;

        // 连吃 5 个 429：一次都不该停用，退避一路涨到顶。
        for expected in 1..=5u32 {
            let before = time::now_secs();
            report_failure(&pool, &key, FailureKind::RateLimited, "被限流，暂时退避").await;
            key = reload(&pool).await;

            assert_eq!(key.failure_count, expected, "计数逐次累计");
            assert_eq!(key.status, KeyStatus::Active, "限流永不停用密钥");
            let cooldown = key.cooldown_until.expect("限流必须退避");
            assert!(
                cooldown >= before + backoff_secs(expected)
                    && cooldown <= time::now_secs() + backoff_secs(expected),
                "第 {expected} 次限流的退避应当是 {} 秒，实际截止 {cooldown}",
                backoff_secs(expected)
            );
        }
    }

    #[tokio::test]
    async fn transient_failures_do_not_disable() {
        // 上游 5xx / 网络抖动连吃 5 次：只退避，不停用——
        // 「短期错误把好密钥永久禁掉」正是要避免的那种死法。
        let pool = test_pool().await;
        let mut key = seed_key(&pool, false).await;

        for _ in 0..5 {
            report_failure(&pool, &key, FailureKind::Transient, "上游返回 502").await;
            key = reload(&pool).await;
        }

        assert_eq!(key.failure_count, 5);
        assert_eq!(key.status, KeyStatus::Active, "上游故障不该停用密钥");
        assert!(key.cooldown_until.is_some(), "瞬时故障也要退避");
        assert_eq!(key.last_error.as_deref(), Some("上游返回 502"));
    }

    #[tokio::test]
    async fn hard_failures_disable_at_threshold_with_a_reason() {
        let pool = test_pool().await;
        let mut key = seed_key(&pool, false).await;

        for _ in 0..FAILURE_THRESHOLD {
            report_failure(&pool, &key, FailureKind::Hard, "上游返回 401 密钥无效").await;
            key = reload(&pool).await;
        }

        assert_eq!(key.failure_count, FAILURE_THRESHOLD);
        assert_eq!(key.status, KeyStatus::Disabled, "连续被拒到达阈值必须停用");
        assert_eq!(
            key.last_error.as_deref(),
            Some("上游返回 401 密钥无效"),
            "停机原因要留痕，否则后台只看到「停了」"
        );
        assert!(key.cooldown_until.is_none(), "硬失败不退避");
    }

    #[tokio::test]
    async fn a_single_hard_failure_does_not_disable() {
        // 阈值是 3：一次被拒（上游打个嗹、限流误报）不该把密钥停掉。
        let pool = test_pool().await;
        let key = seed_key(&pool, false).await;

        report_failure(&pool, &key, FailureKind::Hard, "上游返回 401").await;

        let fresh = reload(&pool).await;
        assert_eq!(fresh.status, KeyStatus::Active);
        assert_eq!(fresh.failure_count, 1);
    }

    #[tokio::test]
    async fn never_disable_survives_hard_failures() {
        let pool = test_pool().await;
        let mut key = seed_key(&pool, true).await;

        for _ in 0..5 {
            report_failure(&pool, &key, FailureKind::Hard, "上游返回 401").await;
            key = reload(&pool).await;
        }

        assert_eq!(
            key.failure_count, 5,
            "计数照记——后台看得见它在连续被拒，只是不停用"
        );
        assert_eq!(key.status, KeyStatus::Active, "永不暂停：只记录，不改状态");
        assert!(key.cooldown_until.is_none());
    }

    #[tokio::test]
    async fn success_clears_count_cooldown_and_error() {
        let pool = test_pool().await;
        let key = seed_key(&pool, false).await;

        report_failure(&pool, &key, FailureKind::RateLimited, "被限流，暂时退避").await;
        report_success(&pool, &key.id).await;

        let fresh = reload(&pool).await;
        assert_eq!(fresh.failure_count, 0, "成功清零失败计数");
        assert_eq!(fresh.cooldown_until, None, "退避随之解除");
        assert_eq!(fresh.last_error, None);
    }
}
