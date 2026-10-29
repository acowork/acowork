# ADR-104: Restructuring the Manifest Declaration Surface — `[config]` Defaults, `[[mcp_servers]]` Recommendations, the capability↔skill Relation, and `labels`

**Status**: Implemented (P1–P5 landed in full)
**Date**: 2026-10-30
**Deciders**: ACowork architecture group

**Related**:
- [ADR-009](./ADR-009-gateway-workspace-isolation.md) (Gateway boundary red line), [ADR-017](./ADR-017-manifest-and-package-format.md) (manifest and package format), [ADR-025](./ADR-025-capability-addressing.md) (capability addressing), [ADR-026](./ADR-026-permission-model.md) (permission model), [ADR-029](./ADR-029-tool-declaration.md) (tool declaration), [ADR-055](./ADR-055-node-agent-topology.md) (Node topology)
- [ADR-061](./ADR-061-context-compression-ratio-threshold.md) (compaction ratio threshold), [ADR-069](./ADR-069-mcp-tool-level-optin.md) (per-tool MCP opt-in), [ADR-072](./ADR-072-mcp-install-package-spec.md) (MCP install package spec), [ADR-074](./ADR-074-context-window-semantics.md) (context window semantics), [ADR-090](./ADR-090-mcp-lsp-install-ownership-node.md) (MCP install belongs to the Node), [ADR-094](./ADR-094-progressive-skill-auto-load-and-load-skill-tool.md) (progressive skills), [ADR-101](./ADR-101-memory-standalone-mcp-service.md) (memory as a service — this ADR freezes "platform-hosted MCP" as a first-class declaration form)

**Companion documents**: [agent-manifest-declaration-plan.md](../../plan/zh/agent-manifest-declaration-plan.md) (code evidence and granularity evaluation), [agent-manifest-ui-walkthrough.md](../../plan/zh/agent-manifest-ui-walkthrough.md) (screen-by-screen walkthrough). Both are Chinese-only working documents.

---

## Summary

**In one sentence**: extend `manifest.toml` from "metadata plus a builtin tool list" into a **complete capability declaration surface** — adding `[config]` (behaviour defaults for `agent_config.json`), `[[mcp_servers]]` (recommended MCP servers, installed on user confirmation), `[capabilities.*].skills` (the real capability→skill relation) and `labels` (attribute tags) — and wire all four into the UI: recommended MCP is installable / activatable / dismissable from the Tools panel, and the Create Agent wizard grows from four steps to five so it covers every field.

**Why**: a manifest that can only say "these are my builtin tools" cannot express "which MCP I need", "which parameters I recommend", or "which skills implement which capability". That knowledge then survives only as hardcoded constants (`PM_DEFAULT_ENABLED_TOOLS`) or verbal agreement. Every missing piece of the declaration surface is a missing piece of checkable, displayable, revertible fact.

**Four invariants** (they shape the whole document):

1. **Recommendation ≠ installation.** `[[mcp_servers]]` is the author's *suggestion*. It lands in `agent_mcp.json.recommended` and never writes `catalog` / `local` / `active_names`. Installing and activating are **user actions** (ADR-090).
2. **Default ≠ override.** `[config]` fills only fields that are `None` in `agent_config.json`; a value the user set is never overwritten (and the operation is idempotent).
3. **Capability ≠ skill.** A skill is an **internal workflow** (not exposed); a capability is an **outward contract** (other agents address it through an intent). The relation is many-to-many.
4. **A declaration must have a visible consequence.** A recommended MCP with nothing to see and nothing to do would be decoration; this ADR's recommendation has four observable outcomes (§5.1).

---

## 1. Facts (taken from the code)

### 1.1 What the manifest declared, and what it did not

`AgentManifest` ([core/acowork-core/src/manifest.rs](../../../core/acowork-core/src/manifest.rs)) held: identity fields, `permissions`, `triggers`, `[llm]`, `[memory]`, `identity_deps`, `[[tools]]`, `[capabilities.*]`, `[resources]`, `[sandbox]`, `[skills].progressive`.

**Missing**: MCP declarations, `agent_config.json` defaults, labels, and the implementation relation behind a capability.

