# ADR-029: Builtin Tools Persistence and Enable Control — agent_tools.json

> **Panel-location revision (2026-11-06, [ADR-108](./ADR-108-right-panel-consolidation.md))**: the right-rail Tools panel no longer exists — every reference below to "the Tools panel" / "the Builtin Tools card" / "the MCP card" now means the **Setup panel** (`AgentSetupTab`); the rail entry and its tab are gone. Endpoints, stores, permissions and switch semantics are unchanged; see ADR-108 for the scope.

> **Chinese source of truth**: [ADR-029](../zh/ADR-029-agent-tools-persistence-and-toggle.md)
> **Terminology**: see [GLOSSARY.md](./GLOSSARY.md)

## Status

Draft (pending decision)

## Date

2026-07-17

## Decision Makers

大鱼 (Dayu)

## Predecessors

ADR-009 (the Gateway no longer writes agent workspace files), ADR-015 (agent startup sequencing)

## Blast radius

**New modules**: `core/acowork-runtime/src/agent_config.rs` (the new `AgentToolsConfig` struct
+ `load`/`save` functions) and `core/acowork-runtime/src/tools/registry.rs` (`activate()` gains
an `enabled_tools` filter parameter).

**Modified modules**: `core/acowork-core/src/protocol.rs` (`RuntimeConfigUpdate` gains
`builtin_tools_enabled`), `core/acowork-runtime/src/agent/agent_core.rs` (`builtin_tools`
becomes `Vec<BuiltinToolEntry>` with an `enabled` field per entry),
`core/acowork-runtime/src/startup/agent_init.rs` (Phase A loads `agent_tools.json` and filters),
`core/acowork-runtime/src/cli.rs` (the `RuntimeConfigUpdate` handler),
`core/acowork-runtime/src/agent/session/session_manager.rs` (a new `UpdateBuiltinTools`
SessionMessage + broadcast), `core/acowork-runtime/src/agent/session/session_task.rs` (handles
that message), `core/acowork-gateway/src/http/agents.rs` (the new
`GET/PUT /api/agents/{id}/builtin-tools` endpoints), `core/acowork-gateway/src/http/routes.rs`,
`core/acowork-gateway/src/http/agent_config.rs` (`AgentConfigResponse` gains `builtin_tools`),
`apps/acowork-desktop/src/stores/` (a new `builtinToolsStore.ts` or an extension of `mcpStore`),
`apps/acowork-desktop/src/components/results/ToolsTab.tsx` (a Builtin Tools section), and
`apps/acowork-desktop/src/i18n/locales/*.json`.

---

## Context

All builtin tools are currently activated unconditionally at agent startup, with no
enable/disable control:

```rust
// core/acowork-runtime/src/tools/registry.rs:43-44
/// All builtin tools are always active — manifest `[[tools]]` is reserved
/// for future scope restriction, not activation filtering.
```

The `[[tools]]` declarations in `manifest.toml` are currently only used for the opt-in
registration of RAG tools and have no filtering effect on ordinary builtin tools. Every sample
agent's manifest.toml says so explicitly:

```toml
# Builtin tools are always active — no need to declare them here.
# The [[tools]] section is reserved for future scope-limiting (optional).
```

**User requirements**: (1) control which builtin tools are enabled at **per-agent** granularity;
(2) persist to `agent_tools.json`, which **must contain all builtin tools**, each with an
`enabled` field; (3) when `agent_tools.json` does not exist, seed the data from the `[[tools]]`
declarations in `manifest.toml`, and once the file exists it is the single source of truth;
(4) the startup flow mirrors `agent_mcp.json`: persisted file → loaded into `AgentCore` →
effective at runtime; (5) the Gateway exposes a REST API to list the tools and set their enable
state; (6) the frontend tools panel lists the builtin tools with a checkbox per tool that calls
the enable API.

**The existing reference pattern** is `agent_mcp.json`, which provides a complete template:
`{work_dir}/config/agent_mcp.json` with `AgentMcpConfig { catalog, local }` loaded by
`load_agent_mcp_config()` and saved by `save_agent_mcp_config()`; the initialization source is an
empty file; runtime updates arrive via `RuntimeConfigUpdate.mcp_servers`; and hot reload uses
`McpConfigNotifier` → `UpdateMcpTools`. `agent_tools.json` follows the same shape with
`AgentToolsConfig { tools: Vec<AgentToolEntry> }`, `load_agent_tools_config()` /
`save_agent_tools_config()`, `RuntimeConfigUpdate.builtin_tools_enabled`, and an
`UpdateBuiltinTools` SessionMessage.

