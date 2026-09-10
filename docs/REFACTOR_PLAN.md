# Aequi 重构规划 v2（决策已冻结）

> 审计对象：`mogumc/aequi` v0.4.7（旧源码最后提交 `2e1c7e4`，dev 分支已清空源码树，本次从 git 历史还原审计）
> 审计规模：31 个 Rust 源文件 / 约 11,000 行 + 构建与 CI
> 重构三轴：**①底层数据结构 ②日志埋点 ③API 结构**
> 能力升级：**④密钥验证 ⑤429 退避与熔断 ⑥分组制（替代等级制）⑦轮询公平性 ⑧两层额度（上游密钥 / 访问密钥积分）⑨数据库分页**
>
> **v2 相对 v1 的变更**：D1–D14 全部冻结并落实到正文；**移除兼容层与周期重置**（D5 / D9 否决）；身份模型改为 UUID + 指纹；模型引入**绑定（Binding）**概念；API 改为 `admin/{域}/{动作}` 约定；新增 §4.6 HTTP 框架专项评估、§五 存储方案（含 §5.6 分页与索引）。
> **v2 修订（额度语义澄清）**：额度分两层且不混用 —— **上游密钥额度**（对接平台 token 配额，触顶自动摘出轮转池、对客户端透明）与**访问密钥积分**（发给朋友，触顶拒绝请求、手动分发）；上游密钥健康态拆细为 `Active / Cooling / SuspendedAuth / QuotaExhausted / Unknown` + 正交的 `enabled`；列表一律改数据库 keyset 分页。
> 本版目标：**可执行的施工文档**，不再保留"待选项"。

---

## 一、核心判断

**【核心判断】** ✅ 值得做，且是三处结构性重写的必要重构，不是"换皮重写"。

| 前置三问 | 结论 | 证据 |
|:---|:---|:---|
| ① 是否为真实生产问题？ | **是** | 222 次提交中 `fix` 55 次（占 25%）。热点集中且反复：`fix(proxy)` 11 次、`fix(admin)` 4 次、`gemini`+`format` 相关 9 次。其中「重试时模型名称重新映射」连续两次修复（`825f09b`、`508860c`）——说明缺陷源于**数据结构设计**而非实现疏忽 |
| ② 是否存在轻量化替代？ | **部分存在，但三处无法回避** | 协议映射规则、计费状态机、token 估算可原样搬运（占存量价值大头）；但状态容器、请求日志/埋点、admin API 三层已到"补丁成本 > 重写成本"的临界点，必须结构重写 |
| ③ 是否影响存量兼容？ | **不需要兼容（D5）** | 旧数据结构本身存在问题，兼容等于"解决了问题却照旧运行"。采用**一次性单向迁移**，新运行时不认识旧格式 |

**结论口径**：保留领域资产（约 55% 代码价值），重写三个架构层，并一次性把五项能力编入领域模型。

---

## 二、关键洞察

### 2.1 数据结构：症结不在"用了什么类型"，而在"同一事实被存了多份"

审计发现 **7 处"同一真源多副本"**，它们是 55 次 fix 的结构性来源：

| # | 重复的事实 | 副本位置 | 后果 |
|:---|:---|:---|:---|
| 1 | 运行期配置 | `RouterState` 字段（`request_timeout` / `max_retries` / `retry_status_codes` / `key_config` / `admin_tokens`）**与** `RuntimeConfig` | **已验证为死字段**：仅在手写 `Clone` 中被读取，业务全部走 `runtime`。SIGHUP 热重载只更新 `runtime`，`RouterState` 上的副本永久陈旧 |
| 2 | 模型→上游关系 | `Upstream.models`、`ModelRoutesFile.models`、`ModelRoutesFile.upstreams` | 同一关系正反两份手工同步（`build_models_index`），三处任一更新即失配 |
| 3 | 模型别名 | `Upstream.model_map` + `Upstream.model_rmap` | 反向表可由正向派生，且**泄漏到重试路径**（见 2.2） |
| 4 | 密钥 | `KeyState.key: Arc<str>` + `KeyState.auth_header: HeaderValue` | 同值双表示，format 变化时易不一致 |
| 5 | 配置展示 | `Config` + `RuntimeConfig` + `RuntimeConfig.preview` | 三份，且脱敏用 `k.contains("token")` 字符串启发式（漏 `password`/`secret` 类键名） |
| 6 | 上游选择计数 | `RouterState.stats.upstream_selected_total` **与** `Upstream.stats.selected_total` | 口径漂移风险 |
| 7 | 密钥用量 | `billing`（余额，独立 std 线程 + mpsc）**与** `key_usage` 树（token/credits，sled + `usage_lock`） | 两条持久化通道，无事务耦合；`check_monthly_reset` 未持 `usage_lock` 即 `tree.clear()`，与 `add_key_usage` 存在丢失更新 |

其他结构性问题：

- **`RouterState` 是 25+ 字段的 God Object，且手写 `Clone`**（`state/mod.rs:119-152`）。该 `Clone` 会**静默重建** `queue_notify: Notify::new()`、`queue_slots: Semaphore::new(..)`、`sched_rr: AtomicUsize::new(..)` —— 任何一次克隆都产生**与源对象不同的队列与调度游标**。这是潜伏缺陷，不是风格问题。
- **"不可变快照"名不副实**：`RouterSnapshot`（`ArcSwap`）本应只读，但内部 `Upstream` 又持有 `ArcSwap<Vec<KeyState>>`、`AtomicUsize`、`Mutex<()>`。改一个 key 的状态要走「全局 `admin_write_lock` → `keys_update_lock` → `rebuild_active_keys()` 全量 O(n) 拷贝 → `ArcSwap::store`」。**key 状态变更成本随密钥总量线性增长，且被全局锁串行化。**
- **加权轮询用 `Vec<usize>` 展开权重**（`schedule`，`MAX_WEIGHT=100`）：100 个上游即 100 元素向量，选择时全局原子 `fetch_add`，内存与竞争双重放大。
- **`RequestLogEntry` 是"肥结构 + 四消费者"**：同一结构同时供 JSONL 落盘、内存环形、SSE 广播、指标聚合。`record()` 一次请求 clone 3–4 次并抢两把全局 `Mutex`；`recent()` 每次管理端轮询都**全量 clone 再 O(n log n) 排序**。
- **`id` 不是请求标识**：进程内 `AtomicU64` 自增，重启归 1，无法作为时序键（`4bad36c` 已为此打补丁改为按 `ts_ms` 排序），更无法跨服务追踪。

### 2.2 本轮新增发现：身份、URI、透传分支、缺失的计费维度

这四点直接决定了 v2 的四项结构变更。

**① 主键使用"明文 / 可变名称"，身份不稳定**

- sled 树名用 `u:{upstream_name}`（`storage.rs:26-28`）——**重命名上游即断链**。
- `key_levels` / `key_usage` / `billing` 三棵树**以密钥明文作为主键**。
- admin API 把密钥明文放进 URI 路径段（`billing.rs:321-337`：`/admin/api/v1/billing/{key}/adjust`），把上游名称放进 URI（`admin/upstreams.rs`：`/upstreams/{id}/keys`）。
- 后果：密钥明文进入**访问日志、反向代理缓存、浏览器历史、监控采集**；名称变更导致数据孤岛；且 `billing_key` 已是"凭证 + 余额主键 + 等级主键 + 用量主键"四重身份。
- **v2 对策**：`UpstreamId = uuid v7`，`name` 仅作可变展示名；`UpstreamKeyId = blake3(secret)[..16]`；`AccessKeyId = uuid`（系统生成，密钥只存哈希）；**ID 一律走 JSON 请求体，不进 URI**。

**② "部分透传 + 部分改写"的双路径是重试缺陷的根源**

- 旧版对 OpenAI 上游走宽松路径，对 Anthropic/Gemini 走转换路径；模型名改写散落在 `forward.rs:420-429`（首次）与 `forward.rs:510-529`（重试）两处，各自重新查 `model_map`。
- 只要上游换一家，就要重新推导模型名——这正是「重试时模型重新映射」连续修两次（`825f09b`、`508860c`）的结构原因。
- **v2 对策**：§4.3 统一输出层——所有上游响应先归一为 Canonical（OpenAI 形态）再输出；模型改写的依据是"被选中的绑定（Binding）"，而不是可被复用的 per-upstream map。**结构上无法再泄漏。**

**③ 无缓存维度，无法表达缓存折扣**

- `compute_credit_cost`（`billing.rs:276-306`）只有 `input` / `output` 两维；`think` 被合并进 `output` 后**丢失独立存储**（`state/mod.rs:701` 把 `thought` 加进 `bill_out` 后即不再区分）。
- **v2 对策**：四维计费 + 四维独立存储（input / output / think / cache），见 §6.5。

**④ 缓存层已存在但计费未跟上**

- 上游（Anthropic / Gemini / 部分 OpenAI 兼容网关）已普遍返回 `cache_creation_input_tokens` / `cache_read_input_tokens` 一类字段，旧版解析后被直接丢弃。
- **v2 对策**：usage 解析层显式识别缓存字段并落库；费率默认 `cache = 0.2`。

### 2.3 日志埋点：有日志，没有"埋点体系"

- **span 不跨任务传播**：`proxy_upstream_response` 用 `tokio::spawn` 起结算任务时**未 `.instrument()`**（`proxy/response.rs:267`），流式请求的计费/落盘日志全部丢失 `proxy.request` / `proxy.forward` 上下文。
- **`RequestTiming.attempts` 恒为 0**（`proxy/response.rs:49`）——字段存在但从未写入，"重试了几次"这一最关键诊断维度**事实上未被记录**。
- **`upstream_ms = total_ms - queue_ms`**（`proxy/response.rs:47`）把排队之后的一切（含多次重试、TTFB、流式传输）都算作上游耗时，语义不准。
- **魔法字符串当枚举**：`token_source: Option<String>` 取值 `"upstream"` / `"estimated"`；错误码散落为裸字符串，无错误目录。
- **无关联 id**：请求、事件、持久化记录、响应四者之间没有可贯通的 id。
- **无 schema 版本**：落盘 JSONL 无 `schema_version`，字段增删只能靠 serde 宽容性兜底。
- **敏感信息明文**：`billing_key` 原样落盘、原样经 SSE 广播、原样出现在 admin 响应中。
- **`init_tracing` 名不副实**：注释写 "optional OTLP layer"，实际只有 `EnvFilter` + stdout。
- **静默降级无计量**：大量 `let _ = …` / `.ok()` / `unwrap_or_default()`，降级后无计数器、无告警。

### 2.4 API 结构：手写路由 + 不一致契约 + 一个真实不通的鉴权路径

- **手写路由三处分散**：`route.rs::handle_api` 的 `match (method, path)` + `billing::handle_billing_key_subroutes` + `admin/upstreams::handle_upstream_subroutes` 三级 `strip_prefix` + `split('/')`。新增一个端点要改 3 个文件；路径段**无 URL 解码**。
- **响应契约不统一**：`api_list_upstreams` 返回**裸 JSON 数组**，`api_add_keys` 返回 `{"ok":true,...}`，`api_billing_overview` 返回 `{"billing":..,"usage":..}`，`api_get_model_costs` 返回以模型名为键的对象。分页只有局部实现。
- **错误体两套 shape**：代理侧 `{"error":{message,type,param,code}}`，上游错误重映射后是 `{"error":{message,code}}`（**缺 `type`**），管理侧又各有差异。
- **⚠️ 一处文档与实现不符的真实缺陷**：README 记载实时统计用 `GET /admin/api/v1/stats/stream?token=xxx`，但代码只接受 `X-Admin-Token` **请求头**。浏览器原生 `EventSource` **无法自定义请求头**，该端点在浏览器中按文档调用必然 401。
- **SSE 实现粗放**：手拼 `data: {json}\n\n`，无 `event:`、无心跳、无 `Last-Event-ID` 续传；统计流每 2 秒**构建一次全量快照**再推送。
- **计费路径硬编码**：`ALLOWED_API_PATHS = ["/v1/chat/completions"]`，`is_billable()` 也只认该路径 —— 新增端点需同时改多处常量，且改漏即产生**不计费的代理流量**。
- **`/health` 恒返 200**，无 readiness/liveness 区分。
- **客户端 IP 可伪造**：`resolve_client_ip` 无条件信任 `X-Forwarded-For` 最左值，无 trusted-proxy 白名单。

### 2.5 构建与部署

- **前端产物入库**：`src/static/dist`（React + MUI hashed bundle）曾随源码提交，提交历史中大量「更新前端构建产物」。v2 保留 **embed 单文件部署**能力，但产物不入库（见 §6.7）。
- **HTTP 栈落后两个大版本**：`hyper 0.14` + `http 0.2`，阻断 `axum` / `tower` / `hyper 1.x` 生态，也是"为什么只能手写路由"的技术根因。
- **`Cargo.toml` 为 musl 目标引入 `openssl` vendored**，但连接层全部基于 `hyper-rustls`，疑似冗余（需实测验证）。
- **`#![forbid(unsafe_code)]` 保留**（值得延续的约束）。

---

## 三、归档决策矩阵

### 3.1 ✅ 完整保留（无需补丁）——直接搬运

| 资产 | 位置 | 保留理由 |
|:---|:---|:---|
| Token 估算权重表与算法 | `util.rs:133-226` | 源自 new-api 的字符类加权启发式，零外部依赖、无 tokenizer 文件，±15–30% 精度已满足"上游不给用量"的兜底场景 |
| SSE 输出内容抽取 | `util.rs:231-268` | 同时累积 `reasoning_content` 与 `content`，思维链模型不丢量（**需扩展缓存字段，§6.5**） |
| 请求内容抽取 | `util.rs:278-307` | 与输出侧口径对齐 |
| 密钥字符校验 | `util.rs:349-363` | 含 `..` 路径穿越防护，是安全边界 |
| 通用工具 | `util.rs` `read_body_limit` / `read_body_bytes` / `spawn_result` / `query_get` / `now_ms` | 无状态纯函数 |
| 代理 URL 解析 | `upstream_client.rs:104-145` | 正确处理 userinfo、省略端口、suffix、百分号解码 |
| SOCKS5 连接器 | `upstream_client.rs:147-232` | 手写 `Service<Uri>` + `AsyncRead/AsyncWrite/Connection` 实现正确完整（仅需随 hyper 1.x 改签名，逻辑不动） |
| gzip 流式解压 | `proxy/response.rs:529-568` | 分块解压的输入/输出游标推进与 `StreamEnd` 处理正确 |
| Anthropic 请求映射规则 | `format/request.rs:319-429` | role 归一、system 抽取合并、tools/tool_choice/stop_sequences 映射 |
| Gemini Interactions 请求映射规则 | `format/request.rs:435-674` | 扁平 tools、`generation_config`、`user_input`/`model_output` 语义、`strip_gemini_unsupported_schema`、`unwrap_mcp_content` |
| Gemini 响应 / SSE 事件映射规则 | `format/response.rs:310-556` | `step.start/delta/stop` + `interaction.completed` → OpenAI chunk |
| 上游错误信息抽取 | `format/response.rs:558-609` | 同时支持 JSON 与 SSE 两种错误载体，含"有 error 无 message"兜底 |
| data URI / MIME 推断 | `format/request.rs:196-240` | 20MB 上下限约束与扩展名映射表 |
| 计费数学模型 | `billing.rs:276-306` | micro-credit 定点（1 credit = 10⁶ µcredit）+ `per_request` 双模式（**扩为四维，§6.5**） |
| 预留-结算状态机 | `billing.rs:176-244` | 防余额超扣的领域正确设计（1 µcredit 轻闸 + 差额结算 + 失败归还） |
| 请求日志轮转与反向读取 | `state/requests.rs:314-447`、`admin/stats.rs:451-552` | 按 UTC 日切割、空文件不归档、4KB 分块逆序读 + 跨块行拼接（**v2 继续作为日志主通道，§5.4**） |
| 优雅关闭语义 | `main.rs:146-206` | SIGINT/SIGTERM + inflight 等待 + 超时兜底 |
| Prometheus 指标**语义定义** | `admin/stats.rs:201-408` | 指标选型合理（仅改名） |
| CI 交叉编译矩阵 | `.github/workflows/rust_release.yml` | 4 target + `target-cpu` 调优 + 手动触发 tag |
| `#![forbid(unsafe_code)]`、LICENSE、icon、Dockerfile 多阶段思路 | 根目录 | 约束与资产 |

