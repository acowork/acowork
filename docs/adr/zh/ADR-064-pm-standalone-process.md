# ADR-064: PM 从 Gateway 解耦为独立进程

**状态**：已决策（2026-09-02，架构评审定案）
**日期**：2026-09-02
**决策者**：架构评审（用户定案：Gateway 零业务铁律）
**关联**：
- [ADR-019](./ADR-019-lsp-relay-standalone-process.md)（LSP Relay 独立进程——本 ADR 的直接先例）
- [ADR-055](./ADR-055-remote-runtime-node-topology.md)（Gateway 收敛为纯网络职责）
- [ADR-061](./ADR-061-pm-storage-tree.md)（PM 目录树存储）
- [docs/design/zh/21-pm-project-management.md](../../design/zh/21-pm-project-management.md)（PM 设计 v1.0，本 ADR 推翻其 D-10"内嵌"决策）
- [docs/plan/zh/pm-dev-plan.md](../../plan/zh/pm-dev-plan.md)（PM 开发计划 v0.3，本 ADR 恢复其 P0"独立子进程"设计）

---

## 背景

### 铁律：Gateway 零业务

Gateway 是整个项目的核心单点。其定位是**纯通信 + 全局资源管理**，不承载任何业务逻辑：

> Gateway 不代理 Agent 的业务逻辑（不代理 LLM 调用、不代理工具执行），只负责必须集中化的协调工作。
> —— [docs/design/zh/04-gateway.md](../../design/zh/04-gateway.md)

ADR-055 进一步收敛：**Gateway 只保留三个纯网络职责——MQTT broker 宿主、HTTP 统一入口、全局资源权威**。embed、LSP relay 均已按此原则独立为子进程（ADR-019）。

### 问题：PM 内嵌违反铁律

PM 设计 v1.0（D-10）把 `acowork-pm` **内嵌进 Gateway 进程**（`nest_service("/api/pm")` 挂载），导致：