## Goals

1. Add the `agent_tools.json` persistence file holding the enable state of every builtin tool
2. Initialization: when the file is absent, generate it from the `[[tools]]` declarations in `manifest.toml`; when present, the file wins
3. Load it into `AgentCore.builtin_tools` at startup (each entry carrying `enabled`), used to filter `all_tools`
4. The Gateway exposes `GET/PUT /api/agents/{id}/builtin-tools`
5. Runtime pushes changes via `RuntimeConfigUpdate` and hot-updates `AgentCore.all_tools`
6. The frontend ToolsTab gains a Builtin Tools section with checkboxes

## Design

### 1. Data model

**`AgentToolsConfig`** (new, in `agent_config.rs`) holds a `tools: Vec<AgentToolEntry>`, where
each `AgentToolEntry` is a `name` (matching `Tool::name()`) plus an `enabled: bool` that
defaults to `true` via `#[serde(default = "default_enabled")]`. Its documented
initialization priority is: if `agent_tools.json` exists, load from the file (the single source
of truth); if not, generate from the `[[tools]]` in `manifest.toml` (declared tools enabled,
undeclared disabled); if the manifest has no `[[tools]]` at all, enable all builtin tools
(backward compatible).

**`AgentCore` extension**: `builtin_tools` changes from `Vec<Arc<dyn Tool>>` to
`Vec<BuiltinToolEntry>`, where `BuiltinToolEntry { tool: Arc<dyn Tool>, enabled: bool }` with
`name()` / `spec()` delegating to the tool. This becomes the single source of truth for both the
full tool list (for the frontend GET) and the enabled subset (for LLM dispatch).

`rebuild_all_tools()` merges only the entries where `enabled == true` into `all_tools`, then
extends with the MCP tools.

### 2. The initialization flow

```
Agent startup
    │
    ├── Step A: register all builtin tools (all_builtin_tools())
    │   producing the full list (including the platform-dependent shell tools)
    │
    ├── Step B: check whether {work_dir}/config/agent_tools.json exists
    │       ├── present → load AgentToolsConfig from the file, then MERGE it with the
    │       │              full list of tools registered in code:
    │       │              in both            → keep the file's enabled value
    │       │              code only          → append with enabled = true
    │       │                                     (a Runtime upgrade adding a tool auto-enables it)
    │       │              file only          → drop it
    │       │                                     (the tool was removed; silently cleaned up)
    │       └── absent → generate the initial config from manifest.toml:
    │                   iterate every registered builtin tool; a tool declared in
    │                   [[tools]] → enabled = true, otherwise enabled = false; the RAG
    │                   tool is only added when the manifest declares it; the shell tools
    │                   are added dynamically per platform detection; then save to agent_tools.json
    │
    ├── Step C: build AgentCore.builtin_tools
    │   merging the full tool list with the enabled states into Vec<BuiltinToolEntry>
    │
    └── Step D: call AgentCore.rebuild_all_tools()
         adding only the enabled tools to all_tools
```

**The merge logic** (the key part) handles the case where `agent_tools.json` has fewer tools
than the code registers — for example after a Runtime upgrade adds one:

```rust
/// Merge code-registered tools with persisted config.
///
/// Rules:
/// - Tools present in both → use persisted `enabled` value
/// - Tools only in code (new tools after upgrade) → append with enabled = false
///   (opt-in: only user-explicitly-enabled tools are true)
/// - Tools only in file (removed tools) → silently dropped
pub fn merge_tools_config(
    code_tools: &[Arc<dyn Tool>],       // from all_builtin_tools()
    persisted: &[AgentToolEntry],        // from agent_tools.json
) -> Vec<BuiltinToolEntry> { ... }
```

Note that the two paths disagree on the default for a newly added tool: the initialization
flow in Step B sets code-only tools to `enabled = true` (a Runtime upgrade auto-enables), while
`merge_tools_config` sets them to `false` (opt-in). The Chinese original states both; the
`merge_tools_config` doc comment and the "New builtin tool (Runtime upgrade)" row in the
boundary table (default `enabled = false`, opt-in) are the operative rule, and the ADR's
intent is that a new tool must be explicitly enabled by the user.

**Initialization examples**:

