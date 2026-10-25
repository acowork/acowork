# ADR-101: 记忆模块从 Runtime 剥离为可替换的独立服务 acowork-memory（MCP 接入）

**状态**：已接受（2026-10-23；两个关键分叉——服务化范围=完整迁移、auto-inject=移除——已由用户定案，P1–P5 实施完成，本 ADR 随 P5 文档收口定案）
**日期**：2026-10-23
**决策者**：用户（定案方向：记忆改为 pm/doc 同构的 MCP 服务，官方服务关闭时记忆面板同步隐藏）

**关联**：
- [ADR-064](./ADR-064-pm-standalone-process.md)、[ADR-070](./ADR-070-doc-standalone-process-and-tree-storage.md)、[ADR-084](./ADR-084-user-standalone-process.md)（服务化范式三连——本 ADR 是同一范式的第四次应用）
- [ADR-051](./ADR-051-runtime-memory-provider-decoupling.md)（Runtime 与 MemoryProvider 解耦的前身——本 ADR 将其推进到终态：记忆逻辑整体离开 Runtime，**取代其中"provider 常驻 Runtime 进程"的落地形态**）
- [ADR-082](./ADR-082-memory-storage-sqlite-vector-fts.md)（存储后端 SQLite——数据模型不变，**存储位置从 Agent workspace 迁至服务数据目录**）
- [ADR-068](./ADR-068-memory-layer-promotion-two-axis-orthogonal.md)、[ADR-071](./ADR-071-distiller-runtime-config-and-trigger.md)、[ADR-057](./ADR-057-compaction-distillation-into-graph.md)、[ADR-062](./ADR-062-memory-quality-config-and-retrieval-gate.md)（蒸馏/遗忘/质量流水线——逻辑随服务迁移）
- [ADR-073](./ADR-073-agent-instance-identity-decomposition.md)（`agent_instance_id` 作为服务命名空间键）
- [ADR-099](./ADR-099-chat-session-llm-router.md)（chat_router 的 `memory_recall` 信息工具——工具来源改为 MCP）
- [ADR-081](./ADR-081-global-search.md)（全局搜索 memory 源——改直连服务）
- [ADR-069](./ADR-069-mcp-tool-level-optin.md)、[ADR-072](./ADR-072-mcp-install-package-spec.md)、[ADR-090](./ADR-090-mcp-lsp-install-ownership-node.md)（MCP 工具 opt-in、stdio/HTTP 双传输安装——第三方记忆 MCP 的替换通道）
- [ADR-009](./ADR-009-gateway-workspace-isolation.md)（Gateway 边界红线——本 ADR 变更"Agent 私有记忆"的归属载体，延续"Gateway 不直读"红线）

---

## 1. 决策摘要

### 1.1 一句话

**把记忆的全部逻辑（存储、检索、蒸馏/遗忘后台流水线、面板管理 API）从 Runtime 进程内迁出，落为 Gateway supervisor 管理的独立进程 `acowork-memory`**：对 Agent 只经 **MCP HTTP**（`memory_store` / `memory_recall`，与 pm/doc 同一 catalog 注入通道），对 Desktop 只经 **Gateway 反代 REST**（面板契约字节不变）。由此获得可替换性：`[memory].enabled=false` → 不 spawn → catalog 不注入 `id=memory` 的 MCP → 面板同步隐藏；用户可安装任意第三方记忆 MCP（stdio 或 HTTP）作为替代。

关键认知：记忆与 pm/doc 的不同之处在于它**不止是一个工具面**——历史上它有 auto-inject、compaction 尾写等旁路。这些旁路在 ADR-068 revision 与 auto-inject 默认 OFF（ADR-060 后）之后已经收敛到"只剩工具 + 后台流水线"，本 ADR 把剩余两块全部迁出，Runtime 达到**零记忆业务代码**。

### 1.2 关键决策表（详细理由见 §4）