### 1.2 `agent_config.json` had no single landing path

- `temperature` / `context_window`: `[llm].temperature` / `[llm].context_window` were the only manifest sources, filled field by field on first start by the `session_init` "Resolve & persist defaults" block.
- `max_output_tokens`: had **no** manifest source at all, while the same-named `[llm].max_tokens` travelled a **different** channel — `agent/context.rs` treated it as a request-level `max_tokens` **hard override**. One field name, two semantics, two channels.
- `max_iterations` / `compression_ratio_threshold`: came from `RuntimeConfig` or a code constant; the author could not express them.
- `session_language` / `search` / `chat_router` / `system_prompt_override`: user or platform domain, and **must not** be package-declarable.

### 1.3 MCP had three states and the package could touch none of them

`AgentMcpConfig { catalog, local, active_names }`: `catalog` is pushed by the Gateway, `local` is installed by the agent's own `mcp_install`, and `active_names = None` means "activate nothing".

The preset registry lived in the **frontend** (`apps/acowork-desktop/src/lib/mcp-presets.ts`); neither the Gateway nor the Runtime knew those ids. The only "package-tailored MCP tool subset" was therefore hardcoded in Rust:

```
PM_DEFAULT_ENABLED_TOOLS          agent_config.rs
default_enabled_tools_for("pm")   agent_config.rs
```

### 1.4 capability had thinned out

`[capabilities.*]` already participated in intent addressing (ADR-025), but **nothing declared what implements it**. The frontend actively propagated a wrong assumption: `CreateWizard.tsx` and `commands/create.rs` each mapped "every SKILL.md → one capability" 1:1, and the i18n copy said so ("each SKILL.md becomes a manifest capability").

### 1.5 skill's only source was the filesystem

`skills/<name>/SKILL.md` is the sole source of truth for a skill, with no place in the manifest — so "which skills implement this capability" and "which tools does this skill need" were **uncheckable at declaration level**.

### 1.6 Root cause

The declaration surface was built for what was enough at the time, so four kinds of knowledge (parameter defaults, MCP needs, implementation relations, attribute tags) live in code, frontend constants, and verbal agreement. The fix is therefore not "add fields" but **add fields + landing path + visible consequence + check point** — dropping any one of the four leaves decoration.

---

## 2. Decisions