- **Scenario A** — `manifest.toml` declares `memory_recall`, `memory_store` and `shell`, so the generated `agent_tools.json` enables exactly those three and marks the other 14 builtin tools (`http_request`, `web_fetch`, `file_read`, `file_write`, `file_edit`, `doc_reader`, `glob_search`, `content_search`, `intent_send`, `ask_user_question`, `codebase`, `todo_write`, `mcp_install`, `mcp_uninstall`) as `"enabled": false`.
- **Scenario B** — no `[[tools]]` in the manifest: all builtin tools default to `enabled = true`, matching the current behavior (backward compatible).
- **Scenario C** — `agent_tools.json` already exists: load it directly and the `[[tools]]` in `manifest.toml` is completely ignored; if the file has fewer tools than the code, the merge logic fills them in.

### 3. The runtime hot-update flow

```
user clicks a checkbox
    ▼
Frontend PUT /api/agents/{id}/builtin-tools
    ▼
Gateway handles the request
    ├── validates the agent exists and is running
    ├── builds RuntimeConfigUpdate { builtin_tools_enabled: Some([...]) }
    └── pushes it to the Runtime over IPC
    ▼
Runtime cli.rs receives RuntimeConfigUpdate
    ├── parses the builtin_tools_enabled list (a partial update)
    ├── merges it into the current AgentCore.builtin_tools (only the listed tools change)
    ├── persists the full set to agent_tools.json
    ├── calls AgentCore.rebuild_all_tools()
    └── broadcasts UpdateBuiltinTools to every session
    ▼
SessionTask handles UpdateBuiltinTools
    ├── updates agent_loop.core.builtin_tools (the enabled states)
    ├── calls agent_loop.core.rebuild_all_tools()
    └── updates the tool_definitions in context_builder (what the LLM sees)
```

### 4. REST API

**`GET /api/agents/{id}/builtin-tools`** returns the full list with the enabled states:
`{ "agent_id": "com.acowork.senior-engineer", "tools": [ {"name": "memory_recall", "enabled": true}, ... ] }`.

**`PUT /api/agents/{id}/builtin-tools`** sets the enabled states. The request body is a
**partial update** carrying only the tools that changed
(`{ "tools": [ {"name": "http_request", "enabled": true} ] }`); the Gateway merges it into the
current config before pushing to the Runtime, and the Runtime persists the full set to
`agent_tools.json`. Both endpoints respond 200 with the complete current configuration.

### 5. The frontend

A `builtinToolsStore.ts` Zustand store holds `tools: BuiltinToolEntry[]` plus `loading`, with
`loadTools(agentId)` and `toggleTool(agentId, toolName)` actions. ToolsTab gains a "Builtin
Tools" section **above** the existing "Web Search Providers" area: a scrollable
(`max-h-48`) bordered list where each tool is a `<label>` with a checkbox bound to `tool.enabled`
and an `onChange` calling `toggleTool(selectedAgentId, tool.name)`, plus the tool name rendered
in small monospace-ish text.

### 6. Boundary cases

| Scenario | Handling |
|---|---|
| **RAG tool** | only appears in the list when the manifest declares `[[tools]] type = "rag"`; enabled by default |
| **Shell tools (multi-platform)** | added dynamically per the platform detection results (on Windows there may be both bash and powershell) |
| **mcp_install / mcp_uninstall** | appear in the list as ordinary builtin tools and can be disabled |
| **agent_tools.json is corrupted** | fall back to the manifest.toml initialization and log a warning |
| **A new builtin tool (Runtime upgrade)** | the merge logic appends it to `builtin_tools` with `enabled = false` by default (opt-in) |
| **A tool is removed from the code** | the merge logic drops it and cleans it up on the next `agent_tools.json` save |
| **All tools are disabled** | the agent can still run but cannot call any builtin tool (MCP tools are unaffected) |
| **agent_tools.json conflicts with manifest.toml** | agent_tools.json wins (once the file exists it is the single source of truth) |
| **The frontend GET needs the full list** | `AgentCore.builtin_tools` is itself the full list (with the enabled flags), so it is returned directly |

### 7. Backward compatibility

1. An **existing agent with no `agent_tools.json`**: on first startup it is generated from
   `manifest.toml`, and if the manifest has no `[[tools]]` then everything is enabled.
2. An **existing `agent_tools.json` missing a new tool**: the merge logic fills it in with
   `enabled = false` (opt-in; the user must explicitly enable it).
3. **API compatibility**: `GET /api/agents/{id}/config` gains a `builtin_tools` field without
   affecting the existing fields.
4. **Frontend compatibility**: an older frontend ignores `builtin_tools` and all tools stay enabled.
5. **The `AgentCore.builtin_tools` type change**: from `Vec<Arc<dyn Tool>>` to
   `Vec<BuiltinToolEntry>` requires adapting every reference to access `.tool`.

