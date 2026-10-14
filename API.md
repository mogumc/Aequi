# Aequi API 文档

> 与 `src/api/router.rs` 同步维护；改路由或结构体时记得一起改这里。
> 前端项目通过这份文档对接。

## 通用约定

### 认证

两套认证互不越权，数据分属两个数据库：

| 体系 | 凭证 | 可用范围 |
| --- | --- | --- |
| 管理员 | 登录令牌（`POST /api/auth/login` 签发） | `/api/*`（除 `/api/key/*`） |
| 访问密钥 | 密钥明文（`ak_` 开头，管理员创建） | `/api/key/*` 与 `/v1/*` |

均通过 `Authorization: Bearer <凭证>` 头携带。

### 错误格式

非 2xx 响应统一为：

```json
{ "error": { "message": "错误描述", "type": "bad_request" } }
```

`type` 取值：`bad_request`、`unauthorized`、`invalid_credentials`、`totp_required`、
`invalid_totp`、`forbidden`、`insufficient_credits`、`upstream_unavailable`、
`not_found`、`database_error`、`internal_error`。

状态码：400 参数、401 认证、402 余额不足、403 禁止、404 不存在、502 上游异常、500 内部错误。

### 其它约定

- 时间一律秒级 Unix 时间戳（INTEGER）。
- 积分在 JSON 里是**四位小数字符串**，同值另有 `*_scaled` 整数字段（1 积分 = 10000）便于精确计算。
- 计费倍率是**万分比整数**（10000 = 1.0 倍），配置在块的模型条目上（`multiplier_bp`）。
  倍率精度为 **0.01**：`multiplier_bp` 必须是 100 的倍数（即两位小数）；
  计费零头**向上取整**（不足一个最小积分单位按 1 计），避免抹零被打磨。
- 列表类接口的 `limit` 类参数都会被服务端收敛到安全区间，传超界值不会报错、只会被夹住。

---

## 公开接口

### `GET /health`

返回 `status`、`version`、`uptime_seconds`、`account_configured`（数据库是否已初始化账号）、`providers`。

### `POST /api/auth/login`

```json
{ "username": "...", "password": "...", "otp": "123456" }
```

`otp` 仅在账号启用 OTP 后必填；未启用时缺失、启用后缺失分别返回 `400` 与 `401 totp_required`。

响应：

```json
{ "token": "...", "expires_at": 1790000000, "account": { "id": 1, "username": "...", "totp_enabled": false, "created_at": 0 } }
```

### `GET /api/auth/providers`

可用第三方登录方式列表（当前为空）。

### `GET /`

302 跳转到 `/web/`。

---

## 管理员接口

以下全部要求管理员令牌。

### 仪表盘统计

#### `GET /api/stats?days=7`

一次拿到仪表盘需要的全部聚合数据。**聚合在数据库侧完成**，不受日志条数上限影响。

`days`：趋势图回看天数，默认 7、收敛到 1-90。

```json
{
  "usage": {
    "requests": 120, "failed": 3,
    "prompt_tokens": 1000, "completion_tokens": 500, "cache_tokens": 100,
    "credits": "0.1500", "credits_scaled": 1500
  },
  "today": { "...字段同 usage，范围为本地时区的今天..." },
  "keys":   { "total": 3, "enabled": 2 },
  "blocks": { "total": 2, "enabled": 2 },
  "trend": [
    { "date": "2026-10-05", "requests": 10, "failed": 0, "tokens": 100, "credits": "0.0100", "credits_scaled": 100 }
  ]
}
```

- `trend` 按**本地时区**切天，只包含有记录的天（无记录的日期不补零）。
- `usage.requests` 含失败请求，`failed` 是其中失败的数量。

### 账号与会话

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| `GET` | `/api/auth/me` | 当前账号（`AccountView`） |
| `POST` | `/api/auth/logout` | 退出当前会话 |
| `POST` | `/api/auth/password` | 改密码：`{ current_password, new_password, confirm_password }`；成功后撤销其它设备（响应含 `revoked`） |
| `POST` | `/api/auth/username` | 改用户名：`{ current_password, username }`；返回更新后的账号 |
| `GET` | `/api/devices` | 在线设备列表，当前设备带 `current: true` |
| `DELETE` | `/api/devices` | 移出除本机外的全部设备，返回 `{ revoked }` |
| `DELETE` | `/api/devices/{id}` | 移出一台设备 |

