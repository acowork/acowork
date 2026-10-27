# Tool System

> Version: v3.5 | Last Updated: 2026-10-23

> **v3.5 change (implemented)**: `memory_recall` / `memory_store` are no longer built-in tools — memory was split out into a Gateway-hosted standalone service, `acowork-memory`, and Agents reach it through the MCP tools `mcp_memory__memory_recall` / `mcp_memory__memory_store` ([ADR-101](../../adr/en/ADR-101-memory-standalone-mcp-service.md)). The `memory:read` / `memory:write` permission entries were removed along with them, as was the "main loop step ② MemoryManager.retrieve() auto-inject" described in §4 / §4.2.1 of this document (auto-inject, ADR-101 decision 5) — memory is now fetched only by an explicit tool call. Those sections are kept as a historical design record; actual behaviour follows ADR-101.

---

The tool system is Agent's sole channel for perceiving and operating the world. LLM calls tools via tool_calls; Runtime parses and routes to corresponding tool implementations. All tool executions undergo permission checks; custom tools run isolated in WASM sandbox.

## 1. Tool Classification

```
Tool Dispatcher
├── Built-in Tools     # Runtime built-in, available in Phase 1
├── WASM Tools         # Bundled with .agent package, executed in Wasmtime sandbox
├── RAG Tools          # Enterprise RAG integration, query external knowledge base
└── Gateway Tools      # Operations requiring Gateway coordination (not triggered by LLM tool_call)
```

| Type     | Source                             | Execution Environment | LLM Directly Callable | Phase |
|----------|------------------------------------|-----------------------|----------------------|-------|
| Built-in | Runtime built-in                   | Host process          | Yes                  | Phase 1 |
| WASM     | .agent package tools/ directory    | Wasmtime sandbox      | Yes                  | Phase 1 (decl + sandbox) / Phase 3 (full permissions) |
| RAG      | manifest declaration, pointing to enterprise RAG service | Remote HTTP call | Yes                  | Phase 2 |
| Gateway  | Gateway Service API                | Gateway process       | No (Runtime internal) | Phase 4 |

## 2. Built-in Tools List

The following tools are built-in implementations of Agent Runtime; Agents can declare use in manifest without providing implementation code.

**Platform Infrastructure-Level Tool Definition**: The scope of built-in tools is limited to **platform infrastructure level** — calling open protocols (HTTP/DNS/filesystem/OS APIs) or local computation (WASM/Embedding), not depending on specific third-party service paid APIs. SaaS integration (Jira/Notion/LinkedIn etc.) is provided by independent Agents, not built-in. 

