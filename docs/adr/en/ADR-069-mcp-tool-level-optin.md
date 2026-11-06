# ADR-069: Per-Tool Opt-In for MCP Tools — agent_mcp_tools.json

> **Panel-location revision (2026-11-06, [ADR-108](./ADR-108-right-panel-consolidation.md))**: the right-rail Tools panel no longer exists — every reference below to "the Tools panel" / "the Builtin Tools card" / "the MCP card" now means the **Setup panel** (`AgentSetupTab`); the rail entry and its tab are gone. Endpoints, stores, permissions and switch semantics are unchanged; see ADR-108 for the scope.

> **Chinese source of truth**: [ADR-069](../zh/ADR-069-mcp-tool-level-optin.md)
> **Terminology**: see [GLOSSARY.md](./GLOSSARY.md)

## Status

Accepted

## Date

2026-09-22

## Decision Makers

大鱼 (Dayu)

## Related

- [ADR-029](./ADR-029-agent-tools-persistence-and-toggle.md) — built-in tool persistence and enable toggles
- [ADR-065](./ADR-065-unify-mqtt-client-lifecycle.md) — unified MQTT client lifecycle

---

## Context

ADR-029 gave built-in tools per-tool opt-in via `agent_tools.json` with an
`enabled` flag per tool. MCP tools still have only a **server-level** switch
(`agent_mcp.json::active_names`), so enabling a server injects **all** of its tools into the
LLM `tool_definitions`:

```rust
// before: inject everything
for prefixed_name in registry.tool_names() {
    if let Some(def) = registry.get_tool_def(&prefixed_name) {
        let wrapper = McpToolWrapper::new(prefixed.clone(), def, registry.clone());
        ...
    }
}
```

Each tool (`mcp_<server>__<tool>`) costs 250–400 tokens of schema, so a server exposing
N tools spends N × 300 tokens of system prompt.

**The concrete pain** — the `pm` server exposes 12 tools with sharply different audiences:

| Tools | Main user | Needed by an ordinary agent |
|-------|----------|--------------------------------|
| `pm_list_my_tasks` / `pm_claim_task` / `pm_submit_task` / `pm_check_task` | engineers | yes |
| `pm_create_project` / `pm_create_task` / `pm_list_projects` / `pm_get_project` / `pm_list_tasks` / `pm_get_task` / `pm_update_task` / `pm_reparent_task` | PM | no |

Handing all 12 to a regular agent spends roughly 3600 tokens on pure noise; only the
PM-role agent needs the full set.

**Design principles agreed for this change**

1. **The backend is the single source of truth for the full tool list.** The frontend
   only renders; it MUST NOT keep any hardcoded tool list or defaults. If the API
   returns too little, the backend is fixed, not the frontend.
2. **The API returns a complete list** — each entry carries `name`, `enabled`, and
   `description`, so the frontend renders directly instead of stitching two sources.
3. **No manifest changes** — the MCP tool subset stays out of `manifest.toml` and remains
   Gateway defaults plus user UI adjustment.
4. **No back-compat code** — the project is in development, so a shape change fails
   loudly and asks the user to delete the file; no silent migration
   (`deny_unknown_fields`).

## Decision

1. A new `agent_mcp_tools.json` stores the **full** tool list per MCP server, each row
   carrying an `enabled` switch.
2. On startup and reconnect the backend reconciles against the server live `tools/list`:
   refresh `description`, keep the user `enabled` choices, give newly discovered tools a
   default, then persist.
3. `McpManager::connect()` filters by the reconciled `enabled` flags, dropping tools that
   are not enabled (guarding against server upgrade drift).
4. A new `GET/PUT /agents/{id}/mcp-tools` API returns and accepts the **complete** list; the
   frontend renders it and toggles row by row.
5. Startup load plus a file-watcher hot reload, reusing the existing MCP config mechanism.

## Non-goals

- **No `manifest.toml` change** — the MCP tool subset does not enter the manifest.
- **No change to `agent_mcp.json::active_names`** — the server-level switch stays; this ADR
  adds tool-level filtering only once a server is active.
- **No change to `is_system_injected_mcp_name`** — still `pm` only.
- **No proto change** — `AvailableMcps.mcp_refs` gains no field.
- **No automatic migration of the old shape** — a v1 `agent_mcp_tools.json`
  (`{"pm": {"enabled_tools": [...]}}`) fails to parse and asks the user to delete it.

## Design

**Data structure** — `workspace/config/agent_mcp_tools.json`, a flat full list:

```json
{
  "servers": {
    "pm": [
      { "name": "pm_list_my_tasks", "enabled": true,  "description": "List tasks assigned to me" },
      { "name": "pm_claim_task",    "enabled": true,  "description": "Claim a task" },
      { "name": "pm_submit_task",   "enabled": true,  "description": "Submit work results" },
      { "name": "pm_check_task",    "enabled": true,  "description": "Check approval status" },
      { "name": "pm_list_projects", "enabled": false, "description": "List all projects" },
      { "name": "pm_get_project",   "enabled": false, "description": "Get project details" }
    ]
  }
}
```

The wire shape is identical across the file, the HTTP request and response, and the
desktop render, so the frontend does no assembly.

```rust
/// Per-agent MCP tools config (flat per-server list).
///
/// Wire shape matches the GET response and PUT request body — three-way
/// identity between `agent_mcp_tools.json`, the HTTP wire shape, and
/// the desktop render.
///
/// `deny_unknown_fields` ensures a v1-shape file
/// (`{"pm": {"enabled_tools": [...]}}`) fails to parse rather than
/// silently mapping to an empty config — no automatic migration by
/// design (project is in active development).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentMcpToolsConfig {
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub servers: HashMap<String, Vec<AgentMcpToolItem>>,
}

/// Single MCP tool row inside a server flat list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentMcpToolItem {
    pub name: String,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}
```