`SessionView`：`id`、`token_prefix`、`login_type`、`ip`、`user_agent`、`created_at`、`last_seen_at`、`expires_at`、`current`。

### OTP

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| `GET` | `/api/otp` | `{ enabled, pending }` |
| `POST` | `/api/otp/setup` | `{ secret, otpauth_url }`，此时尚未启用 |
| `POST` | `/api/otp/enable` | `{ code }` |
| `POST` | `/api/otp/disable` | `{ code? }` |

### 设置

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| `GET` | `/api/settings` | 当前设置 |
| `PATCH` | `/api/settings` | 部分更新 |

```json
{
  "listen": "0.0.0.0:8080",
  "cors_origins": ["*"],
  "trust_proxy": false,
  "token_ttl_hours": 168,
  "log_retention_days": 7
}
```

`listen` 与 `cors_origins` 需重启生效；`log_retention_days` 范围 1-365。

### 访问密钥

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| `GET` | `/api/keys` | 列表（支持分页与搜索，见下） |
| `POST` | `/api/keys` | 创建：`{ name?, credits?, unlimited?, allowed_models? }` |
| `GET` | `/api/keys/{id}` | 详情（含明文） |
| `PATCH` | `/api/keys/{id}` | 更新：`{ name?, enabled?, allowed_models? }` |
| `DELETE` | `/api/keys/{id}` | 删除 |
| `POST` | `/api/keys/{id}/credits` | 调整额度：`{ credits?, unlimited? }`，只改提交的那部分 |
| `GET` | `/api/keys/{id}/usage` | 该密钥的用量（结构同 `/api/key/usage`） |

**无限额度**：`unlimited: true` 的密钥不参与余额判定——扣费时跳过余额检查，
**但每次请求的用量与消耗照常记录**（`usage_records` 里一样能看到）。
它与 `credits` 并存但互不影响：无限时余额字段不参与判定，
保留原值只是为了切回有限额度时有个起点。

**余额不足（透支结算）**：结算（请求结束时按实际用量扣）发现余额小于
本次消耗时，按透支处理——用量记录照常落库（`credits` 记**实际扣掉的
金额** = 全部剩余积分，token 数照实），余额直接置 0，客户端照常拿到
本次响应。结算完余额为 0，入口检查即拒绝后续调用，必须充值才能再用；
单次透支的漏损上限为一笔请求。密钥被停用/删除时仍按错误处理，不记账。

#### `GET /api/keys?page=1&page_size=20&search=xxx`

- `page`：从 1 开始，缺省 1。
- `page_size`：缺省 20、收敛到 1-100。
- `search`：按**名称或密钥前缀**模糊匹配，空串视为不筛选。

响应：

```json
{
  "keys": [ "...KeyView..." ],
  "total": 42,      // 与筛选匹配的总数，不受分页影响
  "page": 1,
  "page_size": 20
}
```

`KeyView`：`id`、`name`、`secret`（明文）、`key_prefix`、`credits`、`credits_scaled`、`unlimited`、`enabled`、`allowed_models`、`created_at`、`updated_at`、`last_used_at`。

**`allowed_models` 的条目是「块 + 模型」的全名**（`block-a/gpt-4o`），不是模型名。
同一个模型名出现在两个块里时，那是**两条彼此独立的授权**——两个块的出口与价格
都可能不同，允许一个不等于允许另一个。若只写模型名，就会出现「只授权了 a 的
gpt-4o，短名请求却被轮询到 b」这种越权。数组为空表示不限制。

### 上游块