| Tool Name | Function | Required Permission | Description |
|-----------|----------|---------------------|-------------|
| ~~`memory_recall`~~ | ~~Semantic search of private SQLite memory layer~~ | ~~`memory:read`~~ | **Migrated** — ADR-101: memory is a standalone service now, so this tool is `mcp_memory__memory_recall` on the Gateway-hosted memory MCP. No permission domain; per-tool opt-in happens in `agent_mcp_tools.json` (ADR-069). Hybrid search (HNSW + BM25) + associative diffusion semantics unchanged |
| ~~`memory_store`~~ | ~~Write to private SQLite memory layer~~ | ~~`memory:write`~~ | **Migrated** — same as above, now `mcp_memory__memory_store`; real-time extraction, the five types and the importance / privacy parameters keep their semantics, only the call channel changed |
| `http_request` | HTTP requests (GET/POST/PUT/DELETE) | `network:<url_pattern>` | Supports method parameter for HTTP method selection; JSON responses auto-parsed; supports JSON body and form |
| `web_fetch` | Fetch web page content | `network:<url_pattern>` | HTML → Markdown conversion, Agent directly receives readable text |
| `shell` | Execute shell commands | `filesystem:exec` | Subject to sandbox restrictions, interruptible on timeout |
| ~~`file_read`~~ | ~~Read file~~ | ~~`filesystem:read:<path>`~~ | **Deprecated** — no longer registered, so a manifest declaration has no effect. File reading goes through `bash`/`powershell`/`shell` (`cat`, `sed -n`, `Get-Content`). The implementation stays in `tools/builtin/file_read.rs`, it is only out of the registry |
| `file_write` | Write file | `filesystem:write:<path>` | Restricted to workspace and authorized directories |
| `file_edit` | Precise file editing | `filesystem:write:<path>` | Line range based editing (replace/insert/delete), more precise than file_write |
| ~~`glob_search`~~ | ~~Filename pattern search~~ | ~~`filesystem:read:<path>`~~ | **Deprecated** — same as above, use the shell's `find` / `Get-ChildItem`; implementation stays in `tools/builtin/glob_search.rs` |
| ~~`content_search`~~ | ~~File content search~~ | ~~`filesystem:read:<path>`~~ | **Deprecated** — same as above, use the shell's `grep -rn` / `Select-String`; implementation stays in `tools/builtin/content_search.rs` |
| `intent_send` | Send Intent to other Agents | `intent:send:<target>` | Routed via Gateway |
| ~~`identity_store`~~ | ~~Write user identity information~~ | ~~`identity:write`~~ | **Deleted** — User identity management migrated to Gateway UserProfile, see `18-user-identity-simplified.md` |
| ~~`identity_query`~~ | ~~Query user identity information~~ | ~~`identity:read`~~ | **Deleted** — User identity management migrated to Gateway UserProfile |
| ~~`identity_observe`~~ | ~~Subscribe to identity change notifications~~ | ~~`identity:observe`~~ | **Deleted** — User identity management migrated to Gateway UserProfile |

> **Deprecation note (`file_read` / `glob_search` / `content_search`)**
>
> These three file-*reading* tools are unregistered from the Runtime
> registry — deprecated, not deleted:
>
> - Sources and unit tests stay compiled
>   (`core/acowork-runtime/src/tools/builtin/{file_read,glob_search,content_search}.rs`);
>   the `pub mod` declarations are not removed.
> - `all_builtin_tools()` no longer constructs them, so they never reach the
>   LLM tool surface, the frontend Tools panel, or the create wizard's builtin
>   tool list.
> - A leftover `[[tools]] name = "file_read"` in a manifest is silently dropped
>   by `init_tools_config_from_manifest` / `merge_tools_config` and written back
>   to `agent_tools.json`, so it cannot linger as an unrenderable ghost row.
>   Clone and publish take the same path — no migration step is needed.
> - The replacement capability is the shell tool surface: `cat` / `Get-Content`
>   to read, `sed -n` for a line range, `find` / `Get-ChildItem` for names,
>   `grep -rn` / `Select-String` for content. Output caps and path validation
>   are enforced by the shell wrapper in `tools/wrappers.rs`, i.e. the same
>   red lines the three dedicated tools used to enforce themselves.
>
> To restore them, follow the checklist in the
> `core/acowork-runtime/src/tools/builtin/mod.rs` module header.

### 2.1 Platform Support Matrix

| Tool Name | Windows | Linux | macOS | Android | iOS | Availability Level |
|-----------|---------|-------|-------|---------|-----|---------------------|
| `http_request` | ✅ | ✅ | ✅ | ✅ | ✅ | All platforms |
| `web_fetch` | ✅ | ✅ | ✅ | ✅ | ✅ | All platforms |
| `shell` | ✅ | ✅ | ✅ | ❌ | ❌ | Desktop only |
| ~~`file_read`~~ | ❌ | ❌ | ❌ | ❌ | ❌ | **Deprecated, not registered** |
| `file_write` | ✅ | ✅ | ✅ | ⚠️ | ⚠️ | All platforms, mobile restricted |
| `file_edit` | ✅ | ✅ | ✅ | ⚠️ | ⚠️ | All platforms, mobile restricted |
| ~~`glob_search`~~ | ❌ | ❌ | ❌ | ❌ | ❌ | **Deprecated, not registered** |
| ~~`content_search`~~ | ❌ | ❌ | ❌ | ❌ | ❌ | **Deprecated, not registered** |
| `intent_send` | ✅ | ✅ | ✅ | ✅ | ✅ | All platforms |
| ~~`identity_store`~~ | ❌ | ❌ | ❌ | ❌ | ❌ | **Deleted** |
| ~~`identity_query`~~ | ❌ | ❌ | ❌ | ❌ | ❌ | **Deleted** |
| ~~`identity_observe`~~ | ❌ | ❌ | ❌ | ❌ | ❌ | **Deleted** |

