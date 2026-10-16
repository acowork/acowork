# ADR-094: Per-Agent Skills Auto-Load (Progressive Default) + `load_skill` Built-in Tool

- **Number**: ADR-094
- **Status**: Accepted (revised 2026-10: load_skill is the sole toggle; summary and per-turn skill slots separated — see D7.1)
- **Date**: 2026-（original）
- **Decision makers**: ACowork Architecture Group
- **Related**: ADR-009 §V-A (single authoritative SKILL.md parser), ADR-040 (usecase-layer separation), ADR-048 (DevMode debug protocol)

## Summary

ACowork agents carry reusable workflows as `skills/<name>/SKILL.md` (frontmatter `name` + `description` + Markdown body). Today, **skills must be pasted into the chat by the user** for the LLM to see them — `SkillMode::Manual` is the factory default.

Two concrete problems:

1. The LLM does not know which skills the agent owns, so it never calls them.
2. Every new session repeats the same paste, violating the principle that "session context should focus on the user's question, not skill metadata."

Industry convention (Claude Code, MCP, Cursor, Windsurf) injects tool metadata (name + description) into the system prompt and exposes a dedicated tool that the LLM uses to pull full instructions on demand. This ADR makes ACowork's skill system match that convention.

## 1. Context and Problem

`AgentSkillsOverride + .agent_skills.json` was the previous override path. It had **only a reader, no writer, no HTTP API, no UI entry point** — a dead-code equivalent. Removing it and folding the per-agent switch into `agent_tools.json::load_skill.enabled` (alongside every other built-in tool) keeps the per-agent tool surface aligned under ADR-040's usecase-layer separation.

The 2026-10 revision collapses the original two-switch design
(`agent_config.json::skills_auto_load` + the `load_skill` tool switch) into
a single source of truth: **`load_skill.enabled`**. See §D4 for the
rationale and §Rejected Alternatives C for the post-mortem on the
two-switch design.

## 2. Decisions

### D1 — Resolution priority for skill mode

| Priority | Source | Field |
|---|---|---|
| 1 (highest) | `agent_tools.json::load_skill.enabled` | `true` → Progressive; `false` → Manual |
| 2 | `manifest.skills.progressive` | Author default (default `false`) |
| 3 | Fallback | Manual |

Implementation: [`resolve_skill_mode()`](../../../core/acowork-runtime/src/cli.rs) in `core/acowork-runtime/src/cli.rs`, called from both `agent_init` and `http/prompts`. Replaces the old `AgentSkillsOverride` + `.agent_skills.json` dead-code path.

**`load_skill.enabled` is the sole source of truth. There is no second field.**

### D2 — UI entry point: the Builtin Tools list

The `load_skill` row lives in the **Tools panel → Builtin Tools list** (next
to `memory_recall` / `file_read` / `http_request` / etc.) and uses the
standard `PUT /agents/{id}/builtin-tools` endpoint. Toggling its Switch
is the **only** way to flip Progressive on or off.

The Agent Setup → Session Parameters card no longer carries an
"Auto-load skills" Switch. The 2026-10 revision deleted it because it
duplicated the same decision on a second surface (see §D4 + §Rejected
Alternatives C).

### D3 — New built-in tool: `load_skill`

[`tools/builtin/load_skill.rs`](../../../core/acowork-runtime/src/tools/builtin/load_skill.rs) (new). After the LLM sees the skills summary in the system prompt, it calls this tool on demand to pull a specific skill's full `SKILL.md` body:

- Input: `{ name: string }` — strict match against the SKILL.md frontmatter `name`
- Behavior: re-reads `<package_dir>/skills/<name>/SKILL.md` (no cache; fresh-load each call — same contract as `http::skills::load_registry`), returns full metadata + instructions
- Reuses the existing SKILL.md parser (the single authoritative parser per ADR-009 §V-A); no second copy is introduced

### D4 — Tight linkage: `load_skill` is the sole Progressive toggle

`load_skill.enabled` simultaneously drives two things:

1. **Whether the LLM can call the `load_skill` tool** — the standard
   `UpdateBuiltinTools { enabled: [...] }` broadcast path; the entry
   appears or disappears in `ContextBuilder::tool_definitions`.