块是代理的调度单位：**一个块只允许一种 API 格式**，内含一组上游密钥，
以及它对外接受的**模型列表**——模型属于块，计费倍率也跟着模型走，
所以同一个模型名在不同块里可以有不同的价。

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| `GET` | `/api/blocks` | 列表（每块带 `key_count` / `available_keys`，以及实时并发负载 `in_flight` / `queued`） |
| `POST` | `/api/blocks` | 创建 |
| `GET` | `/api/blocks/{id}` | 详情 |
| `PATCH` | `/api/blocks/{id}` | 更新（可含 `api_format`；`proxy` 传 `null` 表示清空；`headers` 传 `[]` 表示清空） |
| `DELETE` | `/api/blocks/{id}` | 删除（密钥级联清理） |
| `GET` | `/api/blocks/{id}/keys` | 块内密钥列表 |
| `POST` | `/api/blocks/{id}/keys` | 批量添加：`{ secrets: ["..."], weight?, never_disable? }`；重复条目自动跳过，返回 `{ keys, skipped }` |
| `POST` | `/api/blocks/{id}/keys/remove` | 批量删除：`{ ids }` |
| `POST` | `/api/blocks/{id}/keys/pause` | 批量暂停：`{ ids }` |
| `POST` | `/api/blocks/{id}/keys/resume` | 批量恢复：`{ ids }`（同时清零失败计数） |
| `POST` | `/api/blocks/{id}/keys/never-disable` | 批量设置「永不暂停」：`{ ids, enabled? }`，`enabled` 缺省 `true`（即「设上」） |
| `POST` | `/api/blocks/{id}/keys/{key_id}/test` | 测试单把密钥（不耗 token），结果同步落库 |
| `POST` | `/api/blocks/{id}/test` | **整块连通性测试**（见下） |
| `POST` | `/api/blocks/{id}/models/fetch` | **从上游拉取可用模型列表**（见下） |

**拉取上游模型**（`POST /api/blocks/{id}/models/fetch`）返回 `{ "models": ["..."] }`。
它复用的就是密钥探测那条请求——各家的「列模型」接口本来就是探测密钥用的出口，
所以不会出现「探测通过但拉不到模型」的偏差。响应形状各家不同，服务端统一收敛：
OpenAI / Anthropic 取 `data[].id`，Google 取 `models[].name` 并剥掉 `models/` 前缀。

失败都给出可读原因：块内没有可用密钥报 400，上游拒绝或不可达报 5xx。
**上游不一定提供这个接口，返回的列表也可能不全**——所以前端保留了手动添加模型名，
拉取是主路径而不是唯一入口。

块创建/更新：

```json
{
  "id": "openai-main",
  "name": "OpenAI 官方",
  "api_format": "openai",
  "weight": 1,
  "max_concurrency": 4,
  "base_url": "https://api.openai.com",
  "proxy": "http://127.0.0.1:7890",
  "models": [
    { "name": "gpt-4o", "forward": "gpt-4o-2024-11-20", "multiplier_bp": 15000 },
    { "name": "gpt-4o-mini" }
  ],
  "headers": [
    { "name": "X-Tenant", "value": "demo-tenant" },
    { "name": "anthropic-version", "value": "2099-01-01" }
  ]
}
```

- `api_format`：`openai` | `google` | `anthropic`。`google` 走 v1beta **generateContent**
  （非流式 `POST /v1beta/models/{model}:generateContent`，流式
  `POST /v1beta/models/{model}:streamGenerateContent?alt=sse`），多轮工具对话按
  new-api 的方式回放：assistant 的 functionCall 原样回放并携带官方哨兵
  thoughtSignature（`context_engineering_is_the_way_to_go`，每条 assistant 消息最多
  一处：functionCall → Markdown 图片 → 首个文本），工具结果以 functionResponse 挂在
  `role:"user"` 的 content 里（连续结果合并进同一条）。
- **`id` 只允许小写字母、数字、点、下划线、连字符**（≤ 64 字符）。它会被当作
  请求里模型名的前缀（`块id/模型名`），含 `/` 或空格会直接破坏那条解析规则。
- `models[]`：本块对外接受的模型；`forward` 缺省表示同名转发；
  **`multiplier_bp` 是该模型的计费倍率（万分比整数，10000 = 1.0 倍，精度 0.01）**，缺省 1.0。
- **`max_concurrency`：同时发往该上游的请求数上限**，缺省 0（不限制），最大 1024。
  达到上限后新请求**排队**（FIFO）而不是被拒绝，详见下节。
- `headers[]`：**自定义请求头**，缺省为空数组；详见下节。

#### 块级并发与排队（`max_concurrency`）

每个块独立配置同时发往该上游的请求数上限（`max_concurrency`，0 = 不限制）。
超出的请求**排队等待**，不是拒绝——限流的目的是别把上游打爆，而不是把用户挡在门外。

- **管的是三条流量路径**：对话、流式、生图都过同一个闸门。流式请求的位子
  **持有到流结束**——流占着上游连接，那正是要限的对象。
