# ADR-063: Package-Level LLM Prompt Override Mechanism — Extending the `prompts/` Special-Filename Convention

> **Panel-location revision (2026-11-06, [ADR-108](./ADR-108-right-panel-consolidation.md))**: the Prompts & Skills card described below as living "at the top of the Debug panel" now lives in the **Setup panel** (`AgentSetupTab`) as its Prompts & Skills card; the Debug panel keeps only the debug switches and the context snapshot. Read/override logic, provenance labels and failure-branch semantics are unchanged; see ADR-108 for the scope.


> **Chinese source of truth**: [ADR-063](../zh/ADR-063-package-level-prompt-override.md)
> **Terminology**: see [GLOSSARY.md](./GLOSSARY.md)

**Status**: Decided
**Date**: 2026-09-20
**Decision Makers**: 大鱼 (Dayu)
**Predecessors**:
- [ADR-053](./ADR-053-agent-specific-compaction-prompt.md) (`prompts/summary.md` overriding `COMPACTION_SYSTEM_PROMPT`)
- [ADR-060](./ADR-060-prompt-cache-friendly-context-block-reorg.md) (stable prefix + append at the tail, which constrains the overridable scope of this ADR)
- [ADR-061](./ADR-061-context-compression-byte-budget.md) (the `<summary>` / `<user_intent>` output-format constraints of the compression protocol)

---

## 1. Decision Summary

`prompts/summary.md` (ADR-053) has already proven that "package-level filename overriding a builtin constant" is a simple, zero-cost pattern symmetric with the system prompt. But that mechanism **applies only to the single compaction/distillation directive**; there are still **8 LLM directive prompt constants** in `core/acowork-runtime/src/prompt.rs` and downstream `acowork-grafeo` / `acowork-memory` that remain hardcoded and cannot be rewritten by `.agent` package authors as needed.

This ADR follows ADR-053's isomorphic convention and brings these 8 constants into the `prompts/` filename override mechanism:

1. **4 independent constants in `prompt.rs`**: `PROMPT_BUILDER_FALLBACK` / `SEARCH_SYSTEM_PROMPT` / `COMPACT_PROMPT` / `TITLE_PROMPT` — conventionally `fallback.md` / `search.md` / `compact-template.md` / `title.md` respectively.
2. **4 constants in grafeo / memory**: `EXTRACTION_SYSTEM_PROMPT` / `CONFLICT_CLASSIFICATION_PROMPT` / `GENERALIZATION_PROMPT` / `DEFAULT_ABSTENTION_PROMPT` — conventionally `extraction.md` / `conflict-classification.md` / `generalization.md` / `abstention.md` respectively.
3. **Priority chain** (each independent, each falling back on its own):
   ```
   prompts/<file>.md (package declaration)  >  const PROMPT: &str in prompt.rs (builtin fallback)
   ```
