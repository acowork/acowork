# ADR-111: One Agent, One Name, One File

- **Number**: ADR-111
- **Status**: Implemented
- **Date**: 2026-11-09
- **Decision maker**: Nicholas (requirements and approach)
- **Related**: ADR-073 (instance identity decomposition — this ADR supersedes its "rename through `display_name`" and `overrides`-file clauses), ADR-104 (manifest declaration surface — this ADR removes `display_name` from it), ADR-101 (memory service boundary — this ADR makes bootstrap a converging sync), ADR-009 (Gateway boundary — `install_path` is node-local, so identity must be projected by the Node, never read by the Gateway)
- **Chinese source of truth**: [docs/adr/zh/ADR-111-one-agent-one-name-one-file.md](../zh/ADR-111-one-agent-one-name-one-file.md) (maintained in the same commit)

---

## Summary

**In one sentence**: an agent instance has **exactly one name** — `name` — living in the instance's own `workspace/config/agent_config.json`; `manifest.toml`'s `display_name` and `{instance_id}.overrides.json` are both deleted.

**In one sentence (why)**: `name` and `display_name` were each read as "the real name" by a different layer, so **the name a human sees and the name the agent sees for itself in context were not the same string** — which breaks the platform's core capability, equal communication between humans and agents. A name is not a display preference, it is identity: one field, one writer, one resolution site.

---

## Background (evidence)