## Implementation plan

**Phase 1 — backend data layer (2-3 days)**: in `agent_config.rs` add the `AgentToolsConfig` /
`AgentToolEntry` structs plus `load_agent_tools_config()` / `save_agent_tools_config()` /
`merge_tools_config()`; in `manifest.rs` add `AgentManifest::builtin_tool_names()` returning
every declared builtin tool name; in `agent_core.rs` add `BuiltinToolEntry` and change
`builtin_tools` to `Vec<BuiltinToolEntry>` with the `rebuild_all_tools()` filter; in
`registry.rs` add an `enabled_tools: &[BuiltinToolEntry]` parameter to `activate()` so only the
enabled tools are activated.

**Phase 2 — initialization flow (1-2 days)**: `agent_init.rs` loads `agent_tools.json` in
Phase A (generating it from the manifest when absent), merges, and passes the result to
`registry.activate()`; `cli.rs`'s `RuntimeConfigUpdate` handler handles `builtin_tools_enabled`.

**Phase 3 — the hot-update mechanism (1-2 days)**: `protocol.rs` gains
`builtin_tools_enabled: Option<Vec<AgentToolEntry>>`; `session_task.rs` gains the
`UpdateBuiltinTools` SessionMessage variant plus its handler; `session_manager.rs` gains
`apply_builtin_tools()` + the broadcast.

**Phase 4 — the Gateway API (1 day)**: `agents.rs` gains the `get_agent_builtin_tools()` /
`update_agent_builtin_tools()` handlers; `routes.rs` registers
`GET/PUT /api/agents/{id}/builtin-tools`; the Gateway `agent_config.rs` adds the
`builtin_tools` field to `AgentConfigResponse`.

**Phase 5 — the frontend (1-2 days)**: `builtinToolsStore.ts`; the Builtin Tools section with
checkboxes in `ToolsTab.tsx`; the new i18n keys.

**Phase 6 — tests (1 day)**: unit tests for `AgentToolsConfig` serde roundtrip, the merge
logic and the initialization logic; integration tests for the two endpoints; a frontend test
for checkbox toggle → API call → state update.

## Out of scope (explicit boundaries)

- Not changing the semantics of `[[tools]]` in `manifest.toml` — the manifest is only an initialization data source
- Not touching MCP tools enable/disable (MCP already has its own `agent_mcp.json` + catalog mechanism)
- Not touching WASM tools enable/disable (WASM is an independent subsystem)
- Not touching permissions — disabling a tool only means it is not registered to the LLM, it does not change the permission declaration
- Not modifying the `Tool` trait — enable/disable is a registry-layer concern, not tool behavior

## Appendix: file change list

| File | Change | Notes |
|---|---|---|
| `core/acowork-runtime/src/agent_config.rs` | new | `AgentToolsConfig` + `AgentToolEntry` + load/save/merge |
| `core/acowork-runtime/src/agent/agent_core.rs` | modified | the new `BuiltinToolEntry` + the `builtin_tools` type change + the `rebuild_all_tools` filter |
| `core/acowork-runtime/src/tools/registry.rs` | modified | `activate()` gains an `enabled_tools` parameter |
| `core/acowork-runtime/src/startup/agent_init.rs` | modified | Phase A loads agent_tools.json + merges |
| `core/acowork-runtime/src/cli.rs` | modified | the RuntimeConfigUpdate handler |
| `core/acowork-runtime/src/agent/session/session_task.rs` | modified | the new `UpdateBuiltinTools` |
| `core/acowork-runtime/src/agent/session/session_manager.rs` | modified | the new `apply_builtin_tools()` |
| `core/acowork-core/src/protocol.rs` | modified | `RuntimeConfigUpdate` gains the field |
| `core/acowork-core/src/manifest.rs` | modified | the new `builtin_tool_names()` |
| `core/acowork-gateway/src/http/agents.rs` | modified | the new builtin-tools endpoints |
| `core/acowork-gateway/src/http/routes.rs` | modified | route registration |
| `core/acowork-gateway/src/http/agent_config.rs` | modified | the response gains the field |
| `apps/acowork-desktop/src/stores/builtinToolsStore.ts` | new | the Zustand store |
| `apps/acowork-desktop/src/components/results/ToolsTab.tsx` | modified | the Builtin Tools section |
| `apps/acowork-desktop/src/i18n/locales/*.json` | modified | the new i18n keys |