### 3.2 🟡 保留但需打补丁

| 资产 | 位置 | 现存问题 | 补丁方向 |
|:---|:---|:---|:---|
| `KeyStore` | `storage.rs` | 每次 `open_tree`；整表 `flush`；`add_key_usage` 读-改-写 + 每次 `flush`；`check_monthly_reset` 的 `clear()` 未持锁 | 迁 SQLite（§5）；批量事务；单写者；**用量与账户合一**（§6.5） |
| `BillingStore` | `billing.rs:31-100` | 双持久化通道；flush 失败仅告警；`0` / `-1` 语义靠约定 | 并入访问密钥账户行；`Unlimited` 用 `Option` 显式建模 |
| `Upstream::select_key` / `rebuild_active_keys` | `state/upstream.rs:289-327` | 全量 O(n) 拷贝 + `ArcSwap::store`，且在全局锁内；**位置偏置致前段密钥被反复使用** | **两级 SWRR + LRS，`rebuild_active_keys` 整体移除，§6.4** |
| 加权轮询 | `state/routes.rs:392-429` | `Vec<usize>` 展开权重（≤100 × N） | 上游与密钥统一改 SWRR，消除内存放大与位置偏置（§6.4） |
| 密钥失效恢复任务 | `state/upstream.rs:132-211` | `sleep(10s)`；逐 key 串行；判定过宽；无退避、无指标 | 升级为统一探针服务（§6.1） |
| 429 冷却 | `state/upstream.rs:63-78` | 恒定 3s、无指数无抖动；5xx 不冷却；排队唤醒同步惊群；每次排队 O(U×K) 全量扫描 | 指数退避 + 全抖动 + 上游级半开熔断 + 冷却最小堆（§6.2） |
| SSE usage 解析 | `proxy/usage.rs:151-230` | 仅在 `completion > 0` 时采纳上游 usage；`total` 合并用多层 `max()`；**丢弃缓存字段** | 明确 `TokenSource` 优先级；**补齐 cache / think 四维**（§6.5） |
| 内存请求日志 | `state/requests.rs:81-168` | 肥结构；`record()` 多次 clone + 双全局锁；`recent()` 全量 clone + 排序 | 分离为 `EventBus` / `RecentRing` / `RecordSink` |
| 统计口径 | `proxy/response.rs:44-50` | `upstream_ms` 语义不准；`attempts` 恒 0 | 重定义 timing 分解，真实记录重试次数 |
| SIGHUP 热重载 | `state/mod.rs:740-789` | 与 `RouterState` 字段重复；`queue_max_depth` 缩容不生效 | 消除重复字段；配置单一真源；缩容用令牌回收 |
| 排队实现 | `proxy/forward.rs:540-616` | `Semaphore` + `Notify` + 1s tick 轮询；`next_cooldown_delay` 每次全量遍历 | 保留骨架，去掉 tick 轮询，冷却索引化（§6.2） |
| CORS | `proxy/server.rs:280-333` | `allow-headers` 硬编码；默认 `*` | 配置化并收紧默认值 |
| 客户端 IP 解析 | `proxy/server.rs:249-278` | 无 trusted proxy 白名单，XFF 可伪造 | 增加 `trusted_proxies` 配置 |
| 统一错误体 | `util.rs:26-40` + `format/response.rs:45-78` | shape 不一致（缺 `type`）；错误码为散落字符串 | 统一错误目录（enum → 稳定字符串）+ 一致 envelope |
| Prometheus 输出 | `admin/stats.rs:201-408` | 前缀仍为 `gptload_*`；手写字符串拼接易错 | 改 `aequi_*`；结构化生成（D5 不兼容，**不提供旧别名**） |
| 优雅关闭 | `main.rs:152-184` | 超时后 `std::process::exit(1)`；`store.flush()` 尽力而为 | 保留语义，改为受控停机 + 明确退出码 |
| 上游错误处理 | `format/response.rs:45-78` | 客户端只见映射类型，原始错误仅入日志，无关联手段 | 增加 `request_id` 贯通 |
| 模型路由 | `state/routes.rs` 全量 | 三个真源（见 2.1 表 #2） | 收敛为 **`ModelBinding` 单一真源**（§4.2.1 / §6.3） |
| Gemini `Api-Revision` | `format/request.rs:659-662` | 版本号 `2026-05-20` 硬编码 | 提升为 binding 级可配置 |
| 时区策略 | `Dockerfile` 装 `tzdata`，日志用 UTC 自算日期 | 容器时区与日志时区不一致 | 统一时区策略并配置化 |

### 3.3 🔴 完全使用新架构替代

| 组件 | 位置 | 为何不能补丁 |
|:---|:---|:---|
| `RouterState` 容器 | `state/mod.rs:35-152` | 25+ 字段 God Object；**手写 `Clone` 静默重建 queue/semaphore/调度游标**，任何克隆即产生分叉状态 |
| `RuntimeConfig` | `state/mod.rs:70-81`、`818-842` | 运行配置 / 费率表 / 脱敏序列化副本三种关注点耦合；脱敏靠字符串启发式，安全性不成立 |
| 请求日志事件模型 | `state/requests.rs:6-52` | 肥结构、无 schema 版本、`id` 不可用、凭证明文，且被 4 个消费者耦合 |
| `RequestsLog` + `RequestMetrics` | `state/requests.rs:81-263` | `Mutex<VecDeque>` + `Mutex<Metrics>` + mpsc + broadcast；每请求抢锁多次；4 窗口 gap 填充在时钟跳变时可爆量 |
| admin API 路由层 | `route.rs:102-151`、`billing.rs:321-367`、`admin/upstreams.rs:23-105` | 手写 match + 多级 `strip_prefix` + `split('/')`，无 URL 解码，**ID/密钥进 URI**，新增端点改三处，无统一 envelope |
| SSE 层 | `route.rs:153-248` | 2s 轮询全量快照；无 `event:`/心跳/续传；**EventSource 无法带 header 而代码只认 header** |
| HTTP 栈 | `Cargo.toml`（hyper 0.14 / http 0.2） | 落后两个大版本，是手写路由与手写 SSE 的技术根因（§4.6） |
| 模型目录 | `state/routes.rs:21-49`、`431-450` | 同一关系存正反两份手工同步，需重构为单一 `ModelBinding` 目录 + 派生索引 |
| **存储引擎** | sled 五树 + 定长字节布局 | 定长 4/16/8/32 字节布局无法容纳四维用量、绑定、分组；且事务性缺失导致丢失更新。迁 SQLite（§五） |
| **协议层** | `format/` + `forward.rs` 双路径 | 保留映射规则（3.1），但**调用结构改为统一输出层**，消除透传分支（§4.3） |
| 内嵌前端产物 | `src/static/dist` | 产物入库、无源码、提交历史噪音源。改为构建期注入（§6.7） |

### 3.4 🗑 移除

| 项 | 位置 | 依据（已验证） |
|:---|:---|:---|
| `KeyStore::export_json` / `import_json` | `storage.rs:266-315` | 全仓 grep **无任何调用点**；admin 已有独立导出接口 |
| `x-proxy-token` 清理 | `state/upstream.rs:434` | 鉴权已于 `51cc0df` 移除，仅剩 header 清理残留 |
| `RouterState.request_timeout` / `.max_retries` / `.retry_status_codes` | `state/mod.rs:38-40` | grep 验证：仅在手写 `Clone` 内被读取；**纯写死字段** |
| `RouterState.admin_tokens` / `.key_config` | `state/mod.rs:41,43` | 仅 `main.rs` 启动打印使用，reload 后陈旧 |
| `RequestTiming.attempts` | `state/requests.rs:41`、`proxy/response.rs:49` | 硬编码 `0`，从未写入（以真实 `retry_count` 取代） |
| `RuntimeConfig.preview` + `redact_config_value` | `state/mod.rs:80`、`818-842` | 序列化副本冗余；脱敏策略不可靠（改 `Secret<T>` 类型级脱敏） |
| `ModelRoutesFile.models`（正向索引） | `state/routes.rs:24` | 由绑定派生，双写是失配源 |
| `Upstream.model_rmap` | `state/upstream.rs:26` | 由绑定派生 |
| `Stats.responses_3xx` / `UpstreamStats.responses_3xx` | `state/mod.rs:171,190` | 上游 3xx 无业务含义 |
| `ALLOWED_API_PATHS` 单元素硬编码白名单 | `proxy/forward.rs:286` | 改为配置驱动 + 计费策略表 |
| `RequestLogContext.request_body`（内存留 16KB 明文） | `proxy/mod.rs:65` | 仅用于 token 估算；改为流式估算或不驻留 |
| `#![allow(dead_code)]` | `storage.rs:1` | 掩盖死代码，重构后消除 |
| `Cargo.toml` musl 分支 `openssl` vendored | `Cargo.toml:50-52` | 连接层全部基于 `hyper-rustls`，疑似冗余 —— **移除前需实测验证** |
| 前端构建产物 | `src/static/dist` | 改为构建期注入，确认不回收 |
| **月度重置调度与全局 `tree.clear()`** | `state/requests.rs:526-536`、`storage.rs:148-158` | D9 不设周期重置；由管理员手动分发（§6.5） |
| **`-1` 无限哨兵** | `billing.rs` 多处、`config.rs:206` | 改为 `Option` / enum 显式建模（§八 第 5 条） |
| **`X-Admin-Token` 与代理鉴权混用** | `state/mod.rs:447-455` | 管理端与代理端鉴权分离（§4.5） |

### 3.5 决策冻结表（D1–D12）

| # | 决策点 | **最终结论** | 章节 |
|:---|:---|:---|:---|
| D1 | 前端归属 | 保留 `rust-embed` **单文件部署**；静态资源挂 `/`，**SPA 路由由前端自管**（后端只做 fallback）；后端只提供必要的数据查询与 CRUD 接口 | §6.7 |
| D2 | 存储引擎 | **SQLite**：关系数据入库 + WAL + 单写者 + 批量事务；请求日志**仍走 JSONL 主通道**（可配双写），配套自动清理与 WAL 治理 | §五 |
| D3 | HTTP 框架 | **axum 1.x + tower + hyper 1.x** | §4.6 |
| D4 | 协议范围 | **OpenAI 为唯一对外协议**；上游响应统一归一为 Canonical 后输出；**取消 BYOK 透传** | §4.3 |
| D5 | 兼容级别 | **不兼容**。一次性单向迁移，不保留兼容读层 | §5.5 |
| D6 | 计费费率 | 四维 **input / output / think / cache**；默认倍率 **1 / 1 / 1 / 0.2**；think 计入输出计费但**独立存储**；保留积分制 | §6.5 |
| D7 | crate 粒度 | **单 crate + 强边界模块** | §4.1 |
| D8 | 分组模型 | 分组**只决定可访问的模型集合**；**倍率按模型统一配置**；模型存储**必须携带上游 id**（`ModelBinding`） | §6.3 |
| D9 | 两层额度 | **上游密钥额度**（token 上限 → 自动摘出轮转池，对客户端透明）+ **访问密钥积分**（上限 → 拒绝请求，管理员手动分发）；**两者均无周期重置**。否决：周期重置、把两层混为一谈（恢复方式不同，混用必然误恢复） | §6.5 |
| D10 | 探针策略 | 统一探针服务 + 五态（含 `Unverified`）+ 预算/并发约束 + 指数复核间隔 | §6.1 |
| D11 | 轮询算法 | **两级 SWRR + LRS**（上游级与密钥级），移除活跃集重建 | §6.4 |
| D12 | 熔断粒度 | **上游级半开熔断 + 密钥级指数退避**双层 | §6.2 |
| D13 | 上游密钥额度 | **极简显式计数**（`limit_tokens` + `safety_margin`）→ 触顶置 `QuotaExhausted` 并**摘出轮转池、不随时间恢复**；上游明确报配额耗尽时走**快速确认路径**；**不做**周期/多维/每模型额度、不主动查上游余额 | §6.5.1 |
| D14 | 列表分页 | 统一 **数据库 keyset 分页**（`seq` 游标 + 覆盖索引）；**禁止全量加载后截断**（旧版 `keys.load_full().skip().take()`） | §4.5 / §5.6 |

---

## 四、新架构蓝图

### 4.1 分层与模块结构

**设计原则（D7）**：单 binary crate + 强边界模块。仅把「纯领域模型 + 无 IO」抽为 `core/` 以示约束，日后再按需外提。

```
src/
├── main.rs                 # 仅组装与生命周期（不承载业务）
├── core/                   # 纯领域：零 IO、零 hyper、零 sqlx，可 100% 单测
│   ├── id.rs               # UpstreamId(Uuid7) / BindingId / GroupId / AccessKeyId / UpstreamKeyId(指纹) / RequestId
│   ├── secret.rs           # Secret<T>：Debug/Serialize 自动脱敏
│   ├── config.rs           # AppConfig（不可变）、KeyPolicy、ServerPolicy、BackoffPolicy、ProbePolicy
│   ├── binding.rs          # ModelBinding：display_model × upstream_id × upstream_model（单一真源）
│   ├── group.rs            # Group + GroupPolicy + 授权判定唯一函数
│   ├── routing.rs          # SwrrSelector（上游级 / 密钥级）+ 冷却最小堆
│   ├── backoff.rs          # BackoffPolicy + BreakerState（纯状态机，无 IO）
│   ├── rating.rs           # 四维费率 + compute_credit_cost
│   ├── account.rs          # Credits / UsageCounters(四维) / Reservation / Settlement
│   └── telemetry/{event.rs, codes.rs}   # TelemetryEvent(schema v1) + ErrorCode 目录
├── storage/                # SQLite 端口与适配
│   ├── schema.rs           # 迁移脚本（embedded SQL）
│   ├── repo.rs             # trait：UpstreamRepo / BindingRepo / GroupRepo / KeyRepo / AccountRepo
│   ├── sqlite.rs           # 单写者 actor + 批量事务 + 读连接池
│   └── logsink.rs          # JSONL 日切 + 保留期清理（可切换 sqlite sink）
├── upstream/               # 连接层 + 统一输出层
│   ├── connect.rs          # direct / http-proxy / socks5（迁移现有连接器）
│   ├── probe.rs            # 统一探针服务（§6.1）
│   └── adapter/{openai,anthropic,gemini}.rs   # wire ⇄ Canonical 纯函数
├── gateway/                # 对外 HTTP
│   ├── router.rs           # axum Router + 中间件栈
│   ├── auth.rs             # 管理端鉴权 与 访问密钥鉴权（分离）
│   ├── proxy.rs            # /v1/* 编排（鉴权→绑定解析→分组过滤→选路→预留→转发→结算）
│   ├── admin/              # /admin/{域}/{动作}（§4.5）
│   ├── sse.rs              # 事件流（心跳 + Last-Event-ID）
│   └── web.rs              # rust-embed + SPA fallback（§6.7）
├── runtime/                # 状态容器与后台任务
│   ├── state.rs            # AppState（Arc，**不实现 Clone**）
│   ├── queue.rs            # 容量等待（无 tick 轮询）
│   └── tasks/              # 日志轮转与清理 / 探针调度 / WAL checkpoint
└── static/dist/            # 构建期注入的前端产物（不入库）
```

**关键约束（写成 CI 检查项）**

1. `core/` 禁止依赖 `hyper` / `tokio` / `sqlx` / `sled`。
2. `AppState` **禁止实现 `Clone`**；共享一律 `Arc<AppState>`。
3. 所有 `tokio::spawn` 必须经 `spawn_with_context()`（内部 `.instrument(parent_span)` 或携带 `RequestId`）。
4. 所有对外字段访问不得返回 `&str` 明文凭证，必须经 `Secret<T>`。
5. **`gateway/proxy.rs` 不得出现 `match upstream.format { … }` 形式的差异化输出分支**——格式差异只能出现在 `upstream/adapter/*`（§4.3）。

### 4.2 数据模型重设计

