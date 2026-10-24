# acowork-runtime — Agent Runtime

**Position**: Unified binary that loads .agent package and executes Agent logic. Each Agent is an independent process.

```
crates/acowork-runtime/
├── Cargo.toml
└── src/
    ├── main.rs                    # CLI entry (clap)
    ├── lib.rs                     # library entry
    ├── agent/
    │   ├── mod.rs
    │   ├── loop_.rs               # main loop
    │   ├── context.rs             # context building (prompt + history + identity + skills; the memory-RAG injection was removed by ADR-100 — the `retrieved_memory` slot can now only be filled by a DevMode override)
    │   ├── history.rs             # conversation history management (token budget, trim)
    │   ├── loop_detector.rs       # loop detection
    │   └── budget_guard.rs        # local budget pre-check
    ├── package/
    │   ├── mod.rs
    │   ├── loader.rs              # .agent ZIP parsing + manifest validation
    │   └── prompt_builder.rs      # assemble system prompt from prompts/ + skills/
    ├── providers/
    │   ├─── mod.rs                 # Provider factory + routing
    │   ├── openai.rs              # OpenAI Compatible Provider
    │   ├── anthropic.rs           # Anthropic Provider
    │   ├── ollama.rs              # Ollama Provider
    │   ├── router.rs              # LLM routing (cost/quality/latency strategy)
    │   └── reliable.rs            # retry + fallback chain
    ├── tools/
    │   ├── mod.rs                 # tool registry + dispatch + activation logic
    │   ├── registry.rs            # tool pool registration + manifest-driven activation
    │   ├── permission.rs          # permission check (per manifest)
    │   ├── schema.rs              # tool JSON Schema sanitization
    │   ├── wrappers.rs            # common decorators (RateLimitedTool / PathGuardedTool)
    │   ├── builtin/               # === core Builtin tools ===
    │   │   ├── mod.rs
    │   │   ├── shell.rs           # shell command execution
    │   │   ├── file_read.rs       # read file (supports line number/offset/PDF extraction)
    │   │   ├── file_write.rs      # write file
    │   │   ├── file_edit.rs       # precise string replacement editing
    │   │   ├── glob_search.rs     # Glob pattern file search
    │   │   ├── content_search.rs  # regex file content search (ripgrep)
    │   │   ├── calculator.rs      # arithmetic and statistics
    │   │   ├── http_request.rs    # HTTP request (GET/POST/PUT/DELETE)
    │   │   ├── web_fetch.rs       # fetch web page and convert to plain text
    │   │   ├── weather.rs         # weather query (wttr.in)
    │   │   ├── git_operations.rs  # structured Git operations
    │   │   ├── pdf_read.rs        # PDF text extraction
    │   │   ├── screenshot.rs      # screen capture
    │   │   ├── image_info.rs      # image metadata reading
    │   │   ├── image_gen.rs       # text-to-image (fal.ai)
    │   │   ├── llm_task.rs        # LLM sub-call (no tools, pure text/JSON)
    │   │   └── identity_query.rs  # query identity from system Agent (ACowork-specific)
    │   ├── memory/                # ← deleted by ADR-100: memory tools left the Runtime.
    │   │   (`memory_store` / `memory_recall` are now tools of the Gateway-hosted memory
    │   │    MCP, prefixed names `mcp_memory__*`, opted in per-tool via ADR-069's
    │   │    `agent_mcp_tools.json`)
    │   ├── schedule/              # === scheduled task tools ===
    │   │   ├── mod.rs
    │   │   ├── schedule.rs        # shell scheduled task
    │   │   ├── cron_add.rs        # create Cron task
    │   │   ├── cron_list.rs       # list Cron tasks
    │   │   ├── cron_remove.rs     # delete Cron task
    │   │   ├── cron_update.rs     # update Cron task
    │   │   ├── cron_run.rs        # force run Cron
    │   │   └── cron_runs.rs       # Cron run history
    │   ├── integration/           # === third-party integration tools (on-demand activation) ===
    │   │   ├── mod.rs
    │   │   ├── notion.rs          # Notion API
    │   │   ├── jira.rs            # Jira API
    │   │   ├── google_workspace.rs # Google Workspace (gws CLI)
    │   │   ├── microsoft365.rs    # Microsoft 365 Graph API
    │   │   ├── linkedin.rs        # LinkedIn management
    │   │   ├── discord_search.rs  # Discord message search
    │   │   ├── pushover.rs        # Pushover notification
    │   │   └── composio.rs        # Composio 1000+ app integration
    │   ├── agent/                 # === Agent collaboration tools (ACowork enhancement) ===
    │   │   ├── mod.rs
    │   │   ├── delegate.rs        # subtask delegation (single Agent call)
    │   │   ├── swarm.rs           # Agent swarm coordination (sequential/parallel/routed)
    │   │   ├── intent_send.rs     # Intent send (via Gateway)
    │   │   ├── intent_receive.rs  # Intent receive handling
    │   │   ├── ask_user.rs        # ask user
    │   │   └── escalate.rs        # escalate to human operator
    │   ├── browser/               # === browser tools ===
    │   │   ├── mod.rs
    │   │   ├── browser_open.rs    # open URL
    │   │   ├── browser.rs         # browser automation (pluggable backend)
    │   │   └── browser_delegate.rs # browser task delegation
    │   ├── dev/                   # === developer tools ===
    │   │   ├── mod.rs
    │   │   ├── claude_code.rs     # Claude Code delegation
    │   │   ├── codex_cli.rs       # Codex CLI delegation
    │   │   ├── gemini_cli.rs      # Gemini CLI delegation
    │   │   └── agent_cli.rs       # third-party AI CLI delegation
    │   ├── skill/                 # === Skill dynamic tools ===
    │   │   ├── mod.rs
    │   │   ├── skill_tool.rs      # Skill shell tool
    │   │   └── skill_http.rs      # Skill HTTP tool
    │   ├── mcp/                   # === MCP protocol tools ===
    │   │   ├── mod.rs
    │   │   ├── mcp_client.rs      # MCP client registry
    │   │   ├── mcp_tool.rs        # MCP tool wrapper
    │   │   ├── mcp_transport.rs   # MCP transport layer
    │   │   ├── mcp_protocol.rs    # MCP protocol types
    │   │   └── mcp_deferred.rs    # deferred loading MCP tools
    │   ├── wasm/                  # === WASM sandbox tools ===
    │   │   ├── mod.rs             # WASM tool dispatcher
    │   │   └── sandbox.rs         # Wasmtime sandbox wrapper
    │   ├── sop/                   # === SOP standard operating procedure tools ===
    │   │   ├── mod.rs
    │   │   ├── sop_list.rs
    │   │   ├── sop_execute.rs
    │   │   ├── sop_advance.rs
    │   │   ├── sop_approve.rs
    │   │   └── sop_status.rs
    │   ├── pipeline.rs            # multi-step tool pipeline
    │   ├── knowledge.rs           # knowledge graph tool
    │   ├── canvas.rs              # real-time web canvas
    │   ├── poll.rs                # voting tool
    │   ├── reaction.rs            # Emoji reaction
    │   ├── model_switch.rs        # runtime model switching
    │   ├── model_routing.rs       # model routing config
    │   ├── proxy_config.rs        # proxy settings
    │   ├── backup.rs              # backup tool
    │   ├── data_management.rs     # data retention/purge
    │   ├── security_ops.rs        # security operations
    │   ├── cloud_ops.rs           # cloud operations (read-only)
    │   ├── cloud_patterns.rs      # cloud pattern library
    │   ├── project_intel.rs       # project delivery intelligence
    │   ├── report_template.rs     # report template
    │   ├── workspace.rs           # multi-workspace management
    │   ├── verifiable_intent.rs   # verifiable intent
    │   ├── tool_search.rs         # deferred tool search
    │   └── node.rs                # Node device capability tool
    ├── memory/
    │   └── (emptied by ADR-100: the Memory facade / `MemorySessionHandle` / memory-layer
    │       read-write wrapper all moved into the `acowork-memory` service. Embedding for
    │       memory is no longer supplied by the Runtime either — the service owns its client)
    ├── skills/
    │   ├── mod.rs
    │   ├── loader.rs              # SKILL.md parsing (YAML frontmatter + Markdown body)
    │   └── registry.rs            # Skill registry
    ├── ipc/
    │   ├── mod.rs
    │   ├── transport.rs           # transport layer abstraction (Unix Socket / Named Pipe / Local TCP)
    │   └── client.rs              # Gateway Service API client
    ├── debug/
    │   ├── mod.rs                 # DevMode controller (ADR-048: only exports DebugEventBus + DebugEventSender)
    │   ├── handlers.rs            # Debug Protocol business logic (10 pub async fn, extracted from original server.rs; remaining 12 endpoints reserved)
    │   ├── events.rs              # DebugEventBus (broadcast channel, connecting handlers ↔ MQTT publisher)
    │   ├── controller.rs          # DebugController shared state
    │   ├── observer.rs            # main loop observer (pause/step etc. control signals)
    │   ├── observer_impl.rs       # observer default implementation
    │   └── protocol.rs            # Debug Protocol DTO (ADR-048 only retains data, removes JSON-RPC frames)
    ├── config.rs                  # Agent Runtime config
    └── cli.rs                     # CLI subcommand definitions
```