- **排队上限 30 秒**，超时返回 `502`（`UpstreamUnavailable`，详情「上游通道繁忙，
  排队超时」），且**不进重试**——重试也过不了同一道闸门，只会把队列拖更长。
- **调整即时生效**：调大立即放行；调小最多让已在队列里的请求按旧容量走完
  （上界就是旧容量，不会持续超额）。删除块即撤掉闸门。
- **管理面不排队**：测试密钥、整块测试、拉取模型是一次性人工动作，
  不该被用户流量堵在队里。
- **实时负载随块接口返回**：`BlockView` 带 `in_flight`（正在发往上游的请求数）
  与 `queued`（排在闸门后的请求数），都是**读取瞬间的快照**，不是累计值；
  块不限制并发（`max_concurrency` 为 0）时两者为 `null`——没有闸门，
  就没有这两个数。`in_flight` 打满上限只是满负荷，`queued` 持续不降
  才说明该调大上限或加块了。后台块列表每 5 秒刷一次，看的就是这两个数。

#### 自定义请求头（`headers`）

有些上游要求额外的头：指定版本的 `anthropic-version`、组织/租户头
（`OpenAI-Organization` 之类）、网关自己的路由头，或者认证压根不是标准那三种写法。
这些都按块配置，缺省为空数组。

- **生效范围是整块**：对话、流式、生图、测试密钥、拉取模型走的是同一个出口，
  所以「测试通过」与「真实请求能通」永远是同一个条件。
- **同名覆盖**：头表按「各家固定头 → 认证头 → 自定义头」组装，自定义头放最后，
  于是它会盖掉同名的内置头（`Authorization`、`x-api-key`、`anthropic-version` 都算），
  而且**不会再追加一份**——配一条就只发一条。
  注意头值是字面量、引用不了块里的上游密钥，所以想靠它改认证写法得自己填死。
- 头名大小写不敏感；同一个头名**不允许配两遍**（否则哪条生效说不清）；最多 32 条。
- **由 HTTP 栈自己管的头不允许配置**：`Content-Length`、`Transfer-Encoding`、
  `Connection`、`Upgrade`、`Expect`、`TE`、`Trailer`、`Keep-Alive`——配了只会让请求
  畸形。其余一律放行，`Host` 也可以。
- 头名与头值去掉首尾空白后校验合法性（`\r\n` 之类直接构成头注入，入口就挡），
  不合法返回 400 且**不写库**。
- 更新是**整份替换**：传 `headers: []` 即清空，不传则保持原值（与 `proxy` 同一口径）。
- `BlockView.headers` 明文返回——与 `secret` 同一套取舍，管理员要能取回来核对。

#### `POST /api/blocks/{id}/test`

逐把测试块内**全部**密钥，串行执行（并发打上游容易被限流）。
结果与单把测试同口径：有效则清零失败计数，无效则自动停用
（**即使密钥设了永不暂停也停用**——手动测试是带证据的人工动作，
豁免免的是自动规则，不是这个）。

```json
{
  "block_id": "openai-main",
  "tested": 3,
  "valid": 2,
  "invalid": 1,
  "transient": 0,
  "results": [
    { "key_id": "abc123", "secret_prefix": "sk-abc", "outcome": "valid", "detail": null },
    { "key_id": "def456", "secret_prefix": "sk-def", "outcome": "invalid", "detail": "密钥被上游拒绝" }
  ]
}
```

`outcome`：`valid`（可用）| `invalid`（密钥本身有问题，已停用）| `transient`（限流/上游故障，不算密钥的账）。
`secret_prefix` 只给前 6 位——测试结果可能被贴进工单，完整密钥没必要外带。

`UpstreamKeyView`：`id`、`block_id`、`secret`（明文）、`status`（`active`/`paused`/`disabled`）、`never_disable`、`weight`、`failure_count`、`cooldown_until`、`last_error`、`last_used_at`、`created_at`。

#### 失败分类、退避与永不暂停

密钥生命周期由**失败类型**驱动，不再是「任何失败都计数、够数就停用」：

| 失败类型 | 判据 | 退避 | 自动停用 |
| --- | --- | --- | --- |
| `Hard`（密钥被拒） | 401 / 403 | 不退避 | 连续 3 次停用，`last_error` 写明原因 |
| `RateLimited`（被限流） | 429 | 指数退避 | **永不停用** |
| `Transient`（上游故障） | 5xx / 网络错误 / 200 但结构坏 | 指数退避 | **永不停用** |

