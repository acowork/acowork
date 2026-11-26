# Bot Runtime (Unified Execution Engine)

> Version: v3.6 | Last Updated: 2026-05-06

---

Bot Runtime is the only binary executable provided by the platform, similar to Android's ART virtual machine. Gateway starts one Bot Runtime process for each Bot, passing the .bot package path and Gateway endpoint as startup parameters.

> **v3.7 Change (2026-05-28)**: Context compression strategy greatly simplified — see [ADR-010](../../adr/zh/ADR-010-context-compression-simplification.md). Core change: abandon programmatic folding strategies (Tool Result folding, content folding Phase 1); context compression returns to LLM summary as the sole normal path means. Daily compression flow simplified to: 70% alert → 80% LLM summary (complete context, no folding) → 95% emergency_trim safety net.

> **v3.10 Change (2026-11-23)**: Context layer 1 (System Prompt) splits from one file into two — `prompts/system.md` (role identity, scenario-agnostic) + `prompts/rules.md` (business-scenario rules / constraints / workflow), with the Runtime fixing the section order as `system.md` → `rules.md` → the rest by filename — see [ADR-120](../../adr/en/ADR-120-system-and-rules-prompt-split.md).

> **v3.9 Change (2026-05-28)**: Compaction and Distillation unified — see [ADR-011](../../adr/zh/ADR-011-compaction-as-distillation.md). Core change: Compaction summary and Session distillation merge into a single Compact Model call; summary text used simultaneously for memory replacement and SQLite memory layer experience layer writing ("summary is distillation"). Experience layer write sources simplified to only Compaction and Session close distillation, removing per-round conversation real-time write. SessionState adds `is_compacted` flag to control tail distillation decision.

**Cross-references**:
- Runtime internal structure: This document §2
- Session Actor architecture: `15-conversation-persistence.md` §1.7
- IPC message format: `06-communication.md` §1.5
- Episode extraction mechanism: `15-conversation-persistence.md` §3.3
- Budget allocation strategy: `15-conversation-persistence.md` §1.8

## 1. Startup Method

**Design constraint**: Bot Runtime idle memory footprint target controlled at ~5-10 MB. This goal constrains Runtime's module design — lazy initialization (SQLite, Wasmtime Engine and other heavyweight modules loaded on demand), minimized default cache, zero background polling threads.

> **Verification method**: Phase 3 will report memory usage in real-time via `MemoryMetrics` struct in Debug mode, and provide `/metrics` endpoint for Desktop App display. Phase 2 validates via Rust standard library `alloc::alloc::GlobalStats` (nightly) or external `jemalloc` stats during development. Verification of target constraints is not within Phase 2 functional scope.

```bash
acowork-runtime \
    --agent-id com.example.weather \
    --package-path /path/to/agent-package \
    --work-dir /home/user/.local/share/agent-gateway/agents/com.example.weather/workspace \
    --config-dir /home/user/.local/share/agent-gateway/agents/com.example.weather/config
```

**Startup parameter descriptions:**

| Parameter | Required | Description |
|-----------|----------|-------------|
| `--agent-id` | Yes | Bot identifier, consistent with manifest |
| `--package-path` | Yes | .bot package path (extracted directory or ZIP file) |
| `--work-dir` | Yes | Bot working directory (containing data/, config/, memory/) |
| `--mqtt-port` | No | Gateway embedded MQTT broker port (communicates with Gateway; see ADR-033) |
| `--http-port` | No | Runtime localhost HTTP port (Desktop reverse proxy discovery; see ADR-033) |
| `--config-dir` | No | User config directory (defaults to work-dir/config/) |
| `--dev-mode` | No | Enable developer mode (Debug protocol) |
| `--debug-port` | No | Debug HTTP RPC port (default 19878) |
| `--log-level` | No | Log level (trace/debug/info/warn/error, default info) |

Runtime connects to Gateway's embedded MQTT broker via `--mqtt-port`; MQTT is the unified transport protocol after ADR-033 (replacing historical gRPC bidirectional streaming). See [06-communication.md](./06-communication.md) and [ADR-033](../adr/zh/ADR-033-mqtt-replace-grpc.md).

**Identity information acquisition:**