**Default subset constant (backend only, invisible to the frontend)**

```rust
/// Default `enabled_tools` subset for the `pm` system-injected MCP.
///
/// Picked so a regular (non-PM-role) agent gets the minimum useful
/// surface: read my own tasks, claim one, submit work, check approval
/// status. Anything else (project CRUD, task tree manipulation,
/// reparenting) belongs to PM-role agents, who can extend this via the
/// Tools panel.
pub const PM_DEFAULT_ENABLED_TOOLS: &[&str] = &[
    "pm_list_my_tasks",
    "pm_claim_task",
    "pm_submit_task",
    "pm_check_task",
];
```

**The frontend MUST NEVER reference this constant.** It lives purely on the backend
contract side, materialized into `agent_mcp_tools.json` at startup by
`merge_mcp_tools_config`. The frontend only ever sees the full list with an `enabled` flag
per row.

**Reconcile logic** — `merge_mcp_tools_config` merges the persisted choices with the
live `tools/list` and produces a full list, then persists it:

1. **Row already persisted** → keep the user `enabled` choice (a tool the user turned off is
   not re-enabled by a restart).
2. **Newly discovered tool** (added by a server upgrade) → initial value from
   `default_enabled_tools_for(server)`: `pm` uses `PM_DEFAULT_ENABLED_TOOLS`, every other
   server defaults to `enabled = true`.
3. **description** → always refreshed from the live `tools/list`, so a schema change
   never leaves a stale description.
4. **Server absent from `tools/list`** → that server is dropped entirely.

**Filter logic** — `McpManager::connect()` gains a `work_dir` reconcile step before
filtering:

```rust
pub async fn connect(
    &mut self,
    configs: &[McpServerConfigDef],
    tools_cfg: &AgentMcpToolsConfig,
) -> McpConnectResult {
    // 1. connect_all to obtain the full tool set
    // 2. if work_dir is non-empty → reconcile_and_persist_mcp_tools(work_dir, &registry)
    //    (an empty work_dir means a unit test → use the caller supplied tools_cfg as is)
    // 3. walk tool_names(), skipping any where
    //    tool_allowed(&active_cfg, server, tool) is false
    ...
}
```

```rust
fn tool_allowed(tools_cfg: &AgentMcpToolsConfig, server_name: &str, tool_name: &str) -> bool {
    match tools_cfg.servers.get(server_name) {
        None => true,                                  // server not configured → allow
        Some(rows) => tool_enabled_in(rows, tool_name).unwrap_or(false),
        // row present → use it; row missing → conservatively do not expose
    }
}
```

**Helpers**

```rust
// agent_config.rs
pub fn load_agent_mcp_tools_config(work_dir: &Path) -> Result<Option<AgentMcpToolsConfig>, String>
pub fn save_agent_mcp_tools_config(work_dir: &Path, cfg: &AgentMcpToolsConfig) -> Result<(), String>
pub fn merge_mcp_tools_config(
    persisted: &AgentMcpToolsConfig,
    server_tools: &HashMap<String, Vec<McpToolDescriptor>>,
) -> AgentMcpToolsConfig

// mcp_manager.rs
pub fn collect_server_tools_from_registry(registry: &McpRegistry)
    -> HashMap<String, Vec<McpToolDescriptor>>
pub fn reconcile_and_persist_mcp_tools(work_dir: &Path, registry: &McpRegistry)
    -> AgentMcpToolsConfig
pub async fn connect_mcp_with_reconcile_and_filter(
    work_dir: &Path,
    configs: &[McpServerConfigDef],
) -> McpConnectResult
```

**HTTP API** — `GET /agents/{id}/mcp-tools` returns the full list per server; the
`PUT` body is isomorphic to the GET response, so the backend simply overwrites
`agent_mcp_tools.json` and broadcasts an MCP tool change to force a reconnect. The
frontend sends one PUT per toggle and **keeps no defaults of its own**.

**Frontend** — `ToolsTab.tsx` renders MCP servers as collapsible cards styled like the
Debug panel PROMPT layout, **collapsed by default**, expanding to a second-level list of
the full tool set with one switch per row, indented under the server. All data comes from
`GET /agents/{id}/mcp-tools`; toggling issues a `PUT`. The hardcoded `KNOWN_PM_TOOLS` /
`PM_DEFAULT_ENABLED_TOOLS` copies are **deleted**.

## Compatibility and migration

An old v1 file (`{"pm": {"enabled_tools": [...]}}`) is rejected by
`deny_unknown_fields`, and the log tells the user to delete it so it is rebuilt. **No
silent migration** — the project is in development and carries no data-compat cruft. The only
migration step is the user deleting the file, after which startup reconcile materializes the
full default list again.

## Tests

- `merge_mcp_tools_config`: empty persistence + 12 `pm` tools → 12 rows with `enabled`
  matching `PM_DEFAULT_ENABLED_TOOLS`; non-system servers all `true`; user choices override
  defaults; descriptions refresh; a vanished server is dropped.
- `tool_allowed`: missing server allows; a missing row conservatively denies; `row.enabled`
  takes effect directly; a user opt-out beats the default.
- `load_agent_mcp_tools_config`: missing file → None; v1 shape → error; a valid file parses.
- `save_agent_mcp_tools_config`: atomic write (tmp + rename).
- End to end: connect the `pm` server → reconcile persists 12 rows with 4 enabled → 4 tools
  are injected.