- **指数退避**：30 秒起步，每次连续失败翻倍（30 → 60 → 120 → 240 → 480），
  480 秒封顶；退避期内的密钥不参与调度（`cooldown_until`），到期自动回队。
- **任何成功都清零**失败计数并解除退避——故障过去后密钥自己恢复，不用人工干预。
- **重试与退避是两件事**：可恢复错误（429/5xx/网络/坏结构）会换密钥重试，
  最多 3 次；退避管的是这把钥匙**下一次**什么时候再上岗。
- **`never_disable`（永不暂停）只免「自动停用」这一个动作**：429 与瞬时故障的
  退避照常生效（那是几十秒级的临时避让，不是停用）；手动暂停/恢复不受影响；
  手动测试确认无效仍然停用。计数与 `last_error` 留痕照旧，后台看得见发生了什么。
- 手工入口：`POST /api/blocks/{id}/keys/never-disable`，body `{ ids, enabled? }`，
  `enabled` 缺省 `true`；批量添加密钥时也可以带 `never_disable: true` 入库即生效。

### 请求日志

#### `GET /api/logs`

| 参数 | 说明 |
| --- | --- |
| `limit` | 条数，缺省 100、收敛到 1-500 |
| `before_id` | 游标分页：传上一页最后一条日志的 `id` 取更早一页，0 / 缺省从最新开始 |
| `since` / `until` | 时间范围（含边界），Unix 秒 |
| `status` | `success` 或 `error` |
| `key_id` | 按访问密钥 id 筛选 |
| `model` | 按对外模型名精确匹配 |
| `block_id` | 按上游块筛选 |

全部可省略，省略即该维度不限制。空串视为未传。

响应：

```json
{ "logs": [ "...ProxyLogView..." ], "has_more": true }
```

响应里的 `has_more` 说明是否还有更早的日志；把上一页最后一条的 `id` 传回
`before_id` 即可取下一页。

`ProxyLogView`：`id`、`created_at`、`access_key_id`、`block_id`、`upstream_key_id`、`model`、`ip`、`user_agent`、`status`（`success`/`error`）、`error_detail`（含上游原话，超 2000 字符截断）、`duration_ms`、`prompt_tokens`、`completion_tokens`、`cache_tokens`、`estimated`（true = 本地估算）、`stream`（true = 流式请求）、`credits`。

---

## 密钥自助接口

以下要求访问密钥，**只读**，且只能看到自己。

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| `GET` | `/api/key/me` | 自身信息（`KeySelfView`：`name`、`key_prefix`、`credits`、`credits_scaled`、`unlimited`、`enabled`、`allowed_models`、`created_at`、`last_used_at`——不含明文） |
| `GET` | `/api/key/usage?limit=50&before_id=0` | 自身用量（游标分页） |
| `GET` | `/api/key/models` | 自身可用的模型路由（脱敏视图，见下） |

`/api/key/models` 返回 `{ models: [...] }`，每条为该密钥实际可调用的
「块/模型」路由（与 `/v1/models` 同一套白名单过滤口径，但保留结构化
倍率）：`block_id`（调用前缀）、`model`（模型名）、`multiplier`
（两位小数字符串）。**脱敏视图**：只有调用时本来就公开的信息，
转发名与上游出口不出这个接口；停用块的路由不出现。

用量响应：

```json
{
  "summary": { "requests": 12, "failed": 1, "raw_tokens": 480, "final_tokens": 480, "prompt_tokens": 400, "completion_tokens": 80, "cache_tokens": 0, "credits": "0.0480", "credits_scaled": 480 },
  "has_more": false,
  "retention_days": 7,
  "records": [
    {
      "id": 3, "model": "gpt-4o", "block_id": "block-a", "status": "success",
      "prompt_tokens": 30, "completion_tokens": 10, "cache_tokens": 7,
      "estimated": false, "stream": false,
      "raw_tokens": 40, "multiplier": "1.00", "final_tokens": 40, "credits": "0.0040",
      "error": null, "created_at": 1790000000
    }
  ]
}
```