> ✅ Full support | ⚠️ Restricted support (degraded behavior) | ❌ Unavailable

### 2.2 Cross-Platform Degradation Strategy

**Availability Level Definition:**

| Level | Meaning | Impact on Agent |
|-------|---------|------------------|
| All platforms | Consistent behavior on all platforms | Agent can use without difference |
| Desktop only | Mobile unsupported | When Agent declares `required = true`, mobile install rejected |
| All platforms, mobile restricted | Mobile behavior degraded | Agent can use but needs to adapt to degraded behavior |

**Specific Degradation Behavior:**

`shell` (Desktop only):
- iOS sandbox forbids external process execution; Android only supports it in rare scenarios (root or Termux)
- Desktop also has shell differences: Windows is cmd/PowerShell, Linux/macOS is bash/zsh, command syntax differs
- When Agent declares `required = true` in manifest, mobile install is directly rejected
- When Agent declares `required = false` (default), mobile install succeeds, but `shell` tool is not registered in available tool list

`file_write` / `file_edit` / `doc_reader` (Mobile restricted; `file_read` / `glob_search` / `content_search` are deprecated and unregistered — reading and searching files is carried by the shell tools, whose path scope is bounded by the same allow-list below):
- iOS App Sandbox only allows access to files within its own container
- Android Scoped Storage restricts external storage access
- Degradation behavior: path allow-list automatically narrowed to Agent working directory (`<agent_data_dir>/workspace/`); out-of-range path requests return permission errors
- the shell tools' file search range is likewise limited to the Agent working directory and authorized directories
- Desktop path allow-list controlled by `filesystem:read/write:<path>` permission, can authorize any directory

**Runtime Platform Detection:**

Runtime detects current platform at startup via `std::env::consts::OS` and platform APIs, generates available tool list:

```
Runtime startup
│
├─ Detect platform (desktop / android / ios)
│
├─ Build available tool list
│   ├─ All-platform tools → always register
│   ├─ Desktop-only tools → only register on desktop
│   └─ Mobile-restricted tools → register but inject degraded permission config
│
└─ Inject available tool list into LLM System Prompt
   (LLM only sees tools available on current platform, won't attempt to invoke unavailable tools)
```

**Tool Declaration Example (manifest.toml):**

```toml
[[tools]]
name = "http_request"
type = "builtin"
permissions = ["network:https://api.weather.com"]

# Memory tools are no longer built-ins (ADR-101): they come from the Gateway-hosted
# memory MCP, opted in per tool in `agent_mcp_tools.json` as
# `mcp_memory__memory_recall` (ADR-069). There is no `[[tools]]` entry for them in the
# manifest and no `memory:*` permission left to declare.

# shell declared optional — mobile install not blocked, but tool unavailable
[[tools]]
name = "shell"
type = "builtin"
required = false           # Default false, mobile install not rejected
permissions = ["filesystem:exec"]

# Some Agent strongly depends on shell (e.g. DevOps Agent), declares required = true
# Mobile install rejected, prompts "this Agent requires desktop environment"
[[tools]]
name = "shell"
type = "builtin"
required = true
permissions = ["filesystem:exec"]
```

## 3. WASM Tools (Custom Tool Sandbox)

Custom tools are provided by Agent developers as .wasm files, executed isolated in Wasmtime sandbox.

### 3.1 Runtime Selection: Wasmtime