| # | 决策 | 结论 |
|---|---|---|
| 1 | 剥离边界 | **迁出**：`memory_recall`/`memory_store` 内置工具、distiller/遗忘/consolidation 后台流水线（含 LLM judge、metrics）、面板 HTTP API（`/memory/nodes`、`/memory/stats`、`/memory/distill`、`/memory/consolidation/status`、节点删除、embedding-dim 迁移）、bootstrap manifest 导入（写入入口 D）、全局搜索 memory 源。**移除**：auto-inject（决策 5）；`Permission::MemoryRead` / `Permission::MemoryWrite` 两个权限变体（`acowork-core/src/permission.rs`）连同其 `PermissionTag` 3/4 编码位一并删除——tag **不重编号**，留空档，使其余 tag 的字节值与历史一致。**保留在 Runtime**：仅 catalog 驱动的薄集成（决策 9） |
| 2 | 进程范式 | 复刻 pm/doc/user：`acowork-memory` crate 由纯 lib 改为 **lib + bin**；默认端口 `18084`（18080 embed / 18081 doc / 18082 pm / 18083 user 已占用），冲突自增（max +20）、`--port-file` 上报；`memory_supervisor`（`/health` 轮询 + 指数退避重启）；服务失败不阻塞 Gateway（面板 API 503 + MCP 不注入） |
| 3 | 身份与隔离 | 命名空间键 = `agent_instance_id`（ADR-073：安装时生成的不可变 UUID v4），经 Gateway catalog 的 `X-MCP-Actor: {instance_id}` 模板注入、Runtime 连接时解析。**每实例一库**：`{data_dir}/instances/{instance_id}/memory.db`（沿用 ADR-082 SqliteStore，隔离粒度与现状"每 Agent 一私有库"一致） |
| 4 | 传输选择 | 官方服务 MCP 走 **HTTP（streamable 子集，无状态单 `POST /mcp`）**，端点经 Gateway 反代 `/api/memory/mcp`——只有 HTTP 能同时满足：advertise endpoint 远程 Agent、`X-MCP-Actor`/`X-ACowork-Node-Token` 身份头、面板与工具共用一套服务。**第三方替换不限传输**：stdio 与 HTTP 均由 acowork-mcp 现有能力覆盖（`StdioTransport` + ADR-072 安装器）；开源记忆 MCP 生态以 stdio 为主流形态 |
| 5 | auto-inject 移除 | 每轮 LLM 调用前的"自动检索注入"能力**整体删除**（现状默认 OFF、manifest `[memory.quality].auto_inject_enabled` opt-in）。理由：MCP 协议无"调用前预取注入"的标准钩子，保留它必然要求 Runtime 对特定 server id 特判，与"可插拔"目标矛盾。记忆获取统一为两条主动路径：执行会话的 `mcp_memory__memory_recall` 工具、chat_router 的信息轮（决策 9）。manifest 键废弃（deserial 容忍，无行为） |
| 6 | distiller LLM 凭证 | 服务不再蹭某个 Agent 的 provider。模型候选链与提示词从服务自身配置读取（`{data_dir}/config.toml`）；**credential 解析复用全局资源通道**：服务作为 MQTT 客户端订阅 Gateway broker 的 available providers（同 Runtime 的 pull 语义，ADR-065 生命周期范式）。embedding 调 acowork-embed（`:18080`，同现状 Tier 1 路径） |
| 7 | 面板显隐契约 | Gateway 状态端点暴露 `memory_service: { ready, mcp_injected }`（⇔ `memory_mcp_url.is_some()`）。Desktop 规则：`ready=false` → `RightNavBar` 隐藏 memory tile；当前 activeTab 为 memory 时弹回 status；`GlobalSearchDialog` 隐藏 memory tab。**第三方记忆 MCP 在线时面板保持隐藏**——面板讲的是 acowork-memory 的 REST 契约，不试图适配任意 MCP |
| 8 | 对外契约 | 面板路径 `/api/agents/{id}/memory/*` **与响应 wire shape 字节不变**（Gateway 端从"反代 Runtime"改为"解析 instance → 反代 memory 服务并注入 `?instance_id=`"）；新增服务直通前缀 `/api/memory/*`（MCP + 管理 REST）。Desktop `memoryStore.ts` 因此近零改动 |
| 9 | Runtime 残留集成（唯一例外） | Runtime 检查 catalog 中是否存在 `id=memory` 的 server：存在时 (a) 首次启动把 manifest `[memory]` 声明段 + bootstrap Autobiographical 节点（入口 D）`POST` 到该服务；(b) chat_router 从工具注册表按前缀名解析 memory 召回工具（`mcp_memory__memory_recall`），不存在则该轮不挂载。两者都**不含任何记忆业务逻辑**，第三方 MCP（id 不同）自然不触发 |
| 10 | 数据迁移 | 一次性、**仅 Node 触发**（拥有 node-local fs 权限，ADR-055；Runtime 完全不参与，不设任何迁移上报通道）：守护进程启动后台 pass + `acowork-node migrate-memory` 手动重试，扫已安装实例的 `{workspace}/memory/private.sqlite`。单实例序列：WAL checkpoint 后读库计数 → 0 条记忆只写哨兵不上传 → 否则 `POST {gateway}/api/memory/admin/import?instance_id=`（**原始 `application/x-sqlite3` 字节体**，上限 `STORE_IMPORT_MAX_BYTES`＝64 MiB，超限直接判失败转人工；与 Gateway `GLOBAL_BODY_LIMIT` 刻意相等）→ 服务侧已有记忆则答 `skipped_existing`，**此时本地一行不删**（命名空间非空可能只是 manifest bootstrap 写过身份事实，据此删除会毁掉用户历史的唯一副本）→ 真实导入才 DELETE 本地记忆行（`purge_memory_rows`；文件本身保留，会话元数据与对话索引仍属 Runtime，ADR-082）→ 写 `{workspace}/memory/.migrated` 哨兵记录结果。503 与传输错误按轮次退避重试（6 轮 × 5s，覆盖 Desktop 启动顺序竞态），其余错误记失败、不写哨兵，待人工修正后 `--force` 重试。Gateway 不做任何 fs 读取（红线）。不留双写、不留兼容代码（先例：ADR-084 决策 8） |
| 11 | 打包不变量 | 发布打包现状已"永远排除 `memory/`"（`PACKAGE_ALWAYS_EXCLUDE_DIRS`）。服务化后记忆物理上不在包目录内，"发布不携带 Agent 状态"从排除规则升为**结构性不变量**；`PackageOptions` 中 5 个从未生效的记忆子集字段（`include_episodes` / `include_private_knowledge` / `include_procedural` / `include_autobiographical` / `include_public_knowledge`）随本 ADR 删除——它们对应的读取面在打包器里不存在，`memory/` 早已整目录排除。反序兼容免费：serde 对未知字段默认忽略。"种子记忆随包分发"（服务导出 → 安装导入）明确**out of scope**，另行立项 |
| 12 | 回滚 | `[memory].enabled=false` → 不 spawn → MCP 不注入 → 面板隐藏。Runtime 侧删除不可配置回滚（靠版本回退）。数据面：真实导入的实例本地记忆行已删、服务侧库是唯一副本（回退需从服务侧导出）；`skipped_existing` 与失败尝试原样保留本地行与文件，即天然的未迁移逃生位；哨兵可 `--force` 越过 |