| 旧实体 | 新实体 | 变化 |
|:---|:---|:---|
| `RouterState`（God） | `AppState` | 拆为 `config: ArcSwap<AppConfig>` / `catalog: Arc<Catalog>` / `registry: Arc<UpstreamRegistry>` / `stats: Arc<Stats>` / `bus: EventBus` / `repo: Arc<Repo>` / `queue: Arc<Queue>`；**移除 Clone** |
| `Upstream`（`id` = 名称，可变内嵌） | `UpstreamSpec { id: UpstreamId(Uuid7), name, … }`（不可变）+ `UpstreamRuntime`（原子状态） | **id 与 name 分离**；改配置 = 原子替换 `Arc<UpstreamSpec>`；重命名不再影响任何关联数据 |
| `Upstream.models` + `model_map` + `model_rmap` + `ModelRoutesFile` | **`ModelBinding`** | 单一真源；三处双写与反向表全部消失（§4.2.1） |
| `Vec<Arc<KeyState>>` + `rebuild_active_keys()` + `active_keys` | `ArcSwap<Box<[UpstreamKeySlot]>>` | 结构变更（增删）才换数组；**状态变更不再重建**；资格由原子状态即时判定 |
| `KeyState { key, auth_header, failure_count, status, … }` | `UpstreamKey { seq, id, upstream_id, secret: Secret<Arc<str>>, health, enabled, quota, inflight, cooldown_until_ms, weight, last_selected_seq, served_total, cw, rl_streak, err_streak, meta }` | 明文不再是主键；header 按 format 现场构造；**额度（`quota`）与人工暂停（`enabled`）挂在密钥上**（§6.5.1）；`seq` 供分页排序 |
| `billing_key`（明文，四重身份） | `AccessKey { seq, id: AccessKeyId(Uuid), hash, prefix, groups, credits_cap, credits_used, usage(四维), lifetime_credits_used, enabled, … }` | **系统生成密钥，只存哈希**；ID 与密钥解耦；**这是下游侧，与上游密钥额度是两层不同架构** |
| 旧版 key 列表 `offset/limit` + 全量加载后截断 | 所有列表走 **DB keyset 分页**（`seq` 游标 + 覆盖索引） | 见 §4.5 分页规范 / §5.6；旧版 `upstream.keys.load_full()` 后 `skip/take` 的写法禁止复用 |
| `key_levels`（i32） + `min_key_level`（i32） | `Group` + `GroupPolicy` + 成员关系 | 等级 → 集合（§6.3） |
| `billing` + `key_usage` + `global_stats` | `AccessKey` 单行账户字段 | 三真源合一；全局统计从账户聚合派生 |
| 定长 4/16/8/32 字节键值 | SQLite 强类型列 | 加维度不再需要改格式 |
| `RuntimeConfig` | `AppConfig`（不可变）+ `RateTable`（独立热更新） | 移除 `preview`；配置单一真源 |
| `schedule: Vec<usize>` | `SwrrSelector` | 平滑加权轮询，无权重展开 |
| `RequestLogEntry` | `TelemetryEvent`（enum + `schema_version`） | 见 §4.4 |
| `RequestsLog`（4 消费者耦合） | `EventBus` / `RecentRing` / `RecordSink` / `Metrics` 四者分离 | 每消费者单一职责 |
| `RequestMetrics`（4 窗口 VecDeque） | 预聚合计数器 + 有界桶（惰性补零） | 移除每请求 4 次 VecDeque 更新 |

#### 4.2.1 `ModelBinding`（D8 的核心）

**问题**：对外暴露的是**单一模型 id**，但同一个 id 可能存在于多个上游（且在不同上游对应不同真实模型名）。若只按"模型 id"授权，会出现"分组 A 被允许访问 `gpt-4o`，结果命中了分组 A 本不该访问的上游 B"。

**解法**：模型是一个**绑定**，绑定必须携带上游身份。

```rust
pub struct ModelBinding {
    pub id: BindingId,              // Uuid7
    pub display_model: ModelId,     // 对外暴露的单一 id（可重命名 / 加别名）
    pub upstream: UpstreamId,       // ★ 必带上游身份
    pub upstream_model: String,     // 上游实际模型名
    pub enabled: bool,
    pub rate_override: Option<Rate>,// 默认 None → 用模型级统一倍率
}
```

- 一个 `display_model` 对应 **1..N 个绑定**（多上游负载均衡）。
- 授权与选路顺序固定为：**先解析绑定 → 再按分组过滤绑定 → 再在允许的上游间选路**。
- 模型名改写的依据是**被选中的绑定**（`binding.upstream_model`）；重试时重新解析绑定，因此**不存在"上一家上游的映射泄漏到下一家"的可能**——结构性地修掉 `825f09b` / `508860c` 那类缺陷。
- `Upstream.model_map` / `model_rmap` / `models_routes.json` 全部被本实体取代。

**派生索引（内存，仅供查询加速）**：`by_display: HashMap<ModelId, SmallVec<[BindingId; 2]>>`、`by_upstream: HashMap<UpstreamId, Vec<BindingId>>`。由绑定集合**单向派生**，不落库、不双写。

### 4.3 统一输出层（D4）

**原则**：OpenAI 是**唯一对外协议**。上游可以是 OpenAI / Anthropic / Gemini 三种 wire 格式，但**所有响应在输出前必须归一为 Canonical（OpenAI 形态）**。

```
                    ┌─────────────────────────────────────────────┐
  client(OpenAI) ──▶│ CanonicalRequest  (唯一内部货币)             │
                    └──────────────┬──────────────────────────────┘
                                   │ adapter.encode(format)
              ┌────────────────────┼────────────────────┐
              ▼                    ▼                    ▼
        OpenAI wire         Anthropic wire        Gemini wire
         （近恒等）            （字段映射）          （Interactions）
              │                    │                    │
              └────────────────────┼────────────────────┘
                                   │ adapter.decode(format)
                    ┌──────────────▼──────────────────────────────┐
                    │ CanonicalResponse / Stream<CanonicalChunk>  │
                    └──────────────┬──────────────────────────────┘
                                   │ 单序列化器
                                   ▼
                            client(OpenAI)
```

**硬性规则**

1. **不存在"透传旁路"**。即便上游就是 OpenAI 格式，也必须经 `CanonicalResponse` 出口；"近恒等"只是 OpenAI adapter 内部的实现细节（可直接复用已解析的 `serde_json::Value`，零额外拷贝），**不是一条独立的代码路径**。
2. **取消 BYOK 透传**：客户端凭证一律不转发；上游凭证只来自密钥池。客户端请求头白名单化（只放行 `accept` / `content-type` / `user-agent` 等），杜绝"客户端自带 Authorization 打到上游"。
3. `gateway/proxy.rs` 中不得出现按 format 分支的差异化输出逻辑（CI 检查项 §4.1 约束 5）。
4. 差异全部收敛在三个 adapter 内，且 adapter 是**纯函数**（输入 wire bytes / 输出 canonical），可脱离网络单测——这也让 3.1 中"完整保留"的映射规则能以最干净的形式搬迁。
5. **流式与非流式共用同一 Canonical 类型**：`Stream<CanonicalChunk>` 与非流式 `CanonicalResponse` 由同一 adapter 产出，避免旧版"流式路径与非流式路径双实现导致行为漂移"。

**收益**：重试/故障转移不再需要"重新推导模型名"的脆弱逻辑；新增上游格式 = 新增一个 adapter，`gateway` 零改动；客户端始终拿到结构一致的 OpenAI 响应与错误体。

### 4.4 日志与埋点（三通道分离）

```
                     ┌──────────────────────────────────────────┐
   RequestId(uuidv7) │  贯穿：span / 事件 / 落盘 / 响应头          │
                     └──────────────────────────────────────────┘
                                     │
     ┌───────────────────┬───────────┴───────────┬────────────────────┐
     ▼                   ▼                       ▼                    ▼
 ①结构化日志          ②事件流                  ③持久化记录          ④指标
 tracing + OTLP       EventBus(broadcast)      RecordSink(jsonl)   Prometheus
 运维排障             UI/SSE 实时             审计/计费           aequi_*
```

- **`TelemetryEvent`**：`#[serde(tag = "kind")]` 的 enum，每事件带 `schema_version: u16`（从 1 开始，**本轮能力所需维度一次性加全**，见 §6.6）。
- **关联 id**：`RequestId = uuidv7`（时间有序，可作时序键）；回填响应头 `x-request-id` / `x-upstream-id`。
- **span 传播**：统一 `spawn_with_context()` 包装，杜绝流式结算任务丢上下文。
- **敏感字段**：`Secret<T>` 包装；落盘/广播只留 `AccessKeyId` 与 `UpstreamKeyId`（指纹），明文只存于内存与上游请求头。
- **类型化替代魔法字符串**：`ErrorCode` enum（稳定 `as_str()` + HTTP 状态映射）；`TokenSource::{Upstream, Estimated}`。
- **`timing` 重定义**：`queue_ms` / `upstream_ttfb_ms` / `upstream_total_ms` / `retry_count`（真实计数）/ `total_ms`。
- **静默降级可观测**：所有降级路径必须 `counter!` 或至少 `warn!` 带结构化字段。
- **指标命名**：`gptload_*` → `aequi_*`（D5 不兼容，**不提供旧别名**）。

### 4.5 API 约定（`admin/{域}/{动作}`）

**原则**：管理端 API 统一为 `POST /admin/{domain}/{action}`，参数一律走 **JSON 请求体**；**ID / 名称 / 密钥不得出现在 URI 中**。

**为什么**

1. 规避旧版缺陷：`/admin/api/v1/billing/{key}/adjust` 把**密钥明文写进 URI**（进访问日志、反代缓存、浏览器历史）；`/upstreams/{name}/keys` 把**可变名称**当身份。
2. 无 URL 解码问题：旧版 `split('/')` 不做解码，含特殊字符的 id 直接解析错误。
3. 契约统一：所有动作同一形状，可枚举、可生成文档、可统一鉴权与审计。
4. 扩展成本恒定：新增动作 = 新增一个 handler，不加路由模式。

**约定**

```
POST /admin/{domain}/{action}
Content-Type: application/json
Authorization: Bearer <admin-token>          # 与代理端鉴权分离
{ ...动作参数（含 id、过滤、分页） }

200 { "ok": true,  "data": <T>,    "meta": { "request_id": "...", "cursor": "...", "total": 123 } }
4xx { "ok": false, "error": { "code": "upstream_not_found", "message": "...", "details": [...] },
      "meta": { "request_id": "..." } }
```

**域与动作清单**

| 域 | 语义 | 动作 |
|:---|:---|:---|
| `/admin/upstream/*` | 上游管理 | `list` `get` `create` `update` `delete` |
| `/admin/binding/*` | 模型绑定管理 | `list` `get` `create` `update` `delete` |
| `/admin/model/*` | 对外模型 id 与倍率 | `list` `update` |
| `/admin/group/*` | 分组管理 | `list` `get` `create` `update` `delete` |
| `/admin/ukey/*` | **上游密钥管理** | `list` `import` `delete` `enable` `disable` `validate` `export` |
| `/admin/akey/*` | **访问密钥管理**（发给朋友的） | `list` `create` `update` `delete` `enable` `disable` `groups` `credits` `reset` |
| `/admin/log/*` | 请求日志 | `query` `cleanup` `export` |
| `/admin/stats/*` | 统计与监控 | `overview` `metrics` `stream` |
| `/admin/system/*` | 系统 | `config` `reload` `health` `version` |

**要点**

- `create` 返回一次性明文密钥（访问密钥由系统生成）；`list` / `get` 只返回 `id` + `prefix`，**永不返回明文**。
- 上游密钥与访问密钥**分域**（`ukey` / `akey`），彻底消除旧版"一个 `key` 概念同时指代两种东西"的混乱。
- 过滤统一 `{ filter: {…} }`。

**分页规范（强制：数据库分页，禁止全量读取后截断）**

旧版 `api_list_keys`（`admin/keys.rs:338-348`）的做法是 `upstream.keys.load_full()` 后 `.skip(offset).take(limit)` —— **把全部密钥读进内存再截断**。密钥量大时单次请求的内存与耗时随总量线性增长。v2 明确禁止该模式。

```
请求：{ "filter": { "upstream_id": "<uuid>", "health": "Active" },
        "cursor": "eyJzZXEiOjEyMzR9",   // 不透明游标；首页传 null
        "limit":  100 }                  // 上限 1000，默认 100

响应：{ "ok": true,
        "data": [ { "seq": 1235, "id": "…", "prefix": "sk-…", "health": "Active", … } ],
        "meta": { "request_id": "…", "next_cursor": "eyJzZXEiOjIzMzR9",
                  "has_more": true, "total": 4321 } }
```

| 规则 | 说明 |
|:---|:---|
| **游标分页（keyset）**，不是 `OFFSET` | `OFFSET` 在深分页时要扫描并丢弃前 N 行，仍是 O(offset)；keyset 走到索引位置直接定位，恒为 O(limit) |
| 游标内容 | `base64({ seq, filter_hash })` —— 不透明；`filter_hash` 用于**拒绝"换过滤条件复用旧游标"**（返回 `cursor_filter_mismatch`） |
| 排序 | 固定按 `seq` 升序（`seq` 为 `INTEGER PRIMARY KEY AUTOINCREMENT`）；**不用随机 id 排序**，避免游标语义漂移 |
| `total` | 昂贵的 `COUNT(*)`：默认只在首页计算并随 `meta` 返回；后续页可传 `"with_total": false` 跳过 |
| 列表载荷 | 只含 `id` / `prefix` / 状态 / 额度摘要，**不含密钥明文、不含四维用量明细**（明细另走 `akey/get` 单条查询） |
| 上限 | `limit ≤ 1000`，超出即报 `limit_too_large`（避免一次拉爆） |
| 内存与 DB 的关系 | **路由热路径**需要全部密钥（SWRR 要在全集上评分），故内存持有槽位数组；**管理端列表**则必须走 DB 分页。两者同源于 `upstream_key` 表，由单一写者保持一致 |

**动作补充**（配合 §6.5.1）

| 动作 | 用途 |
|:---|:---|
| `/admin/ukey/list` | 按上游 / 健康态 / 额度状态过滤 + 游标分页 |
| `/admin/ukey/pause` · `/admin/ukey/resume` | 人工暂停 / 恢复（`enabled`） |
| `/admin/ukey/validate` | 触发探针（§6.1） |
| `/admin/ukey/quota` | 设置 `limit_tokens` / `safety_margin` |
| `/admin/ukey/reset` | 清 `used_tokens` 并解除 `QuotaExhausted`（§6.5.1） |
- **代理端 `/v1/*` 保持 OpenAI 契约不变**（`/v1/chat/completions`、`/v1/models`）；`/health`、`/ready`、`/metrics` 保留运维惯例。
- SSE 改为 `POST /admin/stats/stream`，但 `EventSource` 无法发 POST/自定义头 → **客户端改用 `fetch` + `ReadableStream` 解析 SSE**，并在文档中明确（修正 v1 记录的文档缺陷）；服务端实现心跳 + `Last-Event-ID`。

### 4.6 HTTP 框架选型评估（D3）

#### 候选方案对比

| | A. axum 1.x + tower | B. hyper 1.x 手写 | C. actix-web 4 |
|:---|:---|:---|:---|
| 路由 | `matchit` 基数树 + 类型化 extractor | 自己写 `match (method, path)` + `strip_prefix` | 宏路由 / builder |
| 中间件 | **tower / tower-http 生态**，可组合复用 | 自己写，每个都要手写 `Service` | 自有体系（非 tower） |
| SSE | `axum::response::sse::Sse` 内建（含 `KeepAlive`） | 手拼 `data: ...\n\n` | 手动拼 `Bytes` |
| 流式 body | `Body` / `BodyStream` | 原生 `hyper::Body` | 自有 `Payload` |
| 与连接层复用 | **同一套 `http` / `hyper` 类型**，SOCKS5 连接器可原样复用 | 同 | 自有 `actix-http` 类型 → **连接器需重写** |
| 路由+中间件代码量 | 低 | **高（旧版缺陷集中地）** | 中 |
| 性能（预期） | ≈ B（同底层 hyper） | 基准 | 微基准略优 |
| 迁移成本 | 中（连接层改签名） | 低（不换） | **高（类型体系不同）** |

#### 性能预测（用于判断"是否值得"）