| Dimension | Choice | Reason |
|-----------|--------|--------|
| **Runtime** | Wasmtime (Bytecode Alliance) | Best standards compliance, most mature security model |
| **Compiler** | Cranelift (default) / Winch (fast startup) | Cranelift has best overall performance; Winch for startup-sensitive scenarios |
| **WASI Version** | Preview 2 | Directory-level sandbox, most refined capability-based security model |
| **License** | Apache 2.0 | No commercial restrictions |
| **crates.io version** | Locked LTS (e.g. v36.x) | Avoid frequent API changes affecting stability |

**Selection Comparison (Why not Wasmer / Wasmi):**

| Dimension | Wasmtime | Wasmer | Wasmi |
|-----------|----------|--------|-------|
| WASI Preview 2 | Full support | Unsupported | Unsupported |
| Component Model | Reference implementation | Partial support | Unsupported |
| Fuel metering | Mature | Limited | Mature |
| Vendor lock-in | None (pure standard) | WASIX locked | None |
| Cold start | 5.2ms | 6.8ms | ~2ms |
| Execution time | 10.4ms | 12.1ms | ~45ms |
| Security audit | Periodic | No public | Runtime Verification |
| Use case | General sandbox | When non-standard extensions needed | iOS / extreme resource constraints |

- **Why not Wasmer**: Core differentiation feature WASIX is non-standard extension, binaries can only run on Wasmer, has vendor lock-in risk; lack of WASI Preview 2 leads to insufficiently refined sandbox filesystem control
- **Wasmi Alternative**: For Phase 4 mobile adaptation, iOS forbids JIT compilation; Wasmi (pure interpreter) serves as mobile WASM engine

### 3.2 WASM Tool Declaration

```toml
[[tools]]
name = "image_filter"
type = "wasm"
binary = "./tools/image_filter.wasm"
permissions = ["filesystem:read:./input"]

[tools.image_filter.resource_limits]
max_memory_mb = 50
max_execution_time_ms = 5000
```

**Field Descriptions:**

| Field | Required | Description |
|-------|----------|-------------|
| `name` | Yes | Tool name, LLM calls via this name |
| `type` | Yes | Must be `"wasm"` |
| `binary` | Yes | .wasm file path (relative to .agent package root) |
| `permissions` | Yes | Required permission list (displayed at install, validated at runtime) |
| `resource_limits.max_memory_mb` | No | WASM linear memory limit (default 50) |
| `resource_limits.max_execution_time_ms` | No | Execution timeout (default 5000) |

### 3.3 WASM Sandbox Security Model

```
Agent Runtime (host process)
│
│  Tool Dispatcher
│     │
│     ▼
│  Wasmtime Engine
│  ┌──────────────────────────────────────┐
│  │  WASM Instance                       │
│  │  ┌────────────────────────────────┐  │
│  │  │  Tool logic (untrusted code)   │  │
│  │  │                                │  │
│  │  │  Can only access:              │  │
│  │  │  ├─ Own linear memory (sized)  │  │
│  │  │  ├─ Host functions (explicitly │  │
│  │  │  │  registered)                │  │
│  │  │  └─ WASI permissions (declared │  │
│  │  │     in manifest)               │  │
│  │  │                                │  │
│  │  │  Cannot access:                │  │
│  │  │  ├─ Host process memory        │  │
│  │  │  ├─ Other tools' memory/state  │  │
│  │  │  ├─ Undeclared file paths      │  │
│  │  │  ├─ Undeclared network addrs   │  │
│  │  │  ├─ LLM API Key                │  │
│  │  │  └─ Other Agents' data         │  │
│  │  └────────────────────────────────┘  │
│  │                                      │
│  │  Security control layer:             │
│  │  ├─ Fuel metering (CPU time limit)   │
│  │  ├─ Memory limit (linear memory)    │
│  │  ├─ WASI Preview 2 capability sec   │
│  │  └─ Execution timeout (max_execution_time_ms) │
│  └──────────────────────────────────────┘
```

**Security Guarantees:**