1. **Two name fields, three independent derivations.** Before this ADR: `manifest.name` (package name), `manifest.display_name` (a package author's display alias), and `{instance_id}.overrides.json`'s `display_name` (the user's rename). The sidebar, the agent detail panel and PM task assignment each implemented their own `overrides ?? display_name ?? name` cascade — and the PM side (`AgentInfo`) had no such field at all, so `pm_list_agents` could not return any display name. **One read, three derivation chains**: a direct product of having two names.

2. **`overrides.json` was an extra file in the wrong place.** `acowork_core::agent_overrides::overrides_path` built it **outside** the install directory, separated from the semantics it carried (one item on the right-hand agent settings panel): every other setting on that panel (model, temperature, language) is written to `workspace/config/agent_config.json`; only avatar and name were the exception. There was no technical need for it — `AgentConfig` already is that panel's persistence.

3. **`manifest.display_name` was byte-identical to `name` in all 7 shipped packages.** Checked one by one (`agents/*/manifest.toml`): `display_name = "..."` equalled `name = "..."` exactly, so deleting it loses no information. Its only possible meaning — "the package author's nickname for the package" — was never consumed; the one place that read it used it as an instance's initial name, which is `name`'s job.

4. **`agent_config.json` already lives inside `workspace/`, which an upgrade must not touch.** `acowork-node`'s upgrade retention set was `data/` + `config/`, while `workspace/` is the Runtime's work dir (holding `workspace/config/agent_config.json`, sessions, the private SQLite store). Keeping `data/` + `config/` while **deleting** `workspace/` was an existing defect (it silently wiped every session); this ADR fixes it to retain `workspace/`.

5. **The memory service's seed-once bootstrap made a rename self-inconsistent.** The idempotency test in `POST /admin/bootstrap` was "this namespace already has an Identity node, skip everything". The `name` fact is declared by the Runtime on every start (value taken from `agent_config.json`), but after the first seed it was never refreshed — so after a user renamed `SSE` to `SSE.Kevin`, the stored identity node still said `SSE`: a third name drift.

6. **The Gateway cannot read identity files.** `install_path` is node-local (the fs red line in ADR-009 §5), so only the Node can read a remote instance's identity. Identity therefore has to travel **upward as a projection**: `InstalledAgentInfo.identity` (`AgentIdentity { name, avatar, builtin_avatar }`) replaces the old `overrides_json` (a JSON string inside the payload that the Gateway then had to parse itself).

---

## Decisions

### D1 — One instance, one name

`name` is both the name humans see and the name the agent uses for itself. There is no longer a `display_name`, an `alias`, or an `overrides.display_name` layer. The criterion is testable: **any two exits (sidebar / agent detail / PM assignment / group-chat attribution / the agent's own prompt / the memory identity node) must return the same string for the same instance.**

### D2 — The identity triple lives in `agent_config.json`

`AgentConfig` gains `name` / `avatar` / `builtin_avatar`, in the same file and the same read-modify-write as every other panel setting. `{instance_id}.overrides.json` is deleted (the whole `acowork_core::agent_overrides` module goes away; only `AVATAR_EXTENSIONS` / `has_avatar_extension` survive, as `acowork_core::avatar`).

The semantics of `name` are stated in the field's own docs: `None` means **not materialized yet** (the instance has never booted); the first start materializes it from the package name (`startup::session_init`). After that **only an explicit rename writes it** — an upgrade does not, because the file lives under `workspace/` and the package cannot reach it.

### D3 — The manifest declares the package only

`AgentManifest.display_name` is removed, along with `ManifestDraft`'s counterpart. `manifest.name` narrows to **package name = the initial value a new instance is materialized from**, no longer "some instance's name".

### D4 — Precedence: config → manifest, resolved in exactly one place

```
name  = agent_config.name            // user choice / materialized value, authoritative
      ?? manifest.name               // pre-materialization default (= what first boot writes)
avatar= agent_config.avatar|builtin_avatar    // user pick, authoritative
      ?? manifest.avatar|builtin_avatar       // package default
      ?? client-side deterministic fallback   // render fallback
```

Resolution is single-sited: on the Runtime side `http/avatar.rs::effective_config`, on the Gateway side `http/agents.rs::resolve_identity` (`ResolvedIdentity`). **Callers must not re-derive** — a second derivation site is exactly how the sidebar and the detail panel came to disagree.

### D5 — The upward projection: `identity` replaces `overrides_json`

`InstalledAgentInfo.overrides_json` (field 7) is replaced by `identity` (a new `AgentIdentity` message with `name` / `avatar` / `builtin_avatar`). The Node builds it from the instance's `agent_config.json` (`acowork-node::package::read_identity`) and the Gateway drops it straight into its instance view without parsing a string. `AgentListResponse` / `AgentDetailResponse` expose `name` (instance name) + `package_name` (package name) as two mutually exclusive fields that never fall back to each other. PM's `AgentInfo` mirrors that shape, and `assignee_meta` / `created_by_meta` project the same two fields.

### D6 — The name reaches the runtime: `AgentCore::instance_name`

A new `AgentCore.instance_name: Arc<RwLock<String>>` (session clones share the lock; regression test `an_instance_name_is_shared_across_session_clones`). All three consumers read it instead of re-opening a file:

| Consumer | Purpose |
|---|---|
| `chat_router` Block A | `{{agent_name}}` substitution (the chat-layer prompt) |
| `bootstrap_autobio` | the `name` fact declared to the memory service |
| `PUT /agents/{id}/avatar-config` | **hot update** after a rename — otherwise the sidebar changes at once while the agent's own prompt waits for a restart (two names, now separated in time) |

An empty name is rejected: `set_instance_name` ignores blank input and `PUT` answers 400 for `name: ""` ("an agent always has a name").

### D7 — Memory bootstrap converges rather than seeding once

`POST /admin/bootstrap` converges every declared key individually:

- key absent → create (`source = "manifest"`);
- present with `source = "manifest"` and a different value → **update in place**;
- present with `source = "manifest"` and the same value → write nothing (do not bump `updated_at`);
- present with any other `source` (the distiller's `self_evaluation`, a legacy import) → **leave it alone**: the declaration is a default, the observation is a finding;
- a key the caller stops declaring → **not deleted**: bootstrap owns the keys it declares, not the meaning of their absence.

The package's `[memory]` policy is still applied **only on first import** (once the user has touched the panel, `consolidation.json` is authoritative).

### D8 — Upgrades preserve `workspace/`

`acowork-node`'s upgrade retention set: `workspace/` is preserved, everything else is replaced (regression test `upgrade_preserves_instance_workspace`).

### D9 — No compatibility code

The project is not released. Existing `*.overrides.json` files in install directories and `display_name` lines in `manifest.toml` were cleaned up **by hand**; no read branch and no migrator survives in the code. Verified: 0 `*.overrides.json` remain in the live install tree, 5 instances' `agent_config.json` already carry `name` / `avatar`, and the `display_name` lines are gone from all 7 shipped packages.

---

## Update ordering (writers and sequence)

A rename is a cross-process fact change with a fixed order:

1. **UI** → `PUT /agents/{id}/avatar-config` (the body carries only the fields to change, with tri-state semantics: absent / `""` clears / a value);
2. **Runtime**: write `workspace/config/agent_config.json` (the single durable writer), then `core.set_instance_name(...)` (effective immediately inside the process);
3. **Runtime** → Node: `republish_installed_info` re-sends the retained inventory carrying the new `identity`;
4. **Gateway**: `upsert_installed_from_node` overwrites the instance view from `identity`, so the list, the detail panel and PM projections all see the new name;
5. **Next start** (or the same one): `bootstrap_autobio` declares the new `name` to the memory service and D7's convergence refreshes the identity node in place.

No step can leave two names coexisting: the file is authoritative and everything else is its projection, so the next event replays it.

---

## Consequences

**Positive**

- The name has exactly one chain: `agent_config.json → AgentCore.instance_name / the Node's identity projection → Gateway → PM / desktop → the memory identity node`. Consistency is verified by "every exit reads the same value".
- The panel's avatar and name share a file and a write path with every other setting: one file fewer, one path-derivation function fewer.
- `name` + `package_name` on `AgentListResponse` / `AgentInfo` distinguish several instances of one package at the data layer, instead of guessing from a display label.
- An upgrade can no longer destroy an instance's sessions or configuration.

**Costs / known boundaries**

- **The execution session has no `{{agent_name}}` channel.** The body of `prompts/system.md` enters the execution session's system prompt verbatim; only the chat layer (Block A) substitutes the placeholder. All 7 shipped packages declare only a **role** in `system.md` ("a Document Manager"), never an instance name, so today's state is consistent — but a package author who writes a literal name reintroduces the drift. The rule is: **package content must not contain an instance name**; use `{{agent_name}}` (chat layer) or read the memory identity node. Generalizing substitution into the execution session needs a per-turn render hook that interacts with block A's cached-prefix strategy, so it is a separate piece of work.
- **`{{agent_name}}` covers the chat layer only** (a group chat's session-scoped template likewise); both substitution sites live in `chat_router`, not on the execution path.
- ADR-073's "rename via the manifest's `display_name`" clause and its `overrides`-file clauses stop applying as of this ADR (see below).

---

## Superseded decisions

| Old decision | Where | Status |
|---|---|---|
| Rename through `manifest.display_name`; no instance alias | ADR-073 §1.3 / rename flow | **Void**: the manifest has no such field; a rename writes `agent_config.json`'s `name` |
| `InstalledAgentInfo` carries `overrides_json` | ADR-073 impact list · proto | **Void**: replaced by `identity: AgentIdentity` |
| `{instance_id}.overrides.json` as the instance overlay | ADR-073 install-directory layout | **Void**: file and module deleted |
| `display_name` among the manifest's basic fields | ADR-104 field-tier table | **Void**: field removed |
| `POST /admin/bootstrap` seeds once | ADR-101 P1e (write entrypoint D) | **Corrected**: converging sync (D7) |

The entrypoint-D row in `docs/memory-write-entrypoints.md` has been restated accordingly.

---

## Verification

- `cargo test`: core 387 / pm 133 / node 210 / runtime 1961 / the full gateway suite / the full memory suite, all green. New regressions: `an_instance_name_is_shared_across_session_clones`, `upgrade_preserves_instance_workspace`, and memory's `a_rename_converges_the_declared_name`, `a_fact_from_another_source_survives_the_sync`, `an_unchanged_fact_is_not_rewritten`, `a_rename_rewrites_the_identity_fact_in_place`.
- `cargo clippy --all-targets -- -D warnings`: clean across the six crates touched.
- Frontend `tsc --noEmit` clean, `vitest` 311 files / 3070 cases green, `check-i18n` OK.
- Live: `pm_list_agents` returns `name` (instance name) + `package_name` for all 10 instances; `pm_get_project.members` matches; `pm_list_my_tasks`'s `assignee_meta.name` = `SSE.Cassie`.