分页是**游标式**：`before_id` 传上一页最后一条记录的 `id` 即取更早的一页
（0 / 缺省从最新开始），响应里的 `has_more` 说明是否还有更早的记录——
数据持续写入，偏移量分页会漂移，游标不会。日志保留期由后台设置
`log_retention_days`（默认 7 天，1-365 可调）决定，过期由后台任务回收；
响应里的 `retention_days` 就是当前生效值——**汇总与明细都只是这段窗口
内的数据**，前端应据此注明统计口径。

`block_id` 是**这次请求实际命中的上游块**（路由快照）。只写模型名、不带块前缀的
请求会在提供该模型的块之间加权轮询，落点执行时才知道、事后算不出来，所以记账时
记下来。它可能为 `null`：一是本次特性上线**之前的老记录**，二是**选中块之前**
就失败的请求（如模型名解析不出来、余额不足被拦下）。块 id 对密钥持有者不是秘密
——它就是 `块/模型` 调用写法的前缀，`/api/key/models` 本来就给出。
上游密钥、来源 IP、User-Agent 依旧不出这个接口，只在管理员的 `/api/logs` 里。

失败请求同样出现在 `records` 里（`status: "error"`、token 与积分为 0、`error` 为一句脱敏描述：上游错误只给「上游响应异常」或「上游响应异常: 返回 {status}」，不包含上游报错原文；原话与内部细节只在管理员的 `/api/logs` 里）。

---

## 代理接口

入口只认 OpenAI 格式，全部用访问密钥认证（`Authorization: Bearer ak_...`）。

### 指定用哪个块

同一个模型名可以出现在多个块里，这时默认**在所有提供它的块之间加权轮询**。
要指定走某个块，有两种等价写法：

| 方式 | 例子 |
| --- | --- |
| 模型名带块前缀 | `{"model": "block-a/gpt-4o"}` |
| 请求头 | `X-Aequi-Block: block-a`（模型名保持 `gpt-4o`） |

两者都给了且指向不同的块 → **400**，不会猜你想用哪个。

**解析规则是整名优先**：先拿整个字符串去匹配块里的模型，匹配不上才尝试拆前缀。
这样 `anthropic/claude-3` 这种本身带 `/` 的模型名不会被误拆成「块 anthropic 的模型
claude-3」。两级都匹配不上就报 404，**不会静默回退到轮询**——打错一个字就悄悄走
别的块，账目和日志都会说不清。

前缀只是**入口的写法**：发往上游与写进用量/日志的都是剥掉前缀的模型名。
计费倍率取**命中块**里该模型的条目，所以锁定块之后价格随之确定。

调度与授权都按**块粒度**收敛：短名请求只在「该密钥被授权、且提供这个模型」的块
之间轮询。只授权了 `block-a/gpt-4o` 时，短名 `gpt-4o` 也只会走 block-a；
显式指定未授权的块会得到 **403**，而不是悄悄放行。

因此块 id 有格式约束（见「上游块」）：它要当前缀用。

### `GET /v1/models`

供下游客户端（OpenAI SDK 等）列可用模型。返回的是**该密钥实际能调的**模型：
所有**启用中**的块里配置的模型（去重），再按密钥白名单收敛；白名单为空即不限制。

```json
{ "object": "list", "data": [{ "id": "gpt-4o", "object": "model", "created": 1791720000, "owned_by": "aequi" }] }
```

两个要点：

- 列表含**两种写法**：短名（`gpt-4o`，在该密钥被授权的块之间轮询）与带块前缀的
  全名（`block-a/gpt-4o`，锁定那个块）。短名只在**至少有一个被授权的块**提供它时
  才出现——否则列出来也调不通。
- 停用的块不参与调度，它的模型也就不会出现。
- 没被授权的块不会出现在列表里；白名单里残留的**已从所有块删掉**的名字也不会
  ——那种名字本来就调不到。所以这里的列表始终是「块里有的 ∩ 白名单允许的」。

上游是否提供列模型接口与本接口无关：这里的列表来自**块配置**，
不是每次去问上游（那既慢又不稳）。块里的模型可以用
`POST /api/blocks/{id}/models/fetch` 从上游拉取，也可以手动添加。

### `POST /v1/chat/completions`

对外协议**只有 OpenAI 一种**；上游格式由所在块的 `api_format` 决定，内部转换。