User identity information (name, city, language etc.) **is NOT passed via command-line arguments** (avoid `/proc/<pid>/cmdline` leakage). After Runtime starts, it connects via MQTT AgentHello handshake, with Gateway injecting UserProfile as part of AgentHelloResult. Flow:

```
1. Runtime starts, connects to Gateway MQTT endpoint
2. Sends AgentHello (containing agent_id, package_path, work_dir)
3. Gateway returns AgentHelloResult, which contains:
   - user_identity: { name, city, language, ... }
   - provider_list: LLM config list
   - mcp_list: MCP server list
   - Corresponding Key Vault entries (encrypted transfer)
4. Runtime stores in AgentCore, for Context Builder use
```

## 2. Internal Structure

```
Agent Runtime binary
├── Package Loader      # Parse .agent ZIP, load manifest + prompts + skills (config lives in workspace/config, not at package level)
├── AgentCore (Arc)     # Cross-Session shared state
│   ├── Provider       # LLM Provider (direct LLM API connection)
│   ├── Tool Registry  # Tool registry
│   ├── Manifest       # Agent config
│   ├── Budget Guard   # Budget management
│   ├── Model Caps     # Model capabilities pushed by Gateway
│   └── Agent Config   # workspace/config/agent_config.json persistence
├── SessionManager      # Multi-session management
│   └── HashMap<SessionId, SessionHandle>
│       └── SessionHandle { inbound_tx, task: JoinHandle, on_chunk }
├── SessionTask (per session, independent tokio task)
│   └── SessionState   # per-session independent state
│       ├── History Manager     # Conversation history management (token budget, trim)
│       ├── Loop Detector      # Loop detection
│       ├── Model Override     # per-session model selection
│       ├── Conversation       # JSONL persistence
│       └── Token Usage        # per-session usage
├── MQTT Client         # Gateway communication (pub/sub, replaces old IPC)
├── Context Builder     # Assemble system prompt (identity + autobiographical + tools + skills + memory + workspace context)
├── Tool Dispatcher     # Parse LLM output's tool_calls, route to tool implementations
│   ├── Built-in Tools  # Built-in tools
│   ├── MCP Tools       # MCP protocol tools (loaded on demand)
│   ├── WASM Tools      # WASM tools (Wasmtime sandbox execution)
│   └── Gateway Tools   # Operations needing Gateway coordination
├── Permission Checker  # Verify tool calls against manifest permission table
├── Approval Gate       # User confirmation before high-risk tool execution
├── Memory Manager      # Memory lifecycle management
│   ├── Middleware Chain   # Memory middleware
│   ├── Store Backend      # MemoryStore trait implementation
│   └── RagClient (opt)    # RAG retrieval client
├── SQLite memory layer (embedded)     # Private Memory storage engine
├── Skill Loader        # Load Skills (SKILL.md + SQLite memory layer experience layer)
├── Debug/DevMode       # Debug protocol (HTTP RPC + MQTT events, ADR-048; optional)
├── MCP Manager         # MCP server connection management (activate on demand)
└── Budget Manager      # Local budget pre-check + usage reporting
```

## 3. Main Loop

The core of Bot Runtime is the LLM interaction loop:

