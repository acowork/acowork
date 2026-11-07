# ADR-109: Per-session tool narrowing (a narrow-only layer) and the composer `+` menu

- **Number**: ADR-109
- **Status**: Implemented (2026-05-20)
- **Date**: 2026-05-20
- **Decision owner**: Nicholas (plan approved)
- **Related**: ADR-029 (builtin tool persistence / enablement), ADR-069 (MCP tool-level opt-in), ADR-076 (multi-user accounts: session read/write authorization), ADR-082 (Runtime-side SQLite sidecars), ADR-090 (MCP install ownership), ADR-101 (memory-service boundary), ADR-108 (right-panel consolidation)
- **Chinese source of truth**: [docs/adr/zh/ADR-109-session-tool-narrowing.md](../zh/ADR-109-session-tool-narrowing.md) (maintained in the same commit)

---

## Summary

**One line**: tool switches go from **one layer to two** — the *agent* layer (`agent-tools.json` / `agent-mcp.json`, edited in the right-panel settings) decides what an agent has; the *session* layer (a new `SessionMeta.tool_policy` JSON column, SQLite v13) can only **take things away** from it. The same change collapses the composer's two buttons (the skills dropdown on the left, the paperclip on the right) into one `+` menu: four level-1 rows (skills / MCP servers / builtin tools / attachments), level 2 replacing level 1 in the same column, and a level 3 for one server's tools.

**One line (why)**: a session is **one piece of work the user is doing right now**, and the reasonable request there is often "not the shell this time" — not "change who this agent is". Today that request can only be served by editing the agent configuration, which changes every session at once: a session-scoped intention written into agent-scoped state. Meanwhile the composer row already carries five controls; a sixth entry point only squeezes it further.

---

## Background (what the code actually does)

1. **Builtin tools already live per session.** `SessionTask` deep-copies the agent's template tool set at creation; an agent-level `UpdateBuiltinTools` broadcast only overwrites that copy. "Which builtin tools this session runs with" is therefore already a per-session fact in memory — it just has no persisted, user-writable expression.
2. **MCP tool-level gates are pruned at connect time.** A tool that `agent-mcp.json` does not allow never gets a wrapper built (`prefixed_name = server__tool`). So a session layer that *enabled* an agent-disabled tool would have nowhere to put it — not a policy problem, a representability problem. This fact alone forces the one-way design below.
3. **`SessionMeta` already has a JSON column precedent.** `todos` is an `Option<Vec<_>>` serialised into meta (in-memory copy + `write_meta`), and earlier migrations contain the same kind of ALTER. A new column needs no new storage pattern.
4. **The right panel edits the agent layer, not a session layer.** After ADR-108 merged the builtin-tools and MCP cards into the settings panel, their write paths are `PUT /agents/{id}/builtin-tools` / `mcp-servers` / `mcp-tools` — they edit the agent definition. Treating the same panel as a session switchboard would give one panel two meanings.
5. **There is no multi-level horizontal flyout anywhere in the repo.** Every popup (model / workspace / skills / session) is a single column, single level. A flyout needs three panel widths, and the composer sits between a user-resizable right rail and a resizable pane — the second level would already be clipped in a narrow window.

---

## Decisions

### D1 — The session layer is a deny list; it can only narrow

`SessionToolPolicy` has exactly three groups, all in *deny* form:

```jsonc
{
  "disabled_builtin_tools": ["shell"],
  "disabled_mcp_servers": ["playwright"],
  // Persisted as Rust's BTreeMap<String, BTreeSet<String>>: server -> the
  // (unprefixed) tool names trimmed from it. NOT a [{server, tool}] list.
  "disabled_mcp_tools": { "playwright": ["screenshot"] }
}
```

> This shape is a cross-boundary contract: Rust's `SessionToolPolicy`
> (`BTreeMap`) matches the Desktop's `WireSessionToolPolicy`
> (`Record<string, string[]>`), while the UI's semantic model (`McpToolRef[]`)
> lives only in `toolInventory.ts` and crosses the boundary through
> `fromWirePolicy` / `toWirePolicy`. Each side pins the same literal in a test
> (`session_meta::tool_policy_tests::mcp_tool_denies_serialise_as_a_map` and
> "converts the per-server tool map both ways" in `toolInventory.test.ts`),
> because neither side's mocks can see the other.

- Enabling something the agent layer disabled is not expressible (background 2: it does not exist past connect time).
- The three groups are independent: a server-level deny covers every tool under it, while tool-level denies **survive** the server being switched back on (what the user trimmed earlier must not come back behind their back).

**Rejected**: an allow-list override ("which tools I want"). It would force the session layer to re-enumerate every tool after each agent change, and an agent that gains a tool would silently not show it to old sessions — turning "inherit" into "snapshot".

### D2 — `NULL` means inherit; write only after a change

`SessionMeta.tool_policy: Option<SessionToolPolicy>`, `None` = follow the agent layer, is the **default for a new session**. An all-empty policy is normalised to `None` on write (both `set_session_tool_policy` and the `PUT` filter it out).

So "a new session uses the per-agent configuration, and a per-session row is written only once the user changes something" is a **property of the data model**, not a frontend convention: a session nobody ever toggled keeps that column at `NULL` forever.

### D3 — One composition entry point, four call sites