本服务是 **IO 密集**：单请求耗时由上游决定（数十毫秒至数十秒），框架开销占比可忽略。

| 项 | 预估值 | 说明 |
|:---|:---|:---|
| 路由匹配 | ~0.1–0.3 µs | 基数树，与路径段数相关 |
| 5 层中间件（request_id / trace / auth / cors / metrics） | ~0.5–1.5 µs 合计 | tracing 与 metrics 为主要成本，与框架无关 |
| JSON 反序列化 | 与 B 相同 | 同为 `serde_json` |
| **端到端吞吐差异** | **≤ 3%（相对手写）** | 相对上游延迟可忽略 |
| P99 延迟 | 无变化 | 由上游与排队主导 |
| 稳态内存 | 基本无增量 | extractor 借用，无 per-request 额外分配 |

> C 的微基准优势（常见 5–15%）在本场景**无法转化为用户体验差异**，却要付出连接层重写与生态切换成本。

#### 结论：选 A（axum 1.x + tower + hyper 1.x）

1. **直接消灭审计发现的三类缺陷**：手写路由（3 处分散、无 URL 解码）、手写 SSE（无心跳/续传、EventSource 鉴权不通）、手写中间件（CORS / IP 解析硬编码）。这不是"引入新复杂度"，而是**替换掉已有的等价复杂度，且代码量更少**。
2. **与连接层同源**：axum 直接用 `http` / `hyper` 类型，3.1 中要求"完整保留"的 SOCKS5 连接器只需改签名；选 actix 会把这份资产作废。
3. **tower 是现成的正确抽象**：超时、限流、追踪、压缩、CORS 都是既有轮子，避免"自己写一个半对的中间件"。
4. **性能不构成决策因素**，因此不应为 5–15% 的微基准选择更远的生态。

#### 明确**不做**的事（拒绝过度设计）

| 不做 | 原因 |
|:---|:---|
| 不引入 DI 容器 / 服务注册 | 个人项目，`AppState` + 明确构造函数足够 |
| 不引入全功能 ORM（sea-orm / diesel） | 表结构固定简单；`sqlx` 显式 SQL + 编译期校验足够，ORM 会引入映射层与隐式查询 |
| 不引入 OpenAPI 代码生成（utoipa 等） | 端点以"域/动作"枚举为主题，手写一页文档成本更低，且避免宏污染编译时间 |
| 不为"多租户 / 商业化"预留抽象 | 明确面向个人使用与分享给朋友（D6 / D8 / D9） |
| 不引入消息队列 / 分布式缓存 / Redis | 单进程单实例；`EventBus` 用 `tokio::broadcast` 足够 |
| 不拆 crate（D7） | 无复用需求，拆分只增加编译与联调成本 |
| 不做数据库分库分表 / 连接池调优到极致 | 单机个人规模，SQLite 单写者已足够 |

### 4.7 依赖清单

| 依赖 | 现状 | 目标 | 说明 |
|:---|:---|:---|:---|
| `hyper` | 0.14 | 1.x（+ `hyper-util` / `http-body-util`） | 前置条件，解锁 axum 生态 |
| `http` | 0.2 | 1.x | 随 hyper 升级 |
| 新增 | — | `axum` / `tower` / `tower-http` | 路由与中间件（§4.6） |
| 存储 | `sled 0.34` | **`sqlx`**（SQLite，`runtime-tokio` + `macros`） | 见 §五；显式 SQL，不用 ORM |
| TLS | `hyper-rustls 0.24` + `with_native_roots` | 新版 rustls（评估 `aws-lc-rs` / `ring`） | 同时解决 musl 原生证书痛点 |
| 新增 | — | `uuid`（v7）、`blake3`、`smallvec`、`rand` | 身份 / 指纹 / 紧凑集合 / 抖动与密钥生成 |
| 移除 | `openssl`（musl vendored） | 移除并实测 | 疑似冗余 |
| 移除 | `sled` | 迁移完成后从主程序移除 | 仅保留在离线迁移工具（独立 bin / feature） |
| 保留 | `rust-embed` | 保留 | D1 单文件部署 |
| 保留 | `tracing` / `tracing-subscriber`、`serde`+`preserve_order`、`arc-swap`、`ahash`、`bytes`、`flate2`、`percent-encoding`、`toml` | — | 选型合理 |

---

## 五、存储方案（SQLite）与单向迁移

### 5.1 分层原则：关系数据入 SQLite，请求日志走 JSONL

**这是本节最重要的一条判断。** 请求日志是**追加密集型**数据（目标 1.7k RPS ≈ 每天百万行量级），而 SQLite 是**页式 B-tree**：

| 方案 | 写入成本 | 空间治理 | 查询 | 结论 |
|:---|:---|:---|:---|:---|
| 逐条 `INSERT`（自动提交） | 每请求一次事务 + WAL 追加 + 页分裂 | WAL 持续增长、需频繁 checkpoint | 灵活 | ❌ 高 RPS 下成为瓶颈 |
| SQLite + 批量事务 | 可接受（攒批后约 50–100 事务/秒） | 仍需 `DROP` / `VACUUM` 治理 | 灵活（SQL） | 🟡 备选 |
| **JSONL 日切文件** | **O(1) 顺序追加，无事务、无 WAL** | 按日 `rename` 归档、按日期直接删文件 | 时间范围 = 读 1–2 个文件（旧版已有 4KB 反向分块读取） | ✅ **主通道** |

**定案**：`log_sink` 可配置为 `jsonl`（默认）/ `sqlite` / `both`。

- **默认 `jsonl`**：沿用 3.1 中判定"算法正确且高效"的旧实现（UTC 日切、空文件不归档、4KB 反向分块读）。
- **`sqlite`**：给需要 SQL 聚合的场景，必须配合 §5.3 的批量写入与 §5.4 的分区治理。
- **`both`**：SQLite 只存**摘要行**（不含 body），JSONL 存全量，用于审计。

关系数据（上游、绑定、分组、密钥、账户、成员关系）**全部入 SQLite**——写入频率低、需要事务与关联查询，正是 SQLite 的强项。

### 5.2 SQLite 调优清单

```sql
-- 连接建立时执行
PRAGMA journal_mode = WAL;        -- 读写并发：读不阻塞写
PRAGMA synchronous  = NORMAL;     -- WAL 下已足够；FULL 会让写入腰斩且无必要
PRAGMA wal_autocheckpoint = 1000; -- 约 4MB 触发，避免 WAL 无限增长
PRAGMA busy_timeout = 5000;
PRAGMA foreign_keys = ON;
PRAGMA temp_store   = MEMORY;
PRAGMA cache_size   = -16000;     -- 16MB 页缓存（负值 = KiB）
PRAGMA mmap_size    = 268435456;  -- 256MB，读路径受益
PRAGMA page_size    = 4096;
PRAGMA auto_vacuum  = INCREMENTAL;-- 删除日志后可增量回收（须在建库前设定）
```

**必须避免的反模式**

| 反模式 | 后果 | 对策 |
|:---|:---|:---|
| 每请求一个事务并 `fsync` | 吞吐崩塌 | 批量事务（§5.3） |
| `synchronous = FULL` | 写入性能腰斩 | 用 `NORMAL` |
| 事务中做网络 IO / 等待锁 | 长事务阻塞写者、WAL 膨胀 | 事务只包 DB 操作 |
| 热路径 `UPDATE` 造成页分裂 | 写放大 | 账户更新走批量 upsert；日志不做 UPDATE |
| 长期不 checkpoint | WAL 触顶导致写停顿 | 定时 `wal_checkpoint(TRUNCATE)`（§5.4） |
| 日志表建多个二级索引 | 每次插入都要维护索引 | 仅在 `day` 上建索引，或不建索引 |
| 对大 body 建全文索引 | 体积爆炸 | 只存摘要 + 指纹，不存原文 |
| 用 `DELETE` 清理大表 | 产生大量空闲页与碎片 | 用**分区表 + `DROP TABLE`**（§5.4） |

### 5.3 写入模型：单写者 actor + 批量事务

- **连接模型**：`1 个写连接 + N 个读连接`（WAL 允许"多读一写"）。写连接**独占**在一个 actor 任务里，其它模块通过 channel 提交写命令——与 §八 硬约束「单一写者」一致，也顺带消除旧版 `check_monthly_reset` 那类丢失更新。
- **批量提交策略**：写命令在 actor 内累积，满足任一条件即提交一个事务 —— ① 攒够 `batch_max_rows`（默认 256），或 ② 距上次提交超过 `batch_max_delay_ms`（默认 100）。
  - 效果：把 1.7k RPS 的**逐条写**降为 **≤ 20 次事务/秒**。
- **双通道优先级**：
  - **高优先通道**（立即提交，不攒批）：额度/积分扣减——请求路径上必须确认，攒批延迟会影响额度判定实时性。
  - **低优先通道**（攒批）：用量累加、成功率统计、`last_used_at` 等。
- **读路径**：读连接池并发读；`AppState` 维护 `ArcSwap<Catalog>` 热缓存，绝大多数请求**读内存、不读库**。

### 5.4 请求日志方案与自动清理

**JSONL 主通道（默认）**

- 日切：UTC 日变化时 `flush` 后 `rename` 为 `requests-YYYY-MM-DD.jsonl`；空文件不归档（沿用旧版正确逻辑）。
- 写入：`mpsc` 通道 + 攒 256 行 flush；1 秒 tick 兜底 flush。
- 清理：每日 03:00 UTC 调度，删除 `date < today - retention_days` 的归档文件（**按文件名比较，不读内容**，O(文件数)）。
- 查阅：`/admin/log/query` 支持 `from` / `to` / `model` / `akey_id` / `status` 过滤；跨日范围按文件顺序反向分块读取。

**SQLite sink（可选）**

- 表结构：按日分区表 `req_log_YYYYMMDD`（单表 + `day INTEGER` 亦可），只在 `day` 上建索引。
- 写入：走 §5.3 的**低优先通道**批量插入；**不在热路径建索引、不 UPDATE**。
- 清理：**`DROP TABLE req_log_20260901`** —— O(1)，不产生碎片，不需要 `VACUUM`。这是分区表相对 `DELETE` 的核心优势。
- WAL 治理：每小时或当 WAL 文件 > 64MB 时执行 `PRAGMA wal_checkpoint(TRUNCATE)`（由 `runtime/tasks` 调度，**不在请求路径**）。
- 空间回收：`auto_vacuum = INCREMENTAL` + 定期 `PRAGMA incremental_vacuum(1000)`，仅在 `DROP TABLE` 后触发。
- 水位告警：`data_dir` 磁盘使用率超过阈值（默认 85%）时发出事件与指标 `aequi_disk_usage_ratio`。

### 5.5 单向下行迁移（D5）

不做兼容层。迁移是**一次性离线工具**（独立 bin 或 `--migrate` 子命令）：

```
旧：sled 五树(u:{name} / billing / key_levels / key_usage / global_stats)
    + upstreams.json / models_routes.json / models_costs.json / requests*.jsonl
        │
        ▼  只读解析（不含任何兼容 trait 进入运行时）
     迁移器
        │
        ▼  单事务写入
新：SQLite（upstream / binding / model / group / membership / ukey / akey / account）
    + requests*.jsonl（原样保留，无需转换）
```

**映射规则**

| 旧 | 新 |
|:---|:---|
| `upstreams.json` 的 `id`（名称） | `UpstreamSpec { id: Uuid7(新生成), name: 原名称 }`，并输出 **名称 → Uuid 对照表**供人工核对 |
| `Upstream.models` + `model_map` | 展开为 `ModelBinding`（`display_model` ← `model_map` 反向查找，缺失则取上游模型名） |
| `min_key_level = M`（上游） | 该上游**全部绑定**归入组 `lvl_M..lvl_max`（等价性证明见 §6.3） |
| `key_levels` 的 `level = L` | 访问密钥成员关系 `{lvl_0..lvl_L}`；`level = -1` → `__admin__` |
| `billing` 余额 | 访问密钥 `credits_cap`（`-1` → `None` = 无限，**不再是哨兵值**） |
| `key_usage`（tokens, credits） | 访问密钥 `usage.output_tokens` + `credits_used`，其余三维置 0（**旧数据无缓存 / think 维度**） |
| `global_stats` | 丢弃（改为从账户聚合派生） |
| `models_costs.json` | `RateTable`（补全 `cache = 0.2`；`per_request` 保留） |
| 上游密钥（`u:{name}` 树） | `upstream_key` 表：`seq`（自增）+ `id = blake3(secret)[..16]` + `health` + `enabled = true` + **`limit_tokens = NULL`、`used_tokens = 0`**（旧库无此概念，迁移后由管理员按需设置） |

**安全要求**

1. **迁移前强制备份**：整目录 `cp -r` 或 `sqlite3 .backup`，校验可读后再继续。
2. **dry-run**：输出报告（上游 / 密钥 / 绑定 / 分组数量、名称 → Uuid 对照、余额与用量对账差异、无法映射项清单），**不写库**。
3. **对账门槛**：`credits_cap` 合计、`credits_used` 合计、密钥条数、绑定条数四项必须与旧库一致，否则中止。
4. **可回滚**：写入目标为**新目录 / 新文件**，旧数据保持原样；回滚 = 删除新库、恢复配置。
5. **存量日志脱敏**：旧 `requests*.jsonl` 含明文 `billing_key`；迁移工具提供一次性脱敏重写（替换为 `akey_id` 或哈希），默认开启。

### 5.6 列表分页与索引（与数据结构联动）

分页方案必须在**建表时**就确定，因为它决定主键与索引的形状——这是本节与 §4.2 数据结构设计的联动点。

**表结构要点**

```sql
-- 上游密钥：seq 作为分页排序键，id 作为业务身份
CREATE TABLE upstream_key (
    seq         INTEGER PRIMARY KEY AUTOINCREMENT,  -- ★ 分页游标 + 稳定排序
    id          TEXT    NOT NULL UNIQUE,            -- blake3(secret)[..16]
    upstream_id TEXT    NOT NULL,
    prefix      TEXT    NOT NULL,
    health      INTEGER NOT NULL DEFAULT 0,
    enabled     INTEGER NOT NULL DEFAULT 1,
    limit_tokens      INTEGER,                      -- NULL = 不限
    used_tokens       INTEGER NOT NULL DEFAULT 0,
    safety_margin_micro INTEGER NOT NULL DEFAULT 980000,
    secret_enc  BLOB    NOT NULL,
    created_at_ms INTEGER NOT NULL,
    meta_json   TEXT
);

-- 覆盖索引：列表按 (upstream_id, health?) 过滤、按 seq 排序
CREATE INDEX idx_ukey_upstream_seq ON upstream_key (upstream_id, seq);
CREATE INDEX idx_ukey_health_seq   ON upstream_key (upstream_id, health, seq);

-- 访问密钥：同构
CREATE TABLE access_key (
    seq         INTEGER PRIMARY KEY AUTOINCREMENT,
    id          TEXT    NOT NULL UNIQUE,            -- Uuid
    hash        BLOB    NOT NULL UNIQUE,            -- blake3(secret)
    prefix      TEXT    NOT NULL,
    credits_cap_micro  INTEGER,                     -- NULL = 无限
    credits_used_micro INTEGER NOT NULL DEFAULT 0,
    used_input INTEGER NOT NULL DEFAULT 0,
    used_output INTEGER NOT NULL DEFAULT 0,
    used_think  INTEGER NOT NULL DEFAULT 0,
    used_cache  INTEGER NOT NULL DEFAULT 0,
    lifetime_micro INTEGER NOT NULL DEFAULT 0,
    enabled     INTEGER NOT NULL DEFAULT 1,
    created_at_ms INTEGER NOT NULL
);
CREATE INDEX idx_akey_seq ON access_key (seq);

-- 分组成员关系：按组反查密钥也要能分页
CREATE TABLE access_key_group (
    key_seq  INTEGER NOT NULL,
    group_id TEXT    NOT NULL,
    PRIMARY KEY (key_seq, group_id)
);
CREATE INDEX idx_akg_group_seq ON access_key_group (group_id, key_seq);
```

**查询形态（恒为 O(limit)）**

