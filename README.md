# Aequi

带积分计费的 OpenAI 兼容代理网关。上游按「块」组织调度，密钥挂积分对外提供服务。

## 特性

- **OpenAI 兼容入口**：`/v1/chat/completions`（对话）与 `/v1/images/generations`（生图），多模态与工具调用支持
- **多格式上游**：每个块绑定一种上游格式（OpenAI / Google / Anthropic），内部经标准模型双向转换，新增格式只需加一个 adapter
- **平滑加权轮询**：按模型选块、块内选密钥，权重 + 顺序打散流量
- **密钥生命周期自治**：成功清零失败计数、429 自动退避 30 秒、连续失败 3 次自动停用；可暂停 / 恢复 / 测试（测试不耗 token）
- **积分计费**：1 积分 = 10000 最小单位 = 10000 token，按模型倍率（万分比）折算，扣费与记账同一事务，原子 UPDATE 防透支
- **精确埋点**：输入 / 输出 / 缓存 token 分开记录，上游不报用量时按本地启发式估算（带 `estimated` 标记）
- **双轨日志**：管理员看全部细节（块、上游密钥、IP、UA、上游原话），密钥侧只看计费数字与脱敏错误——两张表物理分开；默认保留 7 天，可配置
- **管理面板**：内嵌前端（rust-embed），账号密码 + TOTP 登录、设备管理、上游与密钥管理、请求日志

## 构建

面板源码在项目外的 `../dev-web`（React 19 + Vite），构建产物输出到后端 `web/` 目录：

```bash
cd ../dev-web && npm install && npm run build
cd ../Aequi && cargo build --release
```

跳过前端构建也能编译（build.rs 会兜底创建空的 `web/` 目录，运行时显示「控制台尚未构建」提示页）。

## 运行

```bash
# 无参启动服务（默认监听 0.0.0.0:8080）
./target/release/aequi

# 首次启动通过环境变量初始化管理员账号（仅数据库不存在时生效）
AEQUI_ADMIN_USERNAME=admin AEQUI_ADMIN_PASSWORD=改成随机值 ./target/release/aequi

# 将密码重置为新的随机值（打印一次）
./target/release/aequi reset-password
```

- 没有配置文件：后台设置存 SQLite 的 `settings` 表，首次启动导入默认值，之后以数据库为准（面板可改）
- 数据库默认在工作目录：`aequi.db`（控制面）与 `aequi-data.db`（数据面），可用环境变量覆盖

### 环境变量

| 变量 | 说明 |
| --- | --- |
| `AEQUI_DATABASE_PATH` | 控制面数据库路径（账号、会话、设置） |
| `AEQUI_DATA_DATABASE_PATH` | 数据面数据库路径（密钥、用量、上游、日志） |
| `AEQUI_ADMIN_USERNAME` / `AEQUI_ADMIN_PASSWORD` | 首次启动的管理员账号，仅在控制面数据库不存在时读取 |

## Docker

```bash
docker run -d -p 8080:8080 -v aequi-data:/data \
  -e AEQUI_ADMIN_USERNAME=admin -e AEQUI_ADMIN_PASSWORD=改成随机值 \
  ghcr.io/<owner>/aequi:latest
```

镜像基于 alpine（无头精简系统）+ musl 静态二进制，约 20 MB；非 root 用户运行，`/data` 挂卷即持久化，带健康检查。镜像由 `.github/workflows/docker.yml` 在推 tag 时构建发布（amd64 / arm64）。

## API

完整接口文档见面板源码仓库的 [`dev-web/API.md`](../dev-web/API.md)：38 个端点的请求/响应结构、认证约定、错误格式。

## 测试与 CI

```bash
cargo test
```

- `ci.yml`：每次推送与 PR 跑 `cargo check --all-targets --locked` + `cargo test --locked`
- `rust_release.yml`：推 tag 时构建四个架构的二进制并发布 GitHub Release
- `docker.yml`：推 tag 时构建并发布多架构镜像到 GHCR
- 三条流水线都有测试门：构建前必须先通过测试

## 已知边界

- 流式对话（`stream: true`）对三家上游均已支持（SSE 逐字转发、usage 聚合计费）；生图接口不支持流式
- 上游块目前单 URL（多 URL 轮询未实现）
