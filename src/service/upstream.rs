//! 上游块与密钥的管理与调度。
//!
//! 管理面（增删改查、暂停、恢复、测试）在这里；调度面只有两个入口——
//! 按模型挑块、块内挑密钥，都走平滑加权轮询。
//!
//! 密钥的生命周期是自治的：请求成功清零失败计数；429 进入退避；
//! 连续失败达到阈值自动停用，等管理员确认后恢复。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use sqlx::SqlitePool;

use crate::db;
use crate::error::{Error, Result};
use crate::model::{
    ApiKey, BlockCreate, BlockUpdate, BlockView, KeyStatus, ModelEntry, UpstreamBlock, UpstreamKey,
    UpstreamKeyCreate, UpstreamKeyView,
};
use crate::proxy;
use crate::proxy::adapter::{adapter_for, ProbeOutcome};
use crate::utils::{random, time};

/// 连续失败达到该次数后自动停用密钥。
const FAILURE_THRESHOLD: u32 = 3;
/// 429 退避时长（秒）。
const RATE_LIMIT_COOLDOWN: i64 = 30;
/// 发往上游的请求超时。
/// 整个请求（含读流）的总时长上限。思考型模型输出慢，给足余量。
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(120);
/// 连接池空闲超时。池里放太久的连接可能已被服务端静默关闭，
/// 复用到它就会表现为「响应读到一半流就没了」且概率性出现。
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// 上游的运行时状态：轮询累积值与每块的 HTTP 客户端。
#[derive(Default)]
pub struct UpstreamState {
    /// 平滑加权轮询的累积值。键是块或密钥的 id。
    currents: Mutex<HashMap<String, i64>>,
    /// 每块一个客户端，连接池跨请求复用；块配置变更时失效。
    clients: Mutex<HashMap<String, reqwest::Client>>,
}

impl UpstreamState {
    /// 平滑加权轮询选一个候选。
    ///
    /// 每轮给每个候选加上自身权重，取累积值最大者并扣回总权重——
    /// 权重大的被选得频繁，但不会连续压同一个；同分时按传入顺序取先者，
    /// 这就是「权重 + 顺序」的全部含义。
    pub fn pick(&self, candidates: &[(String, u32)]) -> Option<String> {
        let total: i64 = candidates.iter().map(|(_, weight)| i64::from(*weight)).sum();
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
            "块的 id 只能用小写字母、数字、点、下划线或连字符——它会作为模型名前缀使用"
                .to_string(),
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
    );

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

    let now = time::now_secs();
    let block = UpstreamBlock {
        id,
        name: payload.name.trim().to_string(),
        api_format: payload.api_format,
        weight: payload.weight,
        base_url: payload.base_url.trim().trim_end_matches('/').to_string(),
        proxy: payload.proxy,
        models: payload.models,
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
        weight,
        base_url,
        proxy,
        models,
        enabled,
    } = patch;

    if let Some(models) = &models {
        validate_models(models)?;
    }

    let current = get_block(pool, id).await?;
    let mut block = current;

