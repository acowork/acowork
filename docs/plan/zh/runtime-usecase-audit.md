# acowork-runtime HTTP / MQTT 接口 usecase 层合规性审计

**日期**：2026-02（审计基线：当前工作区 HEAD）
**范围**：`core/acowork-runtime` 内全部 HTTP 路由与全部 MQTT 订阅/分发入口
**判定基准**：[ADR-040：Runtime Adapter 整合 — UseCase Trait 层](../../adr/zh/ADR-040-runtime-adapter-use-case-layer.md)
**性质**：只读分析，未修改任何生产代码。本文是唯一新增文件。

---

## 0. 判定口径（先定义，再统计）

### 0.1 ADR-040 规定的目标结构

`acowork-runtime` 已经在 `src/usecases/` 下定义了 14 个 trait + 11 个 impl：

| trait | 定义 | impl | Handler 侧注入方式 |
|---|---|---|---|
| `SessionMetadataService` | `usecases/session_metadata.rs` | `session_metadata_impl.rs` | `HttpState.session_metadata` late-bind slot |
| `WorkspaceQueryService` | `usecases/workspace_query.rs` | `workspace_query_impl.rs` | `HttpState.workspace_query` |
| `WorkspaceMutationService` | `usecases/workspace_mutation.rs` | `workspace_mutation_impl.rs` | `HttpState.workspace_mutation` |
| `GitQueryService` | `usecases/git_query.rs` | `git_query_impl.rs` | `HttpState.git_query` |
| `AgentToolsService` | `usecases/agent_tools.rs` | `agent_tools_impl.rs` | `HttpState.agent_tools` |
| `AgentConfigService` | `usecases/agent_config.rs` | `agent_config_impl.rs` | `HttpState.agent_config` |
| `AttachmentService` | `usecases/attachment.rs` | `attachment_impl.rs` | `HttpState.attachment` |
| `SessionConfigService` | `usecases/session_config.rs` | `session_config_impl.rs` | `HttpState.session_config` |
| `DebugService` | `usecases/debug_service.rs` | `debug_service_impl.rs` | `HttpState.debug_service` |
| `ChatUsageService` | `usecases/chat_usage.rs` | `chat_usage_impl.rs` | **无 slot**（handler 自己 new，见 §2.1 #5/#6 与 §6 R6） |
| `AgentTokenService` | `usecases/agent_token.rs` | `agent_token_impl.rs` | 只被其它 usecase 内部使用 |
| `McpRecommendation` | `usecases/mcp_recommendation.rs` | 内联于 `agent_tools_impl` | 经 `AgentToolsService` |
| `SessionControlService` | `usecases/session_control.rs` | **无任何实现** | **无**（见 §5.1） |
| `PackageWorkspace` | `usecases/package_workspace.rs` | — | — |

