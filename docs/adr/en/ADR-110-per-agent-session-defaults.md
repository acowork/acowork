# ADR-110: Per-agent New-session Defaults (default model / default reasoning effort)

- **Number**: ADR-110
- **Status**: Implemented
- **Date**: 2026-11-08
- **Deciders**: Nicholas (requirements and approach)
- **Related**: ADR-012 (model selection is per-session), ADR-010 (context compression), ADR-099 (the Chat Router's per-agent model fields), ADR-101 (memory service boundary), ADR-108 (right-rail consolidation), ADR-109 (per-session tool narrowing)
- **Chinese counterpart (source of truth)**: [docs/adr/zh/ADR-110-per-agent-session-defaults.md](../zh/ADR-110-per-agent-session-defaults.md) (maintained in the same commit)

---

## Summary

**In one sentence**: `agent_config.json` gains `default_model` / `default_provider` / `default_reasoning_effort` — a **seed for new sessions**. Creating a session uses them as the initial model / provider / reasoning effort, replacing the previous "inherit whatever the last active session used" rule.

**In one sentence (why)**: A user's expectation of an agent is stable ("this agent runs Opus at high"), while "the value used last time" is an **implicit global**: one session spent trying another model pollutes every session created afterwards, and no UI can set that state back explicitly. Making the initial value explicit swaps an implicit inheritance for an explicit default, without touching ADR-012 — the session's own meta is still authoritative; the per-agent fields only decide **what it is born with**.

---

## Background (evidence from the code)

1. **Per-agent persistence already exists with a single write entry point.** `AgentConfig` (`core/acowork-runtime/src/agent_config.rs`) serialises to `{work_dir}/config/agent_config.json`, already holding `max_output_tokens` / `temperature` / `context_window` / `session_language` / `chat_router`. Its only write path is `PUT /agents/{id}/config` → `RuntimeAgentConfigService::put_config` (`usecases/agent_config_impl.rs`, whose module docs call itself the "single audit point" for this file on the HTTP path). New fields need no new storage.
2. **The per-session store of record is session meta, not this file.** `ConversationSession`'s JSONL meta holds that session's model / provider / reasoning_effort. ADR-012 is therefore untouched: the per-agent fields are **never** written into an existing session.
3. **New-session initialisation has one convergence point.** `SessionManager::build_initial_session_state` (`src/agent/session/session_manager.rs`) falls back to `current_model_and_provider()` when the session has no model/provider of its own; that function returns the most recently activated session's `(model, provider)` pair, or `global_provider_list.first()`. That is the implicit inheritance this ADR replaces.
4. **`reasoning_effort` resolution was hand-copied in two places — and the two copies are currently equivalent.** `build_initial_session_state` spelled out a four-layer chain (persisted → caps.default → Auto when reasoning-capable → `None`), while the HTTP/MQTT readers used the shared `session_config::llm_effects::resolve_effective_reasoning_effort`. Layer for layer they agree today (**verified; there is no behavioural drift**). This ADR routes the new layer through the shared implementation only to remove the risk of a future change being made on one side — it is not a bug fix.
5. **`RuntimeConfigOverrides` cannot express "cleared".** That per-agent in-memory cache serves **hot-reload broadcasts to live sessions** and means "`Some` replaces / `None` preserves", so it cannot say "the user deleted this default". A default must be able to disappear on the next session creation without a restart, so it stays out of that chain (see D3).
6. **`put_config`'s read-modify-write was unserialised across threads.** The PUT DTO is a partial body, and "pick a model" inevitably writes two fields (`default_model` + `default_provider`). Without serialisation each request loads the old file, merges its own half and saves the whole struct, so whichever finishes last discards the other's half. Measured: with the lock swapped for `try_lock`, a concurrent pair write loses its provider half on round 0 (see D4's regression test). ADR-099's router model/provider pair has the same shape and the same pre-existing defect.

---

## Decision

### D1 — Three fields in `agent_config.json`, meaning "new-session initial value"

```jsonc
{
  "default_model": "claude-sonnet-4-6",      // model id (ProviderModelEntry::id)
  "default_provider": "anthropic",           // provider id, paired with the model
  "default_reasoning_effort": "high"         // same spellings as the composer's effort menu
}
```

All three are `Option<String>` with `skip_serializing_if = "Option::is_none"`: **no key = no opinion**. `default_provider` is its own field rather than a folded `provider::model` string because the same model id can be served by two configured providers — the pair is what makes the choice unambiguous; with only `default_model` set, the runtime picks the first provider in the inventory that exposes that id (see D2).

### D2 — Initialisation chain: per-agent default → the old implicit inheritance

`model` / `provider` (the first layer that yields a model wins; the provider always comes from the same layer as the model, so a model never pairs with a provider from another layer):

```
1. agent_config.json default_model (+ default_provider; when absent, the first provider in the inventory serving that model)
2. current_model_and_provider()   <- the only pre-ADR-110 behaviour, kept as the fallback
```

`reasoning_effort` is resolved by the one implementation, `resolve_effective_reasoning_effort`:

```
1. the session meta's persisted value
2. agent_config.json default_reasoning_effort        <- layer added by this ADR
3. the model capability's default_reasoning_effort
4. Auto (reasoning-capable model with no declared default)
5. None (model cannot control reasoning -> the composer hides the control)
```

Setting `default_provider` without `default_model` is **ignored**: a provider alone is not a selectable state in the UI, and picking its first model for it would silently override the most-recently-used choice — worse than ignoring it.

The resolved values are written back into the new session's meta (`update_reasoning_effort` / model / provider), because **session meta remains authoritative**: the default is what the session received at birth, and later edits to the per-agent default do not affect any existing session.

### D3 — Not projected into `RuntimeConfigOverrides`: a seed, not an override

The per-agent defaults are **not** projected onto the hot-reload broadcast, and `AgentSessionDefaults::load` re-reads them from disk on each session creation (one small file read). Both reasons are necessary:

- `RuntimeConfigOverrides` means "`Some` replaces / `None` preserves" and cannot express "cleared"; using it as a cache would keep a deleted default alive until the next restart;
- broadcasting to live sessions is the **wrong semantics** — the default only affects the next session, and a change must not disturb a running one (ADR-012).

`test_agent_defaults_are_not_live_overrides` pins the invariant (the projection must be empty and no value may appear on the broadcast wire).

### D4 — A lock on `put_config`'s load-merge-save (at the single audit point)

`RuntimeAgentConfigService` holds a `std::sync::Mutex<()>` for the whole of `put_config`. `std::sync` rather than `tokio::sync` is deliberate: the critical section has no await point, so the guard can never be held across a suspension.

This exists **so that D1 can actually land**: two of its three fields must be written in one user action (model + provider), and a partial PUT's pair writes have to compose — otherwise the user's default model silently loses half of itself. Fixing it at the audit point, instead of making every client order its own requests, also fixes the same defect in ADR-099's router model/provider pair. The regression test `concurrent_partial_puts_of_a_coupled_pair_both_persist` runs 25 rounds of concurrent pair writes — remove the lock and it fails on round 0.

### D5 — UI option lists share the composer's source, differing only in scope

The Setup panel's "Model Parameters" card gains two dropdowns (default model / default reasoning effort):

- **Model**: options come from `chatStore.availableModels` (the same vault-derived list the composer's `ModelMenu` renders), the value is `provider::model` and the label `providerName · model`; `""` is "Auto (inherit last used)".
- **Reasoning effort**: fixed `Auto` / `Off` entries plus `reasoningLevelsFor(the model's reasoning_efforts)` — the same helper and the same order as the composer's effort menu. When the selected model changes and the old level is not in its declared set, the old value stays as an option so the dropdown never renders blank.

Both write through the existing per-field debounced write path (`saveField`); D4's lock is what makes the pair land consistently.

---

## Consequences

- **Positive**: an agent's "default behaviour" goes from implicit to explicit, editable and clearable; clearing takes effect on the next session with no restart; the three reasoning_effort resolution sites collapse into one implementation, removing an existing drift; paired partial writes to `agent_config.json` no longer overwrite each other.
- **Negative**: every new session pays one small file read (`agent_config.json`, a few KB, already in the OS page cache); the file gains three fields, and files written before this ADR still parse (`serde` default + `#[serde(default)]`).
- **Invariants**:
  - A session that already **explicitly** carries model / provider / effort is unaffected by the per-agent defaults — a default fills only the half the session has no opinion about, and it **never** replaces a model the caller passed explicitly (that replacement was a defect of this implementation, now pinned by step 5 of `new_session_is_seeded_from_agent_defaults_not_last_used`).
  - A session that never had any effort value (including sessions created before this ADR) resolves through the new chain on first open and **pins the result into its own meta**, staying stable from then on. A default can therefore change a session's *effective* effort as the session is first materialised; once a session holds its own value it stops following the default.
  - Session meta remains the sole authority for model / provider / effort (ADR-012 is not weakened).

## Tests

- `session_manager::tests::new_session_is_seeded_from_agent_defaults_not_last_used` — a new session uses the default; after switching models elsewhere, a new session still uses the default; after clearing the default it falls back to inheriting the most recently active session.
- `agent_config::tests::load_reads_persisted_config_and_survives_a_corrupt_file` — persist / read back / degrade to empty defaults on a corrupt file.
- `agent_config_impl::tests::test_agent_default_field_patches` — Set / Clear / empty string (equivalent to clearing) / effort lowercased.
- `agent_config_impl::tests::test_agent_defaults_are_not_live_overrides` — not projected onto the hot-reload broadcast.
- `agent_config_impl::tests::concurrent_partial_puts_of_a_coupled_pair_both_persist` — concurrent paired partial PUTs lose no half (D4).
