# Bot Runtime（统一执行引擎）

> 版本：v3.6 | 更新日期：2026-05-06

---

Bot Runtime 是平台提供的唯一二进制可执行文件，类似 Android 的 ART 虚拟机。Gateway 为每个 Bot 启动一个 Bot Runtime 进程，将 .bot 包路径和 Gateway endpoint 作为启动参数传入。

> **v3.7 变更（2026-05-28）**：上下文压缩策略大幅简化——见 [ADR-010](../../adr/zh/ADR-010-context-compression-simplification.md)。核心变更：放弃程序化折叠策略（Tool Result 折叠、内容折叠 Phase 1），上下文压缩回归 LLM 摘要作为唯一正常路径手段。日常压缩流程简化为：70% 告警 → 80% LLM 摘要（完整上下文，不折叠） → 95% emergency_trim 安全网。

> **v3.10 变更（2026-11-23）**：上下文第 1 层（System Prompt）由单文件拆为两段——`prompts/system.md`（角色身份，场景无关）+ `prompts/rules.md`（业务场景规则/约束/工作流），段序由运行时固定为 `system.md` → `rules.md` → 其余按文件名——见 [ADR-120](../../adr/zh/ADR-120-system-and-rules-prompt-split.md)。

> **v3.9 变更（2026-05-28）**：Compaction 与 Distillation 统一——见 [ADR-011](../../adr/zh/ADR-011-compaction-as-distillation.md)。核心变更：Compaction 摘要与 Session 蒸馏合并为单次 Compact Model 调用，摘要文本同时用于内存替换和 SQLite 经历层写入（"摘要即蒸馏"）。经历层写入来源简化为仅 Compaction 和 Session 关闭蒸馏，移除每轮对话实时写入。SessionState 新增 `is_compacted` 标志控制尾部蒸馏决策。

**交叉引用**：
- 运行时内部结构：本文档 §2
- Session Actor 架构：`15-conversation-persistence.md` §1.7
- IPC 消息格式：`06-communication.md` §1.5
- Episode 提炼机制：`15-conversation-persistence.md` §3.3
- Budget 分配策略：`15-conversation-persistence.md` §1.8

## 1. 启动方式

**设计约束：** Bot Runtime 空闲内存占用目标控制在 ~5-10 MB。该目标约束 Runtime 的模块设计——懒初始化（SqliteStore、Wasmtime Engine 等重量级模块按需加载）、最小化默认缓存、零后台轮询线程。

> **验证方式**：Phase 3 将通过 `MemoryMetrics` 结构在 Debug 模式下实时报告内存占用，并提供 `/metrics` 端点供 Desktop App 展示。Phase 2 通过 Rust 标准库 `alloc::alloc::GlobalStats`（nightly）或外部 `jemalloc` 统计进行开发阶段验证。目标约束的验证不在 Phase 2 功能范围内。

```bash
acowork-runtime \
    --agent-id com.example.weather \
    --package-path /path/to/agent-package \
    --work-dir /home/user/.local/share/agent-gateway/agents/com.example.weather/workspace \
    --config-dir /home/user/.local/share/agent-gateway/agents/com.example.weather/config
```

**启动参数说明：**

| 参数 | 必需 | 说明 |
|------|------|------|
| `--agent-id` | 是 | Bot 标识符，与 manifest 中一致 |
| `--package-path` | 是 | .bot 包路径（解压后的目录或 ZIP 文件） |
| `--work-dir` | 是 | Bot 工作区目录（含 data/、config/、memory/） |
| `--mqtt-port` | 否 | Gateway 嵌入式 MQTT broker 端口（与 Gateway 通讯；见 ADR-033） |
| `--http-port` | 否 | Runtime localhost HTTP 端口（Desktop 反向代理发现；见 ADR-033） |
| `--config-dir` | 否 | 用户配置目录（默认取 work-dir/config/） |
| `--dev-mode` | 否 | 启用开发者模式（Debug 协议） |
| `--debug-port` | 否 | Debug WebSocket 端口（默认 19878） |
| `--log-level` | 否 | 日志级别（trace/debug/info/warn/error，默认 info） |

Runtime 经由 `--mqtt-port` 连接 Gateway 的嵌入式 MQTT broker；MQTT 是 ADR-033 之后的统一传输协议（替代了历史的 gRPC 双向流）。详见 [06-communication.md](./06-communication.md) 与 [ADR-033](../adr/zh/ADR-033-mqtt-replace-grpc.md)。

**身份信息获取：**

用户身份信息（name、city、language 等）**不通过命令行参数传入**（避免 `/proc/<pid>/cmdline` 泄露）。Runtime 启动后通过 gRPC AgentHello 握手，由 Gateway 将 UserProfile 作为 AgentHelloResult 的一部分注入。流程：

```
1. Runtime 启动，连接 Gateway gRPC 端点
2. 发送 AgentHello（含 agent_id、package_path、work_dir）
3. Gateway 返回 AgentHelloResult，其中包含：
   - user_identity: { name, city, language, ... }
   - provider_list: LLM 配置列表
   - mcp_list: MCP 服务器列表
   - search_list: 搜索提供商列表
   - 对应 Key Vault 条目（加密传输）
4. Runtime 存入 AgentCore，供 Context Builder 使用
```

## 2. 内部结构