```
User message / Intent / Scheduled trigger / Interrupt message injection
       │
       ▼
┌──────────────────────────────────────────────┐
│  Agent Runtime main loop [iteration: 0..N]       │
│                                               │
│  ⓪ Message merging (drain from InboundQueue)         │
│     ├─ UserMessage → append to History           │
│     ├─ SystemNotification → append to History    │
│     │   (identity_update / capability_update)│
│     ├─ IntentMessage → append to History         │
│     └─ No new messages → skip                       │
│                                               │
│  ① Budget pre-check
│     ├─ Local budget cache insufficient → handle per action_on_exhaust
│     │   (stop / fallback / warn)
│     └─ Budget exhausted and no fallback → end loop  ──► END
│
│  ② Build context (splice by priority, see §3.1)
│     ├─ System Prompt (from prompts/)
│     ├─ Identity Context (from Gateway injection)
│     ├─ Autobiographical (History Manager triggers
│     │   compression, see §3.1)
│     ├─ Workspace Context (from Gateway push)
│     ├─ Tool Definitions (from manifest.tools)
│     │   └─ Only tools declared in manifest (including RAG tools,
│     │      only registered when manifest declares type=rag)
│     ├─ Capability Overview (from Gateway push)
│     ├─ Skill Instructions (from skills/)
│     ├─ Memory Retrieve → MemoryManager.retrieve()
│     │   ├─ SQLite memory layer channel (always executed)
│     │   │   hybrid_search + graph_expand
│     │   └─ RAG channel (only when manifest declares rag)
│     │       RagClient.query(user message, top_k=3)
│     │       timeout(5s)/unreachable → skip, don't block
│     ├─ Memory Inject → MemoryManager.inject()   
│     │   Trim and format memory context by token budget
│     │   Results labeled by source [Memory] / [RAG:<name>]
│     └─ Conversation history (from History Manager)
│
│  ②.5 Context compression (Token budget management)
│     ├─ Token usage < 70% → log, don't intervene
│     ├─ Token usage ≥ 80% → LLM summary (Compact Model)
│     │   ├─ Full context input (no folding/truncation of any content)
│     │   ├─ Protect system prompt + recent 2-3 rounds
│     │   ├─ Compress middle section into structured summary
│     │   ├─ Full history archived to temp file
│     │   └─ ⚡ Trigger Episode extraction (compressed messages)
│     └─ Token usage ≥ 95% / API reports ContextOverflow
│         → emergency_trim (keep last N non-system messages)
│         → rebuild request, retry
│                                               │
│  ③ Call LLM (direct API)                        │
│     ├─ RateAcquire rate coordination                    │
│     │   ├─ granted: true → continue               │
│     │   ├─ granted: false + retry_after_ms     │
│     │   │   → wait and retry (not counted in LLM retries)│
│     │   └─ 429 insufficient balance (non-retryable) → end      │
│     ├─ streaming or blocking                   │
│     │   └─ tool_calls detected in streaming →     │
│     │     interrupt streaming, store output text     │
│     │     → enter ④                            │
│     └─ Failure → retry or fallback (see §7)     │
│                                               │
│  ④ Parse response                                    │
│     ├─ text → return result/reply to user  ──────────► END│
│     └─ tool_calls → ④.5                       │
│                                               │
│  ④.5 Tool Call deduplication (see §7.5)                │
│     └─ Same (tool_name, params) in same round → skip    │
│                                               │
│  ⑤ Tool dispatch and execution (parallel)                      │
│     ├─ Permission Check (manifest permission table)      │
│     ├─ Approval Gate (high-risk tools, see §7.4)    │
│     │   └─ requires_approval: true → Gateway   │
│     │     send PermissionRequest → wait for user confirmation│
│     │       ├─ User rejects → return error to LLM    │
│     │       └─ User timeout → same as above                │
│     ├─ Execute all tool_calls in parallel (join_all)     │
│     │   ├─ Built-in Tool → execute directly            │
│     │   ├─ RAG Tool → RagClient HTTP call (only │ registered when manifest declares rag)         │
│     │   ├─ WASM Tool → Wasmtime sandbox execution       │
│     │   └─ Gateway Tool → Socket call          │
│     └─ Collect all execution results uniformly, failure → error info as    │
│        tool result                             │
│                                               │
│  ⑥ Append results to history                              │
│     └─ Memory Record → MemoryManager.record()  │
│         Asynchronously record this round of interaction to experience layer (episode)       │
│         (Not Episode extraction, just event recording)          │
│                                               │
│  ⑦ Usage report (async, non-blocking)                    │
│     ⚡ Session end → trigger global summary Episode  │
│                                               │
│  ⑧ Loop detection (see §3.2)                        │
│     ├─ Exact Repeat / Ping-Pong / No Progress  │
│     └─ Three-level progressive response: Warning → Block → Break   │
│                                               │
│  ⑨ Iteration count check                                │
│     └─ Reached max_iterations → force end ──────► END│
│                                               │
│  └─→ Back to ⓪ (next iteration, check message queue first)     │
└──────────────────────────────────────────────┘
```

### 3.1 Context Building Rules

Prompt Builder splices context in the following order, higher priority is more forward (LLM has higher attention weight for forward content):