| # | Decision | Conclusion |
|---|---|---|
| 1 | `[config]` whitelist | Only **behaviour knobs**, 5 of them: `temperature`, `context_window`, `max_output_tokens`, `max_iterations`, `compression_ratio_threshold`. **Excluded**: security boundaries (`shell_approval_threshold`, `approval_timeout_secs`), host resource quotas (`max_sessions`), identity preferences (`session_language`), user-owned overrides (`system_prompt_override`). A package must not lower an approval threshold or claim a user preference |
| 2 | `[config]` vs `[llm]` | `[config]` wins; when absent, fall back to `[llm].temperature` / `[llm].context_window` (existing packages keep working). The projection is `AgentManifest::config_defaults()` — **the single entry point** |
| 3 | Default write semantics | Fill only `None` fields; **never** overwrite a user value; idempotent (no write when nothing changed). `seed_agent_config_from_manifest()` and `AgentConfig::seed_from_manifest()` share `fill_if_empty()` |
| 4 | `max_output_tokens` semantics | A new, independent **ceiling** written into `agent_config.json`. `[llm].max_tokens`' request-level override semantics are **unchanged**, but `context.rs` now takes a `max_output_tokens_limit` parameter instead of reading the manifest — each semantics converges onto its own channel |
| 5 | `[[mcp_servers]]` granularity | **Server + tool-name whitelist.** No tool input schemas are copied — the MCP's own `tools/list` is the single source of truth |
| 6 | `[[mcp_servers]]` forms are mutually exclusive | `preset` (reference a platform preset) / `hosted` (reference a Gateway-hosted service: `pm`, `doc`, `memory`) / `transport` + `url` (inline remote). **Inline stdio is banned** — otherwise a package could name a command to execute on the user's machine while only claiming to "recommend" |
| 7 | Where a recommendation lands | `agent_mcp.json.recommended` (**never** `catalog` / `local` / `active_names`); ignored keys go to `dismissed`. A package upgrade re-projects its recommendations, and drops keys the package no longer declares from `dismissed` (otherwise a stale key would silence a future recommendation of the same name forever) |
| 8 | Where a recommended install lands | The **machine-wide MCP catalog**, carrying an `origin` field: `preset:<id>` / `agent-package:<agent_id>#<key>` / `user`. Rationale in §4.3 |
| 9 | Where the preset registry lives | Moved **into the Gateway** (`acowork-gateway/src/http/mcp_presets.rs`), exposed as `GET /api/mcp-presets` and `GET /api/mcp-presets/{id}`. A preset id is now a **package-visible identifier**, whose meaning cannot live in one client |
| 10 | capability ↔ skill | Many-to-many via `[capabilities.<action>].skills = ["<skill-name>"]`. A reference to a missing skill is an **error** (packaging and install both refuse); a skill `tool_dep` unreachable from the declarations or from a platform-hosted MCP is a **warning** (never blocks) |
| 11 | `labels` | Lowercase kebab, 2–32 characters, at most 12, deduplicated. Used for Agent-list filtering, marketplace attributes and grouping. **Not** involved in permissions, addressing or lifecycle |
| 12 | Single construction point | `ManifestDraft::build(skills)` is the **only** place a manifest is assembled; `create_agent(draft)` and `preview_manifest(draft)` both call it. The frontend no longer builds TOML |
| 13 | Install/packaging-time validation | Both `acowork-node` install and `acowork-sign` packaging call `validate_graph()`: errors refuse, warnings are logged |
| 14 | The hardcode retires | `PM_DEFAULT_ENABLED_TOOLS` / `default_enabled_tools_for("pm")` are deleted; the manifest's `[[mcp_servers]].tools` whitelist drives it instead |
| 15 | Who computes lifecycle state | The **Runtime** synthesizes the 4 states (`not_installed` / `installed_inactive` / `active` / `dismissed`); the client only renders them. Two clients recomputing the same join would eventually disagree |

---

## 3. Data structures

### 3.1 `[config]`

```toml
[config]
temperature = 0.5             # 0.0-2.0
context_window = 128000       # > 0 (0 is not a "no limit" sentinel, ADR-074 §6)
max_output_tokens = 8192      # > 0, per-call output ceiling
max_iterations = 40           # > 0
compression_ratio_threshold = 0.9   # 0.05-0.95 (ADR-061 "saved share")
```

Rust: `ManifestConfigDefaults`, every field `Option` with `skip_serializing_if`; `is_empty()` means the table is **not written at all** ("no opinion" is not the same statement as "explicit default"). `validate()` range-checks each field and names the offending one.

### 3.2 `[[mcp_servers]]`

```toml
# Form A: reference a platform preset (no executable content in the package)
[[mcp_servers]]
preset = "brave-search"
why = "fresh web results"
required_secrets = ["BRAVE_API_KEY"]
tools = ["brave_web_search", "brave_news_search"]
required = false

# Form B: reference a Gateway-hosted service
[[mcp_servers]]
hosted = "pm"
why = "tasks and projects"
required = true

# Form C: inline remote (in-house MCP; must be covered by a Network permission)
[[mcp_servers]]
name = "corp-kb"
transport = "http"
url = "https://mcp.corp.example/kb"
why = "internal knowledge base"
```

Rust: `McpServerDecl`; `key()` resolves the identity key (`name` ?? `preset` ?? `hosted`), `source_kind()` classifies the form, and `validate()` enforces **mutual exclusion**, restricts the inline form to `http`/`sse` with a covering `Network` permission, and rejects duplicate identity keys within one manifest.

### 3.3 `[capabilities.<action>].skills`

```toml
[capabilities.requirements_analysis]
description = "Analyze and document project requirements"
skills = ["requirements-analysis", "risk-assessment"]
```

Rust: `CapabilityDef.skills: Vec<String>`; `DraftCapability` is the author-facing form (it also carries the `action` table key so the wizard can rename in place).

### 3.4 `labels`

