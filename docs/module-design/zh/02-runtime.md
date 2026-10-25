# acowork-runtime — Agent Runtime

**定位**：加载 .agent 包并执行 Agent 逻辑的统一二进制。每个 Agent 是一个独立进程。

```
crates/acowork-runtime/
├── Cargo.toml
└── src/
    ├── main.rs                    # CLI 入口（clap）
    ├── lib.rs                     # 库入口
    ├── agent/
    │   ├── mod.rs
    │   ├── loop_.rs               # 主循环
    │   ├── context.rs             # 上下文构建（prompt + history + identity + skills；memory RAG 注入已随 ADR-101 移除，`retrieved_memory` 槽位只剩 DevMode 覆写能填）
    │   ├── history.rs             # 对话历史管理（token 预算、trim）
    │   ├── loop_detector.rs       # 循环检测
    │   └── budget_guard.rs        # 本地预算预检
    ├── package/
    │   ├── mod.rs
    │   ├── loader.rs              # .agent ZIP 解析 + manifest 校验
    │   └── prompt_builder.rs      # 从 prompts/ + skills/ 组装 system prompt
    ├── providers/
    │   ├── mod.rs                 # Provider 工厂 + 路由
    │   ├── openai.rs              # OpenAI Compatible Provider
    │   ├── anthropic.rs           # Anthropic Provider
    │   ├── ollama.rs              # Ollama Provider
    │   ├── router.rs              # LLM 路由（cost/quality/latency 策略）
    │   └── reliable.rs            # 重试 + fallback 链
    ├── tools/
    │   ├── mod.rs                 # 工具注册表 + 调度 + 激活逻辑
    │   ├── registry.rs            # 工具池注册 + manifest 驱动激活
    │   ├── permission.rs          # 权限校验（根据 manifest）
    │   ├── schema.rs              # 工具 JSON Schema 清洗
    │   ├── wrappers.rs            # 通用装饰器（RateLimitedTool / PathGuardedTool）
    │   ├── builtin/               # === 核心 Builtin 工具 ===
    │   │   ├── mod.rs
    │   │   ├── shell.rs           # Shell 命令执行
    │   │   ├── file_read.rs       # 读取文件（支持行号/偏移/PDF 提取）
    │   │   ├── file_write.rs      # 写入文件
    │   │   ├── file_edit.rs       # 精确字符串替换编辑
    │   │   ├── glob_search.rs     # Glob 模式搜索文件
    │   │   ├── content_search.rs  # 正则搜索文件内容（ripgrep）
    │   │   ├── calculator.rs      # 算术与统计计算
    │   │   ├── http_request.rs    # HTTP 请求（GET/POST/PUT/DELETE）
    │   │   ├── web_fetch.rs       # 获取网页并转纯文本
    │   │   ├── weather.rs         # 天气查询（wttr.in）
    │   │   ├── git_operations.rs  # 结构化 Git 操作
    │   │   ├── pdf_read.rs        # PDF 文本提取
    │   │   ├── screenshot.rs      # 屏幕截图
    │   │   ├── image_info.rs      # 图片元数据读取
    │   │   ├── image_gen.rs       # 文生图（fal.ai）
    │   │   ├── llm_task.rs        # LLM 子调用（无工具，纯文本/JSON）
    │   │   └── identity_query.rs  # 向系统 Agent 查询身份（ACowork 独有）
    │   ├── memory/                # ← ADR-101 删除：记忆工具已离开 Runtime。
    │   │   （`memory_store` / `memory_recall` 现在是 Gateway 托管记忆 MCP 的工具，
    │   │     前缀名 `mcp_memory__*`，经 ADR-069 的 `agent_mcp_tools.json` 做工具级 opt-in）
    │   ├── schedule/              # === 定时任务工具 ===
    │   │   ├── mod.rs
    │   │   ├── schedule.rs        # Shell 定时任务
    │   │   ├── cron_add.rs        # 创建 Cron 任务
    │   │   ├── cron_list.rs       # 列出 Cron 任务
    │   │   ├── cron_remove.rs     # 删除 Cron 任务
    │   │   ├── cron_update.rs     # 更新 Cron 任务
    │   │   ├── cron_run.rs        # 强制运行 Cron
    │   │   └── cron_runs.rs       # Cron 运行历史
    │   ├── integration/           # === 第三方集成工具（按需激活） ===
    │   │   ├── mod.rs
    │   │   ├── notion.rs          # Notion API
    │   │   ├── jira.rs            # Jira API
    │   │   ├── google_workspace.rs # Google Workspace（gws CLI）
    │   │   ├── microsoft365.rs    # Microsoft 365 Graph API
    │   │   ├── linkedin.rs        # LinkedIn 管理
    │   │   ├── discord_search.rs  # Discord 消息搜索
    │   │   ├── pushover.rs        # Pushover 推送通知
    │   │   └── composio.rs        # Composio 1000+ 应用集成
    │   ├── agent/                 # === Agent 协作工具（ACowork 增强） ===
    │   │   ├── mod.rs
    │   │   ├── delegate.rs        # 子任务委派（单次 Agent 调用）
    │   │   ├── swarm.rs           # Agent 群协同（顺序/并行/路由）
    │   │   ├── intent_send.rs     # Intent 发送（通过 Gateway）
    │   │   ├── intent_receive.rs  # Intent 接收处理
    │   │   ├── ask_user.rs        # 向用户提问
    │   │   └── escalate.rs        # 升级到人类操作员
    │   ├── browser/               # === 浏览器工具 ===
    │   │   ├── mod.rs
    │   │   ├── browser_open.rs    # 打开 URL
    │   │   ├── browser.rs         # 浏览器自动化（可插拔后端）
    │   │   └── browser_delegate.rs # 浏览器任务委派
    │   ├── dev/                   # === 开发者工具 ===
    │   │   ├── mod.rs
    │   │   ├── claude_code.rs     # Claude Code 委派
    │   │   ├── codex_cli.rs       # Codex CLI 委派
    │   │   ├── gemini_cli.rs      # Gemini CLI 委派
    │   │   └── agent_cli.rs       # 第三方 AI CLI 委派
    │   ├── skill/                 # === Skill 动态工具 ===
    │   │   ├── mod.rs
    │   │   ├── skill_tool.rs      # Skill Shell 工具
    │   │   └── skill_http.rs      # Skill HTTP 工具
    │   ├── mcp/                   # === MCP 协议工具 ===
    │   │   ├── mod.rs
    │   │   ├── mcp_client.rs      # MCP 客户端注册表
    │   │   ├── mcp_tool.rs        # MCP 工具包装器
    │   │   ├── mcp_transport.rs   # MCP 传输层
    │   │   ├── mcp_protocol.rs    # MCP 协议类型
    │   │   └── mcp_deferred.rs    # 延迟加载 MCP 工具
    │   ├── wasm/                  # === WASM 沙箱工具 ===
    │   │   ├── mod.rs             # WASM 工具调度器
    │   │   └── sandbox.rs         # Wasmtime 沙箱封装
    │   ├── sop/                   # === SOP 标准操作流程工具 ===
    │   │   ├── mod.rs
    │   │   ├── sop_list.rs
    │   │   ├── sop_execute.rs
    │   │   ├── sop_advance.rs
    │   │   ├── sop_approve.rs
    │   │   └── sop_status.rs
    │   ├── pipeline.rs            # 多步骤工具管道
    │   ├── knowledge.rs           # 知识图谱工具
    │   ├── canvas.rs              # 实时 Web 画布
    │   ├── poll.rs                # 投票工具
    │   ├── reaction.rs            # Emoji 反应
    │   ├── model_switch.rs        # 运行时模型切换
    │   ├── model_routing.rs       # 模型路由配置
    │   ├── proxy_config.rs        # 代理设置
    │   ├── backup.rs              # 备份工具
    │   ├── data_management.rs     # 数据保留/清除
    │   ├── security_ops.rs        # 安全运营
    │   ├── cloud_ops.rs           # 云运营（只读）
    │   ├── cloud_patterns.rs      # 云模式库
    │   ├── project_intel.rs       # 项目交付智能
    │   ├── report_template.rs     # 报告模板
    │   ├── workspace.rs           # 多工作区管理
    │   ├── verifiable_intent.rs   # 可验证意图
    │   ├── tool_search.rs         # 延迟工具搜索
    │   └── node.rs                # Node 设备能力工具
    ├── memory/
    │   └── （ADR-101 已清空：Memory 门面 / `MemorySessionHandle` / 记忆层读写全部迁入
    │       `acowork-memory` 服务。Embedding 也不再由 Runtime 提供给记忆——服务自持客户端）
    ├── skills/
    │   ├── mod.rs
    │   ├── loader.rs              # SKILL.md 解析（YAML frontmatter + Markdown body）
    │   └── registry.rs            # Skill 注册表
    ├── ipc/
    │   ├── mod.rs
    │   ├── transport.rs           # 传输层抽象（Unix Socket / Named Pipe / Local TCP）
    │   └── client.rs              # Gateway Service API 客户端
    ├── debug/
    │   ├── mod.rs                 # DevMode 控制器（ADR-048：仅导出 DebugEventBus + DebugEventSender）
    │   ├── handlers.rs            # Debug Protocol 业务逻辑（10 个 pub async fn，从原 server.rs 提取；其余 12 个 endpoint 预留）
    │   ├── events.rs              # DebugEventBus（broadcast channel，连接 handlers ↔ MQTT publisher）
    │   ├── controller.rs          # DebugController 共享状态
    │   ├── observer.rs            # 主循环观察者（pause/step 等控制信号）
    │   ├── observer_impl.rs       # observer 默认实现
    │   └── protocol.rs            # Debug Protocol DTO（ADR-048 后仅保留数据，移除 JSON-RPC 帧）
    ├── config.rs                  # Agent Runtime 配置
    └── cli.rs                     # CLI 子命令定义
```