### 1.3 不变量（必须满足）

1. **Runtime 零记忆业务**：`core/acowork-runtime` MUST NOT 含存储/检索/蒸馏/遗忘逻辑；唯一允许的记忆相关代码是决策 9 的 catalog 驱动集成。`acowork-runtime` 的 Cargo 依赖移除 `acowork-memory`（`acowork-sqlite` 保留——会话索引/对话元数据仍在用）。
2. **Gateway 不直读记忆数据**（延续 ADR-009 §5）：面板数据一律经 `memory_proxy` 反代服务 REST；Gateway 进程 MUST NOT 打开 `instances/*.db`。
3. **面板 wire 契约字节不变**：`MemoryNode` / `MemoryStatsResponse` 等响应 shape 字段不增删不改名（迁移前后 Desktop 各消费点无感）。
4. **`instance_id` 是唯一的记忆命名空间键**：MCP 与 REST 两侧同源（`X-MCP-Actor` / 代理层注入），不存在第二个身份轴。服务端 MUST 拒绝缺失/非法 actor 的写请求（匿名只读策略同 pm/doc）。
5. **鉴权闸门**：服务端口绑定 `127.0.0.1`，只接受 Gateway 反代流量（`X-ACowork-Node-Token` 校验，`AUTH_MODE=multi_user` 下必过，ADR-076）。
6. **遗留库交接只有 Node 一个发起方**：`{workspace}/memory/private.sqlite` 是 node-local 的 Agent 私有数据，读它的代码 MUST 位于 `acowork-node`（ADR-055 拥有本机 fs）；Runtime 不参与迁移（记忆已不是它的子系统），Gateway MUST NOT 直读（ADR-009 §5）。交接通道的唯一入口是服务 `/admin/import`。
7. **测试与产品二进制同边界**：记忆引擎与 Runtime 的检索评测/集成测试 MUST 各自位于 `core/acowork-memory/tests/` 与 `core/acowork-runtime/tests/`；一个 harness MUST NOT 播种或打分另一个引擎的 scope，ground truth 全在另一 scope 的查询按跳过处理（计入 `skipped` 列，不进均值）。两侧共用的指标数学、数据集读取、stand-in 投影与报告形状 MUST 位于 `core/acowork-eval`，且只能以 `[dev-dependencies]` 形式被依赖——测量代码不得链接进任何发布二进制，也不得回住 `acowork-core`（那是发布依赖）；`acowork-eval/tests/shipping_redline.rs` 强制这一条。理由：集成测试跨 crate 互相 import 不可达，而在两侧各抄一份指标定义会让"同一次权重变更"被两把不同的尺子量。