2. **Whether the system prompt carries the skills summary** — the
   `UpdateSkillInstructions { instructions: Option<String> }` broadcast
   path; the section appears or disappears in
   `ContextBuilder::skill_summary` (rendered under the unchanged
   `skill_instructions` section key — see D7.1).

The two paths MUST be dispatched together, in the same toggle. Otherwise
the LLM sees an inconsistent state: either a skills summary with no tool
to expand it, or a `load_skill` tool with no list of what to load.

Implementation: concentrate the `load_skill` toggle into a single helper
[`broadcast_builtin_tools_update`](../../../core/acowork-runtime/src/http/server.rs)
in `http/server.rs`. It first dispatches `UpdateBuiltinTools` and
**immediately** dispatches `UpdateSkillInstructions` (via
[`rebuild_and_dispatch_skill_instructions`](../../../core/acowork-runtime/src/http/prompts.rs)).
Both dispatches share the same agent-level chain
(`dispatch_agent_level_config` → `SessionManager::apply_*` →
per-session `ContextBuilder::set_*`), and the future-session template
(`SessionManagerConfig`) is updated in lockstep.

#### Why linkage is required — three user scenarios

| Scenario | Without linkage | With linkage |
|---|---|---|
| User turns on `load_skill`, expecting the LLM to know which skills exist | Tool is on but the system prompt has no summary → LLM does not know which skills are available | Tool + summary both on |
| User turns off `load_skill`, but Progressive is still enabled → tool vanishes but summary lingers | LLM sees `["greeting", "code_review"]` but calling `load_skill("greeting")` returns "tool not available" | Tool + summary both off |
| User first turns on `load_skill` | n/a | Single PUT opens both, zero extra config |

#### Why "two independent fields with implicit linkage" is rejected

The original v1 design kept `agent_config.json::skills_auto_load` and the
`load_skill` tool switch as two independent surfaces, with implicit
linkage (toggling one auto-writes the other). It was implemented, shipped,
and rolled back after three concrete regressions:

1. **UI drift**: the Agent Setup panel and the Tools panel were not
   synchronised in real time. After Desktop restart, Agent Setup showed
   ON while Tools showed OFF; users had to align them manually.
2. **PUT-order race**: concurrent PUTs (one flipping Progressive, one
   flipping `load_skill.enabled`) had last-writer-wins semantics, so
   the final config did not match user intent.
3. **Two fields → two sources of truth → two failure paths**: keeping
   `agent_config.json.skills_auto_load` in sync with
   `agent_tools.json::load_skill.enabled` was required in three
   different code paths (CLI resolve, HTTP prompts rebuild,
   SessionManager template). Any missed update was a bug.

Tight linkage with a single field, a single dispatch helper, and three
co-located writes **eliminates** all three failure modes.

#### Why `load_skill` should not "self-enable Progressive"

- `load_skill` is a peer of every other built-in tool (`memory_recall`,
  `file_read`, `http_request`, ...). It should not enjoy a
  "self-enable" privilege.
- "Tool enabled implies Progressive" silently mutates user config and
  breaks the opt-in contract.
- The correct relationship is the reverse: **Progressive is enabled if
  and only if `load_skill.enabled == true`**. The tool's state is the
  source of truth; Progressive is a downstream derived state.

### D5 — Cache friendliness

- `resolve_skill_mode` runs once during `agent_init` startup; the result is frozen into the system prompt at that moment.
- Toggling the `load_skill` Switch mid-session dispatches both
  `UpdateBuiltinTools` and `UpdateSkillInstructions` over the existing
  fast channel; the `ContextBuilder` template is rebuilt before the next
  `build_chat_request` call. The current session's next LLM request
  sees the new state immediately.
- The previous v1 design (Progressive Switch applied only on the next
  session) was a deliberate cache-preservation choice; the 2026-10
  revision trades that "zero mid-session cache invalidation" for
  "no UI drift / no PUT race" because the bundled
  `UpdateBuiltinTools` + `UpdateSkillInstructions` dispatch already
  invalidates only the affected ContextBuilder fields, not the
  surrounding system-prompt sections.

### D6 — Dead-code cleanup