## 关键模块说明

### `agent/loop_.rs` — 主循环

主循环是 Agent Runtime 的核心调度单元，其设计要点如下：

```
主循环流程：
⓪ 消息合并 → inbound_queue.drain() → history.append(pending_messages)
   ├─ UserMessage / SystemNotification / IntentMessage 三类
   └─ 无新消息时跳过
① 预算预检 → budget_guard.check()
② 构建上下文 → context.build(manifest, history, …)   // 无记忆注入（ADR-101）
③ 调用 LLM → provider.chat_stream(request)  // 流式
④ 解析响应 → 解析 text / tool_calls
④.5 Tool Call 去重 → dedup (tool_name, params)
⑤ 权限检查 → tool_permissions.check_batch(all_calls)
   ├─ 无权限 → 构造错误 ToolResult，继续
   └─ 有权限 → 进入审批门控
⑤' 审批门控（串行等待） → approval_gate.wait_for_pending()
   ├─ requires_approval: false → 直接通过
   └─ requires_approval: true → 等待用户确认（Desktop App / CLI 通知）
⑤'' 工具调度（并行） → futures::future::join_all(tool_calls)
   ├─ builtin → 直接执行（execute_tool 内部含单工具超时）
   ├─ wasm → wasmtime 沙箱执行
   └─ gateway → ipc_client.send(request)
   迭代整体超时 → tokio::time::timeout 控制 join_all，被 drop 的工具不记录 History
⑥ 结果追加历史 → history.append(tool_results)
⑦ 用量上报 → ipc_client.send(UsageReport) // 异步不阻塞
⑧ 循环检测 → loop_detector.check(history)
⑨ 迭代计数检查 → max_iterations 强制终止
```