| 问题 | 说明 |
|------|------|
| **业务逻辑入 Gateway** | PM 领域逻辑（状态机、依赖图、附件、审核流）编译进 Gateway 二进制，违反"Gateway 零业务"铁律 |
| **依赖重量** | `acowork-pm` 给 Gateway 带入 axum、tower、reqwest、chrono、uuid、indexmap、directories、toml、mime_guess、sha2、hex、image（可选）等依赖（[gateway/Cargo.toml:26](../../../core/acowork-gateway/Cargo.toml#L26)） |
| **故障隔离丢失** | PM panic 可能带崩 Gateway（单点核心进程） |
| **与主导方向矛盾** | ADR-019/055 明确"Gateway 收敛为纯网络职责"，PM 内嵌是往回走 |
| **X-Actor 可伪造** | 当前 X-Actor 由客户端自报（[tasks.rs:166](../../../core/acowork-pm/src/api/tasks.rs#L166) 直接读 header），无 Gateway 注入，身份可伪造 |
| **存储耦合** | PM 数据被 `prepare_pm_data_dir` 强制塞进 `{gateway.data_dir}/acowork-pm`（[config.rs:537](../../../core/acowork-gateway/src/config.rs#L537)），且 `PmConfig::default_data_dir()` 用 `directories::ProjectDirs`（Windows 解析到 `%APPDATA%\com\acowork\pm`），与 `acowork-gateway/`、`acowork-node/` 的 `.acowork/` 平级布局不一致——PM 数据生命周期与 Gateway 数据目录强耦合 |

### 与 ADR-019（LSP）的异同

PM 与 LSP 的**解耦动机不同**（LSP 因阻塞 runtime / 资源竞争被解耦；PM 无此问题），但**架构原则相同**：业务逻辑不得进入 Gateway。本 ADR 依据的是原则而非 LSP 的具体动机。

---

## 目标

1. **Gateway 彻底退出 PM 数据路径**：不再编译 PM 代码、不再 `nest_service` 挂载、不再持有 `PmService` 句柄
2. **PM 作为独立进程**：独立二进制 `acowork-pm`、独立端口、独立生命周期
3. **PM 存储独立于 Gateway**：数据目录 `$HOME/.acowork/acowork-pm/`，与 `acowork-gateway/`、`acowork-node/` **平级**（参考 [acowork-core `default_node_home`](../../../core/acowork-core/src/node.rs#L287)），不再嵌套在 Gateway 数据目录下
4. **Gateway 仅保留**：spawn / monitor / restart（复用 `acowork-core::supervisor`）+ 反向代理 `/api/pm/*` + 注入可信身份（X-Actor / X-MCP-Actor）
5. **对外契约不变**：Desktop 仍走 `{gw}/api/pm/*`；远程 Agent 仍走 `http://{advertise_host}:{gw_http_port}/api/pm/mcp`——两端均无感知
6. **安全改进**：X-Actor / X-MCP-Actor 由 Gateway 反代时注入，杜绝客户端伪造

---

## 可选方案

### 方案 A：独立进程（acowork-pm 独立二进制）— 推荐

**原理**：`acowork-pm` crate 增加 `src/main.rs` 产出独立可执行文件，serve 完整 router（REST + MCP + `/health`）。Gateway 通过 supervisor 模式管理其生命周期（spawn / monitor / restart），与 embed / LSP relay 完全一致。Gateway 反向代理 `/api/pm/*` 到 PM 端口。

```
┌──────────────────────────────────────────────────────────────┐
│                    Gateway (精简: 通信 + 全局资源)              │
│                                                              │
│  ┌─────────────┐  ┌──────────┐  ┌─────────────────────────┐  │
│  │ MQTT broker │  │ HTTP 入口 │  │ PM Supervisor           │  │
│  │ (rumqttd)   │  │ 统一反代  │  │ spawn/monitor/restart   │  │
│  ├─────────────┤  ├──────────┤  └──────────┬──────────────┘  │
│  │ 全局资源    │  │ X-Actor  │             │ SSE heartbeat    │
│  │ 权威        │  │ 注入     │             ▼                  │
│  └─────────────┘  └──────────┘   /api/pm/* → 127.0.0.1:{port} │
└──────────────────────────────────────────────────────────────┘
                    │ spawn + SSE heartbeat + 指数退避重启
                    ▼
┌──────────────────────────────────────────────────────────────┐
│              acowork-pm (独立进程, 端口 18082)                 │
│                                                              │
│  ┌──────────────┐  ┌──────────────┐  ┌────────────────────┐  │
│  │ REST API     │  │ MCP HTTP     │  │ /health            │  │
│  │ /projects    │  │ /mcp         │  │ (supervisor 探活)   │  │
│  │ /tasks       │  │ (JSON-RPC)   │  └────────────────────┘  │
│  │ /attachments │  └──────────────┘                          │
│  └──────────────┘                                            │
│  存储: {data}/acowork-pm/ (目录树, PM 独占)                    │
└──────────────────────────────────────────────────────────────┘
```

**优点**：
- 完全隔离：PM 崩溃不影响 Gateway，Gateway 崩溃后 PM 经 supervisor 超时自退出（复用 ADR-018 模式）
- 复用成熟模式：`acowork-core::supervisor`（ADR-019 抽取）+ `http/proxy.rs` 反向代理（ADR-033 已有 Runtime 反代先例）
- 依赖解耦：Gateway 二进制不再包含 PM 代码
- 身份可信：Gateway 反代时注入 X-Actor / X-MCP-Actor，修复伪造漏洞

**缺点**：
- 需实现 supervisor 生命周期（复用现有构建块，成本低）
- 需实现反向代理（复用现有 `http/proxy.rs` 模式，成本低）
- AgentDirectory 需从"共享 state"改为 HTTP 查询（见 §迁移 Phase 3）

### 方案 B：保持内嵌（现状）

**原理**：维持 `nest_service("/api/pm")` 挂载。

**优点**：零改动。
**缺点**：违反铁律；业务逻辑入 Gateway；故障隔离丢失；X-Actor 可伪造。**否决**。

### 方案 C：独立 crate，仍在 Gateway 进程内

**原理**：PM 已是独立 crate，但仍在 Gateway 进程中以 library 运行。

**优点**：代码隔离。
**缺点**：不解决业务逻辑入 Gateway、故障隔离、依赖重量问题。**否决**（与 ADR-019 否决理由一致）。

---

## 决策

**采用方案 A：PM 独立进程。**

- 恢复开发计划 v0.3 的 P0 设计（独立子进程 + supervisor + 端口分配），推翻设计 v1.0 的 D-10"内嵌"决策
- 与 ADR-019（LSP Relay）、ADR-055（Gateway 收敛）方向一致
- 复用 `acowork-core::supervisor` 与 `http/proxy.rs` 既有基础设施，迁移成本可控

---

## 影响范围

### Phase 0 — acowork-pm 独立可执行（PM 侧）

| 文件 | 变更 |
|------|------|
| `core/acowork-pm/src/main.rs` | **新增**：独立二进制入口，加载 `PmConfig`，serve 完整 router（REST + MCP + `/health`） |
| `core/acowork-pm/src/server.rs` | `start_dev` 从 P0 占位补全为完整 serve（当前只 serve `/health`，[server.rs:91](../../../core/acowork-pm/src/server.rs#L91)） |
| `core/acowork-pm/src/config.rs` | `PmConfig` 增加 `port`（默认 18082）、`enabled`（默认 true）；端口冲突自动递增（恢复计划 v0.3 T0-5） |
| `core/acowork-pm/src/config.rs` | **`default_data_dir()` 改为 `$HOME/.acowork/acowork-pm/`**（镜像 [`acowork-core` `default_node_home`](../../../core/acowork-core/src/node.rs#L287) 模式：`ACOWORK_PM_HOME` env > `$HOME/.acowork/acowork-pm` > `./.acowork-pm`），**替换当前 `directories::ProjectDirs`**（解析到 `%APPDATA%\com\acowork\pm`，与 `.acowork/` 布局不一致） |
| `core/acowork-pm/src/health.rs` | **新增**：`/health` 端点（supervisor 探活契约，复用 `acowork-core::health`） |
| `core/acowork-pm/Cargo.toml` | 增加 `[[bin]]` target；`acowork-core` 依赖（supervisor/health 契约） |

**目标数据布局**（与 acowork-gateway / acowork-node 平级）：

```
$HOME/.acowork/
├── acowork-gateway/     # Gateway 数据（vault, packages, data/）
├── acowork-node/        # Node Agent 数据（identity, packages, logs）
└── acowork-pm/          # PM 数据（projects/, .trash/, logs/）← 独立，平级
```

### Phase 1 — Gateway 反向代理替换 nest_service（Gateway 侧）

| 文件 | 变更 |
|------|------|
| `core/acowork-gateway/src/http/pm_api.rs` | 删除 `pm_routes()`（`nest_service` 挂载）；`GatewayAgentDirectory` 删除（不再共享 state） |
| `core/acowork-gateway/src/http/pm_proxy.rs` | **新增**：`/api/pm/*` → `http://127.0.0.1:{pm_port}/*` 反向代理（复用 `http/proxy.rs` 模式，ADR-033） |
| `core/acowork-gateway/src/http/routes.rs` | `build_router_with_pm` 改为挂载 `pm_proxy` 路由；删除 `nest_service` |
| `core/acowork-gateway/src/http/server.rs` | 删除读取 `pm_service` 句柄逻辑 |
| `core/acowork-gateway/src/gateway/state.rs` | `pm_service: Option<Arc<PmService>>` 删除，改为 `pm_process`（supervisor 状态）；`pm_mcp_url` 保留（advertise endpoint 构造） |
| `core/acowork-gateway/src/gateway/mod.rs` | PM 启动改为 supervisor spawn（非致命）；删除 `PmService::with_agent_directory` 调用 |
| `core/acowork-gateway/src/config.rs` | `[pm]` 段增加 `port` / `enabled`；**删除 `prepare_pm_data_dir`**（不再把 PM 数据塞进 `{gateway.data_dir}/acowork-pm`，PM 数据目录由 PM 自身独立解析） |
| `core/acowork-gateway/Cargo.toml` | **删除 `acowork-pm` 依赖**（Gateway 不再编译 PM 代码） |

### Phase 2 — supervisor 生命周期（Gateway 侧）

| 文件 | 变更 |
|------|------|
| `core/acowork-gateway/src/lifecycle/pm_supervisor.rs` | **新增**：复用 `acowork-core::supervisor`（RestartHistory / 指数退避 / SSE heartbeat / startup grace window），spawn / monitor / restart PM 子进程 |
| `core/acowork-gateway/src/lifecycle/mod.rs` | 注册 `pm_supervisor` |
| `core/acowork-gateway/src/gateway/mod.rs` | 启动时序：spawn PM → 等 `/health` ready → 挂载反代路由 |

### Phase 3 — AgentDirectory 解耦（身份链路）

| 文件 | 变更 |
|------|------|
| `core/acowork-pm/src/mcp/agent_dir.rs` | **新增**：`AgentDirectory` 的 HTTP 实现——`pm_create_task` 校验 assignee 时调 Gateway `/api/agents`（恢复计划 v0.3 T1-11"即时校验兜底"）；启动拉全量 + 周期刷新 |
| `core/acowork-gateway/src/http/pm_proxy.rs` | 反代时注入 `X-Actor`（Desktop 会话用户 / Agent 身份）与 `X-MCP-Actor`（替换 `{agent_id}` 模板）——**安全改进，修复客户端伪造** |
| `core/acowork-gateway/src/mqtt/global_resources_builders.rs` | `pm_mcp_url` 注入逻辑保留（advertise endpoint 不变，远程 Runtime 无感知） |

### Phase 4 — 清理 + 文档 + 验证

| 文件 | 变更 |
|------|------|
| `docs/design/zh/21-pm-project-management.md` | §2.1/§2.3/§8/§12 D-10 更新为"独立进程"；§10.6 监督改为 supervisor |
| `docs/adr/zh/ADR-061-pm-storage-tree.md` | 补充"单进程假设"说明（PM 单实例仍成立，无需加锁） |
| `core/acowork-pm/README.md` | 更新"PM 服务不独立运行"描述 |
| `core/acowork-pm/schemas/README.md` | 更新 Base URL 说明（独立端口 + Gateway 反代） |
| `docs/review/pm-implementation-review.md` | 更新架构风险条目 |

---

## 迁移计划（执行顺序）

1. **Phase 0**：PM 独立可执行 + `/health` + 端口分配 + **数据目录独立解析**（`$HOME/.acowork/acowork-pm/`）→ `cargo run -p acowork-pm` 可独立 serve 全量路由
2. **Phase 1**：Gateway 反代替换 `nest_service` + **删除 `prepare_pm_data_dir`**（先保留 PM 内嵌 + 反代并存，灰度验证）
3. **Phase 2**：supervisor 生命周期（PM 崩溃自动重启）
4. **Phase 3**：AgentDirectory HTTP 化 + X-Actor 注入（安全改进）
5. **Phase 4**：删除 Gateway 对 `acowork-pm` 的依赖 + 文档收口 + 端到端验证

## 数据目录（存储独立）

PM 数据目录直接定为 `$HOME/.acowork/acowork-pm/`（平级）。

> **无迁移 / 无兼容性要求**：项目处于开发期，无存量数据。`PmConfig::default_data_dir()` 直接改为新路径即可，**不实现**旧路径检测 / 文件移动 / 迁移标记等逻辑（YAGNI）。

## 回滚

- **快速回退**：保留 `nest_service` 路径（git 历史），若独立进程引入问题，可回退到内嵌（仅需恢复 `pm_api.rs` + `build_router_with_pm` + `prepare_pm_data_dir`）
- **灰度**：Phase 1 反代与内嵌并存期间，可随时切换
- **数据**：开发期无存量数据，回退无数据影响

## 验收标准

| # | 验收项 |
|---|--------|
| 1 | `cargo tree -p acowork-gateway` 不再包含 `acowork-pm`（Gateway 零业务铁律达成） |
| 2 | `acowork-pm` 可独立启动/停止/重启，serve 全量 REST + MCP + `/health` |
| 3 | PM 数据目录为 `$HOME/.acowork/acowork-pm/`（与 `acowork-gateway/`、`acowork-node/` 平级），Gateway 数据目录下无 `acowork-pm` |
| 4 | PM 进程被 kill → Gateway 自动重启 PM，Gateway 自身不受影响（supervisor 验证） |
| 5 | Desktop 全流程（建项目/任务/看板/审核/附件）经 Gateway 反代无感知 |
| 6 | 远程 Agent 经 advertise endpoint 调 pm MCP 无感知（`X-MCP-Actor` 由 Gateway 注入） |
| 7 | 伪造 `X-Actor` 的请求被 Gateway 覆盖为可信身份（安全改进验证） |
| 8 | `cargo test -p acowork-pm` 全绿；Gateway 测试全绿 |

---

## 决策记录

| 决策点 | 结论 |
|--------|------|
| 部署形态 | **独立进程**（推翻设计 v1.0 D-10"内嵌"） |
| 独立端口 | 默认 18082，冲突自动递增（恢复计划 v0.3 T0-5） |
| 生命周期 | Gateway supervisor（复用 `acowork-core::supervisor`，与 embed/LSP relay 一致） |
| **存储目录** | **`$HOME/.acowork/acowork-pm/`，与 `acowork-gateway/`、`acowork-node/` 平级独立**（镜像 [`acowork-core` `default_node_home`](../../../core/acowork-core/src/node.rs#L287) 模式；替换 `directories::ProjectDirs`；删除 Gateway `prepare_pm_data_dir`） |
| 数据迁移 | **无**（开发期无存量数据，YAGNI，不实现迁移逻辑） |
| 对外契约 | Desktop `/api/pm/*` 与远程 `/api/pm/mcp` 均不变（Gateway 反代） |
| 身份 | X-Actor / X-MCP-Actor 由 Gateway 反代注入（修复客户端伪造） |
| AgentDirectory | PM 侧 HTTP 查询 Gateway `/api/agents`（恢复计划 v0.3 T1-11） |
| 存储并发 | PM 单实例独占数据目录，单写者假设仍成立，无需加锁 |
| 回滚 | 保留内嵌路径可快速回退（开发期无数据影响） |