| Mechanism | Function | Configuration Source |
|-----------|----------|----------------------|
| WASM memory isolation | Tool can only access its own linear memory, cannot exceed bounds | Wasmtime engine-level guarantee |
| Fuel metering | Limit CPU instruction count, prevent infinite loops | Runtime converts from `max_execution_time_ms` |
| Memory limit | Limit linear memory size, prevent OOM | `resource_limits.max_memory_mb` |
| WASI directory allow-list | Can only access manifest-declared paths | `filesystem:read/write:<path>` in `permissions` |
| WASI network allow-list | Can only access manifest-declared addresses | `network:<url_pattern>` in `permissions` |
| API Key invisible | WASM tool cannot read LLM API Key | Host functions don't expose Key, uses secrecy::SecretString |

### 3.4 Host-WASM Communication Protocol

When LLM calls WASM tool, Runtime handles parameter serialization and result deserialization:

```
LLM outputs tool_call:
  { "name": "image_filter", "arguments": {"image_url": "...", "filter": "grayscale"} }
       │
       ▼
Runtime serializes parameters to JSON bytes:
  host_memory → wasm_linear_memory (passed via Host function parameters)
       │
       ▼
WASM tool executes:
  read input → process → write output
       │
       ▼
Runtime deserializes result:
  wasm_linear_memory → host_memory
       │
       ▼
Construct tool result returned to LLM:
  { "filtered_image_url": "...", "status": "success" }
```

**Host Function Interface (Phase 1):**

WASM tools must export the following entry functions:

```rust
// Functions WASM side must export
#[no_mangle]
pub extern "C" fn execute(input_ptr: u32, input_len: u32) -> u32;

// Optional functions WASM side may export (for describing tool's JSON Schema)
#[no_mangle]
pub extern "C" fn schema_ptr() -> u32;
#[no_mangle]
pub extern "C" fn schema_len() -> u32;
```

**Communication Flow:**

1. Runtime serializes `tool_call.arguments` to JSON byte string
2. Writes JSON byte string into WASM linear memory
3. Calls WASM's `execute(input_ptr, input_len)` function
4. WASM tool processes, writes result JSON into linear memory, returns result pointer
5. Runtime reads result JSON from linear memory, deserializes to tool result

**Phase 3+ Upgrade Path**: Component Model provides type-safe interface definitions, replacing manual memory operations. WASM tools can use WIT files to define interfaces; Wasmtime auto-generates type-safe bindings. Phase 1's Host function approach ensures initial simplicity; Component Model ensures long-term extensibility.

### 3.5 WASM Tool Development Toolchain

Recommended flow for Agent developers writing WASM tools:

```
1. Write tool logic in Rust (target: wasm32-wasip2)
   cargo new --lib image_filter
   # Cargo.toml: crate-type = ["cdylib"], target = wasm32-wasip2

2. Implement required export functions:
   - execute(input_ptr, input_len) -> u32
   - schema_ptr() / schema_len() (optional, provides JSON Schema)

3. Compile:
   cargo build --target wasm32-wasip2 --release

4. Place .wasm file in .agent package's tools/ directory

5. Declare tool in manifest.toml (see §3.2)
```

**SDK Support (Phase 2+)**: Provide `acowork-tool-sdk` crate that encapsulates memory allocation, JSON serialization, schema export, etc. boilerplate code; developers only need to implement business logic:

```rust
use acowork_tool_sdk::{tool, ToolInput, ToolOutput};

#[tool(name = "image_filter")]
fn execute(input: ToolInput) -> Result<ToolOutput, ToolError> {
    let image_url: String = input.get("image_url")?;
    let filter: String = input.get("filter")?;
    // ... business logic
    Ok(ToolOutput::from(json!({"filtered_image_url": result})))
}
```

### 3.6 WASM Tool Error Handling

