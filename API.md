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

**余额不足**：结算（请求结束时按实际用量扣）发现余额小于本次消耗时，
该笔消费**整笔不扣**并返回 402「积分不足」，同时**剩余余额清零**——
否则余额永远停在正数、入口检查形同虚设，任何单次消耗大于余额的密钥
都能无限免费调用。清零后必须充值才能再次发起请求。

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

块是代理的调度单位：**一个块只允许一种 API 格式**，内含一组上游密钥。

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| `GET` | `/api/blocks` | 列表（每块带 `key_count` / `available_keys`） |
| `POST` | `/api/blocks` | 创建 |
| `GET` | `/api/blocks/{id}` | 详情 |
| `PATCH` | `/api/blocks/{id}` | 更新（可含 `api_format`；`proxy` 传 `null` 表示清空） |
| `DELETE` | `/api/blocks/{id}` | 删除（密钥级联清理） |
| `GET` | `/api/blocks/{id}/keys` | 块内密钥列表 |
| `POST` | `/api/blocks/{id}/keys` | 批量添加：`{ secrets: ["..."], weight? }`；重复条目自动跳过，返回 `{ keys, skipped }` |
| `POST` | `/api/blocks/{id}/keys/remove` | 批量删除：`{ ids }` |
| `POST` | `/api/blocks/{id}/keys/pause` | 批量暂停：`{ ids }` |
| `POST` | `/api/blocks/{id}/keys/resume` | 批量恢复：`{ ids }`（同时清零失败计数） |
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
  "base_url": "https://api.openai.com",
  "proxy": "http://127.0.0.1:7890",
  "models": [
    { "name": "gpt-4o", "forward": "gpt-4o-2024-11-20", "multiplier_bp": 15000 },
    { "name": "gpt-4o-mini" }
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
  **`multiplier_bp` 是该模型的计费倍率（万分比整数，10000 = 1.0 倍）**，缺省 1.0。
- 密钥生命周期自治：成功清零失败计数、429 退避 30 秒、连续失败 3 次自动停用。

#### `POST /api/blocks/{id}/test`

逐把测试块内**全部**密钥，串行执行（并发打上游容易被限流）。
结果与单把测试同口径：有效则清零失败计数，无效则自动停用。

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

`UpstreamKeyView`：`id`、`block_id`、`secret`（明文）、`status`（`active`/`paused`/`disabled`）、`weight`、`failure_count`、`cooldown_until`、`last_error`、`last_used_at`、`created_at`。

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

用量响应：

```json
{
  "summary": { "requests": 12, "failed": 1, "raw_tokens": 480, "final_tokens": 480, "prompt_tokens": 400, "completion_tokens": 80, "cache_tokens": 0, "credits": "0.0480", "credits_scaled": 480 },
  "has_more": false,
  "records": [
    {
      "id": 3, "model": "gpt-4o", "status": "success",
      "prompt_tokens": 30, "completion_tokens": 10, "cache_tokens": 7,
      "estimated": false, "stream": false,
      "raw_tokens": 40, "multiplier": "1.0000", "final_tokens": 40, "credits": "0.0040",
      "error": null, "created_at": 1790000000
    }
  ]
}
```

分页是**游标式**：`before_id` 传上一页最后一条记录的 `id` 即取更早的一页
（0 / 缺省从最新开始），响应里的 `has_more` 说明是否还有更早的记录——
数据持续写入，偏移量分页会漂移，游标不会。日志保留期由后台设置
`log_retention_days`（默认 7 天，1-365 可调）决定，过期由后台任务回收。

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

### `POST /v1/images/generations`

OpenAI 生图协议，与对话共用认证、调度、重试与双轨日志。

- 请求体：`{ model, prompt, n?, size?, quality?, ... }`——除 `model` 会被替换成块内配置的转发名外，**原样透传**。
- 响应：上游响应原样返回（`{ created, data: [{ url | b64_json }] }`）。
- 上游限制：目前只有 `openai` 格式的块支持生图；请求被路由到 `google` / `anthropic` 块时返回 400。
- 计费：gpt-image-1 等新一代接口自带 `usage`，按 token 计费；DALL-E 系列不报 usage，按每张 5000 token 本地估算（日志带 `estimated: true`）。

### 调度与重试行为（两个代理接口共用）

1. 按模型在启用的块中做平滑加权轮询（权重 + 顺序）。
2. 块内按权重选密钥；退避中、暂停、停用的密钥自动跳过。
3. 429 → 该密钥退避 30 秒并换下一把；401/403/5xx、网络故障、200 但响应结构损坏 → 记失败并换下一把；最多 3 次。
4. 其它 4xx 是请求本身的问题（参数、模型名等），直接透传错误，不重试。
5. 上游不报 usage 时按本地估算计费（CJK 每字 1 token、其余每 4 字符 1 token、图片每张 1000），估算结果在日志里带 `estimated: true`。
6. 计费按对外模型名的倍率折算，缓存 token 只记录不打折；余额为零的密钥在发请求前就被拒绝（402）——**无限额度的密钥除外**，它跳过余额判定但照常记账。

---

## 面板

`/web/*` 为管理面板静态资源（SPA，未匹配文件回落 `index.html`）；未构建前端时显示「控制台尚未构建」提示页。
构建顺序：先在面板源码仓库执行 `npm install && npm run build`，产物输出到后端 `web/` 目录。