### 1.4 部署形态对照

| 维度 | 本 ADR 前 | 本 ADR 后 |
|---|---|---|
| Agent 记忆工具 | 内置工具（进程内） | MCP 工具 `mcp_memory__memory_recall` / `mcp_memory__memory_store` |
| 记忆存储位置 | `{install_path}/workspace/memory/private.sqlite`（node-local） | `{memory.data_dir}/instances/{instance_id}/memory.db`（服务侧，Gateway 主机） |
| distiller/遗忘 | Runtime 内定时器 + Agent provider | 服务内调度器 + 全局资源凭证 |
| 面板数据链路 | Desktop → GW `/api/agents/{id}/memory/*` → 反代 Runtime localhost HTTP | Desktop → GW 同路径 → 反代 acowork-memory REST（注入 instance） |
| 关闭记忆 | 无开关（内置能力） | `[memory].enabled=false`：MCP 消失 + 面板隐藏；可装第三方记忆 MCP 替代 |
| 第三方记忆 MCP 效果 | 与内置工具并存（双通道） | 唯一记忆通道；无面板、无 distiller、无 bootstrap 导入 |

---

## 2. 背景与动机

### 2.1 铁律与既有范式

Gateway/平台收敛方向是"业务出核"：embed、LSP relay（ADR-019）、PM（ADR-064）、doc（ADR-070）、user（ADR-084）依次独立为 Gateway supervisor 托管的进程，Agent 侧统一经 MCP catalog 注入（`build_available_mcps`：`url.is_some()` ⇔ 注入）。**记忆是 Runtime 内最后一块成体系的业务域**（ADR-051 当年只做到 trait 解耦，实现仍编译进 Runtime 二进制）。

### 2.2 现状触点盘点（迁移面）

| # | 触点 | 文件 | 处置 |
|---|---|---|---|
| 1 | 内置工具 | [tools/builtin/memory_recall.rs](../../../core/acowork-runtime/src/tools/builtin/memory_recall.rs)、[memory_store.rs](../../../core/acowork-runtime/src/tools/builtin/memory_store.rs) + `all_builtin_tools` 注册 + [acowork-core BUILTIN_TOOLS](../../../core/acowork-core/src/tools/mod.rs) 两行 | 删除 → 服务 MCP manifest 承接 |
| 2 | auto-inject | [agent/loop_memory.rs](../../../core/acowork-runtime/src/agent/loop_memory.rs)、`loop_context` 调用点、`memory_retrieved_for_session` 状态 | 删除（决策 5） |
| 3 | 后台流水线 | [runtime/src/memory/](../../../core/acowork-runtime/src/memory/)（`consolidation_bg`/`judge_llm`/`llm_adapter`/`metrics`）+ `AgentCore` 的 distiller/forgetting 字段与 `init_memory_provider` | 迁入服务（决策 6） |
| 4 | 面板 HTTP API | [http/memory_query.rs](../../../core/acowork-runtime/src/http/memory_query.rs)、server.rs `/memory/*` handlers、`usecases/memory_query*` | 迁入服务 REST（决策 8） |
| 5 | chat_router 信息工具 | [chat_router/tool.rs](../../../core/acowork-runtime/src/agent/chat_router/tool.rs) 固定名 `memory_recall` | 改按前缀名动态解析（决策 9b） |
| 6 | 全局搜索 memory scope | [http/global_search.rs](../../../core/acowork-runtime/src/http/global_search.rs) | Runtime 端返回 `not_available`；前端 memory tab 直连 `/api/memory/search`（同 doc/project tab 的形态，ADR-081） |
| 7 | 权限表 | [permission.rs](../../../core/acowork-core/src/permission.rs) 的 `MemoryRead`/`MemoryWrite`（`parse`/`tag`/`from_tag_simple`/`to_permission_string`/`matches`/`category`/`type_name`/`type_value`/serde 等全部触点）、[wasi_mapper.rs](../../../core/acowork-runtime/src/tools/wasm/wasi_mapper.rs) 的映射分支、7 个内置示例包 manifest 的 `[[permissions]]` 声明 | 删除。记忆不再有任何权限域：官方服务经 `X-MCP-Actor` + `{node_token}` 自证身份，工具级开关落在 `agent_mcp_tools.json`（ADR-069），不经 permission 体系 |