| Error Type | Handling | Description |
|------------|----------|-------------|
| Fuel exhausted | Terminate execution, return timeout error | Prevent infinite loops |
| Memory exceeded | Terminate execution, return OOM error | WASM memory allocation failure |
| Execution timeout | Terminate execution, return timeout error | `max_execution_time_ms` |
| WASM Trap | Terminate execution, return crash error | Division by zero, stack overflow, etc. |
| Insufficient permission | Reject execution, return permission error | Accessing undeclared path/network |
| Business logic error | Return WASM tool's error JSON | Tool itself returns error info |

All errors do not terminate the main loop. Error info is returned to LLM as tool result; LLM decides next step (change params, change tool, or give up).

## 4. RAG Tools (Enterprise Knowledge Base Integration)

RAG tools let Agents connect to enterprise-built RAG knowledge bases, enabling "dual-channel retrieval" — local SQLite memory layer (personal memory) and enterprise RAG (collective knowledge) queried in parallel, with results spliced into LLM context. ACowork does not host RAG services; only defines standard query protocol (request/response JSON Schema); enterprise RAG adapts to this protocol itself (see `00-prd.md` §1.13).

**Configuration-Driven Opt-In**: RAG is not a default capability; only enabled when manifest declares `[[tools]] type = "rag"`. For Agents without RAG declaration, Tool Dispatcher doesn't register RAG tool, MemoryManager.retrieve() only queries SQLite memory layer channel, behavior identical to no RAG.

**Hybrid Dual Trigger**: RAG has two triggering methods, both driven by manifest configuration:

| Trigger Mode | Timing | Query Params | Description |
|--------------|--------|--------------|-------------|
| Automatic | Step ② MemoryManager Retrieve | User message as query, top_k=3, score_threshold=0.7 | Background knowledge injection, LLM doesn't need to actively judge |
| Explicit | Step ⑤ LLM tool_call | LLM custom query/filter/top_k | Targeted deep query |

Automatic trigger results injected as "background context"; explicit tool results appended to History as "tool return values"; the two occupy different positions in context, no semantic overlap.

### 4.1 RAG Tool Declaration

```toml
[[tools]]
name = "enterprise_knowledge"
type = "rag"
description = "Query enterprise product knowledge base, get product params, tech docs, sales scripts, etc."
# RAG service address (configured by Agent developer or enterprise admin)
[tools.enterprise_knowledge.rag_config]
endpoint = "https://rag.internal.company.com/api/query"
collection = "product_docs"
# Auth credentials reference Vault (not plaintext in manifest)
auth_ref = "vault:company_rag_token"
auth_type = "bearer"              # bearer / api_key / oauth2
# Query parameters
max_results = 5
score_threshold = 0.7
```

**Field Descriptions:**

| Field | Required | Description |
|-------|----------|-------------|
| `name` | Yes | Tool name, LLM calls via this name |
| `type` | Yes | Must be `"rag"` |
| `description` | Yes | Description of RAG knowledge base, helps LLM judge when to call |
| `rag_config.endpoint` | Yes | RAG query service HTTP URL |
| `rag_config.collection` | No | Collection/index/namespace in RAG, for multi-tenant isolation |
| `rag_config.auth_ref` | Yes | Auth credential reference (Vault key ID), not stored plaintext |
| `rag_config.auth_type` | No | Auth method, default `bearer` |
| `rag_config.max_results` | No | Max results per query, default 5 |
| `rag_config.score_threshold` | No | Minimum relevance threshold (0-1), results below this not returned, default 0.7 |

### 4.2 RAG Tool Execution Flow (Explicit Trigger)

LLM actively calls RAG tool via tool_call for targeted deep query:

```
LLM outputs tool_call: { name: "enterprise_knowledge", arguments: { query: "Q3 product release plan" } }
       │
       ▼
Runtime parses tool_call
       │
       ├─ Get auth credentials from Vault (one-time, not cached in process memory)
       │
       ├─ Construct RAG standard query request (POST endpoint)
       │   body: { query, collection, top_k, score_threshold }
       │   headers: { Authorization: Bearer <token> }
       │
       ├─ Send HTTP request (timeout 10 seconds)
       │
       ├─ Parse response, mark source (source_url / chunk_id)
       │
       └─ Construct tool result returned to LLM
```