## Key Module Descriptions

### `agent/loop_.rs` — Main Loop

Main loop is the core scheduling unit of Agent Runtime, its design key points:

```
Main loop flow:
⓪ Message merge → inbound_queue.drain() → history.append(pending_messages)
   ├─ UserMessage / SystemNotification / IntentMessage three types
   └─ Skip when no new messages
① Budget pre-check → budget_guard.check()
② Build context → context.build(manifest, history, …)   // no memory injection (ADR-100)
③ Call LLM → provider.chat_stream(request)  // streaming
④ Parse response → parse text / tool_calls
④.5 Tool Call dedup → dedup (tool_name, params)
⑤ Permission check → tool_permissions.check_batch(all_calls)
   ├─ No permission → construct error ToolResult, continue
   └─ Has permission → enter approval gate
⑤' Approval gate (serial wait) → approval_gate.wait_for_pending()
   ├─ requires_approval: false → pass directly
   └─ requires_approval: true → wait for user confirmation (Desktop App / CLI notification)
⑤'' Tool dispatch (parallel) → futures::future::join_all(tool_calls)
   ├─ builtin → execute directly (execute_tool internal includes single-tool timeout)
   ├─ wasm → wasmtime sandbox execution
   └─ gateway → ipc_client.send(request)
   Iteration overall timeout → tokio::time::timeout controls join_all, dropped tools not recorded in History
⑥ Append results to history → history.append(tool_results)
⑦ Usage report → ipc_client.send(UsageReport) // async non-blocking
⑧ Loop detection → loop_detector.check(history)
⑨ Iteration count check → max_iterations force termination
```