写入入口现状（[docs/memory-write-entrypoints.md](../../memory-write-entrypoints.md)，ADR-068 revision）已高度收敛：C（compaction 尾写）**已于 2026-09-10 删除**（方法 `write_summary_to_provider` 不存在，仅注释残留），有效入口只剩 A（`memory_store` 工具）、D（manifest bootstrap）、E（管理 API）、F（distiller）——本 ADR 把 A/D/E/F 全部落到服务，Runtime 无写入通道残留。

### 2.3 动机

| 动机 | 说明 |
|---|---|
| 可替换性（核心诉求） | 记忆方案快速演进（mem0、mcp-memory-service、Basic Memory 等开源生态），内置形态下换记忆=改 Runtime 代码；MCP 化后换记忆=关官方服务、装第三方 MCP |
| Runtime 减重 | 记忆链路（memory/sqlite-consolidation/distiller/judge）是 Runtime 二进制的最大业务块之一；出核后 Runtime 收敛为循环 + 工具分发 + provider 适配 |
| 故障隔离 | 蒸馏流水线（LLM 调用、SQLite 迁移）panic 不再威胁 Runtime 会话进程 |
| 与用户诉求闭环 | "关闭 memory → 面板消失"复用 pm/doc 的 `url.is_some()` 开关语义 + 决策 7 的前端门控，无需新机制 |

---

## 3. 协议面

### 3.1 MCP（Agent 面）

单 `POST /mcp`，JSON-RPC 2.0 无状态子集，逐字对齐 [acowork-doc MCP](../../../core/acowork-doc/src/mcp/mod.rs)：`initialize` / `notifications/initialized` / `tools/list` / `tools/call`。身份：`X-MCP-Actor` = instance UUID；`X-ACowork-Node-Token` = 机器凭证。

| 工具 | 参数（要点） | 对应现状 |
|---|---|---|
| `memory_store` | `content`、`category`（fact/preference/relation/procedure）、`confidence`、`keywords`、`normalized` | 与内置工具 schema 对齐；写入仍为 Episodic + subtype，由服务端 distiller 归层（ADR-068） |
| `memory_recall` | `query`、`keywords`、`since`/`until`、`limit` | 向量源 = `query` 文本的服务端 embedding（不再取"当前用户消息"——见 §3.3 行为差异）；BM25 源 = keywords |

`tools/list` 返回的工具名经 wrapper 加前缀（`mcp_memory__memory_recall`），opt-in 沿用 ADR-069 的 `agent_mcp_tools.json` 逐工具开关。

### 3.2 REST（面板/管理面，经 Gateway 反代）

`GET /memory/nodes`（分页/过滤/keyword）、`GET /memory/nodes/{nid}`、`DELETE /memory/nodes/{nid}`、`GET /memory/stats`、`POST /memory/distill`、`GET /memory/consolidation/status`（distiller + forgetting 状态与配置读写）、`POST /memory/search`（全局搜索 memory 源）、`POST /admin/import?instance_id=`（迁移：请求体为原始 SQLite 字节，`application/x-sqlite3`，64 MiB 上限；仅 Node 调用，见决策 10）、`POST /admin/bootstrap`（决策 9a）。公开路径保持 `/api/agents/{id}/memory/*` 的既有形态由 Gateway 映射（决策 8）。

### 3.3 已知行为差异（接受并记录）

1. **同会话节点排除降级**：现状 `memory_recall` 由 `ToolContext.session_id` 自动排除本会话节点；MCP 无运行时上下文，降级为不排除（服务端保留可选 `exclude_session_id` 参数，当前无调用方注入）。后果仅是召回冗余（compaction 摘要已在上下文中），无正确性影响。
2. **向量源变化**：现状"当前用户消息驱动向量 + LLM keywords 驱动 BM25"；服务化后仅 `query` 驱动向量。工具描述提示 LLM 传语义完整的问句即可。
3. **auto-inject 消亡**：历史上 opt-in 该能力的 Agent 需在升级后改走显式召回。