| Order | Part | Source | Description |
|-------|------|--------|-------------|
| 1 | System Prompt | `prompts/system.md` + `prompts/rules.md` | Bot identity (role attributes) + business-scenario rules (system of record, tool boundaries, deliverable areas, approval gates, workflow). Section order is fixed by the Runtime as `system.md` → `rules.md` → the rest by filename; see [ADR-120](../../adr/en/ADR-120-system-and-rules-prompt-split.md). Cannot be overridden later |
| 2 | Identity Context | Gateway injection | User identity info (name, city etc.), Bot "knows" user |
| 2.5 | Autobiographical | SQLite memory layer AutobiographicalNode | Bot self-cognition (Identity/Capability/Limitation), injection cap 200 tokens. History Manager detects History node count when building context; exceeds 10 triggers rule engine merge (concatenate events by timeline, deduplicate, truncate to 200 tokens, zero LLM calls); merge executed by Runtime background task, no user intervention needed. Phase 3 can upgrade to LLM semantic summary |
| 2.8 | Workspace Context | Gateway push | Workspace environment info (current selection + high-weight Top2, max 3) |
| 3 | Tool Definitions | `manifest.toml [tools]` | Convert to JSON Schema format tool descriptions for LLM to call |
| 4 | Capability Overview | Gateway push | Installed Bots and their capability summary, so LLM knows who to collaborate with |
| 5 | Skill Instructions | `skills/*/SKILL.md` + SQLite memory layer experience layer | Optional skill instructions, extending Bot behavior patterns. See [13-skill-system.md](./13-skill-system.md) |
| 6 | Memory Context | `MemoryManager.retrieve()` + `MemoryManager.inject()` | Memory retrieval and injection. Retrieve via MemoryStore trait's `hybrid_search` + app-layer multi-hop expansion on SQLite memory channel; if manifest declares RAG (`rag_client: Option<Arc<RagClient>>`), parallel query RAG channel (user message as query, top_k=3, 5s timeout degradation); results labeled `[Memory]` / `[RAG:<name>]` by source, trimmed by token budget for injection. See [05-memory.md](./05-memory.md) §10, [00-prd.md](./00-prd.md) §1.13.1 |
| 7 | Conversation History | History Manager | Complete message sequence of current conversation |

#### 2.8 Workspace Context

Gateway pushes workspace environment information to Runtime via IPC. This context includes the Bot's primary workspace path and user-authorized project directory list.

**Filtering strategy**: To avoid the workspace list being too long causing context bloat, use dynamic filtering:
- Currently selected workspace (`is_current = true`) is always included
- Other workspaces sorted by normalized weight, take Top 2:
  - `normalized_count = select_count / max_select_count` (normalized to [0, 1])
  - `recency_score = 1.0 / (1.0 + days_since_last_select)` (range (0, 1])
  - `score = normalized_count * 0.3 + recency_score * 0.7`
- Inject at most 3 workspace directories

**Injection format**:

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

**Trigger timing**:
- Gateway actively pushes once when Bot starts
- When user switches current workspace via Desktop App, push update in real time

**Token budget allocation and trimming strategy**: When total context length approaches model limit, use three-stage strategy:

1. **70% monitoring**: Report token usage to Gateway via ContextUsage event, don't intervene.
2. **80% LLM summary (Compaction)**: Use Compact Model to perform LLM summary on full conversation history (`compact_via_llm`). Summary text simultaneously used for: (a) replacing memory middle section (`replace_middle_with_summary`, preserve system prompt + last 3 rounds), (b) writing SQLite memory layer experience layer (summary is distillation, ADR-011). After Compaction complete, set `is_compacted = true`; when new user message arrives, reset to `false`.
3. **95% emergency_trim**: Preserve system prompt + last 4 non-system messages, as safety net. Only used when LLM summary cannot execute (API error) or usage spikes to 95%.

> **Design decision**: Context compression is a semantic understanding task; only LLM can reliably judge what info can be discarded. Programmatic strategies (character truncation, FIFO, role folding) essentially use proxy metrics to replace semantic understanding, and will inevitably fail. See [ADR-010](../../adr/zh/ADR-010-context-compression-simplification.md). Compaction and Distillation unified as single call: same summary text both replaces memory (compress context) and writes SQLite memory layer (generate experience memory). See [ADR-011](../../adr/zh/ADR-011-compaction-as-distillation.md).

System Prompt (1), Identity Context (2), Autobiographical (2.5), Workspace Context (2.8), Tool Definitions (3) are always retained, not participating in trimming.