The previous `{work_dir}/.agent_skills.json` path (`AgentSkillsOverride` + old `resolve_skill_mode`) had only a reader — **no writer, no HTTP API, no UI entry point** — a dead-code equivalent. This ADR deletes it:

- Remove `AgentSkillsOverride` and the old `resolve_skill_mode` in [`core/acowork-runtime/src/cli.rs`](../../../core/acowork-runtime/src/cli.rs).
- Rewrite `resolve_skill_mode` to read `agent_tools.json::load_skill.enabled`.
- Keep `startup/mod.rs`'s `super_mod` re-export (add a comment only).

The 2026-10 revision additionally removed the second-field plumbing that
the v1 two-switch design required:

- `AgentConfig.skills_auto_load: Option<bool>` field (deleted from [`agent_config.rs`](../../../core/acowork-runtime/src/agent_config.rs))
- `UpdateAgentConfigRequest.skills_auto_load` field (deleted from [`http/server.rs`](../../../core/acowork-runtime/src/http/server.rs))
- `ConfigField::SkillsAutoLoad` enum variant (deleted from [`usecases/agent_config.rs`](../../../core/acowork-runtime/src/usecases/agent_config.rs))
- `RuntimeConfigOverrides.skills_auto_load` field and its `is_empty` / `merge` / `apply_to_config` / `From<&AgentConfig>` projections (deleted from [`session_manager.rs`](../../../core/acowork-runtime/src/agent/session/session_manager.rs))
- The AgentSetupTab Switch + `WiredField` + debounce + i18n keys (deleted from [`AgentSetupTab.tsx`](../../../apps/acowork-desktop/src/components/right-panel/AgentSetupTab.tsx) + the i18n locale files)
- The `skills_auto_load` wire field in the config-sync helper (deleted from [`agentSetupConfigSync.ts`](../../../apps/acowork-desktop/src/components/right-panel/agentSetupConfigSync.ts))
- `AgentProfileSettings.skillsAutoLoad` field (deleted from [`agentStore.ts`](../../../apps/acowork-desktop/src/stores/agentStore.ts))

### D7 — Skill summary flows as a first-class section (reuses existing data model)

The Progressive-mode skills summary (name + description only) MUST appear
as an **independent section key** in `compute_section_sizes`'s output.
Otherwise the frontend's usage popover ([`SKILL_SECTION_KEYS`](../../../apps/acowork-desktop/src/lib/contextUsageBreakdown.ts)) never matches it (skills category stays at 0%) and the Debug panel's `SECTION_LABELS["skill_instructions"]` is never populated.

This decision makes it explicit: the skills summary is **not embedded**
in the `SystemPromptParts.base` string; it is returned as a separate
`Option<String>` so the caller can `context_builder.set_skill_summary(skills)`. `compute_section_sizes` already has the matching emit path (`if let Some(skills) = builder.skill_instructions() { push skill_instructions section }`) — no new section key needed.

The event chain mirrors `UpdateSystemPrompt` / `UpdateIdentityContext`
exactly (agent-scoped template + per-session broadcast):