```
Agent Runtime 二进制
├── Package Loader      # 解析 .agent ZIP，加载 manifest + prompts + skills（config 在 workspace/config，不在包级）
├── AgentCore (Arc)     # 跨 Session 共享状态
│   ├── Provider       # LLM Provider（直连 LLM API）
│   ├── Tool Registry  # 工具注册表
│   ├── Manifest       # Agent 配置
│   ├── Budget Guard   # 预算管理
│   ├── Model Caps     # Gateway 推送的模型能力
│   └── Agent Config   # workspace/config/agent_config.json 持久化
├── SessionManager      # 多 Session 管理
│   └── HashMap<SessionId, SessionHandle>
│       └── SessionHandle { inbound_tx, task: JoinHandle, on_chunk }
├── SessionTask (per session, 独立 tokio task)
│   └── SessionState   # per-session 独立状态
│       ├── History Manager     # 对话历史管理（token 预算、trim）
│       ├── Loop Detector      # 循环检测
│       ├── Model Override     # per-session 模型选择
│       ├── Conversation       # JSONL 持久化
│       └── Token Usage        # per-session 用量
├── gRPC Client         # Gateway 通信（双向流，替代旧版 IPC）
├── Context Builder     # 组装 system prompt（identity + autobiographical + tools + skills + memory + workspace context）
├── Tool Dispatcher     # 解析 LLM 输出的 tool_calls，路由到工具实现
│   ├── Built-in Tools  # 内置工具
│   ├── MCP Tools       # MCP 协议工具（按需加载）
│   ├── WASM Tools      # WASM 工具（Wasmtime 沙箱执行）
│   └── Gateway Tools   # 需 Gateway 协调的操作
├── Permission Checker  # 根据 manifest 权限表校验工具调用权限
├── Approval Gate       # 高风险工具执行前的用户确认
├── Memory Manager      # 记忆生命周期管理
│   ├── Middleware Chain   # 记忆中间件
│   ├── Store Backend      # MemoryStore trait 实现
│   └── RagClient (opt)    # RAG 检索客户端
├── SqliteStore (嵌入式)     # 私有 Memory 存储引擎（单文件 SQLite）
├── Skill Loader        # 加载 Skills（SKILL.md + SQLite 经验层）
├── Debug/DevMode       # 调试协议（HTTP RPC + MQTT events，ADR-048；可选）
├── MCP Manager         # MCP 服务器连接管理（按需激活）
├── Search Config       # Web 搜索提供商配置（从 Gateway 同步）
└── Budget Manager      # 本地预算预检 + 用量上报
```

## 3. 主循环

Bot Runtime 的核心是 LLM 交互循环：

```
用户消息 / Intent / 定时触发 / 中断消息注入
       │
       ▼
┌──────────────────────────────────────────────┐
│  Agent Runtime 主循环 [iteration: 0..N]       │
│                                               │
│  ⓪ 消息合并（从 InboundQueue drain）         │
│     ├─ UserMessage → 追加到 History           │
│     ├─ SystemNotification → 追加到 History    │
│     │   （identity_update / capability_update）│
│     ├─ IntentMessage → 追加到 History         │
│     └─ 无新消息 → 跳过                       │
│                                               │
│  ① 预算预检
│     ├─ 本地预算缓存不足 → 按 action_on_exhaust
│     │   处理（stop / fallback / warn）
│     └─ 预算耗尽且无 fallback → 终止循环  ──► END
│
│  ② 构建上下文（按优先级拼接，见 3.1）
│     ├─ System Prompt (from prompts/)
│     ├─ Identity Context (from Gateway 注入)
│     ├─ Autobiographical (History Manager 触发
│     │   压缩，详见 3.1)
│     ├─ Workspace Context (from Gateway 推送)
│     ├─ Tool Definitions (from manifest.tools)
│     │   └─ 仅含 manifest 声明的工具（含 RAG 工具，
│     │      仅 manifest 声明 type=rag 时注册）
│     ├─ Capability Overview (from Gateway 推送)
│     ├─ Skill Instructions (from skills/)
│     ├─ Memory Retrieve → MemoryManager.retrieve()
│     │   ├─ SQLite 记忆通道（始终执行）
│     │   │   hybrid_search + graph_expand
│     │   └─ RAG 通道（仅 manifest 声明 rag 时）
│     │       RagClient.query(用户消息, top_k=3)
│     │       超时(5s)/不可达 → 跳过，不阻塞
│     ├─ Memory Inject → MemoryManager.inject()   
│     │   按 token 预算裁剪并格式化记忆上下文
│     │   结果按来源标注 [Memory] / [RAG:<name>]
│     └─ 对话历史 (from History Manager)
│
│  ②.5 上下文压缩（Token 预算管理）
│     ├─ Token 使用率 < 70% → 日志记录，不干预
│     ├─ Token 使用率 ≥ 80% → LLM 摘要（Compact Model）
│     │   ├─ 完整上下文输入（不折叠/截断任何内容）
│     │   ├─ 保护 system prompt + 最近 2-3 轮
│     │   ├─ 压缩中间段为结构化摘要
│     │   ├─ 完整历史归档至临时文件
│     │   └─ ⚡ 触发 Episode 提炼（被压缩的消息）
│     └─ Token 使用率 ≥ 95% / API 报 ContextOverflow
│         → emergency_trim（保留最后 N 条非 system）
│         → 重建请求，重试
│
│                                               │
│  ③ 调用 LLM (直连 API)                        │
│     ├─ RateAcquire 速率协调                    │
│     │   ├─ granted: true → 继续               │
│     │   ├─ granted: false + retry_after_ms     │
│     │   │   → 等待后重试（不计入 LLM 重试次数）│
│     │   └─ 429 余额不足（不可重试）→ 终止      │
│     ├─ streaming 或 blocking                   │
│     │   └─ streaming 中检测到 tool_calls →     │
│     │     中断 streaming，已输出 text 暂存     │
│     │     → 进入 ④                            │
│     └─ 失败 → 重试或 fallback（见第 7 节）     │
│                                               │
│  ④ 解析响应                                    │
│     ├─ text → 返回结果/回复用户  ──────────► END│
│     └─ tool_calls → ④.5                       │
│                                               │
│  ④.5 Tool Call 去重（见 §7.5）                │
│     └─ 同轮相同 (tool_name, params) → 跳过    │
│                                               │
│  ⑤ 工具调度与执行（并行）                      │
│     ├─ Permission Check (manifest 权限表)      │
│     ├─ Approval Gate（高风险工具，见 §7.4）    │
│     │   └─ requires_approval: true → Gateway   │
│     │     发送 PermissionRequest → 等待用户确认│
│     │       ├─ 用户拒绝 → 错误信息返回 LLM    │
│     │       └─ 用户超时 → 同上                │
│     ├─ 并行执行所有 tool_calls（join_all）     │
│     │   ├─ Built-in Tool → 直接执行            │
│     │   ├─ RAG Tool → RagClient HTTP 调用（仅 │
│     │   │   manifest 声明 rag 时注册）         │
│     │   ├─ WASM Tool → Wasmtime 沙箱执行       │
│     │   └─ Gateway Tool → Socket 调用          │
│     └─ 统一收集执行结果，失败 → 错误信息作为    │
│        tool result                             │
│                                               │
│  ⑥ 结果追加到历史                              │
│     └─ Memory Record → MemoryManager.record()  │
│         异步记录本轮交互到经历层（episode）       │
│         （非 Episode 提炼，仅事件记录）          │
│                                               │
│  ⑦ 用量上报（异步，不阻塞）                    │
│     ⚡ Session 结束时 → 触发全局摘要 Episode  │
│                                               │
│  ⑧ 循环检测（见 §3.2）                        │
│     ├─ Exact Repeat / Ping-Pong / No Progress  │
│     └─ 三级渐进响应：Warning → Block → Break   │
│                                               │
│  ⑨ 迭代计数检查                                │
│     └─ 达到 max_iterations → 强制终止 ──────► END│
│                                               │
│  └─→ 回到 ⓪（下一轮迭代，先检查消息队列）     │
└──────────────────────────────────────────────┘
```