```sql
-- 第 1 页
SELECT seq, id, prefix, health, enabled, limit_tokens, used_tokens
  FROM upstream_key
 WHERE upstream_id = ?1
 ORDER BY seq
 LIMIT ?2;
-- 后续页
SELECT ... WHERE upstream_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3;
```

**约束**

| 约束 | 理由 |
|:---|:---|
| 禁止 `OFFSET` 深分页 | `OFFSET n` 仍需扫描并丢弃前 n 行 |
| 禁止在 handler 里 `load_all()` 再 `skip/take` | 即旧版 `admin/keys.rs:338-348` 的写法；内存与耗时随总量线性增长 |
| 排序键必须是 `seq`，不能是随机 `id`/`Uuid` | 随机键无稳定序，游标会漂移；`seq` 单调递增且不回退 |
| 每类列表都要有对应覆盖索引 | 无索引时 `ORDER BY seq LIMIT n` 退化为全表扫描 |
| 内存槽位与 DB 行同源 | 路由用内存全集（SWRR 需要全集评分），列表用 DB 分页；两者由单一写者保持一致，**不互为真源** |
| 删除用软删或 `seq` 空洞 | 避免 `AUTOINCREMENT` 复用导致游标跨页重复/漏项 |

> 注意区分：**内存里持有全部密钥槽位是设计需要**（选路要在全集上按权重评分），这不是问题；**问题只在于把"给管理端列表"也做成了全量加载**。两者目的不同，实现途径也不同。

---

## 六、能力升级规划

| # | 能力 | 核心实体 | 关键机制 | 章节 |
|:---|:---|:---|:---|:---|
| ④ | 密钥验证 | `ProbePolicy` + `UpstreamKey.meta` | 五态判定 + 统一探针 + 预算约束 + 指数复核 | §6.1 |
| ⑤ | 429 退避 | `BackoffPolicy` + `BreakerState` | 指数 + 全抖动 + 上游级半开熔断 + 冷却最小堆 | §6.2 |
| ⑥ | 分组制 | `Group` + `GroupPolicy` + `ModelBinding` | 分组只决定可访问的**模型绑定集合** | §6.3 |
| ⑦ | 轮询公平性 | `SwrrSelector` | 两级 SWRR + LRS，无活跃集重建 | §6.4 |
| ⑧ | 两层额度 | `UpstreamKey.quota`（上游侧） / `AccessKey`（下游侧） | 上游：token 上限 → 自动摘出轮转池（对客户端透明）；下游：积分上限 → 拒绝请求 + 手动分发 | §6.5 |
| ⑨ | 列表分页 | `seq` 游标 + 覆盖索引 | 数据库 keyset 分页，禁止全量加载后截断 | §4.5 / §5.6 |

### 6.1 密钥验证（探针，D10）

#### 6.1.0 现状缺陷（已核对代码）

- 探针唯一且写死 `/v1/models`（`upstream.rs:223-231`）。
- **判定过宽（真实风险）**：`status != 401 && status != 403` 即视为有效（`upstream.rs:250`）→ **404 / 500 / 502 会把失效密钥"恢复"为可用**，造成错路由与资损。
- 调度粗暴：先 `sleep(10s)`，再**逐 key 串行**全量探测（`upstream.rs:139-196`），单 key 超时最长 `revalidation_timeout_secs`；密钥量大时一轮耗时线性增长。
- 触发单一：不支持导入即验、指定验证；`api_test_key` 只验一条且不改状态。
- 结论不落库：只改内存 `status`，重启即丢，无 `last_valid_ms` 可审计。

#### 6.1.1 状态与判定（五态）

```rust
pub enum ProbeOutcome { Unverified, Valid, Invalid, RateLimited, Inconclusive }
```

| 探测结果 | 判定 | 对状态机的作用 |
|:---|:---|:---|
| 命中 `valid`（默认 200） | `Valid` | **仅此时**允许恢复为 `Active`；`fail_streak = 0`；更新 `last_valid_ms` |
| 401 / 403 | `Invalid` | 维持 / 置为 `SuspendedAuth`；`fail_streak += 1` |
| 429 | `RateLimited` | **不改变失效状态、也不恢复**；交由 §6.2 退避 |
| 其它 4xx（含 404）、5xx、超时、连接错误 | `Inconclusive` | **不改变状态**；`fail_streak += 1`；`last_error` 记录原因 |
| 从未探测 | `Unverified` | 初始态；是否可参与选路由 `probe.unverified_usable`（默认 `true`）决定 |

> **★ 核心规则**：只有 `Valid` 能恢复，只有 `Invalid` 能降级。`Inconclusive` 连续超过 `inconclusive_limit`（默认 5）时**只做标记**（`health = Unknown`，管理端可见 + 告警），**不自动降级**——避免把"网络抖动 / 上游维护"误判为"密钥失效"。这一条同时修掉"404/500 恢复死密钥"的漏洞。

**上游密钥健康状态（全项目唯一定义，`health` 只在此处枚举）**

| 状态 | 含义 | 进入方式 | 退出方式 | 参与选路 |
|:---|:---|:---|:---|:---|
| `Active` | 可用 | 初始 / 恢复 | — | ✅ |
| `Cooling` | 退避冷却中（429 或 5xx） | §6.2 退避 | 冷却到期（自动） | ❌ |
| `SuspendedAuth` | 凭证失效 | 401/403 达阈值 | **仅探针返回 200** | ❌ |
| `QuotaExhausted` | 额度耗尽（§6.5.1） | 计数触顶 或 上游明确报配额耗尽 | **仅管理员重置/加额**（探针无效） | ❌ |
| `Unknown` | 连续探测无结论（标记态，非故障态） | `Inconclusive` 连续超限 | 下次探测得出结论 | 按配置 |

**正交的人工意图位（不放进 `health`，避免两个真源互相覆盖）**

| 字段 | 含义 | 语义 |
|:---|:---|:---|
| `enabled: bool` | 人工暂停 / 恢复 | `false` = 管理员停用（对外可表现为 `Paused`）。**与 `health` 是"与"关系**：选路要求 `enabled && health == Active` |

> 为什么不把 `Paused` 做成 `health` 的一个值：一个密钥可能**同时**"被人工停用"且"凭证失效"。若共用一枚字段，恢复其一就会错误地覆盖另一个。用 `enabled` 与 `health` 两个正交维度即无此问题——这也与访问密钥的 `enabled` 保持一致。

> 这张表的价值：旧版只有一个 `status: u8`（0=active / 1=invalid），**无法区分"密钥坏了""额度用完了""被人工停了"**——三者的恢复方式完全不同，混在一起必然导致"探针把额度耗尽的密钥又拉回轮转池"这类错误。

#### 6.1.2 探测请求构造

默认按上游 format 选择探测端点；**可由上游或绑定级 `probe` 覆盖**：

| format | 默认探测 | 凭证注入 |
|:---|:---|:---|
| openai | `GET {base}/v1/models` | `Authorization: Bearer <key>` |
| anthropic | `GET {base}/v1/models` | `x-api-key: <key>` + `anthropic-version: 2023-06-01` |
| gemini | `GET {base}/v1beta/models?key=<key>` | 走 query |

```toml
[upstream.probe]
path       = "/v1/models"    # 覆盖默认
method     = "GET"
timeout_ms = 5000
classify   = { valid = [200], invalid = [401, 403], rate_limited = [429] }

# 可选：对话式探针（部分网关不暴露 /v1/models，或需真实调用才判定）
chat_probe = { enabled = false, model = "<最低成本模型>", max_tokens = 1 }
```

**要点**

- **对话式探针默认关闭**：它会产生真实调用与费用，仅在 `/v1/models` 不可用且用户显式开启时使用，且必须显式指定 `model`。
- 探测请求**不进请求日志、不计积分、不占并发额度**，但**必须发 telemetry 事件**（否则等于制造新的静默路径）。
- 探测请求体最多读 4KB 后即断开（不下载完整模型列表），避免大 payload。

#### 6.1.3 调度、并发与预算

| 维度 | 配置 | 默认 | 说明 |
|:---|:---|:---|:---|
| 全局并发 | `probe.concurrency` | 4 | 全局 `Semaphore` |
| 单上游并发 | `probe.per_upstream` | 2 | 避免单上游被探测打满 |
| 每分钟预算 | `probe.budget_per_min` | 10 | 每上游令牌桶；耗尽则推迟并计 `probe_budget_exhausted_total` |
| 首次启动延迟 | `probe.startup_delay_ms` | 10_000 | 让系统先稳定 |
| 打散 | — | — | 新导入密钥的 `next_probe_at = now + rand(0..30s)`，避免同时爆发 |

**入队规则**：`next_probe_at <= now` 且 `in_queue == false`（用 `AtomicBool` 去重，避免同一密钥被重复入队）。队列按 `next_probe_at` 升序的最小堆取，**不是**遍历全部密钥。

**触发路径三合一**（同一 `KeyProbeService`，避免逻辑分叉）

1. **导入即验**：`/admin/ukey/import` 支持 `verify: true`（异步入队，返回 `queued` 计数）。
2. **手动验证**：`/admin/ukey/validate`，body 传 `{ upstream_id, ids: [...] | all: true }`；异步返回 `job_id`，结果经 `/admin/stats/stream` 推送。
3. **后台复核**：对 `SuspendedAuth` / `Unknown` / `Unverified` 的密钥按 §6.1.4 的间隔排队（`QuotaExhausted` 与 `enabled = false` **不参与**，见 §6.5.1）。

#### 6.1.4 复核退避（避免对不可达上游高频探测）

```
next_probe_at = now + clamp(base_recheck × 2^(fail_streak-1), 60s, 3600s) × jitter(0.5, 1.5)
```

- `base_recheck = 60s`，上限 `3600s`（配置项）。
- `Valid` → `fail_streak = 0` 且 `next_probe_at = None`（**不再周期性探测**——密钥有效就无需反复打扰上游）。
- `Inconclusive` **同样累计** `fail_streak`（但状态不变），使不可达上游被自动降频。
- 抖动保证多密钥不会同时到期。

#### 6.1.5 并发安全：探测结果用 CAS 写回

探测是异步的，期间密钥状态可能被人工改动。写回必须乐观并发控制：

```
if key.health.compare_exchange(expected_health, new_health).is_ok() {
    apply_meta();               // 落库 KeyMeta
} else {
    counter!(probe_superseded_total);   // 丢弃，不覆盖人工结论
}
```

#### 6.1.6 API 与埋点

| 类型 | 名称 |
|:---|:---|
| API | `/admin/ukey/validate`（批量 / 单个）、`/admin/ukey/list`（含 `health` / `last_valid_ms` / `fail_streak`） |
| 事件 | `KeyProbeCompleted { key_id, upstream_id, outcome, latency_ms, source: Import\|Manual\|Sweeper }` |
| 指标 | `aequi_key_probe_total{outcome}`、`aequi_key_probe_duration_seconds`、`aequi_key_probe_queue_depth`、`aequi_key_probe_budget_exhausted_total`、`aequi_key_probe_superseded_total` |

#### 6.1.7 与选路的关系

- `health = Active` 才参与选路（`Unknown` 可配为可参与，以便网络恢复后自动接单）。
- `QuotaExhausted` 与 `enabled = false`（人工暂停）**都不参与探针，也不被探针清除**：额度耗尽与人工停用都与密钥有效性无关，探针返回 200 不足以恢复它们（§6.5.1）。旧版把状态混在一个 `status: u8` 里，无法区分"密钥坏了""额度用完了""被人工停了"三种情况。
- 探针**不修改 `cooldown_until_ms`**（退避由 §6.2 独立管理，避免两套机制互相覆盖）。

### 6.2 429 退避与熔断（D12）

#### 6.2.0 现状缺陷（已核对代码）

- **恒定冷却**：`on_upstream_status`（`upstream.rs:63-78`）取 `Retry-After`，否则用 `rate_limit_cooldown_ms`（默认 3000ms），上限 `max_rate_limit_cooldown_ms`（30s）。连续 429 时冷却恒为 3s——**无指数、无抖动**。
- **同步惊群**：`next_cooldown_delay`（`forward.rs:597-616`）返回所有密钥中**最早的到期时刻**，排队请求在同一毫秒苏醒。
- **无上游级退避**：某上游整体 429/5xx 时，逐 key 轮转仍会把**每一个密钥都撞一遍**才慢下来。
- **5xx 不冷却**：仅 401/403/429 有副作用，5xx 与网络错误不进入任何冷却，故障上游被持续打满。
- **全量扫描**：每次排队等待都遍历"所有上游 × 所有活跃密钥"求最早到期（O(U×K)）。

#### 6.2.1 退避公式与抖动

```
dur_raw   = base_ms × factor^(streak-1)
dur_floor = max(dur_raw, retry_after_ms)          # Retry-After 作为下界
dur_cap   = clamp(dur_floor, 0, max_ms)           # 但受 max_ms 截断
dur       = jitter(dur_cap)
```

| 抖动模式 | 公式 | 适用 |
|:---|:---|:---|
| `full`（默认） | `dur' = uniform(0, dur_cap)` | 标准选择：消除同步、期望等待最短 |
| `decorrelated` | `dur' = min(max_ms, uniform(base_ms, prev_dur × 3))` | 上游对突发敏感时更平滑 |
| `none` | `dur' = dur_cap` | 仅用于测试复现 |

> 上图为默认参数下的曲线（base 1s / factor 2 / max 60s），柱形表示 `full` 抖动的实际取值区间——**同一时刻不同密钥的冷却时长不同**，这正是消除惊群的关键。

#### 6.2.2 `Retry-After` 解析

- 支持两种 RFC 7231 形式：`delta-seconds`（整数）与 `HTTP-date`。
- 非法值、已过去的日期 → `None`（不使用）。
- `Retry-After` 只作**下界**，且受 `retry_after_cap_ms`（默认 300s）二次约束——防上游返回"1 天后重试"把密钥锁死。
- 服务端若同时给出 `Retry-After` 与 `X-RateLimit-Reset`，取更早者（更保守）。

#### 6.2.3 双计数与状态

```rust
pub struct KeyBackoff {
    rl_streak: u32,          // 429 连续次数
    err_streak: u32,         // 5xx / 连接错误 / 超时 连续次数
}
```

- 两条独立计数各自算 `dur`，**取较大者**作为最终冷却。
- `on_status` 配置决定哪些状态触发冷却（默认 `[429, 500, 502, 503, 504]`，另含连接错误与超时）。
- **任一 2xx 清零两者**（`success_reset = true`）。
- 计数与冷却**不持久化**：重启即清空。理由——429 与 5xx 是瞬时状态，重启后上游可能已恢复；持久化反而会带来"陈旧冷却"。**持久化的只有 `SuspendedAuth`（凭证失效）、`QuotaExhausted`（额度耗尽）与 `enabled`（人工意图）**，因为它们需要人工或探针干预，且不能因重启而"复活"。

#### 6.2.4 上游级半开熔断（状态机）

```
                 ┌──────────────────────────────────────────┐
                 │                                          │
                 ▼                                          │
   ┌─────────┐  window 内不同密钥失败数 ≥ threshold  ┌──────────┐
   │ Closed  │ ─────────────────────────────────────▶ │  Open    │
   │(正常放行)│                                        │(整体跳过) │
   └─────────┘ ◀──────────── 探测请求成功 ─────────   └──────────┘
        ▲                                     │             │
        │                                     │      open_ms 到期
        │                              ┌──────┴───────┐     │
        └──────────────────────────────│  HalfOpen    │◀────┘
                                       │(只放 1 个请求)│
                                       └──────┬───────┘
                                              │ 失败 → Open，open_ms × 2（有上限）
```

| 参数 | 默认 | 说明 |
|:---|:---|:---|
| `breaker.enabled` | `true` | 可整体关闭 |
| `breaker.error_threshold` | 3 | 窗口内**不同密钥**的失败数（用去重集合，防止一个坏密钥触发熔断） |
| `breaker.window_ms` | 10_000 | 滑动窗口 |
| `breaker.open_ms` | 10_000 | 初始打开时长 |
| `breaker.max_open_ms` | 120_000 | 连续失败时的退避上限 |
| `breaker.half_open_probes` | 1 | 半开期放行的探测请求数 |

**要点**