`agent/session/session_tool_layer.rs::SessionToolLayer` is the single place the two layers are folded together: it holds the agent-layer baselines (`agent_builtin` / `agent_mcp`) plus the session `policy`, `effective_builtin()` / `effective_mcp()` return `A ∧ B`, and `apply_to(core)` writes the result back into the session's own `AgentCore`. Any implementation that recomputes this on the side drifts from it.

**Why compose on write instead of filtering on read**: `rebuild_all_tools` (dispatch) and `rebuild_context_tool_definitions` (the LLM's spec list) both read `core` directly, so read-time filtering would mean teaching every present and future reader about the session layer; writing into `core` keeps one derivation pipeline.

Composition happens in four places:

1. `SessionTask::new` — `from_core` plus composition from the hydrated policy when a session opens / resumes / forks (it must run **before** the first `apply_to`, or the baseline is already narrowed and the narrowing becomes permanent);
2. when the agent-level `UpdateBuiltinTools` broadcast arrives — `set_agent_builtin` swaps the baseline, then re-compose;
3. when `UpdateMcpTools` arrives — `set_agent_mcp`, then re-compose (that payload is the **whole** agent-level desired state; writing it straight into `core.mcp_tools` would erase the session's switches);
4. when a per-session `PUT` arrives — `SessionMessage::UpdateToolPolicy` → `set_policy` (skipped when content-equal) → `apply_to`.

So an agent-layer change **never erases** the session layer: both broadcasts only replace the baseline, and each session then re-lays its own layer on top.

### D4 — `GET/PUT /sessions/{sid}/tool-policy`, whole-policy replacement

The Runtime exposes `GET /sessions/{sid}/tool-policy` (read authorization) and `PUT` (write authorization); the Gateway reverse-proxies `/api/agents/{id}/sessions/{sid}/tool-policy`, and the ADR-087 classifier keeps the existing `sessions` rules: GET → `view`, PUT → `use`.

- **Replacement rather than per-item toggles**: replaying the same body cannot compound, the UI's optimistic value is exactly what gets persisted, and there is no read-modify-write update to lose.
- **Names are not validated**: a deny list only filters, so an entry naming a tool that no longer exists is inert; validating would turn a concurrent agent-side change into a 400 on an unrelated toggle.
- `PUT` also accepts a **closed** session: it only rewrites meta, and the tool set is composed on the next open (only a live session needs an immediate re-compose).

### D5 — The composer `+` menu drills down in one column

Four level-1 rows: skills / MCP servers / builtin tools / attachments. Level 2 **replaces** level 1 inside the same fixed-height box (with a back chevron); an MCP server row also carries a chevron into level 3 (that server's tools).

- Attachments are not a panel: a click closes the menu and opens the file picker (the same handler the paperclip used).
- The three-state projection of a switch is `effective = agentEnabled && !denied`. A row the agent layer never enabled renders **off and disabled**, with a footer note counting how many rows the agent configuration disabled — the honest presentation of "you cannot widen this here" instead of a switch that looks clickable.
- Each group header carries "Follow Agent defaults" (`resetGroup`) that clears only that group, disabled when the group denies nothing.
- A read-only session (ADR-076 §Decision 4) disables the whole trigger: attachments, skill picking and tool switches are all session writes, and half-working is harder to understand than fully disabled.

### D6 — The right panel keeps agent-layer semantics; the two surfaces share no state

The settings panel keeps editing the agent layer (`agent-tools.json` / `agent-mcp.json`); the `+` menu edits the session layer (meta). They **may legitimately show different values** — that is what two layers mean, not a defect: a per-session change affects only that one session (and its later turns), independently of the per-agent value, while the agent layer keeps describing what the **next new session** starts from. So "the panel says enabled but this session does not run it" is the correct presentation — the panel describes the agent layer, and the session layer only subtracts from it.

What they share is types and fetching (`lib/toolInventory.ts`: `buildToolInventory` folds `/tools` + `/mcp-tools`), never "current state".

### D7 — Only the execution-surface composer gets the `+` menu; group chat and the chat surface do not

Only the execution-session composer (`ChatPanel`) is wired to `ComposerPlusMenu`. The other two composers are deliberately left alone:

- **Group chat (`GroupChatPanel`)**: it has its own `@` routing and multi-agent semantics, and "whose layer" a per-session tool policy means there has no clear answer. Deferred until there is a need; this ADR does not widen its semantics as a side effect.
- **Chat surface (`AgentChatView`)**: an agent has exactly one Chat session, so that layer is effectively per-agent and the right-panel settings already express it; adding a second set of per-session switches there would only blur "which layer am I editing".

---

## Consequences

- **A new SQLite migration, v13** (`ALTER TABLE sessions ADD COLUMN tool_policy JSON`), with `TARGET_SCHEMA` updated; an upgraded database gets `NULL`, which means "inherit everything".
- **Tool sets may differ per session**: two sessions of one agent can run different tool sets. Agent-level approval / risk rules still apply as configured — the session layer only hands out **fewer** tools and bypasses no approval.
- **One extra filter per LLM call**: pure in-memory lookups over three sets, negligible next to a provider round trip.
- **Clean test boundaries**: `session_tool_layer.rs` (pure composition), `SessionManager::set_session_tool_policy` (the live and closed persistence paths), `lib/toolInventory.ts` (pure semantics), `stores/toolPolicyStore.ts` (optimistic write, rollback, newest-edit-wins).
- If "widening at the session layer" is ever wanted, background 2 has to be solved first — the agent layer would need a wrapper for every tool it does not allow. That is a separate decision; changing the policy shape cannot substitute for it.