### 4.2.1 RAG Automatic Retrieval Flow (Main Loop Step ②)

Automatically triggered each iteration (only when manifest declares RAG), uses user message as lightweight query:

```
Step ② MemoryManager.retrieve()
  ├─ SQLite memory layer channel: hybrid_search + graph_expand  ← Always executed
  └─ RAG channel: RagClient.query(user message, top_k=3)  ← Only when manifest declares RAG
     ├─ Success → results labeled [Memory] / [RAG:enterprise_knowledge] by source
     ├─ Timeout(5s) → skip RAG channel, use SQLite memory layer results only
     └─ Unreachable → same, doesn't block Agent
  Results merged, deduplicated, trimmed by token budget then injected into LLM context
```

### 4.3 RAG Tool Degradation and Security

| Rule | Description |
|------|-------------|
| Offline degradation | When RAG service is unreachable, return empty results, don't block Agent execution (both auto and explicit triggers degrade) |
| Credential security | auth_ref references Vault key, Runtime fetches each call, not cached in process memory or env vars |
| Result marking | Each RAG result is labeled source_url and chunk_id for LLM and user traceability |
| Query scope restriction | collection field limits query scope, prevents cross-tenant data leakage |
| Network permission | RAG tool's endpoint controlled by `network:<url_pattern>` permission |

### 4.4 Relationship with Local Memory

RAG tool and local SQLite memory layer are two completely independent retrieval channels:

| Dimension | Local SQLite memory layer (memory_recall) | Enterprise RAG (rag tool) |
|-----------|------------------------------|---------------------------|
| Data ownership | Personal user | Enterprise |
| Storage location | Local file (rusqlite) | Enterprise RAG service (remote) |
| Data type | Personal preferences, interaction history, autobiographical | Product docs, business processes, internal specs |
| Retrieval method | Vector + fulltext + associative diffusion (graph expand) | Vector retrieval + optional hybrid keyword + metadata filter |
| Privacy boundary | Agent-private, filtered by PrivacyLevel when sharing package | Enterprise-managed, Agent read-only |

RAG retrieval results and local SQLite memory layer retrieval results are spliced in the transient layer and uniformly fed into LLM context, but not integrated into the Memory system's abstraction layer — their query paradigms and storage models are entirely different.

**Runtime Behavior Differences Driven by RAG Configuration:**

| Runtime Behavior | manifest No RAG Declaration | manifest Has RAG Declaration |
|------------------|------------------------------|------------------------------|
| Step ② MemoryManager.retrieve() | Query SQLite memory layer channel only | Parallel query SQLite memory layer + RAG dual channels |
| Step ② Context injection | SQLite memory layer retrieval results only | SQLite memory layer + RAG results spliced, labeled by source |
| Step ③ LLM Tool Definitions | RAG tool not included | RAG tool included (can explicitly call) |
| Step ⑤ Tool Dispatch | No RAG tool routing | RAG tool → RagClient HTTP call |

The following operations do not belong to "tools" (not triggered by LLM tool_call), but are Runtime-initiated requests to Gateway in specific flows, communicated via Gateway Service API:

| Operation | Trigger Timing | Description |
|-----------|----------------|-------------|
| `KeyRelease` | After startup handshake | Get LLM API Key (one-time) |
| `IdentityDelivery` | After startup handshake | Get user identity information (actively pushed by Gateway) |
| `CapabilityOverview` | After startup handshake | Get installed Agents' capability summary (actively pushed by Gateway) |
| `IntentSend` / `IntentReceived` | When LLM calls `intent_send` tool | Cross-Agent message routing |
| `BudgetQuery` | During budget pre-check | Query remaining budget |
| `UsageReport` | After each iteration (async) | Report LLM usage |
| `RateAcquire` | Before calling LLM | Apply for rate token |
| `PermissionRequest` | Runtime requests additional permissions | Pop-up for user confirmation |