- 熔断作用于**上游**而非密钥：避免"上游整体故障时把每个密钥依次撞穿"。
- 熔断打开期间，选路**整体跳过该上游**；若该模型的**所有**上游都被跳过 → 直接返回 429/503 并附 `Retry-After = 最早恢复时刻`，**不进入热循环重试**。
- 半开状态只放行 `half_open_probes` 个请求，成功即闭合（清零失败集合），失败则续开并翻倍 `open_ms`。

#### 6.2.5 冷却最小堆（消除全量扫描与惊群）

- 维护 `MinHeap<(expire_at_ms, KeySlotIdx)>` + **惰性删除**：pop 到堆顶时，与密钥当前 `cooldown_until_ms` 比对，不一致即丢弃（陈旧条目）。
- 状态变更时**只 push，不做堆内更新**——保持 O(log n)。
- 排队唤醒：取堆顶有效期，`sleep_until(top + jitter(±5%))`；同时 `select!` 监听 `notify_capacity()`（容量释放即时唤醒）。
- 若堆顶距今超过 `queue_max_wait`，退化为仅监听 `notify`（避免长时间挂起的无意义定时器）。
- 复杂度：**O(log n) / 次**，取代旧版 O(U×K) / 次。

#### 6.2.6 与重试循环、队列的交互

| 场景 | 行为 |
|:---|:---|
| 收到 429 且可重试 | 当前密钥置冷却 → 按绑定重新解析（换上游 / 换密钥）→ 重试；`retry_count += 1` |
| 所有候选都在冷却 | 进入队列等待（若 `queue_enabled`），否则返回 429 + `Retry-After = 最早恢复时刻` |
| 熔断打开且是唯一上游 | 返回 503 `upstream_circuit_open` + `Retry-After`，不计入密钥失败 |
| 重试次数超限 | 返回 502 `max_retries_exceeded`，并在日志记录每一跳（`retry_count` 真实值，取代旧版恒 0 的 `attempts`） |

#### 6.2.7 边界情况清单（必须写进测试）

| 情况 | 期望行为 |
|:---|:---|
| `Retry-After` 在过去 / 非数字 / 超大 | 忽略或截断到 `retry_after_cap_ms`，不报错 |
| 429 后紧接着 200 | `rl_streak` 清零，冷却**立即释放**（不等待到期，因为已确认恢复） |
| 单密钥上游被 429 | 直接 429 + `Retry-After`，不空转 |
| 5xx 与 429 交替 | 取两条计数各自的 `dur` 的较大者，不互相清零 |
| 熔断打开期间恢复 | 半开探测成功后立即闭合，失败才续开 |
| 时钟跳变 | 冷却用**单调时钟**计算时长（`Instant` 偏移），墙钟仅用于展示与持久化字段 |
| 进程重启 | 冷却全清（有意为之）；`SuspendedAuth` / `QuotaExhausted` / `enabled` 保留 |

#### 6.2.8 配置与埋点

```toml
[key.backoff]
base_ms = 1000
factor  = 2.0
max_ms  = 60000
jitter  = "full"
respect_retry_after = true
retry_after_cap_ms  = 300000
on_status = [429, 500, 502, 503, 504]
success_reset = true

[key.backoff.breaker]
enabled = true
error_threshold = 3
window_ms = 10000
open_ms = 10000
max_open_ms = 120000
```

| 类型 | 名称 |
|:---|:---|
| 事件 | `KeyCooldownSet { key_id, reason, dur_ms, streak, jittered }`、`BreakerStateChanged { upstream_id, from, to, errors_in_window }` |
| 指标 | `aequi_key_cooldown_seconds`（直方图）、`aequi_upstream_rate_limited_total{upstream}`、`aequi_breaker_open_total{upstream}`、`aequi_breaker_state{upstream}`（0/1/2 gauge）、`aequi_retry_total{reason}` |

### 6.3 分组制与模型绑定授权（D8）

#### 6.3.0 现状缺陷

- 授权信息拆两半：上游持 `min_key_level: i32`（`config.rs:206`），密钥侧在 `key_levels` 树存 `i32`（`storage.rs:78-86`）。
- **标量无法表达多归属**：一个密钥不能同时属于"内部"和"高级"两个互不包含的组。
- `-1` 哨兵（管理员）与 `0` 默认值散落三处：`select_for_model`（`state/mod.rs:581`）、`usage.rs:50`、admin 校验（`upstreams.rs:152-156`）。
- 等级强制**全序**，而需求是**集合**。
- 无"组"实体：没有组名、说明、默认额度；运营无法列举"系统里有哪些组"。

#### 6.3.1 实体定义

```rust
pub struct GroupId(Arc<str>);                  // "default" / "pro" / "internal" / "__admin__"

pub struct Group {
    pub id: GroupId,
    pub name: String,
    pub description: Option<String>,
    pub bindings: Policy<BindingId>,           // ★ 只决定可访问哪些「模型绑定」
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

pub enum Policy<T> { All, Only(BTreeSet<T>), Except(BTreeSet<T>) }

pub struct AccessKeyGroups {
    pub allow: BTreeSet<GroupId>,
    pub deny: BTreeSet<GroupId>,               // deny 优先
}
```

**D8 约束的落实**

- **分组不携带倍率**：倍率统一在 `RateTable` 按模型配置（§6.5），分组只回答"能访问哪些模型"。
- **模型绑定必须带上游 id**：见 §4.2.1。分组的授权对象是 `BindingId`（= display_model × upstream 的唯一组合），因此**不会出现"同 id 不同上游被错误允许"**。
- 保留组 `__admin__` 取代 `-1` 哨兵；`default` 组在初始化时自动创建，新访问密钥默认属于 `{default}`。

#### 6.3.2 授权判定：全项目唯一函数

```
allowed(binding, akey) -> bool:
    if akey.groups.allow.contains("__admin__"):        return true
    if group_policy_denies(binding, akey.groups.deny): return false
    return group_policy_allows(binding, akey.groups.allow)

group_policy_allows(binding, allow_set) -> bool:
    return ∃ g ∈ allow_set : g.bindings.permits(binding)

Policy::permits(b, x):
    All        => true
    Only(set)  => set.contains(x)
    Except(set)=> !set.contains(x)
```

- **`deny` 优先于 `allow`**。
- 该函数被 `proxy` 的绑定过滤、`/v1/models` 列举、admin 预览**共用**——消除旧版三处重复判断。
- **硬约束**：全项目只允许存在这一处"该密钥是否可使用该模型"的判断（§八 第 6 条）。

#### 6.3.3 请求路径上的判定顺序（固定）

```
1. 认证访问密钥            → 无效则 401 access_key_invalid
2. 额度检查（§6.5）        → 不足则 429 quota_exceeded
3. 解析 model → bindings[] → 无绑定则 404 model_not_found
4. 按 akey 的分组过滤绑定   → 空则 403 model_not_allowed_for_group
5. 在允许的绑定（= 上游集合）上做 SWRR 选路（§6.4）
6. 命中绑定 → 用 binding.upstream_model 改写请求体模型名
7. 在目标上游内按 SWRR + LRS 选密钥
8. 预留额度 → 转发 → 结算
```

第 3–4 步在**选路之前**，保证"不被允许的上游永远不会被选中"，而不是"选中后再校验"。

#### 6.3.4 等级 → 分组的等价迁移（可验证）

设 `maxLevel = max(所有上游 min_key_level, 所有密钥 level)`：

- 生成兼容组 `lvl_0 … lvl_maxLevel`；
- 密钥 `level = L` ⇒ `allow = {lvl_0 … lvl_L}`；`level = -1` ⇒ `__admin__`；
- 上游 `min_key_level = M` ⇒ 该上游**全部绑定**归入 `Only({lvl_M … lvl_maxLevel})`。

**等价性**：`L ≥ M` ⇔ `{0..L} ∩ {M..max} ≠ ∅`。逐条可单测（含 `L = M`、`L = -1`、越界）。
迁移后 `lvl_*` 组作为"兼容组"保留一个版本，运维可在管理端重命名或合并为业务组——**无需再次迁移数据结构**（组已是原生实体）。

#### 6.3.5 API 与埋点

| 类型 | 名称 |
|:---|:---|
| API | `/admin/group/{list,get,create,update,delete}`、`/admin/akey/groups`（整体替换，幂等） |
| 事件 | `AccessDenied { akey_id, model, reason: GroupDenied \| ModelNotFound }` |
| 指标 | `aequi_requests_total{group}`、`aequi_credits_used_total{group}`、`aequi_access_denied_total{reason}` |

> 所有请求事件与指标**一次性**带上 `group` 维度，避免日后二次加标签。

### 6.4 轮询公平性（D11）

#### 6.4.0 "前面密钥被反复使用"的真实成因

`select_key`（`upstream.rs:289-320`）用**全局原子游标 + 位置扫描**：

```
start = key_rr.fetch_add(1);
for i in 0..n { idx = (start + i) % n; ... return first eligible }
```

1. **位置偏置**：`active_keys` 顺序 = 密钥**插入顺序**；新增密钥永远排在尾部，而游标在"第一个可用"处就返回 → 前段密钥命中率系统性偏高。这正是你观察到的现象。
2. **重建错位**：任何一次 `rebuild_active_keys()`（增删一个密钥、恢复/失效）都生成**新数组**，而游标值不变 → 与位置语义脱钩，分布不可预测。
3. **无使用度量**：选路完全不看"谁被用得少"——没有 per-key 请求计数、没有最近使用时间，无法自我纠偏。
4. **重试排除是全局副作用**：`exclude_key` 通过"跳过"实现，重试风暴下游标继续推进，进一步放大偏置。

#### 6.4.1 数据结构（稳定槽位，原子字段）

```rust
struct UpstreamKeySlot {
    // 静态
    id: UpstreamKeyId,
    secret: Secret<Arc<str>>,
    weight: u32,                  // 默认 1
    max_inflight: u32,            // 0 = 不限
    // 选路状态（热路径只读，无锁）
    health: AtomicU8,             // Active | Cooling | SuspendedAuth | QuotaExhausted | Unknown
    enabled: AtomicBool,          // 人工暂停/恢复意图（与 health 正交，避免互相覆盖）
    quota: UpstreamKeyQuota,      // 额度（§6.5.1），可选层
    inflight: AtomicU32,
    cooldown_until_ms: AtomicU64,
    last_selected_seq: AtomicU64, // ★ LRS 主键
    served_total: AtomicU64,      // 公平性审计 / 指标
    // 退避（§6.2）
    rl_streak: AtomicU32,
    err_streak: AtomicU32,
    // 探针元数据（§6.1）
    meta: Mutex<KeyMeta>,
}
```

**关键**：槽位地址稳定（`ArcSwap<Box<[UpstreamKeySlot]>>` 只在**增删**时更换），状态变更**不再触发数组重建**。

#### 6.4.2 两级选择算法

**第一级：上游级**（在"分组过滤后的绑定"所指向的上游集合上做 SWRR）
**第二级：密钥级**（在目标上游内做 SWRR + LRS）

```rust
// 每级同构：先过滤，再 SWRR，平票用 LRS
fn pick(cands: &[&Slot], seq: &AtomicU64, state: &mut SelectorState) -> Option<usize> {
    let elig: SmallVec<[usize; 8]> = cands.iter().enumerate()
        .filter(|(_, s)| s.enabled.load(Relaxed)                 // 人工意图（暂停）
                      && s.health() == Active                    // 自动健康（冷却/失效/额度）
                      && now_ms() >= s.cooldown_until_ms.load(Relaxed)
                      && (s.max_inflight == 0 || s.inflight.load(Relaxed) < s.max_inflight)
                      && !excluded.contains(&s.id))               // 请求局部排除（重试）
        .map(|(i, _)| i).collect();

    if elig.is_empty() { return None; }

    if elig.iter().any(|&i| cands[i].weight > 1) {
        // ① SWRR：平滑加权轮询（权重）
        let mut total = 0i64;
        for &i in &elig { state.cw[i] += cands[i].weight as i64; total += cands[i].weight as i64; }
        let best = *elig.iter().max_by_key(|&&i| {
            (state.cw[i],                       // 权重优先
             std::cmp::Reverse(cands[i].last_selected_seq.load(Relaxed)), // ② 更久未用者优先（LRS）
             std::cmp::Reverse(cands[i].served_total.load(Relaxed)),
             std::cmp::Reverse(i as u64))       // ③ 确定性兜底
        }).unwrap();
        state.cw[best] -= total;
        Some(best)
    } else {
        // 纯 LRS：等价于严格轮转，且对增删自愈
        elig.iter().min_by_key(|&&i| {
            (cands[i].last_selected_seq.load(Relaxed),
             cands[i].served_total.load(Relaxed),
             i as u64)
        }).copied()
    }
}

// 选中后
slot.inflight.fetch_add(1, Relaxed);
slot.served_total.fetch_add(1, Relaxed);
slot.last_selected_seq.store(seq.fetch_add(1, Relaxed), Relaxed);
```

**为什么是 SWRR + LRS 而不是修游标**

| 机制 | 保证 |
|:---|:---|
| SWRR | 长期按权重**比例**分配，且是"平滑"的（不会出现连续同一密钥） |
| LRS | 任意时刻"被冷落最久"的密钥**优先补位**；新增密钥 `seq = 0` ⇒ **一入池即被优先选中** |

两者叠加即满足"前段密钥不再被反复使用"，且**选路完全不看数组位置**——插入顺序不再影响命中率。

#### 6.4.3 复杂度与并发模型

- **复杂度**：O(K) 扫描，K = **该上游的密钥数**（不是全局密钥数），通常 ≤ 100 → 全为原子 load，亚微秒级；**无分配、无数组重建**。
- **并发模型（刻意选择）**：`current_weight` 的读-改-写需要原子性，否则并发选择会丢更新、造成分布偏移。方案：
  - **每个上游一把短锁**（`parking_lot::Mutex<SelectorState>`），只保护 `cw` 数组与计数，**持锁时间纳秒级、不做任何 IO**。
  - 理由：选路发生在**每请求一次**，而请求本身是 IO 密集（数十毫秒）；一把无争用短锁的成本（约 20ns）完全可以忽略，却换来**算法精确正确 + 可确定性测试**。这是对"极致无锁"的**有意放弃**，写在文档里以免后人误优化。
  - 若未来单上游密钥数超过 1000，再考虑双堆结构——**现在不做**（拒绝过度设计）。
- 热路径只读原子（`health` / `inflight` / `cooldown_until_ms`），不触碰 `meta` 的 `Mutex`。

#### 6.4.4 反饥饿保证与测试口径

- **纯 LRS 分支**：等价于在"当前合格集合"上做严格轮转 → 任一持续合格的密钥，最坏间隔 = `K - 1` 次选择。
- **SWRR 分支**：最坏间隔 ≤ `ceil(Σweight / weight_i)`。
- **公平性测试**：
  1. 均匀权重、无冷却、无并发上限 → 10k 次选择，断言 `max/min served ∈ [1, 1.02]`；
  2. 随机注入冷却与在途占用 → 断言"任一持续合格密钥的间隔 ≤ 理论上界"；
  3. 中途新增密钥 → 断言新密钥**被立即选中且其后分布收敛**；
  4. 权重 1:2:3 → 断言实际比例与权重一致（误差 < 2%）。
- 运行时以 `aequi_key_served_skew`（当前 `max/min served` 比值）作为公平性健康度指标。

#### 6.4.5 增删密钥的行为

| 操作 | 行为 |
|:---|:---|
| 新增 | 追加到数组尾部（`ArcSwap` 换新数组，属低频管理操作）；`last_selected_seq = 0` ⇒ 立即可被选中 |
| 删除 | 从数组移除并换新数组；同时清理其 `cooldown` 堆条目（惰性失效即可） |
| 状态变更（冷却/失效/恢复） | **不动数组**，只改原子字段 ⇒ 不存在 O(n) 重建，也不存在旧版"重建后游标错位" |
| 权重变更 | 只改 `weight` 字段 |

#### 6.4.6 与重试、冷却、在途的交互

- **重试排除**：`excluded` 是**请求局部**的 `SmallVec<[UpstreamKeyId; 2]>`，只作为过滤条件，**不推进任何全局游标、不影响 `last_selected_seq`** → 重试不再扭曲分布。
- **在途上限**：`inflight` 在 `KeyGuard` 的 `Drop` 中递减（沿用旧版 RAII 思路），并触发 `notify_capacity()`。
- **冷却**：只影响过滤，不影响评分。