    if let Some(name) = name {
        block.name = name.trim().to_string();
    }
    if let Some(weight) = weight {
        block.weight = weight;
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
    Ok(())
}

/// 按 ID 取块。
pub async fn get_block(pool: &SqlitePool, id: &str) -> Result<UpstreamBlock> {
    db::block::find_block(pool, id)
        .await?
        .ok_or_else(|| Error::NotFound("块不存在".to_string()))
}

/// 列出块，附带密钥统计。
pub async fn list_blocks(pool: &SqlitePool) -> Result<Vec<BlockView>> {
    let blocks = db::block::list_blocks(pool).await?;

    let mut views = Vec::with_capacity(blocks.len());
    for block in &blocks {
        views.push(block_view_of(pool, block).await?);
    }

    Ok(views)
}

/// 单个块的视图。
pub async fn block_view(pool: &SqlitePool, id: &str) -> Result<BlockView> {
    let block = get_block(pool, id).await?;
    block_view_of(pool, &block).await
}

/// 由块记录生成视图，顺带统计密钥的可用情况。
async fn block_view_of(pool: &SqlitePool, block: &UpstreamBlock) -> Result<BlockView> {
    let keys = db::block::list_keys(pool, &block.id).await?;
    let now = time::now_secs();
    let available = keys.iter().filter(|key| key.is_selectable(now)).count();

    Ok(BlockView {
        id: block.id.clone(),
        name: block.name.clone(),
        api_format: block.api_format,
        weight: block.weight,
        base_url: block.base_url.clone(),
        proxy: block.proxy.clone(),
        models: block.models.clone(),
        enabled: block.enabled,
        key_count: keys.len(),
        available_keys: available,
        created_at: block.created_at,
        updated_at: block.updated_at,
    })
}

/// 给块添加密钥。批量提交，要么全进要么全不进。
pub async fn add_keys(
    pool: &SqlitePool,
    block_id: &str,
    payload: UpstreamKeyCreate,
) -> Result<Vec<UpstreamKeyView>> {
    // 块不存在时先给出可读的错误，而不是等外键约束来报。
    if db::block::find_block(pool, block_id).await?.is_none() {
        return Err(Error::NotFound("块不存在".to_string()));
    }

    let now = time::now_secs();
    let mut entries = Vec::with_capacity(payload.secrets.len());
    for secret in &payload.secrets {
        let secret = secret.trim();
        if secret.is_empty() {
            return Err(Error::BadRequest("密钥不能为空".to_string()));
        }
        entries.push((random::string(16), secret.to_string()));
    }

    let mut tx = pool.begin().await?;
    let result = db::block::insert_keys(&mut *tx, &entries, block_id, payload.weight, now).await;
    if let Err(error) = &result {
        // 同块内重复的密钥会让调度对同一把加倍用力，直接拒绝。
        if db::block::is_unique_violation(error) {
            return Err(Error::BadRequest("存在与本块已有密钥重复的条目".to_string()));
        }
    }
    result?;
    tx.commit().await?;

    let mut views = Vec::with_capacity(entries.len());
    for (id, _) in &entries {
        let key = db::block::find_key(pool, block_id, id)
            .await?
            .ok_or_else(|| Error::Internal(anyhow::anyhow!("密钥写入后读取失败")))?;
        views.push(view_key(&key));
    }

    Ok(views)
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
pub async fn pause_keys(
    pool: &SqlitePool,
    block_id: &str,
    ids: &[String],
) -> Result<()> {
    for id in ids {
        db::block::set_key_status(pool, block_id, id, KeyStatus::Paused, time::now_secs())
            .await?;
    }
    Ok(())
}

/// 恢复一批密钥：清掉停用与失败计数，重新参与调度。
pub async fn resume_keys(
    pool: &SqlitePool,
    block_id: &str,
    ids: &[String],
) -> Result<()> {
    for id in ids {
        db::block::set_key_status(pool, block_id, id, KeyStatus::Active, time::now_secs())
            .await?;
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
            disable_key(pool, &key, detail).await?;
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
pub async fn allowed_blocks(
    pool: &SqlitePool,
    model: &str,
    key: &ApiKey,
) -> Result<Vec<String>> {
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
            Some(id) if !allowed.iter().any(|item| item == id) => Error::Forbidden(format!(
                "该密钥无权使用模型 {model}（块 {id}）"
            )),
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

/// 一次失败：累计失败计数，达到阈值自动停用。
pub async fn report_failure(pool: &SqlitePool, key: &UpstreamKey, detail: &str) {
    let failures = key.failure_count + 1;
    let now = time::now_secs();

    if failures >= FAILURE_THRESHOLD {
        tracing::warn!(key_id = %key.id, failures, "密钥连续失败，已自动停用");
        if let Err(error) = disable_key(pool, key, detail).await {
            tracing::warn!(key_id = %key.id, error = %error, "停用密钥失败");
        }
        return;
    }

    let mut connection = match pool.acquire().await {
        Ok(connection) => connection,
        Err(error) => {
            tracing::warn!(key_id = %key.id, error = %error, "获取连接失败");
            return;
        }
    };

    let result = db::block::update_key_runtime(
        connection.as_mut(),
        &key.id,
        failures,
        None,
        Some(detail.to_string()),
        now,
    )
    .await;

    if let Err(error) = result {
        tracing::warn!(key_id = %key.id, error = %error, "记录密钥失败状态失败");
    }
}

/// 被限流：密钥进入退避期，期间不再参与调度。
pub async fn report_rate_limited(pool: &SqlitePool, key: &UpstreamKey) {
    let now = time::now_secs();
    let until = now + RATE_LIMIT_COOLDOWN;

    let mut connection = match pool.acquire().await {
        Ok(connection) => connection,
        Err(error) => {
            tracing::warn!(key_id = %key.id, error = %error, "获取连接失败");
            return;
        }
    };

    let result = db::block::update_key_runtime(
        connection.as_mut(),
        &key.id,
        key.failure_count,
        Some(until),
        Some("被限流，暂时退避".to_string()),
        now,
    )
    .await;

    if let Err(error) = result {
        tracing::warn!(key_id = %key.id, error = %error, "设置密钥退避失败");
    }
}

/// 停用一把密钥。
async fn disable_key(pool: &SqlitePool, key: &UpstreamKey, detail: &str) -> Result<()> {
    db::block::set_key_status(
        pool,
        &key.block_id,
        &key.id,
        KeyStatus::Disabled,
        time::now_secs(),
    )
    .await?;
    let _ = detail;
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