### 3.1 上下文构建规则

Prompt Builder 按以下顺序拼接上下文，越靠前优先级越高（LLM 对靠前内容的注意力权重更高）：

| 顺序 | 部分 | 来源 | 说明 |
|------|------|------|------|
| 1 | System Prompt | `prompts/system.md` + `prompts/rules.md` | Bot 身份定义（角色通用属性）+ 业务场景规则（系统记录、工具边界、交付位置、审批门、工作流）。段序由运行时固定为 `system.md` → `rules.md` → 其余按文件名，见 [ADR-120](../../adr/zh/ADR-120-system-and-rules-prompt-split.md)；不可被后续覆盖 |
| 2 | Identity Context | Gateway 注入 | 用户身份信息（name、city 等），Bot "认识"用户 |
| 2.5 | Autobiographical | AutobiographicalNode（`label=Autobiographical`） | Bot 自我认知（Identity/Capability/Limitation），注入上限 200 token。History Manager 在构建上下文时检测 History 节点数量，超过 10 条时自动触发规则引擎合并（按时间线拼接事件描述、去重、截断至 200 token，零 LLM 调用），合并由 Runtime 的后台任务执行，不需要用户干预。Phase 3 可升级为 LLM 语义摘要 |
| 2.8 | Workspace Context | Gateway 推送 | 工作区环境信息（当前选中 + 高权重 Top2，最多 3 个） |
| 3 | Tool Definitions | `manifest.toml [tools]` | 转换为 JSON Schema 格式的工具描述，供 LLM 调用 |
| 4 | Capability Overview | Gateway 推送 | 已安装 Bot 及其能力摘要，供 LLM 知道可以向谁协作 |
| 5 | Skill Instructions | `skills/*/SKILL.md` + SQLite 经验层 | 可选技能指令，扩展 Bot 的行为模式。详见 [13-skill-system.md](./13-skill-system.md) |
| 6 | Memory Context | `MemoryManager.retrieve()` + `MemoryManager.inject()` | 记忆检索与注入。通过 MemoryStore trait 的 `hybrid_search` + `graph_expand` 检索 SQLite 记忆通道；若 manifest 声明 RAG（`rag_client: Option<Arc<RagClient>>`），并行查询 RAG 通道（用户消息作 query，top_k=3，超时 5s 降级）；结果按来源标注 `[Memory]` / `[RAG:<name>]`，按 token 预算裁剪注入。详见 [05-memory.md](./05-memory.md) §10、[00-prd.md](./00-prd.md) §1.13.1 |
| 7 | Conversation History | History Manager | 当前对话的完整消息序列 |

#### 2.8 Workspace Context（工作区上下文）

Gateway 通过 IPC 推送工作区环境信息到 Runtime。该上下文包含 Bot 的主工作区路径和用户授权的项目目录列表。

**筛选策略**：为避免工作区列表过长导致上下文膨胀，采用动态筛选：
- 当前选中的工作区（`is_current = true`）始终包含
- 其余工作区按归一化权重排序，取 Top 2：
  - `normalized_count = select_count / max_select_count`（归一化到 [0, 1]）
  - `recency_score = 1.0 / (1.0 + days_since_last_select)`（值域 (0, 1]）
  - `score = normalized_count * 0.3 + recency_score * 0.7`
- 最多注入 3 个工作区目录

**注入格式**：

```
## Workspace Environment

Primary workspace (agent home): /path/to/agent/workspace

### User Project Directories
| # | Alias | Path | Access | Current |
|---|-------|------|--------|---------|
| 1 | my-project | /home/user/projects | read-write | * |

When performing file operations, use the directory marked as Current (*) by default.
All listed directories are authorized for access at the indicated permission level.
```

**触发时机**：
- Bot 启动时 Gateway 主动推送一次
- 用户通过 Desktop App 切换当前工作区时实时推送更新

**Token 预算分配与截断策略：** 当上下文总长度接近模型限制时，采用三阶段策略：

1. **70% 监控**：通过 ContextUsage 事件向 Gateway 报告 token 使用率，不做任何干预。
2. **80% LLM 摘要（Compaction）**：使用 Compact Model 对完整对话历史做 LLM 摘要（`compact_via_llm`）。摘要文本同时用于：(a) 替换内存中间段（`replace_middle_with_summary`，保留 system prompt + 最后 3 轮），(b) 写入 SQLite 经历层（摘要即蒸馏，ADR-011）。Compaction 完成后设置 `is_compacted = true`，新用户消息到达时重置为 `false`。
3. **95% emergency_trim**：保留 system prompt + 最后 4 条非 system 消息，作为安全网。仅在 LLM 摘要无法执行（API 报错）或使用率飙升至 95% 时使用。