4. **Loading converges into Phase A**: on the same path as `compaction_prompt` — `agent_init.rs` loads once, the result is stored in `AgentBootContext`, and Phase B injects it into new `AgentCore` fields; Gateway and Standalone modes behave identically.
5. **Three categories explicitly not overridden** (runtime injection blocks / tool descriptions / protocol formats), marked in the prompt-audit documentation; rationale in §3.4.
6. **Debug panel edit entry point** (§3.7): a companion two-layer mechanism of L1 file read/write + L2 DevMode reload — Runtime adds 4 HTTP endpoints (`/api/agents/{id}/prompts[/{name}]`) + 1 Debug RPC (`POST /api/debug/prompts/reload`, via ADR-048's `DebugService` trait late-bind slot); the Desktop DebugPanel gains a resident header "prompt list", clicking which goes through `fileEditorStore.openFileWithContent` to open in FileEditor, grouped by the §3.7.2 labels (🟦 normal segment / ⚙️ task directive), always displayed regardless of DevMode state.

> **About `build_compaction_system_prompt()`**: this function appends identity context after `COMPACTION_SYSTEM_PROMPT` (already overridable by `summary.md` per ADR-053); the structure of that appended block is itself a protocol boundary (the downstream contract of the `<user_intent>` language rules) and is out of this ADR's scope.

---

## 2. Background and Motivation

### 2.1 Status quo: 8 directive prompts are still hardcoded

Per the inventory in [`docs/prompt-audit/zh/runtime-prompts-summary.md`](../../prompt-audit/zh/runtime-prompts-summary.md), `core/acowork-runtime/src/prompt.rs` centralizes 5 production prompt constants + the `build_compaction_system_prompt` concatenation function, and `acowork-grafeo` / `acowork-memory` each have 1–3 independent `const PROMPT`s — 9 LLM directive prompts in total (8 distinct contents after dedup), all builtin.

| Constant | Typical call site | Why it is not generic enough |
|---|---|---|
| `PROMPT_BUILDER_FALLBACK` | `prompt_builder.rs` when there is no prompts/*.md | the package author may want a non-default identity for an "empty package" (e.g. "I am an empty-package placeholder") |
| `SEARCH_SYSTEM_PROMPT` | perplexity.rs backend | an engineering agent wants "return results with a commit hash", a customer-service agent wants "return results with an FAQ link" |
| `COMPACT_PROMPT` | `episode_distill.rs` | the user-prompt wrapper format (`<conversation>`) is too verbose for some models and needs a more concise directive |
| `TITLE_PROMPT` | `compact_session_title_with_llm` | different agents prefer different title styles (e.g. "verb-initial" vs "noun phrase") |
| `EXTRACTION_SYSTEM_PROMPT` | grafeo extraction | per-agent extraction granularity differs |
| `CONFLICT_CLASSIFICATION_PROMPT` | grafeo conflict detection | the definition of what counts as a conflict differs per agent |
| `GENERALIZATION_PROMPT` | grafeo generalization | what "generalizing well" means differs per agent |
| `DEFAULT_ABSTENTION_PROMPT` | memory abstention | an agent that must never abstain needs a different directive |

### 2.2 The boundary with `system_prompt_override`

`system_prompt_override` (an `agent_config.json` field) overrides the **main conversation system prompt** and is a user-level runtime tuning entry point. Package-level `prompts/<file>.md` overrides **task directive prompts**, which are the package author's declaration of the agent's behaviour. The two dimensions are orthogonal:

| Dimension | Entry point | Ownership | Applies to |
|---|---|---|---|
| Main conversation system prompt | `prompts/system.md` + `prompts/*.md` + `system_prompt_override` | package / user | the main conversation identity |
| Task directives (compaction / search / title / extraction / ...) | `prompts/<special-name>.md` | package | all the implicit LLM calls |

`system_prompt_override` is no longer borrowed by the override path (already corrected in ADR-053 §3.2); this ADR continues that principle.

### 2.3 Why do it now

- The project is not live, so there is no external `.agent` package compatibility burden.
- `summary.md` has already landed the complete infrastructure (`load_compaction_prompt` + main-prompt exclusion + Phase A convergence), so horizontal extension is a zero-incremental isomorphic cost.
- The prompt-audit document already provides a clear hardcoded inventory, so the extension cannot "let anything slip through".

---

## 3. Design

### 3.1 File Convention

- Existing normal prompt segments keep going through `prompts/*.md` full concatenation (**status-quo behaviour preserved**, no new filenames introduced).
- Each overridable hardcoded constant corresponds to a **unique** filename; naming collisions (e.g. `compact.md` being easily confused with `summary.md`) are avoided outright.
- Loading and exclusion use the **same exact-filename matching** predicate (consistent with ADR-053 §3.3), guaranteeing that "loaded as the X directive" and "excluded from the main prompt" point at the same file.
- These 9 filenames simultaneously constitute the display scope of the §3.7 Debug panel list — the normal segments `system.md` / `constraints.md` serve as group representatives, are not in the `OVERRIDABLE_PROMPTS` inventory, but have a visual contrast relationship with the list.

### 3.2 Loading and the Resolution Chain

`prompt_builder.rs` gains a generic loader and a filename inventory:

```rust
/// The conventional filename inventory: an exact filename → meaning mapping
/// Both loading and main-prompt exclusion are based on this inventory's "filename" column.
pub const OVERRIDABLE_PROMPTS: &[(&str, &str)] = &[
    ("summary.md", "compaction/distillation system prompt (ADR-053)"),
    ("fallback.md", "PROMPT_BUILDER_FALLBACK"),
    ("search.md", "SEARCH_SYSTEM_PROMPT"),
    ("compact-template.md", "COMPACT_PROMPT"),
    ("title.md", "TITLE_PROMPT"),
    ("extraction.md", "EXTRACTION_SYSTEM_PROMPT (grafeo)"),
    ("conflict-classification.md", "CONFLICT_CLASSIFICATION_PROMPT (grafeo)"),
    ("generalization.md", "GENERALIZATION_PROMPT (grafeo)"),
    ("abstention.md", "DEFAULT_ABSTENTION_PROMPT (memory)"),
];

/// Generic loader: exact filename → Option<String>
/// The behaviour on missing / whitespace-only / permission-or-encoding errors
/// is identical to load_compaction_prompt.
pub fn load_optional_prompt(package_dir: &Path, filename: &str) -> Option<String>;
```

`AgentBootContext` (`startup/context.rs`) gains the corresponding fields (standalone field shape, strictly isomorphic to ADR-053):

```rust
pub struct AgentBootContext {
    // ... existing fields
    pub compaction_prompt: Option<String>,            // ADR-053
    pub fallback_prompt: Option<String>,              // new
    pub search_prompt: Option<String>,                // new
    pub compact_template: Option<String>,             // new
    pub title_prompt: Option<String>,                 // new
    pub extraction_prompt: Option<String>,            // new
    pub conflict_classification_prompt: Option<String>,// new
    pub generalization_prompt: Option<String>,        // new
    pub abstention_prompt: Option<String>,            // new
}
```

**`AgentCore`** (`agent/agent_core.rs`) gains the corresponding fields (same pattern as `compaction_prompt`); the caller's resolution chain:

```rust
// each LLM call site
let prompt = core.title_prompt
    .as_deref()
    .unwrap_or(crate::prompt::TITLE_PROMPT);
```

### 3.3 Main-Prompt Exclusion

`build_system_prompt_with_mode` extends the exclusion list from just `summary.md` to `OVERRIDABLE_PROMPTS.map(|(f, _)| f)` (an exact filename set); any `.md` / `.txt` file hit by that set is skipped when assembling the main system prompt. The semantics are symmetric:

> "Loaded as a task directive" and "excluded from the main prompt" always point at the same file; any other naming (e.g. `SUMMARY.md`, `title.txt`) is treated as a normal prompt segment.

### 3.4 The Three Categories Explicitly Not Overridden

**a. Runtime injection blocks (the 7 §Section templates in `context.rs`)**

Rationale:
- The templates contain **structural placeholders** (`{identity}` / `{memory}` / `{todos}`, etc.), and their position and order affect prompt cache hit rate (ADR-060's Block A stable prefix).
- The template boundaries are depended on by downstream parsing logic (e.g. `<conversation>` wrapping, the `## Environment` section being recognised by compression rules).
- "Changing directive content" and "changing the skeleton" must be separated: the former is brought into package-level override by this ADR, the latter must remain uniformly controlled by the platform.

**b. Tool descriptions (the 22 ToolSpec.description entries)**

Rationale:
- The 22 tools' descriptions are mostly "capability explanations" rather than "task directives", with low isomorphism to ADR-053.
- Some tool descriptions embed cross-field references (e.g. file_read emphasises "locate the line number with content_search first"), so the rewrite risk outweighs the benefit.
- Left as a future extension point: if a clear "agent customises tool behaviour" requirement appears, open a separate ADR (candidate file naming such as `tools/<tool_name>.md`, but requiring re-evaluation).

**c. Protocol formats (the line template in `episode_distill.rs::format_messages`, the TRUNCATED marker in `output.rs`)**

Rationale: the LLM reverse-parses these markers as the semantic boundaries of compression/truncation; making them flexible would directly break the protocol.

### 3.5 What Stays Unchanged

- All builtin constants are retained as fallbacks, with zero behavioural regression.
- The `summary.md` loader (`load_compaction_prompt`) may be replaced by the generic `load_optional_prompt` or retained as a thin wrapper — pick one of the two at implementation time.
- The concatenation structure of `build_compaction_system_prompt(base, identity_context)` is unchanged; only `base` may come from a package declaration.
- The loading convergence point (Phase A → `AgentBootContext` → Phase B injection) is completely consistent with ADR-053.

### 3.6 Limitations and Reservations

- **Static loading at startup**: consistent with the system prompt / compaction prompt; a package upgrade / hot update requires restarting Runtime to take effect.
- **The grafeo / memory constants are currently process-level singletons** (not per-`AgentCore`); whether to promote them to injectable parameters of a `MemoryProvider` trait needs evaluation (an extension on top of the ADR-051 decoupling foundation). This ADR only does "loader + injection point"; the implementation phase evaluates whether a trait refactor is needed.
- **The special semantics of `compact-template.md`**: what it overrides is a user-prompt template (containing the `<conversation>` placeholder), so the package author must retain the `{messages_text}` placeholder or runtime assembly fails. This is explicitly flagged in the documentation.

### 3.7 Debug Panel Edit Entry Point (L1 File Read/Write + L2 DevMode Reload)

After the package-level override mechanism lands, package authors have a "declare intent" channel, but **debugging and iteration are still a blank spot** — after an `.agent` package author modifies `prompts/summary.md`, they must restart Runtime to see the effect. This section defines a minimal Debug panel edit entry point covering three scenarios:

1. **Package authors**: quickly compare the impact of different `summary.md` wordings on compression output in Debug mode.
2. **Operations**: when a compaction output looks wrong, directly open the corresponding `prompts/*.md` to inspect/tweak it.
3. **Teaching**: visually show newcomers which prompts the runtime references and which are excluded.

#### 3.7.1 Overall Strategy: L1 Mandatory + L2 Recommended + L3 Not Done

| Layer | Scope | Implementation | ADR relationship |
|---|---|---|---|
| **L1 mandatory** | File read/write (UI + REST) | 4 new HTTP endpoints + a Debug panel list + FileEditor open/save; after saving, prompt "takes effect after restarting Runtime" or "takes effect after re-entering DevMode" | Compatible with §3.6 "static loading at startup" |
| **L2 recommended** | One-shot reload when DevMode starts | `POST /api/debug/prompts/reload` triggered by the `DebugService` trait after DevMode is enabled; reloads only once, no real-time propagation | Reuses the `reloadSkills` placeholder RPC pattern already present in ADR-048 §4.1 |
| **L3 not done** | Real-time lock-free hot loading (reading the latest value on every LLM call) | would require changing `AgentCore`'s 9 prompt fields to `Arc<arc_swap::ArcSwap<...>>` and rewriting every prompt call site | Directly conflicts with §3.6 "static loading at startup" + §5.3 "hot update rejected" |

**Why L2 covers most "hot loading" needs**: the typical Debug-mode workflow is "edit the prompt → exit Debug → re-enter Debug to see the effect" — that is the instinctive reaction of IDE debugging, not "it must take effect the second I save". Reloading at DevMode startup compresses this workflow to "save → re-enter Debug", which is enough; forcing L3 real-time hot loading would double the code complexity and conflict with the simple design of ADR-053 / §3.6.

**The relationship between L1 + L2 and shell_risk hot loading**: [`core/acowork-runtime/src/security/shell_risk.rs:236-241`](../../../core/acowork-runtime/src/security/shell_risk.rs) has already implemented the pattern of calling `reload_from_disk()` after a PUT + replacing an `Arc<RwLock>` global cache; this section borrows its "PUT writes to disk → trigger reload" approach, but prompts are per-`AgentCore` (not a global singleton), so L2 chose "reload at DevMode startup" rather than "reload at PUT time" — avoiding a split where a running session holds a stale `Arc<AgentCore>` diverging from the new reload result.

#### 3.7.2 UI Design

**Position**: at the top of the Debug panel (immediately below the tab labels), as a **resident header** of the Debug tab's 5 state branches ([ResultsPanel.tsx:281-292](../../../apps/acowork-desktop/src/components/right-panel/AgentSetupTab.tsx#L281-L292)). In states 1/2 (agent not running / DevMode not enabled) it naturally occupies the "blank space above"; in state 5 (connected to DebugPanel) it acts as a collapsible area at the top of the DebugPanel.

**Semantics**: **always displayed** (not gated on DevMode state), rationale:
- Consistent with shell_risk's "edit risk rules" button — the latter is resident UI in AgentSetupTab, gated by no state whatsoever.
- Debug mode is the **primary scenario for editing prompts**; if the list disappeared after DevMode starts, that amounts to taking "debugging prompts" away from the Debug panel.
- The 5 states' render areas already have large blank areas (icon + one line of text + one button); squeezing in a lightweight list does not break the layout.

**List content**: the 9 `OVERRIDABLE_PROMPTS` filenames in the §3.2 order, each row carrying a group label:

| UI label | Meaning | Visual example |
|---|---|---|
| 🟦 Normal segment | enters the main system prompt (e.g. `system.md` / `constraints.md`) | listed under the group "Main dialog" |
| ⚙️ Task directive | referenced on demand by the runtime, overriding the builtin constant in `prompt.rs` | listed under the group "Task directives" |

**File scope**: only the 9 `OVERRIDABLE_PROMPTS` filenames + `system.md` / `constraints.md` (as normal-segment representatives) are shown. Other random `.md` files under the `prompts/` directory (e.g. `examples/notes.md`) are **not shown** — keeping the focus on "the prompts referenced by the runtime".

**Click behaviour**: calls `fileEditorStore.openFileWithContent(agentId, "__agent_home__", "prompts/<name>.md", fetchedContent, "markdown")` to open in FileEditor (referencing the shell_risk open pattern in [AgentSetupTab.tsx:907-933](../../../apps/acowork-desktop/src/components/right-panel/AgentSetupTab.tsx#L907-L933)).

**Behaviour after saving**:

| Scenario | Behaviour |
|---|---|
| DevMode not enabled | Toast: "Saved. Restart Runtime to apply." (consistent with §3.6 static loading) |
| DevMode enabled | Toast: "Saved. Re-enter DevMode to apply." (L2 reloads once at DevMode startup) |
| Externally triggered reload (re-entering DevMode / Runtime restart) | the MQTT `debug/events/onStateChange` event carries a "prompts reloaded" signal, and the Desktop auto-refreshes the list content (reusing ADR-058's `diskConflict` tracking mechanism) |

#### 3.7.3 HTTP Endpoints

4 new Runtime HTTP endpoints + 1 Debug RPC, mirroring shell_risk's routing pattern ([server.rs:631-634](../../../core/acowork-runtime/src/http/server.rs#L631-L634)):

| Method + path | Runtime endpoint | Gateway proxy | Purpose |
|---|---|---|---|
| `GET /api/agents/{id}/prompts` | `GET /agents/{id}/prompts` | `/api/agents/{id}/prompts` | lists the files in that agent's `prompts/` directory that are the intersection with `OVERRIDABLE_PROMPTS` (including per-file size + mtime, for ADR-058 conflict detection) |
| `GET /api/agents/{id}/prompts/{name}` | `GET /agents/{id}/prompts/{name}` | same path | reads a single file's content (UTF-8; 404 when the file is missing) |
| `PUT /api/agents/{id}/prompts/{name}` | `PUT /agents/{id}/prompts/{name}` | same path | writes the file; `name` must be ∈ `OVERRIDABLE_PROMPTS` (preventing arbitrary file writes); writing empty content counts as a delete (the file is kept but emptied, consistent with ADR-053's `load_compaction_prompt` "whitespace-only → None" behaviour) |
| `POST /api/agents/{id}/debug/prompts/reload` | `POST /agents/{id}/debug/prompts/reload` | same path | the L2 trigger point; after DevMode is enabled it is called by the `DebugService` trait; performs "reload `OVERRIDABLE_PROMPTS` → replace the corresponding `AgentBootContext` fields → write the new values through the already-cloned `Arc<AgentCore>` (see the write strategy in §3.7.5)" |

**Response format** (consistent with shell_risk's GET response style):

```jsonc
// GET /api/agents/{id}/prompts
{
  "agent_id": "com.acowork.senior-engineer",
  "prompts": [
    { "name": "summary.md",          "size": 1234, "modified": "2026-09-20T10:30:00Z", "kind": "task" },
    { "name": "system.md",           "size": 567,  "modified": "2026-09-19T08:00:00Z", "kind": "main" },
    { "name": "constraints.md",      "size": 234,  "modified": "2026-09-19T08:00:00Z", "kind": "main" }
  ]
}
```

**Write routing**: PUT goes straight to the filesystem (`fs::write(package_dir.join("prompts").join(name), content)`), **not** through the Workspace file API (`/workspaces/file`) — the latter can only access `work_dir/`, whereas `prompts/` lives in the package install directory.

#### 3.7.4 Debug RPC Integration

Reusing the late-bind slot pattern of the `DebugService` trait from [ADR-048 §4.2](./ADR-048-debug-protocol-mqtt-http.md), adding one method:

```rust
// core/acowork-runtime/src/usecases/debug_service.rs (appended)
#[async_trait]
pub trait DebugService: Send + Sync {
    // ... the existing 10 methods (ADR-048)

    /// L2: trigger one prompt reload after DevMode starts.
    /// Re-reads prompts/<OVERRIDABLE_PROMPTS> → replaces AgentBootContext fields →
    /// writes into the already-cloned Arc<AgentCore> (see the write strategy in §3.7.5).
    /// Failure does not block DevMode startup; returns Ok(ReloadReport) for the caller to toast.
    async fn reload_prompts(&self) -> Result<ReloadReport, DebugError>;
}

pub struct ReloadReport {
    pub loaded: Vec<String>,    // filenames successfully reloaded
    pub missing: Vec<String>,   // files absent from the package directory (keep the old value or None)
    pub failed: Vec<(String, String)>, // (filename, error message)
}
```

**Invocation timing**: append `debug_service.reload_prompts().await` at the end of `enable_debug_mode()` in Phase C of [`core/acowork-runtime/src/startup/subsystems.rs`](../../../core/acowork-runtime/src/startup/subsystems.rs); the result is written into the MQTT `debug/events/onStateChange` payload for the Desktop toast.

**Desktop side**: invokes `reload_prompts` through ADR-048's existing `debug_rpc` generic command, with no new transport code — D7's "documentation sync" already declared "future fill-ins go through ADR-053, with zero transport wiring changes".

#### 3.7.5 Write Strategy: Avoiding `Arc<AgentCore>` Holding Stale Values

**Problem**: `AgentCore` has already been cloned into multiple `Arc<AgentCore>` scattered across sessions / AgentLoops. A simple `Arc::get_mut(&mut arc)` only succeeds when the reference count is 1 — at DevMode startup all sessions already hold clones, so **get_mut necessarily fails**.

**Solution**: wrap `AgentCore`'s 9 prompt fields in an `Arc<std::sync::RwLock<Option<String>>>`:

```rust
pub struct AgentCore {
    // ... existing fields

    /// §3.7.5: wrapped in RwLock so the L2 reload can write through.
    /// Semantics are identical to §3.2's standalone Option<String> fields; the only
    /// difference is the added reload write channel; read sites are always .read().unwrap().as_deref().
    pub(crate) compaction_prompt: Arc<std::sync::RwLock<Option<String>>>,
    pub(crate) fallback_prompt: Arc<std::sync::RwLock<Option<String>>>,
    // ... the other 7
}
```

**Clone semantics**: `Clone for AgentCore` shares the `Arc` (reference +1), not a deep copy — this **differs in behaviour** from the original `Option<String>` fields' Clone (which deep-copies the String), and must be explicitly flagged in the doc-comment:

> `AgentCore::clone()` shares `Arc<RwLock<...>>` for these 9 fields (reference +1); the remaining fields are still Clone'd by value. Writing to one clone in an L2 reload is visible to all clones.

**Read-site refactoring**: §3.2's `core.title_prompt.as_deref().unwrap_or(...)` becomes `core.title_prompt.read().unwrap().as_deref().unwrap_or(...)` — the lock guard is held only for the duration of the `.read()` call, so the overhead is negligible (the same order of magnitude as the original `Option<String>`'s `as_deref()`).

**Write sites** (L2 reload):

```rust
async fn reload_prompts(&self) -> Result<ReloadReport, DebugError> {
    let ctx = self.boot_context.lock().await;
    let new = load_all_overridable_prompts(&ctx.package_dir);
    *ctx.compaction_prompt.write().unwrap() = new.summary.clone();
    *ctx.fallback_prompt.write().unwrap() = new.fallback.clone();
    // ... the other 7
    Ok(new.into_report())
}
```

**Lock granularity**: each field has an independent `RwLock` and they do not lock each other — the L2 reload's concurrency and the LLM call's read can proceed fully in parallel; only a reload write and an LLM read of the **same** field are mutually exclusive.

**Performance**: a single LLM call's prompt resolution chain adds 9 `RwLock::read().try_lock()` calls (measured `RwLock::read()` ≈ 25ns with no write contention; the gap from the original `Option<String>.as_deref()` ≈ 5ns is negligible relative to LLM call latency).

**Write-path validation**: the PUT handler enforces `name` ∈ `OVERRIDABLE_PROMPTS` and contains no `/` `\` `..` (defensive, consistent with shell_risk's write handler).
- **Debug RPC authorisation**: the L2 endpoint `/api/agents/{id}/debug/prompts/reload` must go through DevMode being enabled — the path does not exist when Runtime starts; it only takes effect after `enable_debug_mode` registers it. Following ADR-048 §4.6's late-bind slot pattern.
- **Gateway proxy**: all 5 endpoints go through the Gateway proxy → Runtime localhost HTTP ([core/acowork-gateway/src/http/proxy.rs](../../../core/acowork-gateway/src/http/proxy.rs)), following the same pattern as shell_risk's proxy rules, adding ~10 lines.
- **A note on the grafeo / memory overrides**: the 4 grafeo / memory constants are currently process-level singletons (§3.6 flagged this), so the L2 reload's effective path for them differs — the reload for grafeo / memory inside `reload_prompts` requires evaluating a trait refactor (following the ADR-051 decoupling path). **Not implemented this round**: L2 reload for grafeo / memory; after a PUT of those files the prompt still says "restart Runtime".

---

## 4. Impact

### 4.1 Code Change List

**Core loading + AgentCore fields (§3.1-§3.6)**:

| File | Change |
|---|---|
| `core/acowork-runtime/src/package/prompt_builder.rs` | adds the `OVERRIDABLE_PROMPTS` constant + the `load_optional_prompt` generic loader; the main-prompt exclusion list is extended to the complete exact-filename set; N new unit tests |
| `core/acowork-runtime/src/prompt.rs` | the top `//!` comment gains a "package-level override filename convention" section listing all overridable constants and their filenames |
| `core/acowork-runtime/src/agent/agent_core.rs` | **CHANGE**: §3.2's 9 `Option<String>` fields become `Arc<std::sync::RwLock<Option<String>>>` (see §3.7.5); the `Clone` impl now shares the `Arc` (reference +1); the 9 read sites become `.read().unwrap().as_deref().unwrap_or(const)` |
| `core/acowork-runtime/src/startup/context.rs` | `AgentBootContext` gains the corresponding 9 `Option<String>` fields (**keeping the original type** — Phase A loads once, no lock needed) |
| `core/acowork-runtime/src/startup/agent_init.rs` | Phase A loads all overridable prompts, storing the results in `AgentBootContext` |
| `core/acowork-runtime/src/startup/session_init.rs` | Phase B injects from ctx into `AgentCore`, wrapping with `Arc::new(RwLock::new(ctx.field.clone()))` |
| `core/acowork-runtime/src/cli.rs` | the Standalone branch injects from ctx (eliminating the mode split) |
| Each LLM call site (`episode_distill.rs` / perplexity.rs / grafeo / memory) | the resolution chain becomes `core.<field>.read().unwrap().as_deref().unwrap_or(const)` |

**Debug panel edit entry point (§3.7)**:

| File | Change |
|---|---|
| `core/acowork-runtime/src/http/prompts.rs` (**new**) | 4 axum handlers: `list_prompts` / `get_prompt` / `put_prompt` / `reload_prompts`; the PUT path validation enforces `name ∈ OVERRIDABLE_PROMPTS` and no `/`/`\`/`..` |
| `core/acowork-runtime/src/http/server.rs` | new routes: `GET /agents/{id}/prompts` / `GET /agents/{id}/prompts/{name}` / `PUT /agents/{id}/prompts/{name}` / `POST /agents/{id}/debug/prompts/reload`; the last one goes through ADR-048's `DebugService` late-bind slot |
| `core/acowork-runtime/src/usecases/debug_service.rs` | the `DebugService` trait gains the method `async fn reload_prompts(&self) -> Result<ReloadReport, DebugError>` + the `ReloadReport` DTO |
| `core/acowork-runtime/src/usecases/debug_service_impl.rs` | the `RuntimeDebugService::reload_prompts` implementation: re-reads the 9 files via `load_optional_prompt` → falls back from `Arc::get_mut` failure to writing the `boot_context` fields' `RwLock` → constructs a `ReloadReport` |
| `core/acowork-runtime/src/startup/subsystems.rs` | appends `debug_service.reload_prompts().await` at the end of Phase C's `enable_debug_mode()`, pushing the result via MQTT `debug/events/onStateChange` payload |
| `core/acowork-gateway/src/http/proxy.rs` | adds 5 proxy rules: `/api/agents/{id}/prompts` (GET) + `/api/agents/{id}/prompts/{name}` (GET+PUT) + `/api/agents/{id}/debug/prompts/reload` (POST), forwarding to the Runtime localhost HTTP |
| `apps/acowork-desktop/src/components/debug/PromptList.tsx` (**new**) | the list component, two-group layout (🟦 normal segment / ⚙️ task directive), clicking calls `fileEditorStore.openFileWithContent` |
| `apps/acowork-desktop/src/components/debug/DebugPanel.tsx` | wires `<PromptList agentId={agentId} />` at the top of the DebugPanel; always rendered, not gated by the 5 states |
| `apps/acowork-desktop/src/components/results/ResultsPanel.tsx` | the Debug tab's state 1/2 branches (agent not running / DevMode not enabled) render `<PromptList />` as the main content filling the "blank space above" |
| `apps/acowork-desktop/src/stores/debugStore.ts` or `commands/debug.rs` | a new `reloadPrompts()` client method, going through ADR-048's `debug_rpc` generic command |
| `apps/acowork-desktop/src/i18n/locales/{zh-CN,en,...}.json` | new i18n keys: `debug.promptList.title` / `debug.promptList.mainGroup` / `debug.promptList.taskGroup` / `debug.promptList.savedRestartHint` / `debug.promptList.savedReenterHint` |

**Documentation + protocol**:

| File | Change |
|---|---|
| `examples/*/prompts/*.md` | sample files: supplement the corresponding override files per agent type (`fallback.md` / `search.md` / `title.md` / `extraction.md` / `abstention.md`, etc.) |
| `docs/prompt-audit/zh/runtime-prompts-summary.md` | adds a "supported override filename" column to the §1 / §3 tables; adds an "explicitly not overridden" section referencing this ADR; the top comment in §1 gains a "Debug edit entry point" link to §3.7 |
| `docs/protocols/zh/http.md` | API reference for the 4 new HTTP endpoints + the 1 Debug RPC route |
| `docs/design/zh/03-agent-runtime.md` §7.2 | syncs the description of the prompt reload trigger point (once at DevMode startup) |
| `docs/adr/zh/ADR-048-debug-protocol-mqtt-http.md` | §4.1 / §4.2 note that `reload_prompts` is the D8 placeholder RPC newly implemented by ADR-063 §3.7 |

### 4.2 Behaviour Changes

| Scenario | Before | After |
|---|---|---|
| The package has no `prompts/<file>.md` | uses the builtin default | uses the builtin default (unchanged) |
| The package has `prompts/<file>.md` | (no such concept) | that task's LLM call uses the package-declared directive |
| The package has `prompts/<file>.md` but the file appears in the main system prompt assembly | — | automatically skipped by exact filename (symmetric with summary.md) |
| The user sets `system_prompt_override` | affects only the main conversation | unchanged (orthogonal to this ADR) |
| The user edits `prompts/<file>.md` in the Debug panel and saves (DevMode not enabled) | — | the file is written to disk, with a toast "restart Runtime to apply" |
| The user edits `prompts/<file>.md` in the Debug panel and saves (DevMode enabled) | — | the file is written to disk, with a toast "re-enter DevMode to apply"; on re-entry it auto-reloads (§3.7.4) |

### 4.3 Verification

- `cargo build -p acowork-runtime`: passes.
- `cargo test -p acowork-runtime --lib`: covers all branches of the 8 new loaders + the main-prompt exclusion extension.
- `cargo test -p acowork-grafeo --lib` / `cargo test -p acowork-memory --lib`: covers the fallback paths after the injection-site refactor.
- `cargo clippy --all-targets -- -D warnings`: zero warnings.
- Integration tests: `mqtt_e2e_full` / `conversation_session_tokens` / `builtin_tools_mutation` all green.
- Documentation sync: the `prompt-audit` tables updated + the ADR links effective.

---

## 5. Alternatives

### 5.1 A single "override layer" config file (e.g. `prompts/overrides.json`) (rejected)

Putting all overridable constants into one JSON file: keys are constant names, values are strings. It looks more compact than 8 `.md` files, but:
- it loses Markdown's code-block/list/heading syntax, degrading the editing experience;
- it is asymmetric with `summary.md`, breaking the "all prompts use .md" organisational principle;
- JSON field names are structured identifiers, unfriendly to package authors.

### 5.2 A runtime config entry point (adding a field to `agent_config.json`) (rejected)

8 new fields would bloat `agent_config.json` and confuse the two dimensions of "package declaration" vs "user tuning". After the runtime config is rewritten, all instances using that `.agent` package would diverge in behaviour (same root cause as the semantic confusion already exposed by `system_prompt_override`). A package-level declaration is the single source of truth.

### 5.3 Runtime hot update (watching the prompts/ directory and reloading prompt content) (**partially rejected** — only L3 rejected; L1 + L2 see §3.7)

- It conflicts with ADR-053's "static loading at startup" principle;
- `AgentCore` has already been cloned into multiple `Arc<AgentCore>` scattered across sessions / AgentLoops, so lock-free requires `Arc<arc_swap::ArcSwap<...>>`, and every one of the 9 prompt fields' call sites must be rewritten;
- the semantics of a package upgrade should be unified at a Runtime restart, not split at the prompt layer.

See the three-layer strategy table in §3.7.1 + the `Arc<RwLock<...>>` write strategy in §3.7.5.

### 5.4 Extending the override scope to runtime injection blocks / tool descriptions (rejected)

§3.4 has detailed the rationale. Core: injection blocks are the structural skeleton (cache anchors + protocol boundaries), tool descriptions are tool metadata; both have low isomorphism with "task directive prompts", and forcing the extension would introduce complexity far beyond the benefit. Left as a future extension point.

### 5.5 Changing the loader to a unified HashMap (`<filename, content>`) (rejected)

The `AgentCore.extra_prompts: HashMap<&'static str, String>` shape does have the advantage of "zero changes to AgentCore / AgentBootContext when extending new override items", but it loses type safety (a typo'd key is invisible at compile time) and violates the promise of strict isomorphism with ADR-053. The code duplication of standalone fields is acceptable (9 `Option<String>` + one-to-one `unwrap_or` resolution chains), buying the ability to see at a glance which prompt overrides each agent holds.

### 5.6 Real-time hot loading (`Arc<arc_swap::ArcSwap<...>>`, reading the latest value on every LLM call) (rejected)

This is the "performance optimisation option" discussed at the end of §3.7.5 — changing all prompt fields to `ArcSwap` + `OnceCell` caching to avoid reading a lock on every LLM call. **Rejected**: an LLM call's millisecond-level latency dwarfs the lock overhead (25ns vs 5ns), so the caching benefit does not match the code complexity. If LLM call latency ever drops to microseconds (e.g. local small-model inference), a separate ADR can re-evaluate it.

---

## 6. Follow-ups

- **grafeo / memory injection-point evaluation**: this implementation needs to evaluate whether the users of the 4 constants should become trait-injected (following ADR-051's MemoryProvider decoupling path).
- **Phasing**:
  1. **Phase A (mandatory)**: the 8+1 loaders + `AgentBootContext` fields + Phase B injection + `AgentCore` field type change to `Arc<RwLock<...>>` (§3.7.5) + the §3.7.2 Desktop `PromptList` component; no L2 reload capability, so after saving it prompts "restart Runtime" or "re-enter DevMode".
  2. **Phase B (recommended)**: the `DebugService::reload_prompts` implementation + the call at the end of `enable_debug_mode` (§3.7.4); **L2 reload for the 4 grafeo / memory constants is out of scope this round** — their injection points need a trait refactor (§6.1 follow-up), so they keep "takes effect after restarting Runtime".
- **Regression tests for the `AgentCore` RwLock refactor**: after the read sites of all 9 prompt fields (`episode_distill.rs` / perplexity.rs / grafeo / memory) change to `.read().unwrap()`, add unit tests with 9 concurrent reads + 1 write, ensuring the lock granularity is correct and no deadlock is introduced.
- **Extending the MQTT `debug/events/onStateChange` payload**: the 5 payload fields currently listed in ADR-048 §4.1 need to append `prompts_reloaded: Option<ReloadReport>`, populated only after `reload_prompts()` is called; the Desktop-side toast and list-refresh logic depend on this field.