**流式**：请求带 `"stream": true` 时返回 `text/event-stream`，逐字转发上游增量——
chunk 结构与 OpenAI 完全一致（`chat.completion.chunk`），流末尾依次是 finish_reason chunk、
只含 `usage` 的 chunk（choices 为空数组）与 `data: [DONE]`。三家上游均原生支持流式
（含工具调用分片）；上游不报用量时按聚合文本估算计费。流建立之前上游出错会换密钥重试
（最多 3 次）；流开始后的中途错误以内联 `error` 帧发出并以 `[DONE]` 收尾。

**推理（思考）内容**：思考模型的推理过程按 OpenAI 兼容生态惯例走
`delta.reasoning_content`（非流式在 `message.reasoning_content`），与 `content` 分开——
客户端可据此折叠展示。三家上游的思考通道都会汇入该字段：OpenAI 兼容上游的
`delta.reasoning_content`/`delta.reasoning`、Anthropic 的 `thinking` 块与
`thinking_delta`、Gemini 带 `"thought": true` 标志的 part。思考 token 计入输出用量
参与计费（上游 usage 未报时，估算同样包含推理文本）。

**思考开关与强度**：请求体可带 OpenAI 官方的 `reasoning_effort`（也认 Responses 风格的
`reasoning.effort`），取值 `none` / `minimal` / `low` / `medium` / `high` / `xhigh`；
两个来源都给了且取值不一致 → **400**。网关按各家官方档位换算：

| `reasoning_effort` | `openai` 块 | `anthropic` 块 | `google` 块 |
|---|---|---|---|
| 未提交 | 不写 | 不写 | 不写 |
| `none` | `reasoning_effort: "none"` | `thinking: {type:"disabled"}` | `thinkingLevel: "MINIMAL"` ⚠️ |
| `minimal` | 同名转发 | `output_config.effort: "low"` | `thinkingLevel: "MINIMAL"` |
| `low` / `medium` / `high` | 同名转发 | 同名 `low` / `medium` / `high` | `LOW` / `MEDIUM` / `HIGH` |
| `xhigh` | 同名转发 | `"xhigh"` | `HIGH`（Gemini 无此档，封顶） |

- **客户端不能指定思考预算**：`budget_tokens`、`thinkingBudget`、`enable_thinking` 一律不解析，
  预算以各家官方为准。Anthropic 的手写预算在新模型上已废弃，且要求「≥1024 且小于
  `max_tokens`」；Gemini 3 的 `thinkingBudget` 也已废弃，官方明确会产生间歇性 400。
- **不提交就不添加**：没给 `reasoning_effort` 时，三家的请求体里都不会出现任何思考字段。
- Anthropic 侧非关闭档只写 `output_config.effort`，**不写 `thinking`**：思考由模型按复杂度
  自决（官方说明低 effort 下本就会跳过思考），避免被手写预算的硬约束顶掉请求。
- ⚠️ **Gemini 没有真正关闭思考的档位**：`none` 只能降级为 `MINIMAL`（官方原话是它
  「与大多数查询的不思考设置相当」，**不保证关闭**），网关会记一条告警日志。
- Gemini 的思考档位走 `thinkingLevel`，**请给 `google` 块配 Gemini 3 及以后的模型**；
  2.5 及更早只认已废弃的 `thinkingBudget`。网关同时会带 `includeThoughts: true`
  ——不写这个字段，思考内容不会随响应返回，出口的 `reasoning_content` 就是空的。

**输出上限**：`max_tokens` 与 `max_completion_tokens` 都接受，**并记住用的是哪个**——
`o` 系与 gpt-5 只认 `max_completion_tokens`，所以 `openai` 块按**原字段名**发往上游，
不会把新名字改写成已 deprecated 的 `max_tokens`。两个都给了且**取值不同 → 400**；
取值相同则按 `max_completion_tokens` 走。`anthropic` 块写 `max_tokens`（上游必填，
缺省兜 4096），`google` 块写 `maxOutputTokens`。

注意三家的输出上限都是**「思考 + 正文」共用一个池子**（官方对
`max_completion_tokens` 的定义即「含可见输出与推理 token」）：用高强度思考档时，
按正文长度设的上限会被思考吃掉一部分，需要自己给足。

**转换调整会留痕**：网关有若干处会替你改设置——档位取不到就近降级、超出值域封顶、
入口不认的非标字段直接忽略。这些**都不静默**，统一以结构化告警落日志：
`code` + 字段路径 + 模型名，可按 `code` 直接筛。