AgentLoop 结构体签名（关键字段）：

```rust
pub struct AgentLoop {
    manifest: AgentManifest,
    history: HistoryManager,
    provider: Arc<dyn Provider>,
    tools: Vec<Arc<dyn Tool>>,
    budget_guard: BudgetGuard,
    loop_detector: LoopDetector,
    ipc_client: Option<GatewayClient>,
    inbound_tx: mpsc::Sender<InboundMessage>,   // 外部注入消息的入口
    inbound_rx: mpsc::Receiver<InboundMessage>, // 循环内消费
}

pub enum InboundMessage {
    UserMessage(String),
    SystemNotification(SystemEvent),  // identity_update / capability_update
    IntentMessage(IntentPayload),
}
```

关键设计差异（相比单进程 Agent 循环）：
- 增加 DevMode 步进控制层
- 权限校验在工具调度前执行（步骤⑤→⑤'），而非在安全策略层；Approval Gate 单独作为步骤⑤' 串行等待
- **新增** `inbound_tx/rx` 消息队列，支持循环间隙注入消息
- **新增** 步骤⑤改为并行执行（`join_all`），而非串行 `for` 循环

**Channel 生命周期：** `inbound_rx` 由 Runtime 创建时一并创建（`mpsc::channel(64)`），属于 AgentLoop 内部字段；`inbound_tx` 通过 `AgentLoop::new()` 参数传入，由 Runtime 的 IPC 层持有。Gateway 的 push 消息通过 IPC response 携带 `InboundMessage` 发送至 Runtime IPC 层，IPC 层调用 `inbound_tx.send()` 注入消息，不持有 Runtime 对象本身。