> **设计决策**：上下文压缩是一个语义理解任务，只有 LLM 能可靠判断哪些信息可以丢弃。程序化策略（字符截断、FIFO、角色折叠）本质是用 proxy 指标替代语义理解，必然失效。详见 [ADR-010](../../adr/zh/ADR-010-context-compression-simplification.md)。Compaction 与 Distillation 统一为单次调用：同一个摘要文本既替换内存（压缩上下文），又写入 SQLite 经历层（产生经历记忆）。详见 [ADR-011](../../adr/zh/ADR-011-compaction-as-distillation.md)。

System Prompt（1）、Identity Context（2）、Autobiographical（2.5）、Workspace Context（2.8）、Tool Definitions（3）始终保留，不参与裁剪。

#### 3.1.1 包级 LLM Prompt 覆盖（ADR-063）

> **ADR-063**：Bot 包可声明 9 个 LLM prompt override 文件（`prompts/<name>.md`），覆盖
> 内置常量（`crate::prompt::*` 与 `acowork-memory` 模块的 LLM 指令）。这些文件**不参与**
> §3.1 的主 prompt 拼接（它们的语义是"任务指令性 prompt"而非"身份定义"），只通过
> `load_optional_prompt` 在对应 LLM 调用点注入。

**9 个 canonical 文件名**（大小写敏感，不含 `.md` 扩展名）：

| 文件名（无 .md） | 覆盖的内置常量 | 用途 |
|---|---|---|
| `summary` | `COMPACTION_SYSTEM_PROMPT` | 摘要压缩指令（ADR-053） |
| `fallback` | `PROMPT_BUILDER_FALLBACK` | 主 prompt 拼接后兜底 |
| `search` | `SEARCH_SYSTEM_PROMPT` | perplexity 联网搜索 system prompt |
| `compact-template` | `COMPACT_PROMPT` | 全文压缩模板（含 `{messages_text}`） |
| `title` | `TITLE_PROMPT` | 会话标题生成（含 `{language}` + `{user_message}`） |
| `extraction` | `EXTRACTION_SYSTEM_PROMPT` | SQLite 记忆层 记忆抽取（live reload 延后，§6.1） |
| `conflict-classification` | `CONFLICT_CLASSIFICATION_PROMPT` | SQLite 记忆层 冲突分类（live reload 延后） |
| `generalization` | `GENERALIZATION_PROMPT` | SQLite 记忆层 泛化（live reload 延后） |
| `abstention` | memory `DEFAULT_ABSTENTION_PROMPT` | RAG 弃答（live reload 延后） |

**加载链（三阶段）**：

1. **Phase A**（`startup/agent_init.rs`）：遍历 `OVERRIDABLE_PROMPTS`，调用
   `load_optional_prompt(package_dir, filename)` 一次性加载到 `AgentBootContext`。
   文件不存在 / 全空白 → 该字段保持 `None`（保持原内置行为）。
2. **Phase B**（`startup/session_init.rs`）：把 `ctx.*` 写入 `AgentCore` 对应的
   `Arc<RwLock<Option<String>>>` 字段。Session 启动时通过 `Clone for AgentCore`
   拿到共享 Arc（reference +1），写一处所有 session 都看到。
3. **Phase C**（`startup/subsystems.rs`，DevMode 启用时）：调用
   `DebugService::reload_prompts`，从磁盘重新加载并写入 Arc，使 Debug 面板的"Reload"
   按钮可以即时生效。

**关键不变量**：

- 9 个 prompt 字段在 `AgentCore` 中均为 `Arc<std::sync::RwLock<Option<String>>>`；
  accessor（`compaction_prompt()` 等）每次调用都通过 RwLock 读最新值，**不缓存**。
- `build_system_prompt_with_mode` 排除这 9 个文件名（大小写敏感，精确匹配），但
  其它 markdown 文件（包括 `summary.txt` / `SUMMARY.md` 等变体）仍按普通 section
  拼接 — 主 dialog 不会被 override 文件污染。