AgentLoop struct signature (key fields):

```rust
pub struct AgentLoop {
    manifest: AgentManifest,
    history: HistoryManager,
    provider: Arc<dyn Provider>,
    tools: Vec<Arc<dyn Tool>>,
    budget_guard: BudgetGuard,
    loop_detector: LoopDetector,
    ipc_client: Option<GatewayClient>,
    inbound_tx: mpsc::Sender<InboundMessage>,   // external message injection entry
    inbound_rx: mpsc::Receiver<InboundMessage>, // consumed within loop
}

pub enum InboundMessage {
    UserMessage(String),
    SystemNotification(SystemEvent),  // identity_update / capability_update
    IntentMessage(IntentPayload),
}
```

Key design differences (compared to single-process Agent loop):
- Add DevMode step control layer
- Permission check executed before tool dispatch (step ⑤→⑤'), not at security policy layer; Approval Gate as separate step ⑤' serial wait
- **New** `inbound_tx/rx` message queue, supports injecting messages between loop iterations
- **New** step ⑤ changed to parallel execution (`join_all`), not serial `for` loop

**Channel lifecycle**: `inbound_rx` is created when Runtime starts (`mpsc::channel(64)`), is AgentLoop internal field; `inbound_tx` is passed via `AgentLoop::new()` parameter, held by Runtime's IPC layer. Gateway's push messages carry `InboundMessage` via IPC response to Runtime IPC layer, IPC layer calls `inbound_tx.send()` to inject messages, doesn't hold Runtime object itself.

### `tools/` — Tool System

ACowork-AI **Tool System Design Principles**:

1. **Runtime provides complete tool pool** (~77 core tools), but not every Agent can use all tools
2. **Manifest declaration-driven activation**: `.agent` package's `tools` and `permissions` fields determine which tools are available to the Agent
3. **Tools organized in directories by category**: builtin / schedule / integration / agent / browser / dev / skill / mcp / wasm / sop (`memory/` was removed by ADR-100 — memory tools come from the hosted MCP now)
4. **Security wrapper composition**: Adopt `RateLimitedTool` + `PathGuardedTool` decorator pattern, achieve composable security policies with rate limiting and path validation

```rust
/// Tool activation flow
fn build_tool_registry(manifest: &AgentManifest, all_tools: Vec<Arc<dyn Tool>>) -> Vec<Arc<dyn Tool>> {
    all_tools.into_iter()
        .filter(|tool| manifest.is_tool_allowed(tool.name()))  // manifest declaration filter
        .map(|tool| {
            // apply security decorators
            let guarded = PathGuardedTool::new(tool, security.clone());
            Arc::new(RateLimitedTool::new(guarded, security.clone())) as Arc<dyn Tool>
        })
        .collect()
}
```

**Key Design Points**:

| Dimension | ACowork Design |
|-----------|----------------|
| Tool registration | Two-step: `all_tools()` builds pool + `activate()` filters by manifest |
| Activation mechanism | manifest `tools[]` + `permissions[]` declaration-driven |
| Intent tools | New `intent_send` / `intent_receive` (cross-Agent communication) |
| Identity tools | New `identity_query` (query user identity) |
| Agent collaboration | `delegate` can cross-process (via Gateway Intent) |
| Third-party integration | manifest declaration + runtime configuration dual control |

**Tool classification reference (complete design space, actual activation depends on manifest declaration)**:

> ⚠️ The following table describes the **complete design space** of ACowork tool system. ACowork **Phase 1 only implements 13 built-in tools** (memory×2, network×2, web×2, shell, file×4, intent×1, search×1) and WASM tools. Of those, memory×2 left the builtin surface with ADR-100 (they are memory-MCP tools now). Other categories (Notion/Jira integration, browser, dev tools, MCP, SOP etc.) are Phase 2+ on-demand implementation.

| Category | ACowork Directory | Tool Count | Phase 1 Implemented |
|----------|-------------------|------------|---------------------|
| Core Builtin | `builtin/` | 17 | ✅ 13 (weather/git/pdf/screenshot/image series moved to WASM or Agent built-in) |
| Memory | ~~`memory/`~~ | 0 | ❌ Removed from the Runtime by ADR-100: `memory_store` / `memory_recall` are served by the Gateway-hosted memory MCP |
| Scheduled tasks | `schedule/` | 7 | ❌ Phase 2 |
| Third-party integration | `integration/` | 8 | ❌ Provided by independent Agents |
| Agent collaboration | `agent/` | 6 | ✅ intent_send/receive implemented, others Phase 2 |
| Browser | `browser/` | 3 | ❌ Phase 2+ |
| Developer | `dev/` | 4 | ❌ Phase 2+ |
| Skill dynamic | `skill/` | 2 | ✅ Phase 1 |
| MCP protocol | `mcp/` | 5 | ❌ Phase 2 |
| WASM sandbox | `wasm/` | 2 | ✅ Phase 1 |
| SOP flow | `sop/` | 5 | ❌ Phase 2+ |
| Other tools | root level | ~14 | ❌ Phase 2+ |

**Phase 1 Tool Strategy**: Built-in tools only platform infrastructure level, SaaS integration provided by independent Agents (not built-in). See [12-tool-system.md](../design/en/12-tool-system.md).

### `ipc/transport.rs` — Transport Layer Abstraction

```rust
/// Transport layer trait, different per-platform implementations
#[async_trait]
pub trait Transport: Send + Sync {
    async fn connect(&self, endpoint: &str) -> Result<TransportStream>;
    async fn send_frame(&self, frame: Frame) -> Result<()>;
    async fn recv_frame(&self) -> Result<Frame>;
}

/// Auto-select transport implementation based on endpoint URL scheme
pub fn create_transport(endpoint: &str) -> Box<dyn Transport> {
    match endpoint {
        e if e.starts_with("unix://") => Box::new(UnixSocketTransport::new()),
        e if e.starts_with("pipe://") => Box::new(NamedPipeTransport::new()),
        e if e.starts_with("tcp://") => Box::new(LocalTcpTransport::new()),
        _ => panic!("Unknown endpoint scheme: {endpoint}"),
    }
}
```

### `debug/` — DevMode Module

Phase 2.5 implementation's core module, enables Runtime to support step debugging:

```rust
/// DevMode controller, overlaid on production mode
pub struct DevModeController {
    debugger: Option<DebuggerHandle>,
    snapshot_mgr: SnapshotManager,
    recording_engine: Option<RecordingEngine>,
}

/// Main loop goes through DevMode at each step
impl DevModeController {
    /// Called after each step execution, decides whether to pause
    pub fn on_step(&self, iteration: u32, phase: Phase) -> ControlFlow {
        // Check breakpoints
        // Push DebuggerOnStep event
        // Wait for debugger command (Resume/Pause/Step)
    }
}
```

## Dependencies

- `acowork-core` — shared types
- `acowork-sqlite` — conversation vector index + session meta only (still the same `private.sqlite`); the **memory engine is no longer in this dependency** — `acowork-memory` is a standalone service the Runtime does not link (ADR-100)
- `acowork-vault` — not directly depended on, Key obtained via IPC from Gateway
- `tokio`, `reqwest`, `clap`, `serde_json`
- `wasmtime` (feature-gated: `wasm-tools`)

## Feature Flags

```toml
[features]
default = []
wasm-tools = ["dep:wasmtime"]          # WASM tool sandbox
dev-mode = []                           # DevMode debug support
integration-notion = []                 # Notion API tool
integration-jira = []                   # Jira API tool
integration-google = []                 # Google Workspace tool
integration-microsoft365 = []           # Microsoft 365 tool
integration-linkedin = []               # LinkedIn tool
integration-composio = []               # Composio integration
browser-automation = []                 # browser automation tool
dev-tools = []                          # Claude Code / Codex CLI etc. developer tools
mcp = []                                # MCP protocol tool
sop = []                                # SOP standard operating procedure
hardware = []                           # hardware tools (feature-gated)
```