### `tools/` — 工具系统

ACowork-AI **工具系统设计原则**：

1. **Runtime 提供完整工具池**（~77 个核心工具），但不是每个 Agent 都能用所有工具
2. **Manifest 声明驱动激活**：`.agent` 包的 `tools` 和 `permissions` 字段决定该 Agent 可用哪些工具
3. **工具按类别分目录**：builtin / schedule / integration / agent / browser / dev / skill / mcp / wasm / sop（`memory/` 已随 ADR-101 删除——记忆工具改成托管 MCP 提供）
4. **安全包装器组合**：采用 `RateLimitedTool` + `PathGuardedTool` 装饰器模式，实现限流与路径校验的组合安全策略

```rust
/// 工具激活流程
fn build_tool_registry(manifest: &AgentManifest, all_tools: Vec<Arc<dyn Tool>>) -> Vec<Arc<dyn Tool>> {
    all_tools.into_iter()
        .filter(|tool| manifest.is_tool_allowed(tool.name()))  // manifest 声明过滤
        .map(|tool| {
            // 应用安全装饰器
            let guarded = PathGuardedTool::new(tool, security.clone());
            Arc::new(RateLimitedTool::new(guarded, security.clone())) as Arc<dyn Tool>
        })
        .collect()
}
```

**关键设计要点**：

| 维度 | ACowork 设计 |
|------|-------------|
| 工具注册 | 分两步：`all_tools()` 构建池 + `activate()` 按 manifest 过滤 |
| 激活机制 | manifest `tools[]` + `permissions[]` 声明驱动 |
| Intent 工具 | 新增 `intent_send` / `intent_receive`（跨 Agent 通信） |
| 身份工具 | 新增 `identity_query`（查询用户身份） |
| Agent 协作 | `delegate` 可跨进程（通过 Gateway Intent） |
| 第三方集成 | manifest 声明 + 运行时配置双重控制 |

**工具分类参考（完整设计空间，实际激活取决于 manifest 声明）**：

> ⚠️ 以下表格描述的是 ACowork 工具系统的**完整设计空间**。ACowork **Phase 1 仅实现 13 个内置工具**（memory×2, network×2, web×2, shell, file×4, intent×1, search×1）和 WASM 工具。其中 memory×2 已由 ADR-101 从内置面移出（改走记忆 MCP）。其余类别（Notion/Jira 集成、browser、dev tools、MCP、SOP 等）为 Phase 2+ 按需实现。