#### 6.4.7 埋点

| 类型 | 名称 |
|:---|:---|
| 事件 | `UpstreamSelected { request_id, model, binding_id, upstream_id, key_id, selection_reason, key_inflight }` |
| 指标 | `aequi_key_selected_total{key_id}`、`aequi_key_served_skew{upstream}`、`aequi_upstream_selected_total{upstream}`、`aequi_selector_lock_wait_seconds`（确认短锁无退化） |

### 6.5 额度模型（两层：上游密钥 / 访问密钥）

> **先澄清一处语义**：本项目中"额度"有两个**互不相同**的层，不可混用命名。
> - **上游密钥额度**（本节 6.5.1）：对接第三方平台的密钥配额（例："某平台给的密钥共 1000 万 token"）。触顶后该密钥**自动移出轮转池**，对客户端透明。
> - **访问密钥积分**（本节 6.5.4）：发给朋友的 apikey 的积分上限。触顶后**拒绝该客户端的请求**，需管理员手动加额。
>
> 两者是独立架构：前者管"我能向上游花多少"，后者管"朋友能用我多少"。

#### 6.5.1 上游密钥额度与自动停用（D13）

| | A. 显式额度计数（主动） | B. 反应式（上游报错 + 指数退避） |
|:---|:---|:---|
| 触发 | `used_tokens ≥ limit × safety_margin` | 上游返回错误 |
| **能否真正"停"** | 能。`QuotaExhausted` **不随时间恢复** | **不能**。退避有上限，到期必然重试并继续报错 |
| 停止时机 | 额度耗尽**之前**（提前 `safety_margin`） | 耗尽**之后**，且已白费若干次请求 |
| 依赖上游行为 | 不依赖 | 依赖上游给出**可区分**的错误 |
| 可观测性 | 可显示"剩余额度" | 只有错误计数 |
| 新增成本 | 一个 `u64` 字段 + 一次自增（发生在**本来就在写**的账户行上） | 近乎零 |

**★ 权衡结论：采用 A 的极简版本，并把 B 作为快速确认路径。** 理由：

1. **B 无法满足"自动停用"这一硬性要求**。指数退避只是拉长间隔；要变成"停"就必须引入"连续 N 次某类失败 → 永久停"的规则，那本质上是 A 的变体，只是触发信号更不可靠。
2. **429 / 401 的语义不可分是硬伤**：429 同时表示"限流"与"配额耗尽"，401/403 同时表示"密钥失效"与"配额耗尽"。仅凭错误码无法区分，会导致两种事故——把**限流**误判成永久停用（可用性下降），或把**配额耗尽**误判成限流（持续浪费请求）。
3. **边际成本极低**：本服务本来就逐请求统计四维用量，"这条密钥用了多少 token"是**已有数据**，只是多一个整数字段与一次自增——**不是新增一条计费链路**。
4. **提前停用**（`safety_margin = 0.98`）避免"正好撞墙时正在服务"，同时吸收"上游未返回用量、由估算得出"的误差。

**明确不做（这就是"过度设计"的边界）**

| 不做 | 原因 |
|:---|:---|
| ❌ 周期 / 滚动窗口额度 | D9 已否决周期概念，上游密钥侧同样不需要 |
| ❌ 多维额度 DSL（token + credits + requests 任意组合） | 只做 `limit_tokens` 单维；需要时再加 `limit_requests`，不做组合表达式 |
| ❌ 每模型 / 每分组额度 | 平台配额是**密钥级**的，按密钥设即可 |
| ❌ 主动调用上游余额查询 API | 各平台接口不一，维护面大、易失效 |
| ❌ 自动换密钥 / 自动采购 | 超出个人使用范围 |

**实现（极简）**

```rust
pub struct UpstreamKeyQuota {
    pub limit_tokens: Option<u64>,   // None = 不限；典型："该平台密钥共 1000 万 token"
    pub safety_margin: f64,          // 默认 0.98，提前停用
    pub used_tokens: u64,            // settle 时累加（四维求和）
    pub exhausted_at_ms: Option<u64>,// 触顶时刻，供审计
}
```

- **判定**：`settle` 时累加 `used_tokens`；当 `used_tokens ≥ limit_tokens × safety_margin` → `health = QuotaExhausted`，**立即从轮转池移除**。
- **不自动恢复**：这是与 429 冷却的**本质区别**。恢复只有两条路径 —— ① 管理员 `/admin/ukey/reset`（清 `used_tokens`）；② 调高 `limit_tokens`。**探针不会清除此状态**（密钥确实有效，只是没额度了）。
- **快速确认路径（B 的收敛形态）**：若上游响应命中可配置的"配额耗尽"模式（`402`，或 `403`/`429` 且 body 匹配 `insufficient_quota` / `quota exceeded` / `余额不足` 等正则）→ **立即置 `QuotaExhausted`**，不必等计数触顶。这条路径把"上游明确告知"与"本地计数"合并到同一个终态，避免两套状态。
- **开关**：`quota.enabled = false` 时该密钥退回"纯 gptload 行为"。

**底线功能（无论是否启用计数，都必须实现）**

1. 密钥可用性**验证**（§6.1）；
2. 手动**暂停 / 恢复**（`Paused` 状态，管理员操作）；
3. 失效后**自动摘除**（401/403 达阈值）；
4. 失效后的**复核恢复**（探针返回 200）。

> 即：额度是可选的一层，**验证 + 暂停/恢复 + 自动摘除/恢复 是必做基线**。如果某天认为额度不必要，删掉 6.5.1 的计数器即可，其余不受影响——这正是把它做成独立可选层的原因。

#### 6.5.2 四维用量与费率

```rust
pub struct UsageCounters {   // 四维独立存储
    pub input_tokens:  u64,
    pub output_tokens: u64,
    pub think_tokens:  u64,   // 计入输出计费，但独立存储
    pub cache_tokens:  u64,
}

pub struct Rate {            // 每 1K token 的积分
    pub input:  f64,         // 默认 1.0
    pub output: f64,         // 默认 1.0
    pub think:  f64,         // 默认 1.0（= output）
    pub cache:  f64,         // 默认 0.2
    pub per_request: Option<u64>,   // 保留：图片等非 token 计费场景
}
```

**D6 落实**：默认倍率 `input 1 / output 1 / think 1 / cache 0.2`；**think 按输出费率计费，但存储上与 output 分开**（既能算钱，又能单独看思维链开销）。

> 上游密钥额度的 `used_tokens` 取四维之和（`input + output + think + cache`），与平台"总 token 配额"的口径对齐。

#### 6.5.3 计费公式

```
cost_micro = ceil( (
      input × r_input
    + output × r_output
    + think  × r_think
    + cache  × r_cache
  ) / 1000 × 1_000_000 )                     # micro-credit 定点，沿用旧版精度

per_request 模式：cost_micro = ceil(r_per_request × 1_000_000)
未知模型：使用 RateTable 的默认行（不再是旧版硬编码的 0.1 / 1.0）
最小费用：1 µcredit（沿用）
```

- 保持 micro-credit 定点（1 credit = 10⁶ µcredit），避免浮点累积误差——这是 3.1 判定"完整保留"的数学部分。
- `RateTable` 按**模型 id** 统一配置（D8：不设分组独立倍率）；`ModelBinding.rate_override` 为可选的上游级覆盖，默认 `None`。
- 费率表随 `AppConfig` 热更新，**不重启生效**。

#### 6.5.4 访问密钥账户与积分（D6 / D9）

```rust
pub struct AccessKey {
    pub id: AccessKeyId,             // Uuid，永久身份
    pub hash: [u8; 32],              // blake3(secret)，明文不落库
    pub prefix: String,              // 仅用于管理端展示，如 "sk-aequi-a1b2"
    pub groups: AccessKeyGroups,     // 分组（§6.3）
    pub credits_cap: Option<Credits>,// None = 无限（取代 -1 哨兵）
    pub credits_used: Credits,
    pub usage: UsageCounters,        // 四维
    pub lifetime_credits_used: Credits, // 累计，reset 不清零（审计用）
    pub enabled: bool,
    pub note: Option<String>,
    pub created_at_ms: u64,
    pub last_used_at_ms: Option<u64>,
}
```

- **可用积分** = `credits_cap.map_or(∞, |cap| cap − credits_used)`。
- **无周期重置**：不存 `period` / `next_reset`，没有任何按月清零的调度任务——旧版 `check_monthly_reset` 的全局 `tree.clear()` 及其丢失更新问题**从模型中消失**。
- **手动分发**：管理员通过 `/admin/akey/credits`（加额 / 改上限）与 `/admin/akey/reset`（清零 `credits_used`）操作；两者都写审计事件。

#### 6.5.5 预留-结算（与旧状态机同源，不新开路径）

沿用 3.1 判定"领域正确"的预留-结算模型，但四维化；**访问密钥与上游密钥在同一次结算中各自记账**：

```
reserve(akey_id, est):
    if cap.is_some() && used + reserve_min > cap   → QuotaExceeded
    if !enabled                                     → KeyDisabled
    used += reserve_min(1 µcredit)                  → Reserved

settle(akey_id, ukey_id, actual: UsageCounters):
    # ① 访问密钥侧（积分）
    credits_delta = cost(actual) − reserve_min
    used     += credits_delta
    usage    += actual(四维)
    lifetime += credits_delta
    # ② 上游密钥侧（额度）—— 同一次结算内顺带完成，不新增写路径
    ukey.used_tokens += actual.sum()
    if ukey.limit_tokens.is_some() && ukey.used_tokens >= limit × safety_margin:
        ukey.health = QuotaExhausted          # 摘除轮转池

release(akey_id):
    used −= reserve_min
```

- **两层记账同源**：都在 `settle` 内完成，不新增独立的计费/统计链路（解决 2.1 表 #7 的双通道病灶）。
- 写入走 §5.3 的**高优先通道**（立即提交），保证额度判定实时。
- `access_key` 与账户字段在**同一行**；`upstream_key` 的额度字段也在其自身行内 → 两次单行更新，天然原子。

#### 6.5.6 超额响应语义

```
HTTP 429
{ "error": { "code": "quota_exceeded",
             "message": "access key credits exhausted",
             "details": { "retryable": false, "cap": 100.0, "used": 100.0 } } }
```

- **不带 `Retry-After`**：重试无用，必须由管理员手动加额（与"周期重置会自动恢复"的语义彻底区分）。
- `retryable: false` 显式告知客户端不要自动重试。
- 与"上游 429"（`retryable: true`）在 `code` 上严格区分。
- 软阈值（默认 80%）触发 `QuotaThresholdReached` 事件供告警。
- **上游密钥额度触顶对客户端完全透明**：不产生任何客户端可见错误——选路自然切到其他密钥；只有当该模型的**所有**密钥都 `QuotaExhausted` 时才返回 503 `no_available_key`。

#### 6.5.7 上游密钥的存储与安全

- `upstream_key(seq, id, upstream_id, secret_enc, prefix, health, quota…, meta…)`，`id = blake3(secret)[..16]`。
- 上游密钥**必须可还原**（要发给上游），因此：默认明文存 SQLite（文件权限 `0600`，`data_dir` 建议独立用户）；可选 `AEQUI_MASTER_KEY` 启用字段级加密（ChaCha20-Poly1305），密钥不进数据库。
- **API 永不返回明文**：`/admin/ukey/list` 只返回 `id` + `prefix`；只有 `import` 时客户端单向提交。
- 导入时逐条做字符校验（沿用 3.1 的 `validate_key_chars`）+ 同上游内指纹去重。
- 明文只存在于**内存**（`Secret<Arc<str>>`）与**发往上游的请求头**，不进日志、不进事件、不进 URI。

### 6.6 埋点维度一次性加全

`schema_version = 1` 即包含本轮全部能力所需维度：

| 事件 | 字段 |
|:---|:---|
| `RequestFinished` | `request_id`、`akey_id`、`group`、`model`、`binding_id`、`upstream_id`、`key_id`、`status`、`retry_count`、`timing{queue,ttfb,total}`、`usage{input,output,think,cache}`、`credits`、`token_source`、`error_code` |
| `UpstreamSelected` | `request_id`、`binding_id`、`upstream_id`、`key_id`、`selection_reason`、`key_inflight`、`skipped{cooldown,inflight,excluded}` |
| `KeyCooldownSet` | `key_id`、`reason(429/5xx/network)`、`dur_ms`、`streak`、`jittered` |
| `BreakerStateChanged` | `upstream_id`、`from`、`to`、`errors_in_window` |
| `KeyProbeCompleted` | `key_id`、`outcome`、`latency_ms`、`source` |
| `AccessDenied` | `akey_id`、`model`、`reason` |
| `QuotaThresholdReached` | `akey_id`、`used`、`cap`（**访问密钥积分**软阈值） |
| `QuotaExceeded` | `akey_id`、`used`、`cap` |
| `AccountAdjusted` | `akey_id`、`delta`、`new_cap`、`operator` |
| `UpstreamKeyQuotaExhausted` | `ukey_id`、`upstream_id`、`used_tokens`、`limit_tokens`、`trigger: Counter \| UpstreamReported`（**上游密钥额度**，§6.5.1） |
| `UpstreamKeyLifecycle` | `ukey_id`、`action: Pause \| Resume \| ResetQuota \| SetQuota`、`operator` |

| 指标 | 说明 |
|:---|:---|
| `aequi_requests_total{group,model,status}` | 请求计数 |
| `aequi_tokens_total{model,dim}` | `dim ∈ {input,output,think,cache}` |
| `aequi_credits_used_total{group}` | 积分消耗 |
| `aequi_key_selected_total{key_id}` / `aequi_key_served_skew{upstream}` | 选路公平性 |
| `aequi_key_cooldown_seconds` / `aequi_breaker_open_total` / `aequi_breaker_state` | 退避与熔断 |
| `aequi_key_probe_total{outcome}` / `aequi_key_probe_queue_depth` | 探针 |
| `aequi_access_denied_total{reason}` / `aequi_quota_exceeded_total` | 授权与访问密钥积分 |
| `aequi_ukey_quota_used_ratio{ukey_id,upstream}` / `aequi_ukey_quota_exhausted_total{upstream}` / `aequi_ukey_health{health}` | 上游密钥额度与健康态分布 |
| `aequi_pagination_scan_rows` | 分页实测扫描行数（验证未退化为全表扫描） |
| `aequi_disk_usage_ratio` / `aequi_wal_size_bytes` | 存储健康 |

> **CI 门槛**：任何新机制若未同时提交事件与指标定义，不予合入——把旧版"静默降级无计量"从流程上堵死。

### 6.7 前端与 embed（D1）

- `rust-embed` 编译进 `src/static/dist`（构建期产出）。
- **路由优先级**：

  | 路径 | 处理 |
  |:---|:---|
  | `/v1/*` | 代理（OpenAI 契约） |
  | `/admin/*` | 管理 API（§4.5） |
  | `/health` `/ready` `/metrics` | 运维端点 |
  | 其它 `GET` | 命中静态文件则返回；未命中回落 `index.html` |

- **SPA 路由由前端自管**：后端只做 `index.html` fallback，不解析前端路由。
- 缓存策略：hashed 资源 `Cache-Control: public, max-age=31536000, immutable`；`index.html` 用 `no-cache`。
- 安全头：CSP、`X-Content-Type-Options: nosniff`、`Referrer-Policy`。
- **产物不入库**：`build.rs` 在缺失时给出明确报错；同时提供 `--no-default-features`（不 embed）用于纯 API 构建。
- 后端只提供"必要的数据查询与增删改查接口"，不含任何服务端页面渲染逻辑。

### 6.8 落地顺序