```toml
labels = ["project-management", "agile", "team"]
```

Rust: `AgentManifest.labels: Vec<String>`; `is_valid_label()` and the frontend's `manifestRules.isValidLabel()` are aligned character for character; `MAX_MANIFEST_LABELS = 12`.

### 3.5 Declaration-graph validation

`AgentManifest::validate_graph(&[SkillDecl]) -> ManifestGraphReport`:

1. Every `capabilities.*.skills` entry must resolve inside the package → **error**.
2. Every skill `tool_dep` must be reachable from `[[tools]]` ∪ `[[mcp_servers]].tools`, or belong to a platform-hosted MCP (`mcp_<server>__<tool>` with `server ∈ {memory, pm, doc}`), or belong to a server this package declares → otherwise **warning**.

The split is a **capability** boundary: the packager (`acowork-sign`) can list `skills/<dir>/` but cannot parse `SKILL.md` frontmatter (that parser lives in the Runtime, which `core` must not depend on). The capability→skill half is therefore checkable at every layer and is an error; the tool-reachability half needs frontmatter and is a warning.

---

## 4. Landing paths

### 4.1 `[config]` → `agent_config.json`

```
manifest.toml [config]
  └─ AgentManifest::config_defaults()        (single projection, includes the [llm] fallback)
      └─ AgentConfig::seed_from_manifest()   (fill_if_empty: only fills None)
          └─ agent_config.json
```

Two call sites: startup (`session_init`) and install/upgrade (`seed_agent_config_from_manifest`). Both go through `fill_if_empty()`, so "the manifest never overwrites a user value" is a property of **one function**, not of scattered `if`s.

### 4.2 `[[mcp_servers]]` → `agent_mcp.json.recommended`

```
manifest.toml [[mcp_servers]]
  └─ project_recommended_mcp_servers()   (re-projected on every agent start)
      └─ agent_mcp.json { recommended: [...], dismissed: [...] }
          └─ build_recommendation_views(cfg, installed, active)
              └─ the `recommended` field of GET /agents/{id}/tools
                  └─ the Tools panel's "recommended MCP" group
```

`recommended` is **derived state**: rewritten on every start, so a package upgrade refreshes its own recommendations, and `dismissed` is pruned against what the package currently declares. `catalog` / `local` / `active_names` belong to their own owners and this flow **does not touch them**.

### 4.3 Where a recommended install lands: the catalog, not `local` (Decision 8 expanded)

| Destination | Consequence | Verdict |
|---|---|---|
| `agent_mcp.json.local` | Private to the agent; lost on upgrade/reinstall; N agents wanting the same MCP produce N spawn configs and N copies of the key | ✗ |
| User-level `mcp-catalog.json` **without** a source field | Servers appear out of nowhere elsewhere; "who installed this?" is unanswerable | ✗ |
| User-level `mcp-catalog.json` **with** `origin` | Reuses the existing ADR-090 install pipeline (probe/install/secrets in one place); the source is answerable; other agents *see* it but never auto-activate it | ✓ |

The third option is the decision. `origin` is written by the **Gateway** (clients never format it), with three values: `preset:<id>`, `agent-package:<agent_id>#<key>`, `user`.

**Idempotency**: if the name already exists the endpoint returns `200` and **overwrites nothing** — in particular not `env`. The user's API key is the one field a package upgrade must never reset, and a package re-runs its recommendation on every start.

### 4.4 Lifecycle state (synthesized by the Runtime, rendered by clients)

Precedence: **dismissed > active > installed > not_installed**.

Dismissal outranks everything because it is the only *user* intent in the list: an installation or activation that happened earlier must not silently resurrect a suggestion the user explicitly ignored.

| state | the one primary action |
|---|---|
| `not_installed` | install (materialize into the catalog) |
| `installed_inactive` | activate for this agent |
| `active` | none — the tools are already there |
| `dismissed` | restore (behind the "N ignored" disclosure) |

### 4.5 Dismiss / restore

`PUT /api/agents/{id}/mcp-recommendations/{key}` (the Gateway reverse-proxies it to the Runtime). Dismissal is a **persistent write**, not a local hide — it is consistent across machines and clients.