| Layer | Reuse |
|---|---|
| Field | `ContextBuilder::skill_summary: Option<String>` (session-lifetime summary) + `ContextBuilder::active_skill: Option<String>` (this turn's command-selected skill — see D7.1) |
| Shared template | `SessionManagerConfig::skill_summary: Option<String>` (new) |
| Inbound event | `InboundMessage::UpdateSkillInstructions { instructions: Option<String> }` (new) |
| Session event | `SessionMessage::UpdateSkillInstructions { instructions: Option<String> }` (new) |
| Broadcast | `SessionManager::apply_skill_instructions()` (new) |
| Wire protocol | `SectionMeta { key, size_bytes, token_estimate, hash }` + `OnContextBuiltParams { sections: ContextSections }` (**existing**, generic) |
| Debug render | `DebugPanel::SECTION_LABELS["skill_instructions"]` + `SECTION_ORDER` (existing) |
| Usage render | `contextUsageBreakdown::SKILL_SECTION_KEYS` + `ContextUsageIcon::CATEGORY_META` (existing) |

**Hard constraint**: no new data structures, no new event types, no new
section keys — the `skill_instructions` key from emit through wire to
render already exists end-to-end. The only thing missing was that
`prompt_builder` was folding the summary into the base string. This
section fixes that data-flow break, not the architecture.

### D7.1 — Summary slot separated from the per-turn skill slot (2026-10 revision)

The **section key `skill_instructions` stays unchanged** (the usage popover
and Debug panel need no edits), but `ContextBuilder` no longer carries two
different lifetimes in one field:

| Slot | Lifetime | Written by | Meaning of a clear |
|---|---|---|---|
| `skill_summary` | session-lifetime | Phase A seeding, `apply_skill_instructions` broadcast, debug patch | switched to Manual — summary withdrawn |
| `active_skill` | one turn | `SessionMessage::ChatMessage::active_skill` (frontend `command` → resolved against `SkillRegistry`) | no skill this turn — drop last turn's so it cannot leak |

`ContextBuilder::skill_section()` composes them at render time (summary
first, then `### Selected skill`), so the LLM still sees exactly one
`## Skill Instructions` block, and `skill_instructions()` returns that
composed text — the `compute_section_sizes` emit path is untouched.

**Why the split was required**: both slots shared one field, so the per-turn
clear that prevents a selected skill from leaking across turns — which runs on
the *normal* chat path, where no skill is selected — also destroyed the
session-lifetime summary. Symptom: in Progressive mode the LLM never saw the
skill list, from the first message of any new session. The clear is correct for
`active_skill` and destructive for `skill_summary`; sharing a field guarantees
the fault.

**Why not keep a copy of the summary in the caller and restore it**: that
creates a second authoritative source, after which every writer (seed /
hot-reload broadcast / debug patch / per-turn) must remember to sync it — miss
one and you have the next bug of this kind. Same reasoning by which §4 rejects
"two independent switches plus reconciliation logic". Splitting the field makes
the misuse unrepresentable: the public `clear_skill_instructions()` is deleted,
so any code that wants to "clear skills" must state which lifetime it means.

**Out of scope**: `active_skill` lives in Block A (system prompt), so the block
changes on turns that select a skill, costing ADR-060 prompt-cache hits. This
predates the split; the fix neither introduces nor opportunistically changes
that. Moving it to a non-cached block needs its own decision.

## 3. Rejected Alternatives

### A. Keep `.agent_skills.json` as a separate file

- Current state: reader exists, but no writer / HTTP / UI
- Fixing it = building full infra (`AgentSkillsOverrideService` trait + `usecases` route + HTTP API + Gateway reverse proxy + Frontend fetch + UI)
- Effort: ~200 lines across multiple files
- Downside: overlaps with `agent_config.json` (temperature, max_iterations, etc.); breaks ADR-040's layering
- **Rejected**: YAGNI + DRY

### B. Make Progressive the manifest default (non-overridable)

- Pro: zero user config
- Con: forces Progressive on every agent, violating "per-agent defaults should be per-agent controllable"; user cannot opt out for a single agent
- **Rejected**: violates the "per-agent default is per-agent controlled" design principle

### C. v1 design — two independent fields with implicit linkage

- Implementation: keep `agent_config.json::skills_auto_load` and the
  `load_skill` tool switch as two independent surfaces; the PUT handler
  auto-writes `load_skill.enabled = true` when Progressive flips to
  `true` (and vice versa).
- Was implemented, shipped, and rolled back after three regressions:
  - The bidirectional auto-write logic had three different
    implementations (PUT handler, session spawn, test fixture); any
    missed update was a drift bug.
  - `agentSetupConfigSync` was first extracted specifically to fix the
    "Auto-load skills" Switch silently failing to sync after Desktop
    restart — the very bug that motivated the 2026-10 revision.
  - Concurrent PUTs ("Progressive=true & load_skill=false" in parallel)
    had last-writer-wins semantics; the final config did not match
    user intent.
- **Rejected**: two independent switches are an invitation to drift.
  Since linkage is required (§D4), two fields are redundant — putting
  the toggle on `load_skill` is simpler and consistent with every
  other built-in tool.

### D. `list_my_skills` + `load_full_skills` dual-tool approach

- Pro: zero cost for simple sessions
- Con: every extra tool call forces the LLM to recompute prompt cache hit boundaries (tool output enters message history)
- **Rejected**: worse cache friendliness than Progressive; permanent tool-schema overhead ~200 tokens

## 4. Implementation Checklist

| # | Change | File |
|---|---|---|
| 1 | Remove `AgentSkillsOverride` dead code | [`core/acowork-runtime/src/cli.rs`](../../../core/acowork-runtime/src/cli.rs) |
| 2 | Rewrite `resolve_skill_mode` (read `agent_tools.json::load_skill.enabled`) | [`core/acowork-runtime/src/cli.rs`](../../../core/acowork-runtime/src/cli.rs) |
| 3 | New `LoadSkillTool` built-in tool | [`core/acowork-runtime/src/tools/builtin/load_skill.rs`](../../../core/acowork-runtime/src/tools/builtin/load_skill.rs) |
| 4 | Register `load_skill` in `all_builtin_tools()` | [`tools/builtin/mod.rs`](../../../core/acowork-runtime/src/tools/builtin/mod.rs) |
| 5 | Add `load_skill` to `BUILTIN_TOOLS` static list | [`acowork-core/src/tools/mod.rs`](../../../core/acowork-core/src/tools/mod.rs) |
| 6 | Add `load_skill` to all 7 example manifests' `[[tools]]` (first-boot seed) | [`examples/`](../../../examples/) |
| 7 | `broadcast_builtin_tools_update` also dispatches `UpdateSkillInstructions` | [`http/server.rs`](../../../core/acowork-runtime/src/http/server.rs) |
| 8 | Frontend i18n `load_skill` description gains "on = auto-inject skills summary" hint | [`i18n/`](../../../apps/acowork-desktop/src/i18n/locales/) |
| 9 | `test_compute_skill_instructions_payload_priority` covers three `load_skill.enabled` cells | [`http/prompts.rs`](../../../core/acowork-runtime/src/http/prompts.rs) |
| 10 | ~~Add `skills_auto_load: Option<bool>` to `AgentConfig`~~ (deleted 2026-10) | n/a |
| 11 | ~~Add `skills_auto_load` to `UpdateAgentConfigRequest`~~ (deleted 2026-10) | n/a |
| 12 | ~~`ConfigField::SkillsAutoLoad` + dispatch + projection~~ (deleted 2026-10) | n/a |
| 13 | ~~`RuntimeConfigOverrides.skills_auto_load`~~ (deleted 2026-10) | n/a |
| 14 | ~~AgentSetupTab Session Parameters Switch + WiredField + i18n~~ (deleted 2026-10) | n/a |
| 15 | ~~`AgentProfileSettings.skillsAutoLoad`~~ (deleted 2026-10) | n/a |

## 5. Risks and Guardrails

- **`load_skill` is seeded as `enabled = true` in the example manifests**, following the same first-boot path as every other built-in tool.
- **Toggling `load_skill` mid-session**: takes effect immediately via the
  fast channel; `ContextBuilder` is rebuilt before the next
  `build_chat_request`. No "next session only" lag.
- **User turns off `load_skill`** (= explicit Progressive off): the LLM
  sees no skill summary and no `load_skill` tool. This is the intended
  state and there is no longer any way to land in an inconsistent
  "summary without tool" or "tool without summary" state.
- **Single toggle, single dispatch helper**: both events
  (`UpdateBuiltinTools` + `UpdateSkillInstructions`) live in
  `broadcast_builtin_tools_update`. Future patches that add another
  field touching skill mode must extend that helper, not introduce a
  second endpoint.

## 6. Future Evolution (out of scope for this ADR)

- **Tiered summary**: when the number of skills exceeds 20, downgrade to name + truncated description (YAGNI, not implemented).
- **Per-skill overrides**: if some skills are particularly large or sensitive, the override may be pushed down to a `.agent_skills.json` file (at that point, this ADR's "store in `agent_tools.json`" decision can be revisited).

## 7. Cross-references

- Replaces: old `AgentSkillsOverride` in [`core/acowork-runtime/src/cli.rs`](../../../core/acowork-runtime/src/cli.rs)
- Replaces: old `resolve_skill_mode` `.agent_skills.json` path in [`core/acowork-runtime/src/cli.rs`](../../../core/acowork-runtime/src/cli.rs)
- Reuses: [`SkillRegistry::load_from_dir`](../../../core/acowork-runtime/src/skills/parser.rs)