`HttpState`（[server.rs:195](core/acowork-runtime/src/http/server.rs#L195)）同时把**内部模块**直接挂在 state 上，这是 B 类违规的物理入口：

`agent_core`、`session_manager_slot`、`conversation_index`、`rag_provider`、`workspace_resolver`、`workspace_watchers`、`latest_session`、`dispatch_tx`、`mqtt_client`、`shell_risk_rules`。

### 0.2 四类判定

| 类 | 定义 |
|---|---|
| **A 合规** | handler 的业务调用**只**经 `state.<usecase_slot>` 或 `Arc<dyn XxxService>` 抵达实现，handler 自身只做协议转换 / 参数归一 / 授权前置 |
| **B 违规 · handler 直接调用内部模块** | handler（或其 `&HttpState` 辅助函数）直接访问 `state.agent_core` / `session_manager` / `conversation_index` / `rag_provider` / `crate::<内部模块>::*`，业务逻辑与内部依赖落在 adapter |
| **C 违规 · usecase 内部绕过抽象** | 有 usecase trait，但 impl 依赖具体 service、adapter 类型或可变全局状态（层次倒置 / 隐式单例） |
| **N 基础设施** | 不承载业务逻辑的运维入口（仅 `GET /health`），不纳入 A/B 分母 |

> 说明：brief 要求的三类无法容纳 `GET /health`（无业务、也无 usecase 可依），因此单列 N 并给出独立计数，A+B+C 之和仍可与端点总数对齐。

### 0.3 统计口径

- 一个 **route 注册 + 一个 HTTP 方法** = 一个入口。一个 handler 被两个方法复用（如 `GET/PUT /agents/{id}/config`）算两个入口。
- MQTT 一个 **被订阅的 topic 分支** = 一个入口（`acowork/global/#` 通配下的 5 个具名分支分别计）。
- 授权辅助 `authorize_read/authorize_write` 直接读盘的横切问题单独统计（§4.3），不重复计入每条 handler。

---

## 1. 总览统计

### 1.1 HTTP

| 项 | 数量 |
|---|---|
| route 注册 | **72** |
| handler 绑定（方法 × 路由） | **91** |
| handler 函数（`async fn(State<HttpState>, …)`） | **91**（1:1，无未挂载 handler） |

| 类 | 数量 | 占比 |
|---|---|---|
| **A 完全合规** | **49** | 53.8% |
| **B handler 直接调用内部模块** | **41** | 45.1% |
| **N 基础设施（/health）** | **1** | 1.1% |
| **C usecase 内部绕过** | **0（入口维度）** / 4 项（impl 维度，见 §5） | — |

### 1.2 MQTT

| 项 | 数量 |
|---|---|
| 生产订阅 | **4**（`acowork/global/#`、`agents/{id}/sessions/control/#`、node LSP topic、node info topic） |
| topic 级分发入口 | **9**（global 5 个具名分支 + 1 个通配 cache 分支 + control 1 + node LSP 1 + node info 1） |

| 类 | 数量 |
|---|---|
| **A 合规** | **0** |
| **B 无 usecase，直接调用内部模块** | **9** |
| **C** | 0 |

### 1.3 一句话结论

> **HTTP 层约一半入口（49/91）已收敛到 usecase trait；MQTT 层一条都没收敛（0/9）。**
> 成因不是「忘记改」，而是 `SessionControlService` trait **至今没有任何实现**（其自身 doc 注释标注为 *shelved*），于是**整个会话控制面**（17 个 HTTP 写入口 + MQTT dispatch）在架构上没有可依的 usecase，只能直接操作 `SessionManager`。

---

## 2. HTTP 入口逐条调用链

### 2.1 `http/server.rs` 主体（52 个入口）

| # | 入口 | handler | 调用链 | 类 |
|---|---|---|---|---|
| 1 | GET /health | [server.rs:963](core/acowork-runtime/src/http/server.rs#L963) | handler → `degraded_reasons` / `mqtt_lifecycle_stamp` → `mqtt_client` | N |
| 2 | GET /sessions | [server.rs:1039](core/acowork-runtime/src/http/server.rs#L1039) | `SessionMetadataService::list_sessions` → `conversation` 扫描 + `AgentTokenService` + `ChatUsageService` | A |
| 3 | GET /sessions/latest | [server.rs:1223](core/acowork-runtime/src/http/server.rs#L1223) | `SessionMetadataService::get_latest_session` | A |
| 4 | GET /sessions/token-stats | [server.rs:1294](core/acowork-runtime/src/http/server.rs#L1294) | `SessionMetadataService::token_stats` | A |
| 5 | GET /chat-usage/{chat_id} | [server.rs:1323](core/acowork-runtime/src/http/server.rs#L1323) | handler → **`state.agent_core`** → 自行 `RuntimeChatUsageService::new` → `get_chat` | **B** |
| 6 | DELETE /chat-usage/{chat_id} | [server.rs:1352](core/acowork-runtime/src/http/server.rs#L1352) | 同上 → `forget_chat` | **B** |
| 7 | GET /sessions/{sid}/config | [server.rs:1400](core/acowork-runtime/src/http/server.rs#L1400) | `[authorize_read]` → `SessionConfigService::get_config` | A |
| 8 | PUT /sessions/{sid}/config | [server.rs:1437](core/acowork-runtime/src/http/server.rs#L1437) | `[authorize_write]` + 区间校验 → `SessionConfigService::apply_config` | A |
| 9 | GET /sessions/{sid}/messages | [server.rs:1494](core/acowork-runtime/src/http/server.rs#L1494) | `[authorize_read]` → `SessionMetadataService::get_messages` | A |
| 10 | POST /conversation/index/rebuild | [server.rs:1540](core/acowork-runtime/src/http/server.rs#L1540) | handler → **`state.conversation_index`**（[1544](core/acowork-runtime/src/http/server.rs#L1544)）→ `ConversationIndex::rebuild` | **B** |
| 11 | GET /sessions/{sid} | [server.rs:1563](core/acowork-runtime/src/http/server.rs#L1563) | `[authorize_read]` → `SessionMetadataService::get_session` | A |
| 12 | GET /workspaces | [server.rs:1623](core/acowork-runtime/src/http/server.rs#L1623) | `WorkspaceQueryService::list_workspaces` | A |
| 13 | GET /workspaces/tree | [server.rs:1641](core/acowork-runtime/src/http/server.rs#L1641) | `WorkspaceQueryService::list_tree` | A |
| 14 | GET /workspaces/file | [server.rs:1660](core/acowork-runtime/src/http/server.rs#L1660) | `WorkspaceQueryService::read_file` | A |
| 15 | GET /workspaces/raw/{*path} | [server.rs:1688](core/acowork-runtime/src/http/server.rs#L1688) | `WorkspaceQueryService::read_file_raw` | A |
| 16 | GET /workspaces/find | [server.rs:1731](core/acowork-runtime/src/http/server.rs#L1731) | `WorkspaceQueryService::find_files` | A |
| 17 | GET /workspaces/search | [server.rs:1750](core/acowork-runtime/src/http/server.rs#L1750) | `WorkspaceQueryService::search_files` | A |
| 18 | GET /git/status | [server.rs:1791](core/acowork-runtime/src/http/server.rs#L1791) | `GitQueryService::status` | A |
| 19 | GET /git/diff | [server.rs:1811](core/acowork-runtime/src/http/server.rs#L1811) | `GitQueryService::diff` | A |
| 20 | GET /git/log | [server.rs:1829](core/acowork-runtime/src/http/server.rs#L1829) | `GitQueryService::log` | A |
| 21 | POST /git/revert | [server.rs:1849](core/acowork-runtime/src/http/server.rs#L1849) | `GitQueryService::revert` | A |
| 22 | POST /workspaces | [server.rs:1868](core/acowork-runtime/src/http/server.rs#L1868) | `WorkspaceMutationService::create_workspace` → `reload_workspace_resolver` + `sync_workspace_watchers` + 可见性断言 | A（+ 应收敛 R2） |
| 23 | PUT /workspaces/{ws_id} | [server.rs:1977](core/acowork-runtime/src/http/server.rs#L1977) | `::update_workspace` → 同上一套收尾 | A（+R2） |
| 24 | PUT /workspaces/{ws_id}/prompt-file | [server.rs:2006](core/acowork-runtime/src/http/server.rs#L2006) | `::set_prompt_file` → `reload_workspace_resolver` | A（+R2） |
| 25 | PUT /workspaces/{ws_id}/fs-watch | [server.rs:2055](core/acowork-runtime/src/http/server.rs#L2055) | handler → `state.workspace_resolver` 校验 + 自写 path-traversal 防护 + `state.workspace_watchers`（[2107](core/acowork-runtime/src/http/server.rs#L2107)）→ `set_watch_targets` | **B** |
| 26 | DELETE /workspaces/{ws_id} | [server.rs:2113](core/acowork-runtime/src/http/server.rs#L2113) | `::delete_workspace` → 收尾同 #22 | A（+R2） |
| 27 | POST /workspaces/file | [server.rs:2139](core/acowork-runtime/src/http/server.rs#L2139) | `WorkspaceMutationService::create_file` | A |
| 28 | PUT /workspaces/file | [server.rs:2160](core/acowork-runtime/src/http/server.rs#L2160) | `::write_file` | A |
| 29 | DELETE /workspaces/file | [server.rs:2181](core/acowork-runtime/src/http/server.rs#L2181) | `::delete_file` | A |
| 30 | POST /workspaces/dir | [server.rs:2198](core/acowork-runtime/src/http/server.rs#L2198) | `::create_dir` | A |
| 31 | DELETE /workspaces/dir | [server.rs:2215](core/acowork-runtime/src/http/server.rs#L2215) | `::delete_dir` | A |
| 32 | POST /workspaces/copy | [server.rs:2237](core/acowork-runtime/src/http/server.rs#L2237) | `::copy_item` | A |
| 33 | POST /workspaces/rename | [server.rs:2269](core/acowork-runtime/src/http/server.rs#L2269) | `::rename_item` | A |
| 34 | POST /sessions/{sid}/files | [server.rs:2348](core/acowork-runtime/src/http/server.rs#L2348) | `[authorize_write]` → `AttachmentService::upload_file` | A |
| 35 | GET /files/{document_id} | [server.rs:2457](core/acowork-runtime/src/http/server.rs#L2457) | `AttachmentService::read_file` | A |
| 36 | GET /agents/{id}/config | [server.rs:2519](core/acowork-runtime/src/http/server.rs#L2519) | `AgentConfigService::get_config` | A |
| 37 | PUT /agents/{id}/config | [server.rs:2585](core/acowork-runtime/src/http/server.rs#L2585) | `AgentConfigService::put_config` → `dispatch_agent_level_config` + 重发 retained `config`（[2680](core/acowork-runtime/src/http/server.rs#L2680)） | A（+R6） |
| 38 | GET /agents/{id}/tools | [server.rs:3086](core/acowork-runtime/src/http/server.rs#L3086) | `AgentToolsService::get_merged_tools` | A |
| 39 | GET /agents/{id}/mcp-servers | [server.rs:3146](core/acowork-runtime/src/http/server.rs#L3146) | `AgentToolsService::get_mcp_servers` | A |
| 40 | PUT /agents/{id}/mcp-servers | [server.rs:3198](core/acowork-runtime/src/http/server.rs#L3198) | `AgentToolsService::put_mcp_servers` **+** handler 自查 `crate::agent_config::load_agent_mcp_config`（[3235](core/acowork-runtime/src/http/server.rs#L3235)）+ `crate::mcp_ensure::clear_status`（[3249](core/acowork-runtime/src/http/server.rs#L3249)）+ `crate::mcp_ensure::ensure_newly_enabled`（[3278](core/acowork-runtime/src/http/server.rs#L3278)） | **B**（主体 A，diff/副作用在 adapter） |
| 41 | PUT /agents/{id}/mcp-recommendations/{key} | [server.rs:3330](core/acowork-runtime/src/http/server.rs#L3330) | `AgentToolsService::put_mcp_recommendation` | A |
| 42 | GET /agents/{id}/mcp-ensure/{name} | [server.rs:3390](core/acowork-runtime/src/http/server.rs#L3390) | handler → `crate::mcp_ensure::ensure_status`（[3397](core/acowork-runtime/src/http/server.rs#L3397)）→ 进程级 `static REGISTRY` | **B** |
| 43 | GET /agents/{id}/mcp-tools | [server.rs:3420](core/acowork-runtime/src/http/server.rs#L3420) | `AgentToolsService::get_mcp_tools` | A |
| 44 | PUT /agents/{id}/mcp-tools | [server.rs:3459](core/acowork-runtime/src/http/server.rs#L3459) | `AgentToolsService::put_mcp_tools` → `mcp_notifier.notify()` | A |
| 45 | GET /agents/{id}/providers | [server.rs:3544](core/acowork-runtime/src/http/server.rs#L3544) | handler → `crate::agent_config::load_agent_provider_config(&state.work_dir)`（[3557](core/acowork-runtime/src/http/server.rs#L3557)） | **B** |
| 46 | GET /agents/{id}/builtin-tools | [server.rs:3599](core/acowork-runtime/src/http/server.rs#L3599) | `AgentToolsService::get_builtin_tools` | A |
| 47 | PUT /agents/{id}/builtin-tools | [server.rs:3637](core/acowork-runtime/src/http/server.rs#L3637) | `AgentToolsService::put_builtin_tools` → `broadcast_builtin_tools_update` | A（+R3） |
| 48 | GET /agents/{id}/status | [server.rs:3749](core/acowork-runtime/src/http/server.rs#L3749) | handler → `state.latest_session`（[3757](core/acowork-runtime/src/http/server.rs#L3757)）+ `state.live_embed_dim()`（[3758](core/acowork-runtime/src/http/server.rs#L3758) → `agent_core.live_embedding_provider`） | **B** |
| 49 | GET /agents/{id}/shell-risk-rules | [server.rs:3781](core/acowork-runtime/src/http/server.rs#L3781) | handler → `crate::security::shell_risk::generate_user_rules_toml` / `embedded_defaults`（[3829](core/acowork-runtime/src/http/server.rs#L3829)–[3842](core/acowork-runtime/src/http/server.rs#L3842)）+ 首读落盘模板 | **B** |
| 50 | PUT /agents/{id}/shell-risk-rules | [server.rs:3873](core/acowork-runtime/src/http/server.rs#L3873) | handler → TOML 校验 + 写盘 + `crate::security::shell_risk` 解析（[3900](core/acowork-runtime/src/http/server.rs#L3900)）+ 原地改写 `shell_risk_rules` | **B** |
| 51 | GET /agents/{id}/rag/status | [server.rs:3942](core/acowork-runtime/src/http/server.rs#L3942) | handler → `state.rag_provider`（[3947](core/acowork-runtime/src/http/server.rs#L3947)）→ `EmbeddingProvider::name` | **B** |
| 52 | POST /agents/{id}/rag/query | [server.rs:3978](core/acowork-runtime/src/http/server.rs#L3978) | handler → `state.rag_provider`（[3985](core/acowork-runtime/src/http/server.rs#L3985)）→ `query_with_params` + 结果整形 | **B** |

### 2.2 `http/session_control.rs` — 会话控制面（17 个入口，全部 B）

共同链路：handler → `authorize_read/write` → **`session_manager(&state)`**（[session_control.rs:224](core/acowork-runtime/src/http/session_control.rs#L224)，直接从 `session_manager_slot` 取 `Arc<Mutex<SessionManager>>`）→ `SessionManager::<具体方法>`。

| # | 入口 | handler | SessionManager 上的具体方法 | 类 |
|---|---|---|---|---|
| 53 | POST /intent-context/close | [session_control.rs:121](core/acowork-runtime/src/http/session_control.rs#L121) | `close_intent_context`（+ node token 校验） | **B** |
| 54 | POST /sessions | [session_control.rs:334](core/acowork-runtime/src/http/session_control.rs#L334) | `create_frontend_session` + `set_session_visibility` + `stamp_chat_binding` | **B** |
| 55 | POST /sessions/{sid}/open | [session_control.rs:480](core/acowork-runtime/src/http/session_control.rs#L480) | `resume_session` + `announce_session_opened` | **B** |
| 56 | POST /sessions/{sid}/close | [session_control.rs:517](core/acowork-runtime/src/http/session_control.rs#L517) | `close_session` + `publish_lifecycle` | **B** |
| 57 | DELETE /sessions/{sid} | [session_control.rs:550](core/acowork-runtime/src/http/session_control.rs#L550) | `delete_session` + `publish_lifecycle` | **B** |
| 58 | DELETE /sessions | [session_control.rs:596](core/acowork-runtime/src/http/session_control.rs#L596) | `crate::conversation::scan_sessions_from_meta`（[604](core/acowork-runtime/src/http/session_control.rs#L604)）→ `clearable_session_ids` → 逐个 `delete_session` | **B** |
| 59 | PUT /sessions/{sid}/visibility | [session_control.rs:639](core/acowork-runtime/src/http/session_control.rs#L639) | `set_session_visibility` | **B** |
| 60 | GET /sessions/{sid}/tool-policy | [session_control.rs:691](core/acowork-runtime/src/http/session_control.rs#L691) | `session_tool_policy` | **B** |
| 61 | PUT /sessions/{sid}/tool-policy | [session_control.rs:723](core/acowork-runtime/src/http/session_control.rs#L723) | `set_session_tool_policy` | **B** |
| 62 | PUT /sessions/{sid}/workspace | [session_control.rs:772](core/acowork-runtime/src/http/session_control.rs#L772) | `route_workspace_switch` | **B** |
| 63 | POST /sessions/{sid}/messages | [session_control.rs:852](core/acowork-runtime/src/http/session_control.rs#L852) | `ensure_session_active` → `dispatch_session_action`（[884](core/acowork-runtime/src/http/session_control.rs#L884)） | **B** |
| 64 | POST /sessions/{sid}/stop | [session_control.rs:955](core/acowork-runtime/src/http/session_control.rs#L955) | `dispatch_session_action` | **B** |
| 65 | POST /sessions/{sid}/continue | [session_control.rs:976](core/acowork-runtime/src/http/session_control.rs#L976) | `dispatch_session_action` | **B** |
| 66 | POST /sessions/{sid}/approval | [session_control.rs:1025](core/acowork-runtime/src/http/session_control.rs#L1025) | `dispatch_session_action` | **B** |
| 67 | POST /sessions/{sid}/answer | [session_control.rs:1067](core/acowork-runtime/src/http/session_control.rs#L1067) | `dispatch_session_action` | **B** |
| 68 | POST /sessions/{sid}/cancel-tool | [session_control.rs:1102](core/acowork-runtime/src/http/session_control.rs#L1102) | `dispatch_session_action` | **B** |
| 69 | POST /sessions/{sid}/compress | [session_control.rs:1138](core/acowork-runtime/src/http/session_control.rs#L1138) | `dispatch_session_action` | **B** |

### 2.3 `http/debug.rs`（12 个入口）

| # | 入口 | handler | 调用链 | 类 |
|---|---|---|---|---|
| 70–79 | POST /api/debug/{resume,pause,step,stop,rewind}、GET /api/debug/{state}、GET /api/debug/context/{iteration}[/sections/{section}]、POST /api/debug/context/{patch,re-execute} | [debug.rs:211](core/acowork-runtime/src/http/debug.rs#L211)–[371](core/acowork-runtime/src/http/debug.rs#L371) | `resolve_service(&state)`（[debug.rs:195](core/acowork-runtime/src/http/debug.rs#L195)）→ `Arc<dyn DebugService>` | A |
| 80 | POST /api/debug/enable | [debug.rs:437](core/acowork-runtime/src/http/debug.rs#L437) | handler → `crate::startup::debug_enable::enable_debug_mode_and_fill_slot(&debug_service, &mqtt_client, &session_manager_slot, …)`（[440](core/acowork-runtime/src/http/debug.rs#L440)） | **B**（应用函数非 trait） |
| 81 | POST /api/debug/disable | [debug.rs:493](core/acowork-runtime/src/http/debug.rs#L493) | handler → `crate::startup::debug_enable::disable_debug_mode_and_clear_slot(&debug_service, &session_manager_slot)`（[494](core/acowork-runtime/src/http/debug.rs#L494)） | **B** |

### 2.4 其余子路由（10 个入口）

| # | 入口 | handler | 调用链 | 类 |
|---|---|---|---|---|
| 82 | GET /sessions/{sid}/rounds | [rounds.rs:118](core/acowork-runtime/src/http/rounds.rs#L118) | `[authorize_read]` → `session_paths`（[rounds.rs:257](core/acowork-runtime/src/http/rounds.rs#L257) 自拼 `work_dir/conversations`）→ `crate::rounds::fold_or_rebuild`（[149](core/acowork-runtime/src/http/rounds.rs#L149)）→ `crate::rounds::project`（[167](core/acowork-runtime/src/http/rounds.rs#L167)） | **B** |
| 83 | GET /sessions/{sid}/rounds/{round_id} | [rounds.rs:45](core/acowork-runtime/src/http/rounds.rs#L45) | 同上 → `crate::rounds::project`（[57](core/acowork-runtime/src/http/rounds.rs#L57)） | **B** |
| 84 | GET /search | [global_search.rs:237](core/acowork-runtime/src/http/global_search.rs#L237) | 合规部分：`WorkspaceQueryService::search_files` / `GitQueryService::log` / `AgentConfigService::get_config`；**违规部分**：`state.conversation_index`（[279](core/acowork-runtime/src/http/global_search.rs#L279)）+ `embed_query` 读 `state.agent_core`（[96](core/acowork-runtime/src/http/global_search.rs#L96)）+ `crate::conversation::read_session_meta`（[225](core/acowork-runtime/src/http/global_search.rs#L225)）+ `crate::search::analyzer`（[289](core/acowork-runtime/src/http/global_search.rs#L289)） | **B**（混合） |
| 85 | POST /search/summary | [search_summary.rs:301](core/acowork-runtime/src/http/search_summary.rs#L301) | handler → `state.agent_core`（[314](core/acowork-runtime/src/http/search_summary.rs#L314)）→ `state.session_manager_slot`（[331](core/acowork-runtime/src/http/search_summary.rs#L331)）→ `core.resolve_summary_llm`（[352](core/acowork-runtime/src/http/search_summary.rs#L352)）→ `provider.chat`（[381](core/acowork-runtime/src/http/search_summary.rs#L381)）→ `core.accumulate_llm_usage`（[398](core/acowork-runtime/src/http/search_summary.rs#L398)） | **B**（最重） |
| 86 | POST /agents/{id}/prompts/reload | [prompts.rs:29](core/acowork-runtime/src/http/prompts.rs#L29) | handler → `state.agent_core`（[46](core/acowork-runtime/src/http/prompts.rs#L46)）→ `crate::package::prompt_builder::reload_prompts_into_core`（[58](core/acowork-runtime/src/http/prompts.rs#L58)）→ `crate::skills::parser::SkillRegistry::load_from_dir`（[70](core/acowork-runtime/src/http/prompts.rs#L70)）→ `dispatch_agent_level_config` → 重建 system prompt（[93](core/acowork-runtime/src/http/prompts.rs#L93)） | **B** |
| 87 | GET /agents/{id}/skills | [skills.rs:111](core/acowork-runtime/src/http/skills.rs#L111) | `load_registry(&state)`（[skills.rs:181](core/acowork-runtime/src/http/skills.rs#L181) 读 `package_dir/skills` + 解析 SKILL.md）→ 分页 | **B** |
| 88 | GET /agents/{id}/skills/{name} | [skills.rs:128](core/acowork-runtime/src/http/skills.rs#L128) | 同上 | **B** |
| 89 | GET /agents/{id}/skills/{name}/history | [skills.rs:151](core/acowork-runtime/src/http/skills.rs#L151) | 同上 + 历史文件读取 | **B** |
| 90 | GET /agents/{id}/avatar-config | [avatar.rs:88](core/acowork-runtime/src/http/avatar.rs#L88) | `effective_config`（[avatar.rs:272](core/acowork-runtime/src/http/avatar.rs#L272)）→ `crate::package::loader::load_manifest` + `load_overrides` 裸 `std::fs` 读盘 | **B** |
| 91 | PUT /agents/{id}/avatar-config | [avatar.rs:104](core/acowork-runtime/src/http/avatar.rs#L104) | `save_overrides`（[avatar.rs:253](core/acowork-runtime/src/http/avatar.rs#L253)，tmp+rename 自实现原子写） | **B** |

---

## 3. MQTT 入口逐条调用链（9 个，全部 B）

| # | 订阅 / 分支 | 订阅点 | 分发 handler | 最终内部模块 | 类 |
|---|---|---|---|---|---|
| M1 | `acowork/global/bootstrap` | [client.rs:1287](core/acowork-runtime/src/mqtt/client.rs#L1287) | `RuntimeHandler::on_publish`（[client.rs:637](core/acowork-runtime/src/mqtt/client.rs#L637)）分支 [643](core/acowork-runtime/src/mqtt/client.rs#L643) | `AvailableResourceCache::update_from_mqtt` | **B**（cache 层，可接受） |
| M2 | `acowork/global/user_profile` | 同上 | 分支 [661](core/acowork-runtime/src/mqtt/client.rs#L661) → `identity_update_tx` → `mqtt_only_loop`（[gateway_loop.rs:412](core/acowork-runtime/src/startup/gateway_loop.rs#L412)） | `SessionManager::update_user_identity` | **B** |
| M3 | `acowork/global/mcps` | 同上 | 分支 [671](core/acowork-runtime/src/mqtt/client.rs#L671)：proto→def 转换、目录 diff、`crate::agent_config::load_agent_mcp_config`（[762](core/acowork-runtime/src/mqtt/client.rs#L762)）、`save_agent_mcp_config_catalog`（[772](core/acowork-runtime/src/mqtt/client.rs#L772)）、`mcp_notifier.notify()` | `agent_config` 模块 + 落盘 | **B** |
| M4 | `acowork/global/providers` | 同上 | 分支 [791](core/acowork-runtime/src/mqtt/client.rs#L791) → `crate::agent_config::save_agent_provider_config_from_available`（[833](core/acowork-runtime/src/mqtt/client.rs#L833)）→ `provider_update_tx` → [gateway_loop.rs:430](core/acowork-runtime/src/startup/gateway_loop.rs#L430) | `SessionManager::update_global_provider_list` | **B** |
| M5 | `acowork/global/embedding_models` | 同上 | 分支 [869](core/acowork-runtime/src/mqtt/client.rs#L869) → `embedding_update_tx` → [gateway_loop.rs:458](core/acowork-runtime/src/startup/gateway_loop.rs#L458) | `SessionManager::handle_embedding_config_update` | **B** |
| M6 | `acowork/global/#` 其余 | 同上 | `available_cache` 通配更新 | `AvailableResourceCache` | **B**（cache） |
| M7 | `acowork/agents/{id}/sessions/control/#` | [client.rs:1293](core/acowork-runtime/src/mqtt/client.rs#L1293) | `control_tx`（[client.rs:913](core/acowork-runtime/src/mqtt/client.rs#L913)）→ [gateway_loop.rs:62](core/acowork-runtime/src/startup/gateway_loop.rs#L62) → `control_handler::parse_control_payload` → `control_action_to_inbound`（[gateway_loop.rs:190](core/acowork-runtime/src/startup/gateway_loop.rs#L190)）→ `dispatch_inbound`（[gateway_loop.rs:529](core/acowork-runtime/src/startup/gateway_loop.rs#L529)）→ `route_intent_inbound`（[gateway_loop.rs:1154](core/acowork-runtime/src/startup/gateway_loop.rs#L1154)） | `SessionManager`（`chat_continuations` / `emit_session_*`）+ `chat_router::runner` | **B** |
| M8 | node LSP relay topic | [client.rs:1303](core/acowork-runtime/src/mqtt/client.rs#L1303) | 分支 [917](core/acowork-runtime/src/mqtt/client.rs#L917) → `lsps_update_tx` → [gateway_loop.rs:487](core/acowork-runtime/src/startup/gateway_loop.rs#L487) | `SessionManager::handle_lsp_relay_update` | **B** |
| M9 | node info topic | [client.rs:1318](core/acowork-runtime/src/mqtt/client.rs#L1318) | 分支 [925](core/acowork-runtime/src/mqtt/client.rs#L925) → `node_proxy_update_tx` → [agent_init.rs:457](core/acowork-runtime/src/startup/agent_init.rs#L457) | 重发 retained `http_endpoint` | **B** |

**MQTT 侧无任何 usecase 调用（0 处引用 `crate::usecases`）。** 事实依据：

```bash
grep -rn "crate::usecases" core/acowork-runtime/src/mqtt/ core/acowork-runtime/src/startup/gateway_loop.rs
# 无输出
```

**MQTT 控制面现状（ADR-076 §决策 4 之后的真实面）**：`ControlAction` 只剩 `IntentReceived` 一个变体（[control_handler.rs:63](core/acowork-runtime/src/mqtt/control_handler.rs#L63)），用户动作全部改走 HTTP。因此 M7 的链路虽长，但**唯一**业务落点是 `route_intent_inbound`。

---

## 4. 违规点清单（B 类，41 项）

### 4.1 会话控制面（17 项）— 最严重

**根因**：[usecases/session_control.rs:10](core/acowork-runtime/src/usecases/session_control.rs#L10) 自述

> **Implementation status (ADR-040 P2-D/H — shelved)**：`SessionManager` 的 mutation 方法要求 `&mut self`，与 `Arc<dyn SessionControlService>` 不兼容。该 trait 目前仅作架构文档；生产路径 `gateway_loop::dispatch_inbound` 直接调用 `session_manager.*`。

事实核验：

```bash
grep -rn "SessionControlService" core/acowork-runtime/src/
# 只有 usecases/mod.rs:42 的 re-export —— 零实现、零使用
```

违规点：
- [session_control.rs:224](core/acowork-runtime/src/http/session_control.rs#L224) `session_manager(&state)` 直接从 slot 取具体类型 `Arc<Mutex<SessionManager>>`，**全部 17 个控制入口都经过它**——`session_manager(&state)` 在 [session_control.rs](core/acowork-runtime/src/http/session_control.rs#L224) 内有 **11 处**调用（第 149/342/488/527/558/611/655/699/733/784/884 行），配合 `authorize_read/authorize_write` 的 14 处调用（其内部又走 `load_meta` 直接读盘）。
- 为什么算违规：ADR-040 §2.1 的架构图明确要求 `gateway_loop dispatch` 与 axum handler **都**经 `Arc<dyn SessionControlService>`；现状是两侧都拿到具体 `SessionManager`，所有 create/close/delete/visibility/workspace/tool-policy 的业务不变式（状态机、生命周期事件、所有权）都写在 handler + `SessionManager` 之间，adapter 一旦分支（HTTP / MQTT / CLI），规则就会各写一遍——ADR-040 记录的 `list_sessions` token bug 正是这个模式的产物。
- 建议改法：给 `SessionManager` 的 mutation 面加内部可变性（或用 `&self` + `Mutex` 字段），实现 `RuntimeSessionControlService`；handler 只保留 `authorize_*`（授权属于 adapter，因为 header 只有 adapter 看得见）+ 调 trait。`dispatch_session_action`（[server.rs:2959](core/acowork-runtime/src/http/server.rs#L2959)）应收敛为 `SessionControlService::send_message/stop/…`。

### 4.2 Agent 配置 / MCP / RAG / Shell 规则（8 项）

| 违规点 | 为什么算违规 | 建议改法 |
|---|---|---|
| [server.rs:3557](core/acowork-runtime/src/http/server.rs#L3557) `get_agent_providers` → `crate::agent_config::load_agent_provider_config` | 已有 `AgentConfigService`，同文件其它端点都走它；此端点绕过 usecase 直接读 `agent_provider.json` | 在 `AgentConfigService` 增加 `get_providers()`，handler 取 slot |
| [server.rs:3235](core/acowork-runtime/src/http/server.rs#L3235)/[3249](core/acowork-runtime/src/http/server.rs#L3249)/[3278](core/acowork-runtime/src/http/server.rs#L3278) `put_agent_mcp_servers` | 主体已走 `AgentToolsService::put_mcp_servers`，但 **prev/next diff、`clear_status`、`ensure_newly_enabled`** 三个副作用在 adapter，且读的是同一份 `agent_mcp.json`（usecase 内部也读）→ 双读、双写窗口 | diff + enable 编排移入 `AgentToolsService::put_mcp_servers`（返回 `newly_enabled`），handler 只 `notify()` |
| [server.rs:3397](core/acowork-runtime/src/http/server.rs#L3397) `get_agent_mcp_ensure` → `crate::mcp_ensure::ensure_status` | 读进程级 `static REGISTRY: OnceLock<Mutex<HashMap<…>>>`（[mcp_ensure.rs:72](core/acowork-runtime/src/mcp_ensure.rs#L72)）；adapter 直连隐式全局单例，无法注入、无法在测试中隔离 | 包一层 `McpEnsureService`（或把 registry 挂到 `HttpState`） |
| [server.rs:3757](core/acowork-runtime/src/http/server.rs#L3757)/[3758](core/acowork-runtime/src/http/server.rs#L3758) `get_agent_status` | 直接读 `latest_session` 快照 + `agent_core.live_embedding_provider()`；`/agents/{id}/status` 是 Desktop 轮询端点，混入两个内部依赖 | 增加 `AgentStatusService::get_status()` |
| [server.rs:3829](core/acowork-runtime/src/http/server.rs#L3829)–[3842](core/acowork-runtime/src/http/server.rs#L3842) `get_shell_risk_rules` | 模板生成、`std::fs` 读、**GET 副作用（首次访问落盘建文件）**、embedded fallback 全在 handler | `ShellRiskService::{get_effective, put_override}` |
| [server.rs:3900](core/acowork-runtime/src/http/server.rs#L3900) `put_shell_risk_rules` | TOML 校验 + 写盘 + 解析 + 原地改写 state 上的 `shell_risk_rules`，4 个副作用在 adapter | 同上，impl 内持有规则源 |
| [server.rs:3947](core/acowork-runtime/src/http/server.rs#L3947) `get_rag_status` / [3985](core/acowork-runtime/src/http/server.rs#L3985) `post_rag_query` | adapter 直接 `state.rag_provider` 并做结果整形（`content/source_url/chunk_id/score` 映射是业务投影） | `RagQueryService::{status, query}` |
| [server.rs:1544](core/acowork-runtime/src/http/server.rs#L1544) `rebuild_conversation_index` | adapter 直接操作 ADR-081 `ConversationIndex`（全量 rebuild 是运维业务动作） | 归入 `SessionMetadataService` 或独立 `ConversationIndexService::rebuild` |

### 4.3 授权前置直接读盘（横切，22 处调用 / 20 个 handler）

- [session_control.rs:92](core/acowork-runtime/src/http/session_control.rs#L92) `load_meta` → `crate::conversation::read_session_meta`：**adapter 直接读会话 meta**，而 `SessionMetadataService::get_session` 已经具备同样能力（`session_metadata_impl.rs` 内部同样调 `conversation::*`）。
- 影响：`authorize_read/authorize_write` 共 22 处调用（server.rs 7、rounds.rs 2、session_control.rs 13），即 **另有一半「A 类」handler 在授权阶段绕过了 usecase**。
- 与 §4.1 的关系：授权（谁能读/写）依赖 Gateway 注入的 header，属 adapter 职责；**但「读 meta」不是**。建议把 meta 读取下沉为 `SessionMetadataService::authorize(sid, scope) -> SessionMeta`，adapter 只传 scope。
- 另外 [global_search.rs:225](core/acowork-runtime/src/http/global_search.rs#L225) 与 [session_control.rs:604](core/acowork-runtime/src/http/session_control.rs#L604) 各自又调了一次 `crate::conversation::{read_session_meta, scan_sessions_from_meta}` —— 三处重复实现同一读取。

### 4.4 单点重灾区（2 项）

| 违规点 | 事实 | 建议改法 |
|---|---|---|
| [search_summary.rs:314](core/acowork-runtime/src/http/search_summary.rs#L314)–[398](core/acowork-runtime/src/http/search_summary.rs#L398) | 一个 HTTP handler 内完成：读 `agent_core` → 读 `session_manager_slot` 取 model/provider → `resolve_summary_llm` → 组 `ChatRequest` → `provider.chat`（带超时）→ `accumulate_llm_usage`。**这是一次完整的 LLM 业务调用，整体落在 adapter** | 抽 `SearchSummaryService`（impl 持 `Arc<AgentCore>`），handler 只做 body 校验与错误映射 |
| [global_search.rs:96](core/acowork-runtime/src/http/global_search.rs#L96) `embed_query` | `pub(crate)` 辅助函数直接取 `agent_core.embedding_provider` 并 embed；4 个 scope 的融合、排序、`extra` 语义全在 656 行 handler 文件里 | 抽 `SearchService`（scope 由 usecase 决定，adapter 只转 wire），`embed_query` 移入 impl |

### 4.5 文件/技能/头像（5 项）

[rounds.rs:257](core/acowork-runtime/src/http/rounds.rs#L257)（自拼 `conversations` 路径）、[rounds.rs:149](core/acowork-runtime/src/http/rounds.rs#L149)/[167](core/acowork-runtime/src/http/rounds.rs#L167)（`crate::rounds` 直调）、[skills.rs:181](core/acowork-runtime/src/http/skills.rs#L181)（SKILL.md 解析 + 分页）、[avatar.rs:272](core/acowork-runtime/src/http/avatar.rs#L272)/[253](core/acowork-runtime/src/http/avatar.rs#L253)（manifest 解析 + 原子写）。

注：`rounds.rs` 的模块注释（[rounds.rs:17](core/acowork-runtime/src/http/rounds.rs#L17)）明确写了 *"Disk-only … no service slot or live session handle is needed"*，这是**有意的设计判断**，不是疏漏。若维持该判断，应在 ADR-040 里显式登记豁免，否则这条会持续被判为违规。

---

## 5. C 类（usecase 内部绕过抽象 / 层次倒置，4 项）

### 5.1 `SessionControlService` 零实现（框架层缺失）
见 §4.1。这是 B 类 41 项中 17 项 + MQTT M7 的共同根因。

### 5.2 usecase impl 依赖 adapter 类型
[usecases/session_config_impl.rs:16](core/acowork-runtime/src/usecases/session_config_impl.rs#L16) `use crate::http::SharedAgentCore;`，字段 [session_config_impl.rs:53](core/acowork-runtime/src/usecases/session_config_impl.rs#L53) `core_slot: SharedAgentCore`。

- 为什么算违规：`SharedAgentCore` 是**HTTP adapter 模块定义**的类型；usecase 层反向依赖 adapter，ADR-040「adapter 依赖 trait」的方向被倒置。同时 `get_config` 每次外部读都要穿透到活的 `AgentCore` 取有效 `reasoning_effort`，等于 usecase 依赖了运行时全局单例。
- 建议改法：把 `SharedAgentCore` 提升到中立模块（如 `agent::agent_core` 或 `usecases::ports`），并定义窄接口 `ReasoningEffortResolver`，impl 只注入该接口。

### 5.3 usecase impl 反向依赖另一个 impl 的内部函数
[usecases/git_query_impl.rs:34](core/acowork-runtime/src/usecases/git_query_impl.rs#L34) `use crate::usecases::workspace_mutation_impl::{resolve_within_static, resolve_workspace_root};`

- 为什么算违规：impl A 依赖 impl B 的 `pub(crate)` 自由函数获取「工作区根 + 路径穿越防护」——这是两个 usecase 共享的**中间件**，却寄生在某个 impl 文件里。改 impl B 的签名会静默破坏 impl A。
- 建议改法：抽 `usecases/path_guard.rs`（或 `workspace::resolver`）作为共享内核，两个 impl 都依赖它。

### 5.4 usecase impl 的可变后注入状态
[usecases/session_config_impl.rs:60](core/acowork-runtime/src/usecases/session_config_impl.rs#L60) `usage_recompute: RwLock<Option<UsageRecompute>>` + [`:79`](core/acowork-runtime/src/usecases/session_config_impl.rs#L79) `set_usage_recompute(&self, cb)`。

- 说明：这是「Phase B 才拿得到 SessionManager」的妥协，比直接读全局好；但它让同一个 trait 对象的行为依赖注入时序，读代码无法确定 `apply_config` 是否真的会重算 usage。
- 建议改法：与 §6 R1 的 slot 收敛一起处理——在 impl 构造时要求依赖齐全，或显式返回「未注入」状态而不是静默跳过。

---

## 6. 应收敛项（不计违规，但应合并为一个 usecase）

| # | 应收敛项 | 现状证据 | 建议 |
|---|---|---|---|
| **R1** | usecase slot 解析样板 | `service not ready` 返回体在 server.rs **35 处**，形如 `state.X.lock().await.as_ref().ok_or((503, json!({"error": "… not ready"})))?` | 统一成 `fn svc<T>(&self) -> Result<Arc<dyn T>, ApiError>`；`debug.rs:195 resolve_service` 已是正确范式，其余 8 个服务没有 |
| **R2** | 工作区变更后的收尾不变式 | `reload_workspace_resolver` + `sync_workspace_watchers` 在 [1882/1884](core/acowork-runtime/src/http/server.rs#L1882)、[1994/1996](core/acowork-runtime/src/http/server.rs#L1994)、[2029](core/acowork-runtime/src/http/server.rs#L2029)（**漏了 watcher sync**）、[2129/2131](core/acowork-runtime/src/http/server.rs#L2131) 手工重复 4 次 | `WorkspaceMutationService::*` 内部完成后置收尾（impl 已持 `work_dir`，只差 resolver/watcher 句柄）；当前的 4 处不一致证明了这个不变式不能再靠人记 |
| **R3** | 「dispatch 一条 InboundMessage」 | `dispatch_agent_level_config` / `dispatch_session_action` 被 10 个 handler 各自调用（put_agent_config、prompts reload、builtin-tools、send/stop/continue/approval/answer/cancel/compress） | 收敛为 `SessionControlService` 的具名方法（`send_message` / `stop` / `approve` / `apply_config`…），InboundMessage 的构造归属 usecase |
| **R4** | `/agents/{id}/*` 的 instance 校验 + 404 body | `state.instance_matches(&id)` 出现 15 次，404 JSON 手写 15 次（`{"error": "instance_id mismatch…"}`） | axum extractor（`Path<InstanceId>`）或 middleware |
| **R5** | 「usecase 返回 → 广播 → 重发 retained」三段式 | put_agent_config（[2674](core/acowork-runtime/src/http/server.rs#L2674)/[2688](core/acowork-runtime/src/http/server.rs#L2688)）、put_builtin_tools（[3675](core/acowork-runtime/src/http/server.rs#L3675)）、put_agent_mcp_servers（[3265](core/acowork-runtime/src/http/server.rs#L3265)）各自拼 | usecase 返回一个 `Effects { overrides, config_json, newly_enabled }`，adapter 只负责把 effects 打到 MQTT/dispatch |
| **R6** | `ChatUsageService` 无 slot | [server.rs:1328](core/acowork-runtime/src/http/server.rs#L1328)/[1358](core/acowork-runtime/src/http/server.rs#L1358) 在 handler 里 `RuntimeChatUsageService::new(core)`；而同一 impl 已经作为 `SessionMetadataService` 的构造依赖存在（`session_metadata_impl.rs:20`） | 加 `HttpState.chat_usage` slot，与其余 9 个 usecase slot 一致；顺带消掉 §2.1 #5/#6 的 B 判定 |
| **R7** | 会话 meta / conversations 路径解析 | [session_control.rs:92](core/acowork-runtime/src/http/session_control.rs#L92)、[global_search.rs:225](core/acowork-runtime/src/http/global_search.rs#L225)、[rounds.rs:264](core/acowork-runtime/src/http/rounds.rs#L264) 三处各自 `work_dir.join("conversations")` | 一个 `ConversationPaths` 中间件（`conversations_dir(sid)` / `jsonl(sid)` / `rounds(sid)`） |
| **R8** | Skill registry 加载两处 | [skills.rs:181](core/acowork-runtime/src/http/skills.rs#L181) `SkillRegistry::load_from_dir` 与 [prompts.rs:70](core/acowork-runtime/src/http/prompts.rs#L70) 同一调用 | 一个 `SkillCatalogService::load()` |
| **R9** | 搜索策略解析 | [global_search.rs:124](core/acowork-runtime/src/http/global_search.rs#L124) `resolve_search_policy` 在 adapter 把 `AgentConfig` 投影成 `SearchPolicyConfig` | 移入 `AgentConfigService`（`get_search_policy`），adapter 不参与策略推导 |
| **R10** | debug enable/disable | [debug.rs:440](core/acowork-runtime/src/http/debug.rs#L440)/[494](core/acowork-runtime/src/http/debug.rs#L494) 调 `startup::debug_enable` 自由函数并传入 3 个 slot | `DebugService::{enable, disable}`，槽位由 impl 自己持有 |

---

## 7. 覆盖范围与复核方法

### 7.1 覆盖清单（全部 `core/acowork-runtime/src`）

| 层 | 文件 |
|---|---|
| HTTP adapter | `http/server.rs`(8037)、`http/session_control.rs`(1340)、`http/global_search.rs`(656)、`http/search_summary.rs`(649)、`http/debug.rs`(567)、`http/rounds.rs`(448)、`http/avatar.rs`(348)、`http/prompts.rs`(347)、`http/skills.rs`(310)、`http/mod.rs`(23) |
| MQTT adapter | `mqtt/client.rs`(3172)、`mqtt/available_cache.rs`(1088)、`mqtt/debug_events.rs`(418)、`mqtt/agent_config_publisher.rs`(167)、`mqtt/control_handler.rs`(139)、`mqtt/mod.rs`(22) |
| MQTT 分发（事实上的 MQTT usecase） | `startup/gateway_loop.rs`(2372)、`startup/agent_init.rs` |
| usecase 层 | `usecases/` 全部 24 个文件（trait 14 + impl 11 + `mod.rs`） |
| 被直接引用的内部模块 | `agent/`（`agent_core`、`session/*`、`session_config`、`chat_router/*`）、`conversation.rs`、`conversation_index.rs`、`providers/`、`tools/`（含 `workspace_resolver`）、`skills/`、`package/`、`rounds.rs`、`mcp_ensure.rs`、`security/shell_risk.rs`、`embedding/`、`search/` |
| 已确认**不存在**的目标 | 无 `mem` / `memory` 模块（ADR-101：memory 业务在独立 service；`global_search` 的 `memory` scope 明确返回 `not_available`，[global_search.rs:449](core/acowork-runtime/src/http/global_search.rs#L449)） |

### 7.2 复核命令（可直接重跑）

```bash
cd core/acowork-runtime

# 1) HTTP 路由全量（72 route / 91 op）
grep -rn "\.route(\|\.merge(" src/http/
grep -rn "\.route(" src/http/ | wc -l

# 2) HTTP handler 全量（91 个，签名含 State<HttpState>）
grep -rn "^pub(crate) async fn \|^async fn " src/http/*.rs | grep -c "fn "

# 3) 每个 handler 对内部 state 的直连
grep -rn "state\.\(agent_core\|session_manager_slot\|conversation_index\|rag_provider\|workspace_resolver\|workspace_watchers\|latest_session\|dispatch_tx\)" src/http/

# 4) handler 对内部模块的直连（排除 usecases / http）
grep -rn "crate::\(agent\|providers\|tools\|skills\|package\|conversation\|rounds\|mcp_ensure\|security\|embedding\|search\)::" src/http/ \
  | grep -v "usecases/"

# 5) usecase 是否真的被用（A 类的正面证据）
grep -rn "state\.\(session_metadata\|workspace_query\|workspace_mutation\|git_query\|agent_tools\|agent_config\|attachment\|session_config\|debug_service\)" src/http/ | wc -l

# 6) SessionControlService 是否落地
grep -rn "SessionControlService" src/            # 只有 mod.rs 的 re-export

# 7) MQTT 订阅与分发
grep -rn "\.subscribe(" src/mqtt/client.rs
grep -rn "crate::usecases" src/mqtt/ src/startup/gateway_loop.rs   # 空 → MQTT 侧 0 处 usecase

# 8) 授权前置直接读盘
grep -rn "authorize_read(\|authorize_write(" src/http/ | wc -l        # 22
grep -rn "read_session_meta\|scan_sessions_from_meta" src/http/      # 3 处（非测试）
```

### 7.3 复核时容易踩的两个坑（本报告已规避）

1. **`grep "state\.X"` 会漏掉换行调用**：本项目大量使用
   ```rust
   let svc = state
       .agent_tools
       .lock()
   ```
   `state.` 与字段名不在同一行，单行 grep 判为「未使用 usecase」，会把合规 handler 误判成 B。本报告对每个可疑 handler 都做了**逐函数体读取**（脚本按 `{}` 配平切体），不依赖单行 grep。
2. **`impl MqttClientHandler` 的分支匹配不是 `.route(`**：MQTT 入口要靠 `topic ==` / `topic.starts_with` 在 [client.rs:637](core/acowork-runtime/src/mqtt/client.rs#L637) 内逐 `else if` 数，用 `subscribe(` 只能拿到 4 个订阅、拿不到 9 个分支。

### 7.4 未覆盖 / 待确认

| 项 | 原因 |
|---|---|
| `acowork-gateway` 侧反代到 Runtime 的路径与 Runtime 内部路由的一一对应 | brief 范围限定在 `core/acowork-runtime`；Gateway 侧 `proxy.rs` 的路由表未纳入本次统计，若要做「入口 → 反代 → Runtime」全链，需要另开一轮 |
| `POST /sessions/{sid}/files` 的 multipart 分片落盘细节 | 只确认了经 `AttachmentService`，未逐行验证 impl 内的分片/大小限制 |
| `available_cache`（`acowork/global/*`）是否应算业务 | 判定为「传输层缓存」，故 M1/M6 仍计入 B 但标注可接受；若团队认为它属于资源管理层，MQTT 的 A/B 计数会变成 A=2、B=7 |