**Key Principle**: LLM calls and tool execution do not go through Gateway — Agent connects directly to LLM API, executes tools locally. Gateway only manages what must be centralized.

## 5. Tool Dispatch Flow

Tool Dispatcher works in main loop Step ⑤ (see `03-agent-runtime.md`):

```
LLM outputs tool_calls: [{name, arguments}, ...]
       │
       ▼
Process each tool_call one by one:
       │
       ├─ ① Find tool definition
       │    ├─ Matching name found in manifest.tools → continue
       │    └─ Not found → return error tool result: "Unknown tool"
       │
       ├─ ② Permission check
       │    ├─ Check if tool_call needs permission that is declared but not authorized
       │    ├─ Insufficient permission → return error tool result
       │    └─ Permission passes → continue
       │
       ├─ ③ Route to tool implementation
       │    ├─ type = "builtin" → Built-in tool executes directly
       │    ├─ type = "rag" → RagClient HTTP call (only registered when manifest declares)
       │    ├─ type = "wasm" → Wasmtime sandbox execution
       │    └─ intent_send → Gateway routing
       │
       ├─ ④ Execute tool
       │    ├─ Success → construct tool result
       │    └─ Failure → construct error tool result (see §3.6)
       │
       └─ ⑤ Append to conversation history → next iteration
```

## 6. Design Decision Records

| Decision | Choice | Reason |
|----------|--------|--------|
| WASM runtime | Wasmtime | Best standards compliance (WASI Preview 2 + Component Model), no vendor lock-in, mature security audit |
| Not Wasmer | WASIX lock-in risk | WASIX is non-standard extension, binaries only run on Wasmer; WASI Preview 2 missing |
| Wasmi alternative | iOS / embedded | Pure interpreter, iOS forbids JIT; minimal attack surface (2 dependencies), passes security audit |
| WASI version | Preview 2 | Directory-level sandbox + capability security, most secure for untrusted tool scenarios |
| Host-WASM communication | Host functions + JSON (Phase 1) → Component Model (Phase 3+) | Phase 1 keeps simple, Component Model provides long-term type-safe upgrade path |
| Tool execution failure | Return error to LLM for decision | LLM can autonomously adjust strategy, more flexible than direct termination |
| Fuel metering | Enabled | Prevent malicious/defective WASM tools infinite loops |
| API Key invisible to WASM | secrecy::SecretString | WASM tools are untrusted code, must never get LLM API Key |
| SDK deferred | Phase 2+ | Phase 1 manual export functions sufficient, SDK lowers barrier but doesn't block core |
| Builtin scope | Platform infrastructure level only | SaaS integration (Jira/Notion/LinkedIn etc.) provided by independent Agents, not built-in; vertical capabilities via WASM Tool or independent Agent |
| web_fetch built-in | Yes | Nearly all Agents need it, platform-level infrastructure |
| file_edit/glob_search/content_search built-in | Yes (`glob_search`/`content_search` later deprecated — see §2.1) | File ops trio (read+write+edit+search); missing any one leads Agent to inefficient simulation with file_write |
| RAG tool type | Independent type="rag", config-driven Opt-In | Enterprise RAG is external service integration, not built-in tool nor WASM tool, needs independent declaration and execution model; only registers when manifest declares, zero intrusion for Agents without RAG |
| RAG credential security | Vault reference, runtime fetch | Consistent with built-in tool API Key management, not plaintext in manifest or process env vars |
| RAG trigger model | Hybrid dual trigger (auto + explicit) | Auto trigger (Step ② Retrieve) solves "LLM doesn't know whether to query"; explicit trigger (Step ⑤ tool_call) solves "needs more precise query"; both opt-in driven by manifest config (ADR-012, see plan-p4.md) |
| RAG protocol adaptation | ACowork defines standard protocol, enterprise RAG self-adapts | No adapter for each RAG implementation; enterprise side ensures their RAG service is compatible with standard query interface; follows PRD "pure integration, not hosting" principle |