---

## 4. 决策详述与理由

### 决策 1/2：为什么是 Gateway 托管服务而非 Node sidecar

记忆数据的归属介于两者之间：它像 doc 一样是"跨会话的持久业务库"，又像 workspace 一样"跟着实例走"。选择 Gateway 托管的理由：(a) 面板契约已定在 Gateway HTTP 入口，反代同源最省；(b) advertise endpoint 模型让远程 Runtime 天然可达（Node sidecar 则每台机器一个服务、面板聚合困难）；(c) ADR-064/070/084 范式与 supervisor/proxy 代码可直接复刻。**被否**：Node sidecar（部署与聚合复杂度不成比例，v1 无 per-machine 记忆的强需求）。

### 决策 3：每实例一库 vs 单库多 namespace

选每实例一库：与现状（每 Agent 一个私有 SQLite）粒度一致、迁移是文件级搬运（`/admin/import` 整库上传）、删除实例即删目录、互不放大故障面。**被否**：单库 + namespace 列——隔离弱、误查跨实例风险、stats 迁移复杂。

### 决策 4：为什么官方服务坚持 HTTP、第三方不嫌弃 stdio

HTTP 是本平台的"共享服务"传输：headers 承载双身份（actor/node token）、Gateway 反代与 if-watch 换址（ADR-080）全部现成。stdio 服务由 Runtime 在 node 本机 spawn，身份靠 spawn 时的进程环境自证——第三方 stdio 记忆 MCP 的隔离责任由其自身承担（通常是单用户本地库），本平台不为其提供命名空间语义。替换场景下两种传输都可安装（`McpServerConfigDef` command/args 与 url/headers 双形态均已支持）。

### 决策 5：auto-inject 删除的代价论证

auto-inject 是 Runtime 与"特定记忆实现"之间唯一无法用 MCP 表达的耦合（协议无 pre-LLM 钩子）。保留 = 永久特判官方 server id + 保留检索代码在 Runtime = 可插拔目标破产。且其默认 OFF、opt-in 面极小，代价可忽略。若未来确有需求，正确路径是 MCP 协议的 context/prompt-hook 能力成熟后再议，不预先保留死代码（原则出处：[memory-write-entrypoints.md](../../memory-write-entrypoints.md)"死代码不保留"）。

### 决策 6：distiller 凭证走全局资源

服务订阅 available providers 与 Runtime 消费方式同构，集中权威（Gateway/vault）不变；避免在 `{data_dir}/config.toml` 里落明文 key。**被否**：(a) distiller 留 Runtime 远程驱动存储——违背不变量 1；(b) 服务自带独立 apikey 配置——凭证分散、轮换困难。

### 决策 9：薄集成为什么可接受

(a) bootstrap 推送的语义是"包内容分发"（manifest 是权威来源，见入口 D 定义），数据由 Runtime 侧产生、目标是任意 id=memory 服务，属配置管道而非记忆逻辑；(b) chat_router 解析是纯注册表查找。二者对第三方 MCP 均自然失活。此边界如需收窄（例如把 (a) 移到 Gateway 安装流程），不改变本 ADR 其余决策。

---

## 5. 后果

**收益**：Runtime 零记忆业务；换记忆=改配置；记忆流水线故障与 Agent 会话进程隔离；打包不变量升级为结构性保证。

**代价与风险**：