- 写入路径（PUT `/agents/{id}/prompts/{name}`）做原子 rename（`tmp` + `rename`），
  1 MiB 大小上限、空白拒绝、路径穿越拦截 — Runtime 的 `http/prompts.rs` 是
  白名单与原子语义的唯一权威，Gateway 仅做透明反代（[protocols/zh/http.md §5.6](../../protocols/zh/http.md#56-包级-llm-prompt-覆盖-prompts)）。
- Live reload 只覆盖 `compaction / fallback / search / title / compact-template`
  这 5 个 runtime 内调用点；SQLite 记忆层 / memory 的 4 个常量是模块级 `&'static str`，
  重新加载需重启 Runtime（ADR-063 §6.1 已记录此限制）。

### 3.2 循环检测策略

防止 LLM 陷入重复调用工具的死循环。采用三种检测模式 + 三级渐进响应。

**三种检测模式：**

| 模式 | 检测规则 | 默认阈值 | 典型场景 |
|------|---------|---------|---------|
| Exact Repeat | 连续 N 次相同的 `(tool_name, params)` | 3 | LLM 反复用相同参数调用同一工具 |
| Ping-Pong | 两个工具交替调用 A→B→A→B 达 N 个周期 | 4 | tool_A 的结果触发 tool_B，tool_B 又触发 tool_A |
| No Progress | 同一工具不同参数但结果哈希相同，连续 N 次 | 5 | LLM 换着花样调同一工具但得不到新信息 |

**三级渐进响应（每种模式独立计数）：**

| 级别 | 触发条件 | 行为 |
|------|---------|------|
| Warning | 第一次命中检测阈值 | 向对话历史注入系统警告消息："检测到重复调用 [tool_name]，请尝试不同的方法。"，LLM 在下一轮看到警告后自主调整策略。迭代继续。 |
| Block | 第二次命中（Warning 后再次触发） | 拒绝本次工具调用，构造错误 tool result："工具调用被阻止：循环检测触发。"，LLM 被迫换工具或改参数。迭代继续。 |
| Break | 第三次命中 | 终止当前迭代，将循环检测信息作为最后的 assistant 消息写入历史，向用户返回提示。退出主循环。 |

**配置：**

```toml
# manifest.toml 中可覆盖默认值
[loop_detection]
exact_repeat_threshold = 3     # Exact Repeat 连续相同调用阈值
ping_pong_threshold = 4        # Ping-Pong 交替周期阈值
no_progress_threshold = 5      # No Progress 无进展阈值

# 精细控制（可选，默认继承 loop_detection 配置）
[loop_detection.exact_repeat]
enabled = true
[loop_detection.ping_pong]
enabled = true
[loop_detection.no_progress]
enabled = true  # No Progress 需要计算结果哈希，成本略高，可按需关闭
```

**实现要点：**

- 检测范围是步骤⑥追加到历史后的完整调用序列，不是"当前迭代的 tool_calls"——因为步骤⑥在步骤⑧之前，所以不会漏检
- 每种模式的计数器在连续命中中断时重置为 0：如果 LLM 成功调用了不同工具（未触发同一模式），计数器归零。三级响应的升级仅在同一模式的连续命中内生效
- No Progress 的结果哈希使用 tool result 的前 256 字符 + 长度的组合哈希，避免对大结果全文计算
- Warning 消息不计入用户的对话历史 token 预算（属于系统消息）

### 3.3 InboundQueue（消息注入队列）

AgentLoop 维护一个 `mpsc::channel` 作为入站消息队列，允许外部在循环运行期间向 Bot 注入消息。每轮迭代开始前（步骤⓪）drain 队列，将待处理消息合并到对话历史。

**消息类型：**

| 类型 | 来源 | 处理方式 |
|------|------|---------|
| `UserMessage` | Desktop App / CLI（用户在 Bot 运行中追加内容）| 作为新的 `user` 消息追加到 History |
| `SystemNotification` | Gateway push（identity_update / capability_update）| 作为 `system` 消息追加，LLM 下轮可见 |
| `IntentMessage` | 其他 Bot（通过 Gateway 路由）| 作为 `user` 消息（包含 Intent 元数据）追加 |

**设计要点：**

- 队列容量建议 64，超出时背压（sender 阻塞）而非丢弃，避免消息丢失
- drain 操作非阻塞（`try_recv` 而非 `recv`）——队列为空时零等待直接进入步骤①
- 消息注入不打断正在执行的步骤，只在迭代边界（步骤⓪）生效
- `inbound_tx: mpsc::Sender<InboundMessage>` 由 Runtime 的 IPC 层持有，Gateway 通过 push 消息调用它
- 与"任务级并行"区分：InboundQueue 的目标是**上下文补充**，不创建新的执行分支

### 3.4 工具并行执行

步骤⑤从串行改为并行执行，原因：LLM 一次响应中可能包含多个独立的 `tool_calls`（如同时查询天气 + 查询日历），它们之间没有数据依赖，串行执行会放大总延迟。

**执行流程：**

```
// 串行（旧）
for tool_call in deduped_calls {
    let result = execute_tool(tool_call).await?;
    results.push(result);
}

// 并行（新）
let futures: Vec<_> = deduped_calls.iter()
    .map(|call| execute_tool(call))
    .collect();
let results = futures::future::join_all(futures).await;
```

**权限检查与 Approval Gate：** 仍然串行执行于并行执行**之前**——先对所有 tool_calls 批量做 permission check，需要用户确认（`requires_approval: true`）的工具先通过 Approval Gate，全部确认后再并行执行。这样避免在等待用户确认时浪费时间串行执行其他工具。

**并发安全（设计约束）：** 每个工具调用获取独立的资源句柄，不共享可变状态。设计约束如下：
- Built-in Tool：必须设计为可安全并发调用（若引入缓存/连接池需额外同步）
- WASM Tool：每次调用创建独立的 Wasmtime Instance
- Gateway Tool：每次 IPC 请求携带独立 `call_id`，Gateway 侧无状态路由

**失败处理：** `join_all` 等待所有工具完成（不短路），单个工具失败不影响其他工具的执行结果，失败的 tool_call 返回包含错误信息的 `ToolResult`，让 LLM 在下一轮决定如何处理。

### 3.5 工具并行 + 超时/取消语义

并行执行引入了三层超时/取消的交互语义，需要明确定义各层职责，避免实现分歧：

**三层超时定义：**

| 层级 | 控制点 | 触发时机 | 行为 |
|------|--------|---------|------|
| 迭代整体超时 | 主循环层（步骤⑨）| `iteration_timeout_ms` 到达 | drop所有未完成的 tool future，返回已收集的结果，迭代终止 |
| 单工具超时 | 工具执行层（步骤⑤内部）| 单个 `execute_tool` 调用超时 | 该工具返回 `ToolResult { ok: false, error: "timed out" }`，其他工具继续 |
| LLM 调用超时 | 步骤③ | LLM 响应超过 timeout | 整个步骤③ abort，该迭代终止（不进入步骤⑤） |

**迭代整体超时时 join_all 的行为：**

当迭代整体超时触发时，主循环通过 `tokio::time::timeout` 或 `tokio::spawn` 的 abort handle 取消步骤⑤的 future。此时 join_all 内部会产生 `JoinError`（未被 await 的 future 被 drop），处理策略为：
- 超时前已完成执行的工具：结果正常收集（符合预期）
- 超时前已启动但未完成的工具：被 drop，结果丢失，不写入 History
- 超时触发前已经收集到一部分结果：这些结果仍然有效

> **注意**：这是 join_all "等待所有工具完成" 与 "迭代超时直接 drop future" 之间的语义边界。设计上选择**不等待未完成工具**——因为超时意味着本次迭代已经超出预期时间，继续等待会进一步延迟响应给用户。

**实现约束（spawn + select 方案）：**

要实现"迭代超时时部分工具结果仍可用"的语义，不能用 `timeout(join_all(...))`（Rust 中该组合要么全返回要么全 drop），需要改用 `tokio::spawn` 每工具独立运行 + `tokio::select!` 轮询：

```rust
use tokio::sync::mpsc;

let (tx, mut rx) = mpsc::channel::<(usize, ToolResult)>(deduped_calls.len());

// ① 为每个 tool_call spawn 一个独立 task
let handles: Vec<tokio::task::JoinHandle<()>> = deduped_calls
    .iter()
    .enumerate()
    .map(|(idx, call)| {
        let tx = tx.clone();
        tokio::spawn(async move {
            let result = tokio::time::timeout(
                Duration::from_millis(TOOL_TIMEOUT_MS),  // 单工具超时
                execute_tool(call)
            ).await.unwrap_or_else(|_| ToolResult {
                ok: false,
                error: "tool execution timed out"
            });
            let _ = tx.send((idx, result)).await;  // 结果写入 channel
        })
    })
    .collect::<Vec<_>>();

// ② 迭代整体超时控制：减去步骤①②③④已消耗时间
let deadline = Instant::now() + Duration::from_millis(iteration_timeout_ms - elapsed);
let mut results: Vec<(usize, ToolResult)> = Vec::with_capacity(deduped_calls.len());
let total = deduped_calls.len();

while results.len() < total {
    tokio::select! {
        // 有结果到达则收集
        entry = rx.recv() => {
            if let Some((idx, result)) = entry {
                results.push((idx, result));
            }
        }
        // 迭代整体超时：abort 未完成 task，停止等待
        _ = tokio::time::sleep_until(deadline.into()) => {
            for handle in handles {
                handle.abort();  // 不等待，立即取消
            }
            break;
        }
    }
}

// ③ 按原顺序组装结果，未完成的 slot 填入超时错误
results.sort_by_key(|(idx, _)| *idx);
let tool_results: Vec<ToolResult> = (0..total)
    .map(|i| {
        results.iter()
            .find(|(idx, _)| *idx == i)
            .map(|(_, r)| r.clone())
            .unwrap_or_else(|| ToolResult {
                ok: false,
                error: format!(
                    "iteration timed out, tool {} not completed",
                    deduped_calls[i].name
                )
            })
    })
    .collect();
```

**关键约束：**
- 单工具超时在每个 spawn 内独立处理（`tokio::time::timeout`），独立于迭代整体超时
- 迭代整体超时通过 `tokio::select!` + `deadline` 控制，超时后调用所有 `handle.abort()`，不等待 join
- `handle.abort()` 后该 slot 在结果 Vec 中填充明确的超时错误，不记 History（等 LLM 下一轮决定如何处理）
- 迭代超时时，应在 History 中记录一条系统消息：`"[iteration timed out after N ms, N tool(s) not completed]"`，其中 N 为未完成的工具数
- `rx.recv()` 循环中通过 `while results.len() < total` 防止 select 空转，确保在收集到全部结果后立即退出循环

### 3.6 循环退出条件

| 条件 | 触发时机 | 行为 |
|------|---------|------|
| LLM 返回纯 text | 步骤 ④ | 正常结束，返回结果给用户 |
| 预算耗尽 | 步骤 ① | 按 `action_on_exhaust` 处理；stop 则终止 |
| 达到 max_iterations | 步骤 ⑨ | 强制终止，返回已执行结果 |
| 循环检测 Break | 步骤 ⑧ | 三级响应中的 Break 级别，终止迭代并通知用户 |
| 单轮迭代超时 | 步骤 ③/⑤ | 超时后终止当前迭代 |
| Gateway 停止信号 | 任意步骤 | 优雅退出，保存当前状态 |
| LLM 调用重试耗尽 | 步骤 ③ | 无 fallback provider 时终止 |
| Context exceeded 恢复失败 | 步骤 ③ | emergency_trim 安全网无法满足时终止（见 §7.1） |

## 4. Runtime 默认配置

当 manifest.toml 中未显式声明时，Runtime 使用以下默认值：

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `max_iterations` | 50 | 单次对话最大迭代次数（可通过 Gateway `RuntimeConfigUpdate` 运行时覆盖） |
| `iteration_timeout_ms` | 30000 | 单轮迭代超时（含 LLM 调用 + 工具执行） |
| `history_max_tokens` | 128000 | 对话历史上限（超过后触发 trim/compress） |
| `loop_detection.exact_repeat_threshold` | 3 | Exact Repeat 检测阈值 |
| `loop_detection.ping_pong_threshold` | 4 | Ping-Pong 交替周期阈值 |
| `loop_detection.no_progress_threshold` | 5 | No Progress 无进展阈值 |
| `loop_detection.no_progress.enabled` | true | No Progress 检测开关（需计算结果哈希） |
| `llm.routing.retry.max_attempts` | 3 | LLM 调用重试次数 |
| `llm.routing.retry.backoff` | "exponential" | 重试退避策略 |
| `llm.routing.retry.max_wait_ms` | 30000 | 重试最大等待时间（RateAcquire retry_after 上限） |
| `approval.timeout_ms` | 60000 | 高风险工具用户确认超时 |

## 5. Built-in Tools 清单

详见 [12-tool-system.md](./12-tool-system.md) 第 2 节。

## 6. Gateway Tools（需 Gateway 协调的操作）

详见 [12-tool-system.md](./12-tool-system.md) 第 4 节。

## 7. 错误处理策略

### 7.1 LLM 调用失败

```
LLM 调用失败（网络超时 / API 错误 / token 超限）
       │
       ▼
分类错误类型：
  ├─ Context Window Exceeded（上下文溢出）
  │   ├─ Reactive Recovery（见下方）
  │   └─ 恢复失败 → 终止循环
  │
  ├─ Rate Limited（429）
  │   ├─ 可重试限流（并发/频率限制）
  │   │   ├─ 解析 Retry-After header
  │   │   ├─ 等待 min(retry_after, max_wait_ms)
  │   │   ├─ 尝试 API Key 轮换（Vault 多 Key）
  │   │   └─ 计入重试次数
  │   └─ 不可重试限流（余额不足/套餐限制）
  │       └─ 立即终止，不计入重试次数
  │
  ├─ 网络超时 / 500 / 502（可重试错误）
  │   ├─ 按 manifest.llm.routing.retry 配置重试
  │   │   ├─ max_attempts (默认 3)
  │   │   └─ backoff: exponential
  │   └─ 重试成功 → 继续
  │
  └─ 其他错误（401/403 等，不可重试）
      └─ 立即终止
       │
       ▼
重试耗尽 → 检查 fallback:
     ├─ manifest 中配置了 fallback provider → 切换到 fallback
     │   └─ fallback 也失败 → 终止循环，返回错误信息
     └─ 无 fallback → 终止循环，返回错误信息
```

**RateAcquire 行为（步骤③速率协调）：**

| 场景 | 行为 | 计入 LLM 重试次数 |
|------|------|------------------|
| granted: true | 继续调用 | 不适用 |
| granted: false + retry_after_ms | 等待指定时间后重试 RateAcquire（最多 3 次） | 否（独立机制） |
| granted: false + 无 retry_after | 等待 backoff 基础时间（默认 1s）后重试（最多 3 次） | 否 |
| RateAcquire 重试 3 次仍 granted: false | 降级为 granted: true（由 LLM API 的 429 兜底） | 否 |
| 429 余额不足/套餐限制（识别特定错误码如 1113/1311） | 立即终止 | 否 |

**Context Exceeded Reactive Recovery（上下文溢出恢复）：**

当 LLM API 返回 context window exceeded 错误时，Runtime 尝试恢复：

```
Context Window Exceeded 错误
       │
       ▼
Step 1: Emergency History Trim（删除最早的非 system 消息，保留最近 4 条）
       │
       ├─ 重新估算 token → 满足 → 重试 LLM
       │
       ▼
Step 2: 报错终止，提示用户对话过长
```

Reactive recovery 与步骤 ②.5 的三阶段压缩互补：LLM 摘要（80%）和 emergency_trim（95%）在调用前预防（覆盖大部分场景），Reactive 在 API 报错后兜底。Recovery 最多执行 1 次（不循环恢复），避免无限重试。

### 7.2 工具执行失败

工具执行失败**不终止循环**。错误信息作为 tool result 返回给 LLM，由 LLM 决定下一步（换参数重试、换工具、或放弃）：

```
工具执行失败（WASM 崩溃 / 权限不足 / 超时）
       │
       ▼
构造 error tool result:
  { "error": true, "message": "工具执行超时", "tool_name": "http_request" }
       │
       ▼
追加到对话历史 → LLM 在下一轮迭代看到错误信息并自主决策
```

### 7.3 Gateway 断连

```
Gateway Socket 断连
       │
       ▼
进入优雅降级模式:
  ├─ 本地 LLM 推理继续（已有 API Key）
  ├─ 缓存待上报的用量数据
  ├─ Intent 收发暂停（无法路由）
  ├─ 定期尝试重连 Gateway
  │
  └─ 超过 reconnection_timeout (默认 60s) → 保存状态后退出
```

### 7.4 Approval 机制（高风险工具确认）

与 08-security.md §9 对应。高风险工具（文件写入、网络请求、Intent 发送、shell 执行）在执行前需用户确认。

**触发条件：** 当 manifest.toml 中工具声明了 `requires_approval: true`，或 Permission Checker 判定操作属于高风险类别时触发。

**流程（在步骤⑤ Permission Check 之后执行）：**

```
Permission Check 通过 → Approval Gate 检查
       │
       ├─ requires_approval: false → 直接执行
       │
       └─ requires_approval: true → 发送 PermissionRequest 到 Gateway
            │
            ├─ Gateway 通过 HTTP API 转发到 Desktop App
            ├─ Desktop App 弹出确认对话框：
            │     "Agent [name] 请求执行 [tool_name]：[参数摘要]"
            │     [允许] [拒绝]
            │
            ├─ 用户允许 → 继续执行工具
            ├─ 用户拒绝 → 构造错误 tool result 返回给 LLM
            └─ 超时 (默认 60s) → 视为拒绝，返回超时错误给 LLM
```

**Approval Gate 不阻塞 LLM 推理：** 当等待用户确认时，Runtime 进程不退出，其他 Bot 不受影响。超时后 LLM 收到拒绝结果，可以自主调整策略（换工具、换参数）。

**Desktop App 不可用时：** 如果 Desktop App 未连接（纯 CLI 模式），Approval 降级为 manifest 配置的默认策略：`approval_fallback = "allow" | "deny"`。默认 `deny`（安全优先）。Gateway 转发失败（HTTP 超时/错误/Desktop App 响应格式异常）时同样按 `approval_fallback` 策略处理，并记录错误日志。

### 7.5 Tool Call 去重

防止 LLM 在单次响应中返回重复的 tool_calls。

**去重规则：** 步骤④输出 tool_calls 列表 → 步骤④.5 对列表做 HashSet 去重——如果出现多个相同的 `(tool_name, params)` 组合，只保留第一个，后续的跳过并在历史中记录去重日志 → 输出去重后的 tool_calls 列表到步骤⑤。

去重是步骤⑧循环检测的补充：循环检测处理跨迭代的重复调用，Tool Call 去重处理单次响应内的重复调用。

## 8. 设计决策记录

| 决策 | 选择 | 理由 |
|------|------|------|
| 身份信息传输 | 通过 Gateway Socket 推送 | 避免命令行参数泄露（/proc 可读） |
| 工具执行失败处理 | 返回错误给 LLM 决策 | LLM 有能力自主调整策略，比直接终止更灵活 |
| Gateway 断连 | 优雅降级而非立即退出 | 保留本地推理能力，短暂断连不影响体验 |
| 循环检测 | 三种模式 + 三级渐进响应 | 比单阈值 terminate 更精细：Warning 让 LLM 自主修正，Block 阻止无意义的重复，Break 作为最后手段。Ping-Pong 和 No Progress 覆盖更复杂的死循环模式 |
| 上下文裁剪 | 三阶段 LLM 驱动：70% 告警 → 80% Compaction 摘要（Compact Model，摘要即蒸馏）→ 95% emergency_trim | 仅 LLM 能可靠判断信息可丢弃性，Compaction 与 Distillation 统一（ADR-010 + ADR-011） |
| Approval 机制 | Gateway 转发 → Desktop App 确认 | 高风险操作需用户知情同意；CLI 模式下降级为 manifest 配置的默认策略 |
| Tool Call 去重 | 单轮 HashSet 去重 | 成本极低，防御 LLM 单次响应内重复调用 |
| Rate Limit 分层 | 区分可重试限流 / 不可重试余额不足 | 避免对余额不足的错误做无意义重试 |
| Streaming + tool_calls | 检测到 tool_calls 立即中断 streaming | 标准 streaming + function calling 处理模式（OpenAI/Anthropic SDK 均采用），已输出的 text 暂存到历史 |
| Autobiographical 压缩 | History Manager 规则引擎合并（零 LLM 调用） | Phase 1 用确定性合并（拼接+去重+截断），避免额外 API 成本；Phase 3 升级为 LLM 语义摘要 |
| 记忆接入方式 | MemoryManager 生命周期阶段调用（RXT-01/02） | Runtime 不直接调用 SQLite 记忆层，通过 trait 接入，记忆迭代不影响 Runtime |

### 设计演进记录

| 版本 | 变更 | 来源 |
|------|------|------|
| v3.3 | 循环检测升级为三种模式 + 三级渐进响应 | 架构设计 |
| v3.3 | 新增 Tool Call 单轮去重 | 架构设计 |
| v3.3 | Rate Limit 分层（可重试/不可重试 + Key 轮换 + retry_after 解析） | 架构设计 |
| v3.3 | 新增 Approval Gate（步骤⑤ Permission Check 后） | 架构评审 |
| v3.3 | 步骤③ 补充 RateAcquire 失败行为 + Streaming tool_calls 状态机 | 架构评审 |
| v3.3 | §3.1 补充 Autobiographical 压缩触发方、动态 N 计算、token 预估 | 架构评审 |
| v3.4 | 主循环记忆触发点改为 MemoryManager 生命周期阶段调用 | Runtime 可扩展性设计准则 |
| v3.4 | 新增 §9 Runtime 可扩展性设计准则 + 紧耦合审计 | 架构审查 |
| v3.7 | 上下文压缩策略大幅简化：移除所有程序化折叠，改为三阶段 LLM 驱动 | ADR-010 |
| v3.9 | Compaction 与 Distillation 统一（摘要即蒸馏），移除每轮 SQLite 记忆层 写入，SessionState 新增 is_compacted 标志 | ADR-011 |

## 9. Runtime 可扩展性设计准则

### 9.1 设计原则

**Runtime 是稳定的执行引擎，不是业务逻辑的容器。** 任何可能频繁迭代的功能（记忆、工具、Skill、LLM 路由）都不应硬编码在 Runtime 主循环里，而是通过标准化接口接入。

**具体准则：**

| 编号 | 准则 | 说明 |
|------|------|------|
| RXT-01 | 依赖倒置 | Runtime 依赖 trait/接口，不依赖具体实现。核心模块（Memory/Tool/LLM/Skill）必须通过 trait 接入 |
| RXT-02 | 生命周期钩子 | Runtime 主循环的关键位置定义标准化生命周期阶段，功能模块通过 handler 注册响应 |
| RXT-03 | 配置外置 | 所有可调参数（阈值、超时、策略）通过 manifest.toml + 系统默认值注入，不硬编码 |
| RXT-04 | 中间件管线 | 功能管线（记忆/工具执行/LLM 调用）支持中间件插入，无需修改 Runtime 或核心实现 |
| RXT-05 | 存储可替换 | 存储后端通过 trait 抽象，实现可替换（rusqlite / Sled / 远程服务 / 内存 mock） |
| RXT-06 | 事件可观测 | 关键操作发布事件，Desktop App / 日志 / 监控可订阅，不影响核心管线性能 |

### 9.2 紧耦合审计（v3.4）

基于上述准则，对 Runtime 当前设计进行紧耦合审计：

| 模块 | 紧耦合点 | 风险等级 | 状态 | 说明 |
|------|---------|---------|------|------|
| **Memory** | Runtime 直接调用 SQLite 记忆层 hybrid_search / graph_expand | 🔴 高 | ✅ 已修复（v3.4） | 改为 MemoryManager 生命周期阶段调用，详见 05-memory.md §10 |
| **Memory** | SQLite 记忆层 与 rusqlite 紧耦合 | 🟡 中 | ✅ 已修复（v3.4） | 引入 MemoryStore trait，SqliteStore 作为实现 |
| **Memory** | 遗忘参数硬编码（λ=0.03, FLOOR=0.05 等） | 🟡 中 | ✅ 已修复（v3.4） | DecayConfig 参数化，通过 manifest 注入 |
| **Tool Dispatch** | Tool Dispatcher 直接匹配 tool_name 字符串路由 | 🟡 中 | 📋 待 Phase 2 | 未来考虑 Tool trait + ToolRegistry 注册机制 |
| **LLM Client** | LLM Provider 切换硬编码在 routing 表 | 🟢 低 | 📋 可接受 | Provider 差异大，trait 抽象收益有限，当前 routing 表已足够灵活 |
| **Skill Loader** | SKILL.md 解析与 Markdown 格式紧耦合 | 🟢 低 | 📋 可接受 | 格式演进频率低，SKILL.md 格式已有社区标准约束 |
| **History Manager** | 对话历史存储在进程内存（Vec<Message>） | 🟢 低 | 📋 可接受 | Phase 1 单进程足够，Phase 3 云端同步时再抽象 |
| **Loop Controller** | 循环检测阈值硬编码为默认值 | 🟢 低 | ✅ 已有 manifest 覆盖 | §3.2 的 loop_detection 配置已支持 manifest 覆盖 |
| **Budget Manager** | 本地预算缓存结构硬编码 | 🟢 低 | 📋 可接受 | 预算模型相对稳定，不需要频繁变化 |
| **Prompt Builder** | 上下文构建顺序硬编码 | 🟢 低 | 📋 可接受 | 顺序变化频率极低，各部分已模块化 |

**审计结论：**

- 🔴 高风险：已全部修复（Memory 是唯一高风险项，v3.4 已通过 MemoryStore trait + 生命周期阶段解决）
- 🟡 中风险：Memory 项已修复；Tool Dispatch 的 trait 化推迟到 Phase 2（Phase 1 工具类型固定，string 路由够用）
- 🟢 低风险：均为可接受状态，暂不需要处理

**持续审计机制：** 每次设计评审（内部或外部 review）时，新增功能必须通过 RXT-01~06 准则检查。如果引入新的紧耦合，必须在设计决策记录中说明原因和未来解耦计划。