| 阶段 | 工作 | 退出条件 |
|:---|:---|:---|
| **P1 领域内核** | `core::{id, secret, config, binding, group, routing, backoff, rating, account, quota, telemetry}` | 选择器公平性测试、退避边界测试、授权判定与 level→group 等价测试、额度触顶判定测试全绿；`core` 无 IO 依赖（CI 校验） |
| **P2 存储层** | SQLite schema（含 `seq` 自增主键与覆盖索引）+ repo trait + 单写者 actor + 日志 sink；**游标分页查询**；迁移工具（dry-run + 对账 + 回滚） | dry-run 对账四项一致；批量事务实测 ≥ 1.7k RPS 写入不堆积；WAL 尺寸受控；`EXPLAIN QUERY PLAN` 证明列表查询走索引、无全表扫描 |
| **P3 协议层** | `upstream/adapter/{openai,anthropic,gemini}` 迁移全部映射规则；探针服务 | 三格式的既有 `#[cfg(test)]` 用例全绿；404/500 不再误恢复 |
| **P4 网关** | axum + `/admin/{域}/{动作}` + **keyset 分页** + embed + SSE（心跳 / Last-Event-ID） | 端点契约测试通过；翻页无重复/漏项；换过滤条件复用游标被拒；SPA fallback 与静态缓存策略验证 |
| **P5 编排接线** | 绑定解析 → 分组过滤 → 两级选路 → 四维计费 → 退避/熔断 → 探针 → **两层额度记账** | shadow 双跑：新旧选路分布偏差 < 阈值；429 冷却曲线符合预期；公平性指标达标；额度触顶后自动摘除且不复活 |
| **P6 迁移与上线** | 单向迁移 + 存量日志脱敏 + 文档更新 | 存量实例可无损升级；旧版本归档 |

---

## 七、回归与验收

### 7.1 必须兼容的对外资产

D5 选择不兼容，因此**只有两项契约必须保持**：

1. **代理端协议**：`/v1/chat/completions`、`/v1/models` 必须保持 OpenAI 契约（含流式 chunk 形状与 `finish_reason` 语义）。这是客户端契约，不可破坏。
2. **运维端点**：`/health`（liveness）、`/ready`（readiness，检查 DB 与磁盘）、`/metrics`（Prometheus 文本格式）。

其余全部可破坏：`config.toml` 字段、旧 admin API、sled 数据、指标名前缀。

### 7.2 历史缺陷回归清单

| 历史缺陷 | 回归用例 |
|:---|:---|
| 重试时模型名重新映射错误（`825f09b`、`508860c`） | 多上游、各绑定 `upstream_model` 不同，强制触发重试，断言每跳请求体的模型名与**该跳绑定**一致 |
| 跨重启请求日志时序错误（`4bad36c`） | 重启后 `log/query` 顺序按 `ts` 正确 |
| 流式分块被缓冲（`42eaf95`） | 流式响应首字节到达时间与上游 TTFB 差值 < 阈值 |
| Gemini 工具调用增量字段演进（`cd0129d`） | 未知事件类型忽略而非报错；工具调用分片正确拼装 |
| 模型映射需同步进请求体（`aceafad`） | 断言转发请求体中的 `model` 已被改写为 `binding.upstream_model` |

### 7.3 本轮新增能力的验收项

| 能力 | 验收 |
|:---|:---|
| 密钥验证 | 404 / 500 / 超时**不得**恢复失效密钥；仅 200 恢复；401/403 降级；429 不改变状态；探测预算生效；结论跨重启保留 |
| 429 退避 | 冷却按 `base×factor^(n-1)` 增长并受 `max_ms` 截断；抖动生效；**并发排队请求的唤醒时刻分散**（无同步惊群）；`Retry-After` 作为下界且被 cap；2xx 立即释放冷却 |
| 熔断 | 窗口内不同密钥失败数达阈值即打开；打开期间整体跳过该上游；半开成功即闭合、失败续开且 `open_ms` 翻倍 |
| 分组授权 | level→group 等价性（含 `-1` / `L=M` / 越界）；**同 display_model 在不同上游时按绑定分别授权**（不被错误放行）；`deny` 优先 |
| 轮询公平性 | 均匀权重 10k 次 `max/min served ∈ [1,1.02]`；权重 1:2:3 比例误差 < 2%；新增密钥立即被选中；重试不扭曲分布；无 `rebuild_active_keys` 调用 |
| **上游密钥额度** | 触顶后**立即摘出轮转池**且**不随时间自动恢复**（区别于 429 冷却）；探针返回 200 **不清除** `QuotaExhausted`；`safety_margin` 提前停用生效；上游明确报配额耗尽时走快速确认路径；所有密钥同时额度耗尽 → 503 `no_available_key`（客户端**不会**看到上游额度相关错误）；`quota.enabled = false` 时行为退回纯 gptload |
| **密钥状态与人工控制** | `enabled` 与 `health` 正交：人工暂停期间凭证失效，恢复其一时不覆盖另一；暂停/恢复后选路立即生效 |
| **访问密钥积分** | 并发预留-结算无丢失更新；四维用量分别落库且 `think` 与 `output` 不混淆；`cap = None` 为无限；超额返回 `retryable: false`；手动 `reset` 不清 `lifetime` |
| **列表分页** | 分页查询为 O(limit)（`EXPLAIN QUERY PLAN` 命中覆盖索引、无全表扫描）；`OFFSET` 不出现于任何列表 SQL；换过滤条件复用旧游标返回 `cursor_filter_mismatch`；新增密钥不会导致跨页重复或漏项；`limit > 1000` 被拒 |
| 统一输出层 | 三种上游格式的响应归一后结构一致；`gateway/proxy.rs` 无 format 分支（CI 校验）；客户端自带 `Authorization` 不被转发 |
| 存储 | 1.7k RPS 下写队列不堆积；WAL 尺寸受控；日志清理后空间可回收；迁移 dry-run 对账四项一致 |

### 7.4 性能门槛

- 代理吞吐、P99 延迟、内存占用**不低于旧版基线**（旧版声称 1,700+ RPS @ 53 key / 100 并发、约 18MB）。
- 必须消除的旧版开销：`save_global_tokens` 每请求 flush、`rebuild_active_keys` 的 O(n) 全量拷贝、`recent()` 的全量 clone + 排序、`next_cooldown_delay` 的 O(U×K) 扫描。
- 选路不得引入分配或全局锁（每上游一把短锁，见 §6.4.3）。
- 结算写放大：每请求 ≤ 1 次账户写入。

---

## 八、防返工硬约束（评审门槛）

> 这十四条是合入门槛。任何一条被破坏，本轮新增能力就会退化为"贴上去的补丁"，从而制造第二次重构。

1. **领域模型一次到位**：`UpstreamKeySlot` / `ModelBinding` / `Group` / `AccessKey` / `BackoffPolicy` / `ProbePolicy` 在 P1 即含全部字段，不预留"以后再补"。
2. **单一身份来源**：`UpstreamId` / `BindingId` / `GroupId` / `AccessKeyId` / `UpstreamKeyId` 一律为不可变 id（Uuid7 或指纹）；`name` 仅为可变展示名；**禁止用名称或明文密钥作为主键、树名、URI**。
3. **单一写者**：密钥运行态与访问密钥账户只有一条写路径（单写者 actor 或单行原子更新）；严禁多任务各自 mutate —— 旧版 `check_monthly_reset` 的丢失更新就是反例。
4. **记录格式版本化 / 强类型**：落盘事件带 `schema_version`；DB 用强类型列，禁止定长字节拼包；未知字段容忍、缺字段取默认。
5. **无哨兵值**：`Unlimited` / `Admin` / `None` 一律 `Option` / enum 显式建模，杜绝 `-1`、`0` 这类靠约定传播的魔法值。
6. **授权判定单一函数**：全项目只允许存在一处"该访问密钥是否可使用该模型绑定"的判断（§6.3.2）。
7. **埋点维度一次加全 + 机制必带指标**：新机制若无事件与指标定义，不得合入。
8. **API 约定统一**：`/admin/{域}/{动作}` + JSON body + 统一 envelope；**ID / 名称 / 密钥不进 URI**；分页游标化。
9. **额度与积分共用 reserve/settle**：禁止第二计费路径；额度检查与积分扣减在同一临界区/同一行。
10. **格式差异只准出现在 adapter**：`gateway` 层不得有 `match format` 分支；**禁止 BYOK 透传**；所有响应必须经 Canonical 出口。
11. **策略即数据 + 热更新**：退避 / 探针 / 费率 / 分组全部落在 `AppConfig`（`ArcSwap`），调参不发版，也避免"热重载只更新一部分"的旧病。
12. **迁移可验证**：迁移器必须 dry-run + 四项对账 + 可回滚；等级→分组映射必须有等价性单测。
13. **列表一律数据库分页**：任何返回集合的端点必须使用 keyset 游标 + 覆盖索引；**禁止 `load_all()` 后 `skip/take`**，禁止 `OFFSET` 深分页（唯一例外：内存路由用的槽位全集，它不对外返回）。
14. **两层额度不得混用**：上游密钥额度（自动摘除，对客户端透明）与访问密钥积分（拒绝请求，管理员手动加额）必须是独立字段、独立恢复路径、独立错误语义；**恢复方式不同的状态不得共用一枚字段**。

---

## 九、风险清单

| 风险 | 等级 | 缓解 |
|:---|:---|:---|
| **SQLite 在极端写入下成为瓶颈**（日志走 sqlite sink 时） | 中 | 默认日志走 JSONL；sqlite sink 强制走批量事务 + 分区表 + `DROP TABLE` 清理；压测门槛写入 §7.4 |
| **WAL 膨胀导致写停顿** | 中 | `wal_autocheckpoint` + 定时 `wal_checkpoint(TRUNCATE)` + `aequi_wal_size_bytes` 告警 |
| **迁移过程数据丢失或对账不平** | **高** | 迁移前强制备份；dry-run 报告；四项对账门槛；写入新目录、旧数据不动；回滚 = 删新库 |
| **上游密钥明文存储被读取** | 中 | 文件权限 `0600`；可选 `AEQUI_MASTER_KEY` 字段级加密；API 永不回显明文；落盘/事件只留指纹 |
| **上游级熔断误开导致可用性下降** | 中 | 阈值按"**不同密钥**失败数"计算（单密钥故障不触发）；半开探测；指标告警；可一键关闭 |
| **探针被上游判定为滥用** | 低–中 | 全局 + 单上游双并发限制、每分钟预算、指数复核间隔、启动打散；对话式探针默认关闭 |
| **等级→分组迁移语义歧义** | 中 | 使用有等价证明的映射（§6.3.4）+ 边界单测（`-1` / `L=M` / 越界） |
| **Gemini Interactions 为演进中协议** | **高** | `Api-Revision` 与事件名提升为 binding 级可配置；对未知事件保持向后兼容（忽略而非报错）+ 契约测试 |
| **四维计费改动影响存量积分** | 中 | 迁移时 `cache = 0`、`think` 归入 `output`，不虚构历史数据；费率表落库可见可改；迁移前后对账 `credits_used` |
| **公平选择器的短锁被误优化为无锁** | 低 | §6.4.3 写明"有意为之"的理由；`aequi_selector_lock_wait_seconds` 监控 |
| **前端产物缺失导致构建失败** | 低 | `build.rs` 明确报错 + `--no-default-features` 纯 API 构建路径 |
| **hyper 0.14 → 1.x 涉及连接层改造** | 中 | SOCKS5 逻辑保留、仅改签名；先做最小可行连接层 + 连通性测试 |
| **上游密钥额度计数不准**（上游不回 usage，只能估算） | 中 | `safety_margin` 默认 0.98 提前停用；估算来源在事件中标注 `token_source`；必要时改用上游明确报错触发的快速确认路径 |
| **把"限流"误判为"配额耗尽"导致密钥被永久摘除** | 中 | 快速确认路径仅在 body 命中明确配额正则或 `402` 时生效；普通 429 只走冷却（§6.2），不置 `QuotaExhausted`；指标 `ukey_quota_exhausted_total` 突增告警 |
| **管理端列表仍有全量加载路径** | 中 | 硬约束 §八 第 13 条 + CI 检查 `load_all` 不出现在 handler；`aequi_pagination_scan_rows` 监控扫描行数 |
| **分页期间数据变动导致跨页重复/漏项** | 低 | 用单调 `seq` 作排序键（不用随机 id）；删除用软删保留 `seq` 空洞，不复用自增号 |

---

## 附 A：决策记录（D1–D14）

| # | 决策 | 结论摘要 | 被否决的选项及原因 |
|:---|:---|:---|:---|
| D1 | 前端归属 | 保留 embed 单文件部署；SPA 路由前端自管；后端只提供数据 CRUD/查询接口 | 去掉 UI（丧失单文件部署价值）；独立仓库（增加部署摩擦） |
| D2 | 存储引擎 | SQLite（关系数据）+ JSONL（请求日志主通道） | 继续 sled（定长布局与事务缺失无法容纳新模型）；纯 SQLite 日志（追加密集场景不划算） |
| D3 | HTTP 框架 | axum + tower + hyper 1.x | 手写 hyper（缺陷集中地，代码量更大）；actix（生态隔离，作废 SOCKS5 连接器资产） |
| D4 | 协议范围 | OpenAI 唯一对外协议；统一输出层归一；无 BYOK 透传 | 保留透传（产生双路径与 BYOK 管理负担，且是历史缺陷根源） |
| D5 | 兼容级别 | 不兼容，一次性单向迁移 | 完全兼容（等于带着问题照旧运行）；部分兼容读层（增加长期维护面） |
| D6 | 计费费率 | 四维 input/output/think/cache，默认 1/1/1/0.2；think 独立存储；保留积分制 | 沿用两维（无法表达缓存折扣与思维链开销） |
| D7 | crate 粒度 | 单 crate + 强边界模块 | 多 crate（无复用需求，只增加编译与联调成本） |
| D8 | 分组模型 | 分组只决定可访问的模型绑定集合；倍率按模型统一配置；绑定必须带上游 id | 分组独立倍率（new-api 式商业化臃肿）；仅按模型 id 授权（同 id 跨上游会误放行） |
| D9 | 两层额度 | 上游密钥额度（token 上限 → 自动摘出轮转池，对客户端透明）+ 访问密钥积分（上限 → 拒绝请求，管理员手动分发）；均无周期重置 | 周期重置（无商业化计划，且引入丢失更新与月初窗口）；把两层混用（恢复方式不同，必然互相误恢复） |
| D10 | 探针策略 | 五态判定 + 统一探针 + 并发与预算约束 + 指数复核 + CAS 写回 | 沿用"非 401/403 即有效"（会把 404/500 误判为恢复） |
| D11 | 轮询算法 | 两级 SWRR + LRS，移除活跃集重建 | 仅修游标起点（位置偏置仍在）；纯随机（不可解释、难测试） |
| D12 | 熔断粒度 | 上游级半开熔断 + 密钥级指数退避 | 仅密钥级（上游整体故障时会撞穿所有密钥） |
| D13 | 上游密钥额度 | 极简显式计数（`limit_tokens` + `safety_margin`）→ `QuotaExhausted` 摘出轮转池、不随时间恢复；上游明确报错时走快速确认路径；不做周期/多维/每模型额度、不主动查上游余额 | 纯反应式（退避有上限，到期必重试，**无法真正"停用"**；且 429/401 无法区分"限流"与"配额耗尽"）；完整额度 DSL（过度设计） |
| D14 | 列表分页 | 数据库 keyset 分页（`seq` 游标 + 覆盖索引 + 无 `OFFSET`） | 沿用旧版 `load_all().skip().take()`（内存与耗时随总量线性增长）；`OFFSET` 深分页（O(offset) 扫描） |

---

## 附 B：旧版本资产价值分布

| 分类 | 代码量占比 | 说明 |
|:---|:---|:---|
| ✅ 完整保留（搬运） | ~15% | 工具函数、协议映射规则、计费数学、日志轮转算法、SOCKS5 连接器 |
| 🟡 保留 + 补丁 | ~40% | 存储、计费状态机、选择算法、编排逻辑、CI/部署 |
| 🔴 新架构替代 | ~40% | 状态容器、日志/埋点、API 路由、SSE、**HTTP 栈、存储引擎、协议调用结构** |
| 🗑 移除 | ~5% | 死代码、死字段、重复副本、`-1` 哨兵、月度重置 |

> 约 **55% 的代码价值可以延续，40% 必须结构性重写，5% 直接删除**。
>
> 引入 §六 五项能力后，原本列在"保留 + 补丁"中的**密钥选择、429 冷却、密钥失效恢复、计费存储、KeyStore** 五处被直接吸收进领域模型，从"打补丁"升级为"结构替代"，补丁债进一步下降。