---

## 5. User-visible behaviour

### 5.1 The four real jobs of a recommended MCP (the admission criterion)

1. **Visible**: a "recommended MCP" group at the top of the Tools panel renders the author's `why`, the required secrets, and the tool names it would add.
2. **Actionable in one click**: `not_installed` calls `POST /api/mcp-catalog/from-declaration` (the Gateway owns the recipe and the `origin`; the client builds neither).
3. **It really changes the tool set**: `installed_inactive` → activate goes through the **same** `PUT /mcp-servers` path the ordinary switch uses, so the app has exactly one activation mechanism.
4. **The gap is visible**: a missing secret names `BRAVE_API_KEY` and points at the only place that fixes it.

An empty `recommended` renders **no group at all** (no "nothing recommended" placeholder).

### 5.2 The install-destination notice (mandatory)

A server installed from that group enters the **machine-wide MCP catalog**, so the card must say: "Added to this machine's MCP catalog; other agents can see it, but it is only active for this one." Without it, users find "servers that appeared out of nowhere" in another agent's Tools panel.

### 5.3 Agent-list badge

Shown only when a `required = true` entry has a state ∉ {`active`, `dismissed`}. `dismissed` does not count as unmet (the user already refused); when everything is satisfied the badge is **absent** (no "healthy" noise).

### 5.4 Create Agent wizard: 4 steps → 5 steps

| Step | Declares | May advance when |
|---|---|---|
| 1 Basic | `agent_id`, `name`, `display_name`, `role`, `version`, `author`, `description`, **`labels`** | reverse-domain id + non-empty name + ≤12 labels |
| 2 Model & params | the five `[config]` fields | every filled field is in range (all-empty is legal) |
| 3 Tools & MCP | `[[tools]]` + `[[mcp_servers]]` | always (everything here is optional) |
| 4 Skills & capabilities | skill ZIPs + `[capabilities.*]` (action / description / skills / optional JSON schema) | every ZIP parsed, action names valid and unique, JSON valid |
| 5 Preview | — | the backend reports no error |

Two structural constraints:

- **The preview is no longer hand-built.** The old `buildPreviewText()` assembled TOML in the frontend and began with a `[package]` table that does not exist in `AgentManifest` (serde silently drops unknown fields, so a manifest built that way would have lost everything the preview promised). The preview now calls `preview_manifest(draft)`, sharing `ManifestDraft::build` with `create_agent(draft)` — **drift is impossible by construction**.
- **`[[mcp_servers]].tools` is not collected in the wizard.** The server is not connected, so `tools/list` is unavailable; asking the user to type tool names would only produce fake declarations. The subset is picked in the Tools panel after installation.

### 5.5 Skill vs capability (stated in the UI)

Step 4 must say: "A skill is an internal workflow and is not exposed; declare outward-facing actions in the capabilities section below." The old copy ("each SKILL.md becomes a manifest capability") carried wrong knowledge and has been rewritten.

---

## 6. Rejected alternatives

| Alternative | Why rejected |
|---|---|
| Keep the frontend preset table and only add a `preset` string to the package | Once a preset id is a package-visible identifier, its meaning cannot live in one client; a second client, Gateway-side validation and the install pipeline all need to resolve it |
| An inline stdio form | It would let a package name an executable to run on the user's machine while only claiming to "recommend" — contradicting "a package carries no executable content" |
| Write recommendations straight into `local` | See §4.3: private, easily lost, duplicated spawn configs and secrets |
| Write recommendations into `active_names` | It would turn "the author suggests" into "silently enabled", bypassing the ADR-090 user confirmation |
| Keep capability ↔ skill at 1:1 | It is factually wrong (one capability is often implemented by several skills) and mistakes an internal workflow for an outward contract |
| Let `[config]` declare security thresholds | A package would gain the ability to lower an approval gate — a privilege escalation |
| Let the frontend compute recommendation state | Two clients joining the same data would eventually disagree; a user would see "installed" here and "install" there |
| Reuse `[llm].max_tokens` for `max_output_tokens` | The semantics differ (request-level override vs config-level ceiling); reusing the field would re-merge two channels |

---