| 风险 | 缓解 |
|---|---|
| 记忆调用多一跳网络（loopback/反代） | 与 pm/doc 同量级；召回超时按 MCP 工具超时策略（constants::MCP_*）处理，失败=工具错误而非会话中断 |
| 服务宕机 → 记忆能力整体不可用 | supervisor 指数退避重启 + catalog 撤下后面板/工具一致降级为"无记忆"；显式优于隐式（内置时代 store 打开失败只打日志） |
| 迁移窗口数据双源 | 一次性 pass（决策 10）：真实导入后立即 DELETE 本地记忆行并落 `.migrated` 哨兵，同一文件的会话/对话数据不受影响；`skipped_existing` 采取"宁可留着"的不删除分支；失败不写哨兵，单实例可 `--force` 重试，不放大故障面 |
| 多用户/远程部署下面板可访问性扩大（记忆离开 node-local 目录，中心化到 Gateway 主机） | 数据主权方向与 doc 一致，属范式内既有取舍；AUTH_MODE/节点 token 闸门不变量 5 覆盖 |
| 第三方记忆 MCP 无面板/治理 | 属替换的语义本身，写入本文档 §1.4 对照表与前端隐藏逻辑，UI 不做任何假装支持 |
| **Breaking**：仍声明 `type = "MemoryRead"` / `"MemoryWrite"`（或字符串式 `memory:read`）的旧包在 manifest 解析期硬失败——`Permission` 的自定义 serde 对未知 type 返回 `Unknown permission type`，`parse` 返回 `unknown category`。不做静默忽略 | 有意为之（决策 10「不留兼容代码」）：保留一个无人读取的权限位比报错更难排查。alpha 期接受破坏，7 个内置示例包已随本 ADR 改完（删权限块与 `[[tools]] memory_*` 声明，注释指向 `mcp_memory__*`）。`parse` 的 unknown-category 提示已去掉 memory，不会误导用户去声明它；解析失败信息落在包安装/加载日志上，第三方包按上述两行改法即可恢复 |
| Clone 语义变化：`acowork-node` 的 Full clone 仍拷 `private.sqlite` + `*.wal` + `*.shm` 并 `purge_conversation_state()`，但记忆已按 `instance_id` 住在服务侧，**clone 不继承服务侧记忆**；`.migrated` 哨兵也不随 clone 复制 | 接受。这是决策 3（每实例一库）的直接推论：实例私有记忆本来就不应随复制扩散，克隆体从空白记忆开始，与 clone 主动跳过 `conversations/` 的既有语义一致。若源工作区尚未迁移，其遗留记忆行会被 Node 上传到克隆体自己的 `instance_id` 命名空间（与源实例各一份）——单向交接的预期行为，不做跨命名空间去重。"让克隆体继承记忆"= 服务导出 → 安装时导入，即决策 11 明确 out of scope 的"种子记忆"通道，需另行立项 |

**测试与验收（摘要）**：服务集成测试复刻 [doc 的 MCP↔REST 双向审阅用例](../../../core/acowork-doc/tests/api_mcp_integration.rs)（实例隔离、匿名只读、distill 手动触发）；Gateway 注入/跳过两测（同 pm T4-1）；Desktop 门控测试（tile 显隐 + tab 弹回）；`dev/ci.sh` 的 `run_gateway_fs_redline` MUST 通过；`memory_retrieval_bench` 随逻辑迁入服务。检索 harness 也按不变量 7 拆分：memory scope 归 `core/acowork-memory/tests/retrieval_eval.rs`，conversation scope 归 `core/acowork-runtime/tests/retrieval_eval.rs`（离线变体）与 `global_search_e2e.rs`（真实 HTTP + 生产模型），`acowork-runtime/tests/` 下不再留任何记忆引擎测试；共用的指标内核落 `core/acowork-eval`（dev-dependency-only）。

## 6. 实施阶段

| 阶段 | 内容 | 交付判据 |
|---|---|---|
| P1 | acowork-memory lib+bin 服务（MCP + REST + 流水线迁入） | 服务独立启动，集成测试绿 |
| P2 | Gateway：`MemoryConfig` / `memory_supervisor` / `memory_proxy` / catalog 注入 / 状态暴露 | `url.is_some()` ⇔ 注入测试绿；面板路径后端切换 |
| P3 | Runtime 删除面（§2.2 全表）+ chat_router 动态解析 + 薄集成 | `cargo build/clippy/test` 绿；acowork-memory 不在 runtime 依赖中 |
| P4 | Desktop：门控 + 全局搜索直连 | 关服务 → 面板消失的端到端手测/测试 |
| P5 | Node 迁移通道（`memory_migration` pass + `acowork-node migrate-memory` + 服务 `/admin/import`）+ 文档收口（本 ADR 状态定案、module-design/protocols/AGENTS.md 更新） | pass 单测覆盖导入/跳过/超限/退避重试/哨兵；`dev/ci.sh` 全绿含 `run_gateway_fs_redline`；存量实例迁移演练通过 |

P1/P2 可并行；P3+P4 同批切换（避免"半内置半服务"中间态）；P5 最后。