被忽略的非标字段（逐个对应各家的真实拼写）：`thinking`、`enable_thinking`、`think`、
`chat_template_kwargs`、`thinking_budget`、`thinking_token_budget`、`include_reasoning`，
以及 `reasoning` 里除 `effort` 以外的键（`enabled` / `max_tokens` / `exclude`）。

| `code` | 含义 |
|---|---|
| `effort_adjusted` | 档位被就近取到别的档（Anthropic 没有 `minimal`，落到 `low`——**比请求的更强**） |
| `effort_capped` | 档位超出上游值域被封顶（Gemini 没有 `xhigh`，顶到 `HIGH`） |
| `thinking_disable_unsupported` | 上游没有关闭思考的能力，降级为最省档（Gemini 的 `none` → `MINIMAL`） |
| `reasoning_field_ignored` | 入口不认该字段，已忽略 |

「不提交就不添加」说的是**不替你加**；而一旦你提交了、网关却用不上，就一定有一条记录
——否则「设了 `high` 却只有 `medium` 的效果」这类问题在日志里查不出来。

### `POST /v1/images/generations`

OpenAI 生图协议，与对话共用认证、调度、重试与双轨日志。

- 请求体：`{ model, prompt, n?, size?, quality?, ... }`——除 `model` 会被替换成块内配置的转发名外，**原样透传**。
- 响应：上游响应原样返回（`{ created, data: [{ url | b64_json }] }`）。
- 上游限制：目前只有 `openai` 格式的块支持生图；请求被路由到 `google` / `anthropic` 块时返回 400。
- 计费：gpt-image-1 等新一代接口自带 `usage`，按 token 计费；DALL-E 系列不报 usage，按每张 5000 token 本地估算（日志带 `estimated: true`）。

### 调度与重试行为（两个代理接口共用）

1. 按模型在启用的块中做平滑加权轮询（权重 + 顺序）。
2. 块内按权重选密钥；退避中、暂停、停用的密钥自动跳过。
3. 进入块内并发闸门：达到该块 `max_concurrency` 上限时**排队**（上限 30 秒，超时返回 502 且不重试）。
4. 429 → 该密钥指数退避（30 秒起步每次翻倍、480 秒封顶）并换下一把，**不停用**；401/403 → 记硬失败，连续 3 次自动停用（`never_disable` 的密钥除外）；5xx、网络故障、200 但响应结构损坏 → 指数退避并换下一把，**不停用**；以上可恢复错误合计最多换 3 次密钥。
5. 其它 4xx 是请求本身的问题（参数、模型名等），直接透传错误，不重试。
6. 上游不报 usage 时按本地估算计费（CJK 每字 1 token、其余每 4 字符 1 token、图片每张 1000），估算结果在日志里带 `estimated: true`。
7. 计费按对外模型名的倍率折算；缓存命中的 token **计入输入、按原价结算**（不做折扣），同时在用量里单独记一份便于算命中率。三家上游的口径已归一，两处易漏的输入都要自己补：**其一**，Anthropic 的 `input_tokens` 不含缓存，适配器会补上 `cache_read_input_tokens + cache_creation_input_tokens`；**其二**，Google 的 `toolUsePromptTokenCount`（工具执行结果回灌给模型的输入）不在 `promptTokenCount` 里——官方 `totalTokenCount` 是 `prompt + candidates + toolUsePrompt + thoughts` 四项相加，故适配器把它并入输入（OpenAI 的工具结果本就在 `messages` 里、Anthropic 的 `tool_result` 本就在 `input_tokens` 里，三家口径一致）。用量里的 `cache_tokens` 只记**读取命中**的部分——缓存**写入**（Anthropic 的 `cache_creation_input_tokens`、本次请求首次建缓存）要下次请求才可能被读到，本次不属于命中，因此计入输入但不计入 `cache_tokens`。余额为零的密钥在发请求前就被拒绝（402）——**无限额度的密钥除外**，它跳过余额判定但照常记账。

---

## 面板

`/web/*` 为管理面板静态资源（SPA，未匹配文件回落 `index.html`）；未构建前端时显示「控制台尚未构建」提示页。
构建顺序：先在面板源码仓库执行 `npm install && npm run build`，产物输出到后端 `web/` 目录。