## 7. Test boundaries

| Layer | What is asserted |
|---|---|
| `acowork-core` | the `ManifestConfigDefaults` range matrix; mutual exclusion of the three forms and rejection of inline stdio; `labels` boundaries (2/32/12/dedupe/case); the `validate_graph` error/warning split (including platform-hosted reachability); `config_defaults()` `[config]`-first + `[llm]` fallback; the `ManifestDraft::build` mapping matrix and "preview equals create" |
| `acowork-runtime` | seed idempotency and never-overwrite; `recommended` projection leaving `catalog`/`local`/`active_names` untouched; 4-state precedence; dismiss/restore persistence |
| `acowork-gateway` | the preset registry's HTTP projection; `from-declaration`'s three `origin` values, idempotency, and `env` never being reset; `labels` surfacing in the agent list/detail |
| `acowork-node` / `acowork-sign` | install/packaging refusing a dangling capability→skill edge; warnings never blocking |
| Desktop | one case per recommendation state plus both failure paths; every wizard step gate; **the draft sent to preview and to create being field-for-field identical**; the tag input's commit/reject boundaries |

The frontend additionally follows a "mirror is a subset" rule: a client-side rule may be **stricter** than Rust (reject more) but never looser — over-rejection only annoys, while under-rejection produces a dialog that says "fine" and then fails at create.

---

## 8. Risk and rollback

| Risk | Mitigation |
|---|---|
| `[config]` and `[llm]` both present → ambiguous behaviour | `config_defaults()` is the only projection and `[config]`-first is pinned by a unit test |
| Renaming a preset id breaks published packages | `id` is an API contract: add an alias, never rename (stated in the registry) |
| Re-projecting recommendations on every start writes to disk | `project_recommended_mcp_servers` returns `false` and skips the write when nothing changed |
| A package upgrade "resurrecting" a dismissed suggestion | `dismissed` is pruned against what the package declares, but a dismissed key the package still declares is kept |
| The recommendation group becoming noise | an empty list renders nothing; once fully handled it is collapsed by default (the header carries the count) |

Rollback: `[config]`, `labels` and `skills` are all **optional** fields that old clients ignore through serde; clearing `[[mcp_servers]]` returns to "no recommendations". The four declarations are independent and can be reverted in stages.

---

## 9. Open / deferred

1. Collecting `[[mcp_servers]].tools` **inside the wizard** (needs a connected server — the chicken-and-egg of "declare after install").
2. **Cross-client consistency** of the recommendation group (the mobile form was not designed together with this ADR).
3. Whether `[config]` should extend to **platform policy** fields such as `search` / `chat_router`. Currently judged user/platform domain; revisit if a concrete need appears.

## 10. Where It Landed

| Layer | Files |
|---|---|
| core | `core/acowork-core/src/manifest.rs` (the four declarations + `validate` + `validate_graph` + `config_defaults` + `ManifestDraft`), `core/acowork-core/tests/example_manifests.rs` |
| runtime | `agent_config.rs` (seeding / `recommended` projection / removal of `PM_DEFAULT_ENABLED_TOOLS`), `usecases/mcp_recommendation.rs` (state derivation), `agent/agent_core.rs`, `agent/context.rs` |
| gateway | `http/mcp_presets.rs` (registry lifted out of the client), `http/mcp_catalog_api.rs` (`from-declaration`), `http/agents.rs` (`labels`) |
| node / sign | `package/install.rs`, `packager.rs` (packaging / install-time graph validation) |
| desktop | `lib/mcpPresets.ts`, `components/right-panel/McpRecommendationGroup.tsx`, `components/agent-list/CreateWizard.tsx`, `components/common/TagInput.tsx`, `lib/createWizardDraft.ts`, `lib/manifestRules.ts`, `src-tauri/src/commands/create.rs` |
| examples | all seven `examples/*/manifest.toml` now carry `labels` / `[config]` / `[[mcp_servers]]` / `capabilities.*.skills`, pinned as a regression corpus by `example_manifests.rs` |

`examples/agent-packages/*.agent` are build artifacts and were not repackaged here (the next `dev/common/build-agent` run picks the new sections up).

---