### 3.2 Loop Detection Strategy

Prevent LLM from falling into dead loops of repeatedly calling tools. Three detection modes + three-level progressive response.

**Three Detection Modes:**

| Mode | Detection Rule | Default Threshold | Typical Scenario |
|------|----------------|-------------------|------------------|
| Exact Repeat | N consecutive identical `(tool_name, params)` | 3 | LLM repeatedly calls same tool with same params |
| Ping-Pong | Two tools alternate A→B→A→B for N cycles | 4 | tool_A result triggers tool_B, tool_B triggers tool_A |
| No Progress | Same tool different params but result hash identical, N consecutive | 5 | LLM tries different ways to call same tool but gets no new info |

**Three-Level Progressive Response (each mode counts independently):**

| Level | Trigger Condition | Behavior |
|-------|-------------------|----------|
| Warning | First hit detection threshold | Inject system warning into conversation: "Detected repeated calls to [tool_name], please try a different approach." LLM sees warning next round and autonomously adjusts strategy. Iteration continues. |
| Block | Second hit (triggers again after Warning) | Reject this tool call, construct error tool result: "Tool call blocked: loop detection triggered." LLM forced to switch tool or change params. Iteration continues. |
| Break | Third hit | End current iteration, write loop detection info as final assistant message in history, return hint to user. Exit main loop. |

**Configuration:**

```toml
# Can override defaults in manifest.toml
[loop_detection]
exact_repeat_threshold = 3     # Exact Repeat consecutive same call threshold
ping_pong_threshold = 4        # Ping-Pong alternating cycle threshold
no_progress_threshold = 5      # No Progress no-progress threshold

# Fine-grained control (optional, inherits loop_detection config by default)
[loop_detection.exact_repeat]
enabled = true
[loop_detection.ping_pong]
enabled = true
[loop_detection.no_progress]
enabled = true  # No Progress requires computing result hash, slightly more expensive, can be disabled as needed
```

**Implementation notes:**

- Detection range is the complete call sequence after step ⑥ appending to history, not "current iteration's tool_calls" — because step ⑥ is before step ⑧, won't miss detection
- Each mode's counter resets to 0 when continuous hit is interrupted: if LLM successfully calls a different tool (didn't trigger same mode), counter resets to zero. Three-level response escalation only takes effect within the same mode's consecutive hits
- No Progress's result hash uses tool result's first 256 chars + length combination hash, avoiding full text calculation for large results
- Warning message not counted in user's conversation history token budget (is system message)

### 3.3 InboundQueue (Message Injection Queue)

AgentLoop maintains a `mpsc::channel` as inbound message queue, allowing external to inject messages into Bot during loop runtime. Before each iteration begins (step ⓪), drain queue, merge pending messages into conversation history.

**Message types:**

| Type | Source | Processing |
|------|--------|------------|
| `UserMessage` | Desktop App / CLI (user appends content while Bot is running) | Append as new `user` message to History |
| `SystemNotification` | Gateway push (identity_update / capability_update) | Append as `system` message, LLM sees in next round |
| `IntentMessage` | Other Bots (routed via Gateway) | Append as `user` message (with Intent metadata) |

**Design notes:**

- Queue capacity recommended 64, backpressure on overflow (sender blocks) instead of drop, to avoid message loss
- Drain operation non-blocking (`try_recv` not `recv`) — when queue empty, zero wait, directly enter step ①
- Message injection doesn't interrupt currently executing step, only takes effect at iteration boundary (step ⓪)
- `inbound_tx: mpsc::Sender<InboundMessage>` held by Runtime's IPC layer, Gateway invokes it via push message
- Distinguished from "task-level parallelism": InboundQueue's goal is **context supplementation**, not creating new execution branches

### 3.4 Tool Parallel Execution

Step ⑤ changes from serial to parallel execution, reason: LLM response may contain multiple independent `tool_calls` (e.g. query weather + query calendar simultaneously), no data dependencies between them, serial execution amplifies total latency.

**Execution flow:**

```rust
// Serial (old)
for tool_call in deduped_calls {
    let result = execute_tool(tool_call).await?;
    results.push(result);
}

// Parallel (new)
let futures: Vec<_> = deduped_calls.iter()
    .map(|call| execute_tool(call))
    .collect();
let results = futures::future::join_all(futures).await;
```