| 分类 | ACowork 目录 | 工具数 | Phase 1 实现 |
|------|-------------|--------|------------|
| 核心 Builtin | `builtin/` | 17 | ✅ 13 个（weather/git/pdf/screenshot/image 系移到 WASM 或 Agent 内置） |
| Memory | ~~`memory/`~~ | 0 | ❌ ADR-101 已移出 Runtime：`memory_store` / `memory_recall` 由 Gateway 托管的记忆 MCP 提供 |
| 定时任务 | `schedule/` | 7 | ❌ Phase 2 |
| 第三方集成 | `integration/` | 8 | ❌ 由独立 Agent 提供 |
| Agent 协作 | `agent/` | 6 | ✅ intent_send/receive 已实现，其余 Phase 2 |
| 浏览器 | `browser/` | 3 | ❌ Phase 2+ |
| 开发者 | `dev/` | 4 | ❌ Phase 2+ |
| Skill 动态 | `skill/` | 2 | ✅ Phase 1 |
| MCP 协议 | `mcp/` | 5 | ❌ Phase 2 |
| WASM 沙箱 | `wasm/` | 2 | ✅ Phase 1 |
| SOP 流程 | `sop/` | 5 | ❌ Phase 2+ |
| 其他工具 | 根级 | ~14 | ❌ Phase 2+ |

**Phase 1 工具策略**：内置工具仅限平台基础设施级，SaaS 集成由独立 Agent 提供（不内置）。详见 [12-tool-system.md](../design/zh/12-tool-system.md)。

### `ipc/transport.rs` — 传输层抽象

```rust
/// 传输层 trait，各平台实现不同
#[async_trait]
pub trait Transport: Send + Sync {
    async fn connect(&self, endpoint: &str) -> Result<TransportStream>;
    async fn send_frame(&self, frame: Frame) -> Result<()>;
    async fn recv_frame(&self) -> Result<Frame>;
}

/// 根据端点 URL scheme 自动选择传输实现
pub fn create_transport(endpoint: &str) -> Box<dyn Transport> {
    match endpoint {
        e if e.starts_with("unix://") => Box::new(UnixSocketTransport::new()),
        e if e.starts_with("pipe://") => Box::new(NamedPipeTransport::new()),
        e if e.starts_with("tcp://") => Box::new(LocalTcpTransport::new()),
        _ => panic!("Unknown endpoint scheme: {endpoint}"),
    }
}
```

### `debug/` — DevMode 模块

Phase 2.5 实现的核心模块，使 Runtime 支持步进调试：

```rust
/// DevMode 控制器，叠加在生产模式之上
pub struct DevModeController {
    debugger: Option<DebuggerHandle>,
    snapshot_mgr: SnapshotManager,
    recording_engine: Option<RecordingEngine>,
}

/// 主循环每步都经过 DevMode
impl DevModeController {
    /// 每步执行后调用，决定是否暂停
    pub fn on_step(&self, iteration: u32, phase: Phase) -> ControlFlow {
        // 检查断点
        // 推送 DebuggerOnStep 事件
        // 等待调试器命令（Resume/Pause/Step）
    }
}
```

## 依赖

- `acowork-core` — 共享类型
- `acowork-sqlite` — 仅会话向量索引 + session 元数据（它仍复用同一 `private.sqlite`）；**记忆引擎已不在此依赖**，`acowork-memory` 是独立服务，Runtime 不再 link 它（ADR-101）
- `acowork-vault` — 不直接依赖，Key 通过 IPC 从 Gateway 获取
- `tokio`, `reqwest`, `clap`, `serde_json`
- `wasmtime` (feature-gated: `wasm-tools`)

## Feature Flags

```toml
[features]
default = []
wasm-tools = ["dep:wasmtime"]          # WASM 工具沙箱
dev-mode = []                           # DevMode 调试支持
integration-notion = []                 # Notion API 工具
integration-jira = []                   # Jira API 工具
integration-google = []                 # Google Workspace 工具
integration-microsoft365 = []           # Microsoft 365 工具
integration-linkedin = []               # LinkedIn 工具
integration-composio = []               # Composio 集成
browser-automation = []                 # 浏览器自动化工具
dev-tools = []                          # Claude Code / Codex CLI 等开发者工具
mcp = []                                # MCP 协议工具
sop = []                                # SOP 标准操作流程
hardware = []                           # 硬件工具（feature 门控）
```