**Permission check and Approval Gate**: Still serial, executed **before** parallel execution — first batch permission check for all tool_calls, tools requiring user confirmation (`requires_approval: true`) first go through Approval Gate, all confirmed then parallel execute. This avoids wasting time serial-executing other tools while waiting for user confirmation.

**Concurrency safety (design constraint)**: Each tool call obtains independent resource handle, doesn't share mutable state. Design constraints:
- Built-in Tool: Must be designed to be safely concurrently callable (additional synchronization needed if introducing cache/connection pool)
- WASM Tool: Each call creates independent Wasmtime Instance
- Gateway Tool: Each IPC request carries independent `call_id`, Gateway side has stateless routing

**Failure handling**: `join_all` waits for all tools to complete (no short-circuit), single tool failure doesn't affect other tools' execution results, failed tool_call returns `ToolResult` containing error info, letting LLM decide how to handle in next round.

### 3.5 Tool Parallel + Timeout/Cancel Semantics

Parallel execution introduces three-layer timeout/cancel interaction semantics, need to clearly define each layer's responsibility, avoid implementation divergence:

**Three-layer timeout definition:**

| Layer | Control Point | Trigger Timing | Behavior |
|-------|---------------|----------------|----------|
| Iteration overall timeout | Main loop layer (step ⑨) | `iteration_timeout_ms` reached | Drop all unfinished tool futures, return collected results, end iteration |
| Single tool timeout | Tool execution layer (step ⑤ internal) | Single `execute_tool` call timeout | That tool returns `ToolResult { ok: false, error: "timed out" }`, other tools continue |
| LLM call timeout | Step ③ | LLM response exceeds timeout | Entire step ③ aborts, that iteration ends (doesn't enter step ⑤) |

**Behavior of join_all when iteration overall timeout triggers:**

When iteration overall timeout triggers, main loop cancels step ⑤'s future via `tokio::time::timeout` or `tokio::spawn`'s abort handle. At this point `JoinError` is produced inside join_all (unawaited futures are dropped), handling strategy:
- Tools completed before timeout: results normally collected (as expected)
- Tools started but not completed before timeout: dropped, results lost, not written to History
- Some results collected before timeout: those results are still valid

> **Note**: This is the semantic boundary between join_all "wait for all tools to complete" and "iteration timeout directly drops future". Design choice is **don't wait for unfinished tools** — because timeout means this iteration has already exceeded expected time, continuing to wait further delays user response.

**Implementation constraint (spawn + select scheme):**

To implement "when iteration times out, partial tool results still available" semantics, can't use `timeout(join_all(...))` (Rust this combo either returns all or drops all), need to use `tokio::spawn` to run each tool independently + `tokio::select!` to poll:

```rust
use tokio::sync::mpsc;

let (tx, mut rx) = mpsc::channel::<(usize, ToolResult)>(deduped_calls.len());

// ① Spawn independent task for each tool_call
let handles: Vec<tokio::task::JoinHandle<()>> = deduped_calls
    .iter()
    .enumerate()
    .map(|(idx, call)| {
        let tx = tx.clone();
        tokio::spawn(async move {
            let result = tokio::time::timeout(
                Duration::from_millis(TOOL_TIMEOUT_MS),  // single tool timeout
                execute_tool(call)
            ).await.unwrap_or_else(|_| ToolResult {
                ok: false,
                error: "tool execution timed out"
            });
            let _ = tx.send((idx, result)).await;  // result written to channel
        })
    })
    .collect::<Vec<_>>();

// ② Iteration overall timeout control: subtract time consumed by steps ①②③④
let deadline = Instant::now() + Duration::from_millis(iteration_timeout_ms - elapsed);
let mut results: Vec<(usize, ToolResult)> = Vec::with_capacity(deduped_calls.len());
let total = deduped_calls.len();

while results.len() < total {
    tokio::select! {
        // result arriving, collect
        entry = rx.recv() => {
            if let Some((idx, result)) = entry {
                results.push((idx, result));
            }
        }
        // iteration overall timeout: abort unfinished tasks, stop waiting
        _ = tokio::time::sleep_until(deadline.into()) => {
            for handle in handles {
                handle.abort();  // don't wait, cancel immediately
            }
            break;
        }
    }
}

// ③ Assemble results in original order, unfinished slots filled with timeout error
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

**Key constraints:**
- Single tool timeout handled independently inside each spawn (`tokio::time::timeout`), independent of iteration overall timeout
- Iteration overall timeout controlled via `tokio::select!` + `deadline`; on timeout, call all `handle.abort()`, don't wait for join
- After `handle.abort()`, that slot in results Vec filled with explicit timeout error, not recorded to History (wait for LLM's next round decision)
- On iteration timeout, record a system message in History: `"[iteration timed out after N ms, N tool(s) not completed]"`, where N is unfinished tool count
- In `rx.recv()` loop, use `while results.len() < total` to prevent select idle loop, ensure exit immediately after collecting all results

### 3.6 Loop Exit Conditions

| Condition | Trigger Timing | Behavior |
|-----------|----------------|----------|
| LLM returns pure text | Step ④ | Normal end, return result to user |
| Budget exhausted | Step ① | Handle per `action_on_exhaust`; stop ends |
| Reached max_iterations | Step ⑨ | Force end, return executed results |
| Loop detection Break | Step ⑧ | Break level in three-level response, end iteration and notify user |
| Single iteration timeout | Step ③/⑤ | End current iteration after timeout |
| Gateway stop signal | Any step | Graceful exit, save current state |
| LLM call retry exhausted | Step ③ | End when no fallback provider |
| Context exceeded recovery failure | Step ③ | End when emergency_trim safety net cannot satisfy (see §7.1) |

## 4. Runtime Default Configuration

When manifest.toml doesn't explicitly declare, Runtime uses the following defaults:

| Parameter | Default | Description |
|-----------|---------|-------------|
| `max_iterations` | 50 | Max iterations per conversation (can be overridden via Gateway `RuntimeConfigUpdate` at runtime) |
| `iteration_timeout_ms` | 30000 | Single iteration timeout (includes LLM call + tool execution) |
| `history_max_tokens` | 128000 | Conversation history upper limit (triggers trim/compress when exceeded) |
| `loop_detection.exact_repeat_threshold` | 3 | Exact Repeat detection threshold |
| `loop_detection.ping_pong_threshold` | 4 | Ping-Pong alternating cycle threshold |
| `loop_detection.no_progress_threshold` | 5 | No Progress no-progress threshold |
| `loop_detection.no_progress.enabled` | true | No Progress detection switch (requires result hash computation) |
| `llm.routing.retry.max_attempts` | 3 | LLM call retry count |
| `llm.routing.retry.backoff` | "exponential" | Retry backoff strategy |
| `llm.routing.retry.max_wait_ms` | 30000 | Retry max wait time (RateAcquire retry_after upper limit) |
| `approval.timeout_ms` | 60000 | High-risk tool user confirmation timeout |

## 5. Built-in Tools List

See [12-tool-system.md](./12-tool-system.md) §2.

## 6. Gateway Tools (Operations Requiring Gateway Coordination)

See [12-tool-system.md](./12-tool-system.md) §4.

## 7. Error Handling Strategy

### 7.1 LLM Call Failure

```
LLM call failure (network timeout / API error / token limit exceeded)
       │
       ▼
Classify error type:
  ├─ Context Window Exceeded
  │   ├─ Reactive Recovery (see below)
  │   └─ Recovery failure → end loop
  │
  ├─ Rate Limited (429)
  │   ├─ Retryable rate limit (concurrency/frequency limit)
  │   │   ├─ Parse Retry-After header
  │   │   ├─ Wait min(retry_after, max_wait_ms)
  │   │   ├─ Try API Key rotation (Vault multi-key)
  │   │   └─ Counted in retry count
  │   └─ Non-retryable rate limit (insufficient balance/package limit)
  │       └─ End immediately, not counted in retry count
  │
  ├─ Network timeout / 500 / 502 (retryable errors)
  │   ├─ Retry per manifest.llm.routing.retry config
  │   │   ├─ max_attempts (default 3)
  │   │   └─ backoff: exponential
  │   └─ Retry success → continue
  │
  └─ Other errors (401/403 etc., non-retryable)
      └─ End immediately
       │
       ▼
Retry exhausted → check fallback:
```

[continues with detailed error handling... see zh/03-agent-runtime.md §7.1-§7.6 for full content]