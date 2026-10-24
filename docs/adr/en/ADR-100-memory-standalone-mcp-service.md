# ADR-100: Extracting the Memory Module from the Runtime into a Replaceable Standalone Service acowork-memory (MCP Integration)

**Status**: Accepted (2026-10-23; the two key forks — service scope = full migration, auto-inject = removed — are settled by the user, P1–P5 are implemented, and this ADR is settled by the P5 documentation closeout)
**Date**: 2026-10-23
**Deciders**: the user (settled direction: memory becomes an MCP service shaped like pm/doc; closing the official service also closes the memory panel)

**Related**:
- [ADR-064](./ADR-064-pm-standalone-process.md), [ADR-070](./ADR-070-doc-standalone-process-and-tree-storage.md), [ADR-084](./ADR-084-user-standalone-process.md) (the service-ization trilogy — this ADR is the fourth application of the same paradigm)
- [ADR-051](./ADR-051-runtime-memory-provider-decoupling.md) (the predecessor "Runtime only knows the MemoryProvider trait" — this ADR carries it to its end state: memory logic leaves the Runtime entirely, **superseding its landing shape of "the provider lives in the Runtime process"**)
- [ADR-082](./ADR-082-memory-storage-sqlite-vector-fts.md) (SQLite storage backend — data model unchanged, **storage location moves from the agent workspace to the service data dir**)
- [ADR-068](./ADR-068-memory-layer-promotion-two-axis-orthogonal.md), [ADR-071](./ADR-071-distiller-runtime-config-and-trigger.md), [ADR-057](./ADR-057-compaction-distillation-into-graph.md), [ADR-062](./ADR-062-memory-quality-config-and-retrieval-gate.md) (distillation / forgetting / quality pipelines — migrate with the service)
- [ADR-073](./ADR-073-agent-instance-identity-decomposition.md) (`agent_instance_id` as the service namespace key)
- [ADR-099](./ADR-099-chat-session-llm-router.md) (the chat_router `memory_recall` information tool — its source becomes MCP)
- [ADR-081](./ADR-081-global-search.md) (global search memory source — now talks to the service directly)
- [ADR-069](./ADR-069-mcp-tool-level-optin.md), [ADR-072](./ADR-072-mcp-install-package-spec.md), [ADR-090](./ADR-090-mcp-lsp-install-ownership-node.md) (per-tool MCP opt-in, stdio/HTTP dual-transport install — the third-party memory MCP replacement channel)
- [ADR-009](./ADR-009-gateway-workspace-isolation.md) (the Gateway boundary red line — this ADR changes the carrier of "agent-private memory" while continuing the "the Gateway never reads it directly" rule)

---

## 1. Decision summary

### 1.1 In one sentence

**Move all memory logic (storage, retrieval, the distiller/forgetting background pipelines, the panel admin API) out of the Runtime process into a standalone `acowork-memory` process managed by the Gateway supervisor**: Agents reach it **only over MCP HTTP** (`memory_store` / `memory_recall`, injected through the same catalog channel as pm/doc), Desktop reaches it **only through the Gateway reverse proxy REST** (panel contract byte-stable). This buys replaceability: `[memory].enabled=false` → no spawn → the catalog does not inject the `id=memory` MCP → the panel hides in lockstep; the user may install any third-party memory MCP (stdio or HTTP) instead.

Key insight: unlike pm/doc, memory was never just a tool surface — it historically had side channels (auto-inject, compaction tail writes). Those converged — after the ADR-068 revision and auto-inject defaulting to OFF (post ADR-060) — down to "tools + background pipeline"; this ADR migrates both remaining pieces, leaving the Runtime with **zero memory business code**.

### 1.2 Key decision table (rationale in §4)

| # | Decision | Conclusion |
|---|---|---|
| 1 | Extraction boundary | **Migrated out**: the `memory_recall`/`memory_store` builtin tools, the distiller/forgetting/consolidation background pipeline (incl. LLM judge, metrics), the panel HTTP API (`/memory/nodes`, `/memory/stats`, `/memory/distill`, `/memory/consolidation/status`, node deletion, embedding-dim migration), the bootstrap manifest import (write entrypoint D), the global-search memory source. **Removed**: auto-inject (Decision 5); the `Permission::MemoryRead` / `Permission::MemoryWrite` variants in `acowork-core/src/permission.rs`, deleted together with their `PermissionTag` 3/4 encoding slots — the tags are **not renumbered**, the gap stays so every other tag keeps its historic byte value. **Kept in the Runtime**: only the catalog-driven thin integration (Decision 9) |
| 2 | Process paradigm | Clone of pm/doc/user: the `acowork-memory` crate goes from lib-only to **lib + bin**; default port `18084` (18080 embed / 18081 doc / 18082 pm / 18083 user are taken), conflict auto-increment (max +20), `--port-file` reporting; `memory_supervisor` (`/health` polling + exponential-backoff restart); a failed service never blocks the Gateway (panel API 503 + no MCP injection) |
| 3 | Identity & isolation | Namespace key = `agent_instance_id` (ADR-073: immutable UUID v4 generated at install), delivered via the Gateway catalog's `X-MCP-Actor: {instance_id}` template and resolved by the Runtime at connect time. **One DB per instance**: `{data_dir}/instances/{instance_id}/memory.db` (reuses the ADR-082 SqliteStore; the isolation granularity matches today's "one private store per agent") |
| 4 | Transport choice | The official service's MCP goes over **HTTP (stateless single `POST /mcp`, streamable subset)**, endpoint reverse-proxied by the Gateway at `/api/memory/mcp` — only HTTP simultaneously provides: advertise-endpoint reach for remote Agents, the `X-MCP-Actor`/`X-ACowork-Node-Token` identity headers, and one service backing both panel and tools. **Third-party replacements are transport-agnostic**: stdio and HTTP are both covered by existing acowork-mcp capability (`StdioTransport` + the ADR-072 installer); the open-source memory MCP ecosystem is predominantly stdio-shaped |
| 5 | auto-inject removed | The "retrieve-and-inject before every LLM call" capability is **deleted outright** (today default OFF, manifest `[memory.quality].auto_inject_enabled` opt-in). Rationale: MCP has no standard pre-call fetch hook; keeping it would force the Runtime to special-case a specific server id, contradicting the pluggability goal. Memory access converges on two active paths: the `mcp_memory__memory_recall` tool in execution sessions, and the router's information rounds (Decision 9). The manifest key is retired (deserialization tolerates it, no behavior) |
| 6 | Distiller LLM credentials | The service no longer piggybacks on any Agent's provider. The model candidate chain and prompts come from the service's own config (`{data_dir}/config.toml`); **credential resolution reuses the global-resource channel**: the service is an MQTT client subscribing to the Gateway broker's available providers (same pull semantics as the Runtime, ADR-065 lifecycle paradigm). Embedding calls acowork-embed (`:18080`, same as today's Tier 1 path) |
| 7 | Panel show/hide contract | The Gateway status endpoint exposes `memory_service: { ready, mcp_injected }` (⇔ `memory_mcp_url.is_some()`). Desktop rules: `ready=false` → `RightNavBar` hides the memory tile; an active memory tab bounces back to status; `GlobalSearchDialog` hides the memory tab. **With a third-party memory MCP online the panel stays hidden** — the panel speaks the acowork-memory REST contract and does not attempt to adapt to arbitrary MCPs |
| 8 | External contract | The panel path `/api/agents/{id}/memory/*` and its response **wire shape stay byte-stable** (the Gateway side switches from "reverse-proxy the Runtime" to "resolve the instance → reverse-proxy the memory service injecting `?instance_id=`"); a new service-direct prefix `/api/memory/*` (MCP + admin REST) is added. Desktop `memoryStore.ts` therefore needs near-zero changes |
| 9 | Residual Runtime integration (the only exception) | The Runtime checks whether the catalog contains a server with `id=memory`: when present, (a) on first start it `POST`s the manifest `[memory]` declaration section + bootstrap Autobiographical nodes (entrypoint D) to that service; (b) the chat_router resolves the memory recall tool from the tool registry by prefixed name (`mcp_memory__memory_recall`), mounting nothing that round if absent. Neither contains **any memory business logic**; third-party MCPs (different id) simply never trigger it |
| 10 | Data migration | One-shot, **triggered by the Node only** (holder of node-local fs rights, ADR-055; the Runtime is not involved at all and carries no migration-reporting channel): a background pass at daemon start plus the `acowork-node migrate-memory` operator retry, scanning each installed instance's `{workspace}/memory/private.sqlite`. Per instance: checkpoint the WAL and count memory rows → zero rows means write the sentinel and upload nothing → otherwise `POST {gateway}/api/memory/admin/import?instance_id=` carrying the **raw `application/x-sqlite3` bytes** (ceiling `STORE_IMPORT_MAX_BYTES` = 64 MiB, deliberately equal to the Gateway's `GLOBAL_BODY_LIMIT`; anything larger fails and is a manual job) → if the namespace already holds memories the service answers `skipped_existing` and **not one local row is deleted** (a non-empty namespace can just mean the manifest bootstrap wrote identity facts; deleting on that reading would destroy the only copy of the user's history) → only a real import is followed by a local DELETE of the memory rows (`purge_memory_rows`; the file stays — session meta and conversation index still belong to the Runtime, ADR-082) → write `{workspace}/memory/.migrated` recording the outcome. 503s and transport errors are a boot-order race, so they are retried on bounded rounds (6 × 5s, covering the Desktop startup order); every other error is recorded as a failed attempt without a sentinel, for `--force` once an operator has acted. The Gateway performs no fs reads (red line). No dual-write, no compatibility code (precedent: ADR-084 Decision 8) |
| 11 | Packaging invariant | Publishing already always excludes `memory/` (`PACKAGE_ALWAYS_EXCLUDE_DIRS`). Once service-ized, memory is physically outside the package dir — "a release carries no agent state" is promoted from an exclusion rule to a **structural invariant**; the five never-effective memory-subset fields in `PackageOptions` (`include_episodes` / `include_private_knowledge` / `include_procedural` / `include_autobiographical` / `include_public_knowledge`) are deleted with this ADR — the packager never read them, and `memory/` was already excluded wholesale. Backward compatibility comes free: serde ignores unknown fields. "Seed memories shipped with the package" (service export → install-time import) is explicitly **out of scope**, to be chartered separately |
| 12 | Rollback | `[memory].enabled=false` → no spawn → no MCP injection → panel hidden. The Runtime-side deletion is not config-rollback-able (version revert). Data side: for an instance that really imported, the local memory rows are gone and the service DB is the only copy (a revert means exporting from the service); `skipped_existing` answers and failed attempts leave the local rows and the file untouched, which is the natural not-yet-migrated escape hatch; a sentinel is crossed with `--force` |

### 1.3 Invariants (must hold)

1. **Zero memory business in the Runtime**: `core/acowork-runtime` MUST NOT contain storage/retrieval/distillation/forgetting logic; the only permitted memory-related code is the Decision 9 catalog-driven integration. The `acowork-runtime` Cargo dependency on `acowork-memory` is removed (`acowork-sqlite` stays — conversation index / session metadata still use it).
2. **The Gateway never reads memory data directly** (continues ADR-009 §5): panel data goes exclusively through `memory_proxy` to the service REST; the Gateway process MUST NOT open `instances/*.db`.
3. **The panel wire contract stays byte-stable**: no fields added, dropped, or renamed in `MemoryNode` / `MemoryStatsResponse` etc. (every Desktop consumer is unaffected across the switch).
4. **`instance_id` is the only memory namespace key**: same source on both sides (MCP via `X-MCP-Actor`, REST via proxy injection) — there is no second identity axis. The service MUST reject writes from missing/illegal actors (anonymous read-only policy as in pm/doc).
5. **Auth gate**: the service binds `127.0.0.1` and accepts only Gateway-proxied traffic (`X-ACowork-Node-Token` validated, enforced under `AUTH_MODE=multi_user`, ADR-076).
6. **The Node is the only party that hands over a legacy store**: `{workspace}/memory/private.sqlite` is node-local agent-private data, so the code reading it MUST live in `acowork-node` (ADR-055 owns the machine filesystem); the Runtime takes no part in migration (memory is no longer its subsystem) and the Gateway MUST NOT read it directly (ADR-009 §5). The single entry of the hand-over channel is the service's `/admin/import`.
7. **Tests follow the same boundary as the binaries**: the memory engine's and the Runtime's retrieval evals and integration tests MUST live in `core/acowork-memory/tests/` and `core/acowork-runtime/tests/` respectively; one harness MUST NOT seed or score the other engine's scope, and a query whose ground truth lies entirely in the other scope is skipped (counted in the `skipped` column, averaged out). The metric math, dataset reading, stand-in projection and report shape they share MUST live in `core/acowork-eval`, depended on as a `[dev-dependencies]` entry only — measurement code must not be linked into any shipped binary, and must not move back into `acowork-core` (which is a shipping dependency). `acowork-eval/tests/shipping_redline.rs` enforces this. Rationale: integration tests cannot import each other across crates, and re-deriving the metric definitions on both sides means one weight change gets judged by two different yardsticks.

### 1.4 Deployment shape, before vs after

| Dimension | Before | After |
|---|---|---|
| Agent memory tools | Builtin tools (in-process) | MCP tools `mcp_memory__memory_recall` / `mcp_memory__memory_store` |
| Memory storage location | `{install_path}/workspace/memory/private.sqlite` (node-local) | `{memory.data_dir}/instances/{instance_id}/memory.db` (service-side, Gateway host) |
| distiller / forgetting | Timers inside the Runtime + the Agent's provider | Scheduler inside the service + global-resource credentials |
| Panel data path | Desktop → GW `/api/agents/{id}/memory/*` → reverse proxy to Runtime localhost HTTP | Desktop → GW same path → reverse proxy to acowork-memory REST (instance injected) |
| Turning memory off | No switch (built-in capability) | `[memory].enabled=false`: MCP disappears + panel hides; a third-party memory MCP can replace it |
| Third-party memory MCP effect | Coexists with the builtin tools (dual channel) | The only memory channel; no panel, no distiller, no bootstrap import |

---

## 2. Background & motivation

### 2.1 The iron rule and the existing paradigm

The platform keeps converging on "business out of the core": embed, the LSP relay (ADR-019), PM (ADR-064), doc (ADR-070) and user (ADR-084) became Gateway-supervisor-managed processes one after another, each reaching Agents through the MCP catalog injection (`build_available_mcps`: `url.is_some()` ⇔ injected). **Memory is the last coherent business domain compiled into the Runtime** (ADR-051 only reached trait-level decoupling — the implementation still lived in the Runtime binary).

### 2.2 Current touchpoint inventory (the migration surface)

| # | Touchpoint | Files | Disposition |
|---|---|---|---|
| 1 | Builtin tools | [tools/builtin/memory_recall.rs](../../../core/acowork-runtime/src/tools/builtin/memory_recall.rs), [memory_store.rs](../../../core/acowork-runtime/src/tools/builtin/memory_store.rs) + `all_builtin_tools` registration + the two [acowork-core BUILTIN_TOOLS](../../../core/acowork-core/src/tools/mod.rs) rows | Delete → carried by the service MCP manifest |
| 2 | auto-inject | [agent/loop_memory.rs](../../../core/acowork-runtime/src/agent/loop_memory.rs), the `loop_context` call site, the `memory_retrieved_for_session` state | Delete (Decision 5) |
| 3 | Background pipeline | [runtime/src/memory/](../../../core/acowork-runtime/src/memory/) (`consolidation_bg`/`judge_llm`/`llm_adapter`/`metrics`) + `AgentCore`'s distiller/forgetting fields and `init_memory_provider` | Migrate into the service (Decision 6) |
| 4 | Panel HTTP API | [http/memory_query.rs](../../../core/acowork-runtime/src/http/memory_query.rs), the `/memory/*` handlers in server.rs, `usecases/memory_query*` | Migrate into the service REST (Decision 8) |
| 5 | chat_router information tool | the fixed name `memory_recall` in [chat_router/tool.rs](../../../core/acowork-runtime/src/agent/chat_router/tool.rs) | Resolve by prefixed name dynamically (Decision 9b) |
| 6 | Global-search memory scope | [http/global_search.rs](../../../core/acowork-runtime/src/http/global_search.rs) | Runtime returns `not_available`; the frontend memory tab hits `/api/memory/search` directly (same shape as the doc/project tabs, ADR-081) |
| 7 | Permission table | `MemoryRead`/`MemoryWrite` in [permission.rs](../../../core/acowork-core/src/permission.rs) (every touchpoint: `parse`/`tag`/`from_tag_simple`/`to_permission_string`/`matches`/`category`/`type_name`/`type_value`/serde), the mapping branch in [wasi_mapper.rs](../../../core/acowork-runtime/src/tools/wasm/wasi_mapper.rs), and the `[[permissions]]` declarations of the seven builtin example packages | Delete. Memory has no permission domain any more: the official service authenticates itself with `X-MCP-Actor` + `{node_token}`, and the tool-level switch lives in `agent_mcp_tools.json` (ADR-069), outside the permission system |

Write entrypoints (see [docs/memory-write-entrypoints.md](../../memory-write-entrypoints.md), ADR-068 revision) had already converged: C (compaction tail write) **was deleted on 2026-09-10** (the `write_summary_to_provider` method no longer exists — only comments remain), leaving A (`memory_store` tool), D (manifest bootstrap), E (admin API) and F (distiller) standing — this ADR lands A/D/E/F all in the service, leaving zero write channels in the Runtime.

### 2.3 Motivation

| Motivation | Note |
|---|---|
| Replaceability (the core ask) | Memory approaches evolve fast (the mem0 / mcp-memory-service / Basic Memory open-source ecosystem); built-in means swapping memory = changing Runtime code, MCP-ized means swapping = turn off the official service, install a third-party MCP |
| Runtime weight | The memory chain (memory / sqlite-consolidation / distiller / judge) is one of the largest business blocks in the Runtime binary; after extraction the Runtime converges on loop + tool dispatch + provider adapters |
| Fault isolation | A panic in the distillation pipeline (LLM calls, SQLite migration) no longer threatens the Agent's session process |
| Closing the user requirement | "memory off → panel gone" reuses the pm/doc `url.is_some()` switch semantics plus the Decision 7 frontend gate — no new mechanism |

---

## 3. Protocol surface

### 3.1 MCP (Agent-facing)

Single `POST /mcp`, the stateless JSON-RPC 2.0 subset, aligned verbatim with the [acowork-doc MCP](../../../core/acowork-doc/src/mcp/mod.rs): `initialize` / `notifications/initialized` / `tools/list` / `tools/call`. Identity: `X-MCP-Actor` = instance UUID; `X-ACowork-Node-Token` = machine credential.

| Tool | Parameters (essentials) | Mapping to today |
|---|---|---|
| `memory_store` | `content`, `category` (fact/preference/relation/procedure), `confidence`, `keywords`, `normalized` | Aligned with the builtin tool schema; writes remain Episodic + subtype, layered by the server-side distiller (ADR-068) |
| `memory_recall` | `query`, `keywords`, `since`/`until`, `limit` | Vector source = server-side embedding of the `query` text (no longer the "current user message" — see §3.3); BM25 source = keywords |

Tool names returned by `tools/list` gain the wrapper prefix (`mcp_memory__memory_recall`); opt-in follows the ADR-069 `agent_mcp_tools.json` per-tool toggles.

### 3.2 REST (panel/admin surface, via the Gateway reverse proxy)

`GET /memory/nodes` (paging/filter/keyword), `GET /memory/nodes/{nid}`, `DELETE /memory/nodes/{nid}`, `GET /memory/stats`, `POST /memory/distill`, `GET /memory/consolidation/status` (distiller + forgetting state and config read/write), `POST /memory/search` (the global-search memory source), `POST /admin/import?instance_id=` (migration: the request body is the raw SQLite bytes, `application/x-sqlite3`, 64 MiB ceiling; called by the Node only — Decision 10), `POST /admin/bootstrap` (Decision 9a). The public path keeps its existing `/api/agents/{id}/memory/*` shape, mapped by the Gateway (Decision 8).

### 3.3 Known behavior deltas (accepted and recorded)

1. **Same-session node exclusion degraded**: today `memory_recall` auto-excludes the calling session's nodes via `ToolContext.session_id`; MCP carries no runtime context, so the default becomes "exclude nothing" (the server keeps an optional `exclude_session_id` parameter with no caller injecting it today). Consequence is mere recall redundancy (compaction summaries are already in context), no correctness impact.
2. **Vector source change**: from "current user message drives the vector + LLM keywords drive BM25" to "`query` alone drives the vector". The tool description instructs the LLM to pass a semantically complete question.
3. **auto-inject dies**: agents that had opted in must move to explicit recall after the upgrade.

---

## 4. Decision detail & rationale

### Decisions 1/2: why a Gateway-hosted service rather than a Node sidecar

Memory sits between two homes: like doc it is a cross-session durable business store, like the workspace it "follows the instance". Gateway hosting wins because (a) the panel contract already lives on the Gateway HTTP entry, so same-origin proxying is cheapest; (b) the advertise-endpoint model makes the service reachable to remote Runtimes naturally (a Node sidecar means one service per machine and awkward panel aggregation); (c) the ADR-064/070/084 paradigm — supervisor/proxy code — is directly clonable. **Rejected**: Node sidecar (deployment and aggregation complexity out of proportion; no v1 demand for per-machine memory).

### Decision 3: one DB per instance vs one DB with namespaces

One DB per instance: matches the current granularity (one private SQLite per agent), migration is a file-level move (whole-store upload at `/admin/import`), deleting an instance deletes its directory, no shared blast radius. **Rejected**: single DB + namespace column — weaker isolation, cross-instance query risk, messier stats migration.

### Decision 4: why the official service insists on HTTP while third parties stay stdio-friendly

HTTP is this platform's "shared service" transport: headers carry both identities (actor / node token), and the Gateway reverse proxy plus if-watch address drift (ADR-080) are all pre-built. stdio servers are spawned by the Runtime on the node itself, identity self-attested through the spawn environment — the isolation duty of a third-party stdio memory MCP belongs to that server itself (typically a single-user local store); the platform provides no namespace semantics for it. Both transports are installable in the replacement scenario (`McpServerConfigDef` supports the command/args and url/headers shapes already).

### Decision 5: cost argument for deleting auto-inject

auto-inject is the one coupling between the Runtime and "a specific memory implementation" that MCP cannot express (no pre-LLM hook in the protocol). Keeping it = permanently special-casing the official server id + keeping retrieval code in the Runtime = pluggability defeated. It defaults OFF and its opt-in footprint is tiny, so the cost is negligible. If the need resurfaces, the right path is revisiting once MCP's context/prompt-hook capabilities mature — no dead code held in advance (principle source: [memory-write-entrypoints.md](../../memory-write-entrypoints.md) "dead code is not kept").

### Decision 6: distiller credentials via global resources

The service consuming available providers mirrors exactly how the Runtime does, keeping the central authority (Gateway/vault) intact and keeping plaintext keys out of `{data_dir}/config.toml`. **Rejected**: (a) keeping the distiller in the Runtime driving remote storage — violates invariant 1; (b) a service-local apikey config — credentials scattered, rotation hard.

### Decision 9: why the thin integration is acceptable

(a) The bootstrap push is semantically "package content distribution" (the manifest is the authority — see entrypoint D's definition): data produced Runtime-side, destined for whatever service registers as id=memory — a config pipe, not memory logic; (b) the chat_router resolution is a pure registry lookup. Both deactivate naturally for third-party MCPs. Should this boundary need narrowing later (e.g. moving (a) into the Gateway install flow), no other decision of this ADR changes.

---

## 5. Consequences

**Upside**: zero memory business in the Runtime; swapping memory becomes a config change; memory pipeline faults are isolated from the Agent session process; the packaging invariant is upgraded to a structural guarantee.

**Downside / risks**:

| Risk | Mitigation |
|---|---|
| One more network hop for memory calls (loopback / reverse proxy) | Same order as pm/doc; recall timeouts follow the MCP tool timeout policy (`constants::MCP_*`); a failure surfaces as a tool error, never a session abort |
| Service down ⇒ memory capability wholly unavailable | Supervisor exponential-backoff restart + once the catalog entry is withdrawn, panel and tools degrade consistently to "no memory"; explicit beats implicit (in the built-in era a store-open failure only logged) |
| Dual sources during migration | One-shot pass (Decision 10): a real import is immediately followed by deleting the local memory rows and writing the `.migrated` sentinel, while the session/conversation data in the same file is untouched; a `skipped_existing` answer takes the conservative branch and deletes nothing; a failure writes no sentinel, so one instance can be retried with `--force` without widening the blast radius |
| Panel accessibility widens under multi-user / remote deploys (memory leaves the node-local dir, centralizing on the Gateway host) | Same data-sovereignty direction as doc — an existing trade-off within the paradigm; covered by invariant 5 (AUTH_MODE / node-token gate) |
| Third-party memory MCPs get no panel/governance | That is the semantics of replacement itself; stated in the §1.4 table and enforced by the frontend hiding logic — the UI never pretends support |
| **Breaking**: an older package that still declares `type = "MemoryRead"` / `"MemoryWrite"` (or the string form `memory:read`) now fails hard at manifest parse — the custom `Permission` serde returns `Unknown permission type` and `parse` returns `unknown category`. No silent ignore | Deliberate (Decision 10, "no compatibility code"): keeping a permission bit nobody reads is harder to debug than an error. Accepted while the project is alpha; the seven builtin example packages were updated with this ADR (permission blocks and `[[tools]] memory_*` declarations dropped, comments pointing at `mcp_memory__*`). The `parse` unknown-category hint no longer mentions memory, so it never invites the declaration; the parse failure surfaces in the package install/load log, and a third-party package recovers with the two edits above |
| Clone semantics change: `acowork-node`'s Full clone still copies the `private.sqlite` + `*.wal` + `*.shm` triplet and calls `purge_conversation_state()`, but memory now lives on the service side keyed by `instance_id`, so **a clone inherits no service-side memory**; the `.migrated` sentinel is not copied either | Accepted. It is a direct consequence of Decision 3 (one DB per instance): instance-private memory should never propagate through duplication, and a clone starts from a blank memory slate — exactly the intent behind clone already skipping `conversations/`. If the source workspace was never migrated, its legacy memory rows get uploaded by Node into the *clone's own* `instance_id` namespace (one copy each, source and clone) — expected behaviour for a one-way handoff, with no cross-namespace dedup. "Letting a clone inherit memory" means service export → install-time import, i.e. the seed-memory channel Decision 11 explicitly places out of scope; it needs its own charter |

**Test & acceptance (summary)**: service integration tests clone [doc's MCP↔REST review-loop case](../../../core/acowork-doc/tests/api_mcp_integration.rs) (instance isolation, anonymous read-only, manual distill trigger); Gateway injection/skip tests mirror pm T4-1; Desktop gating tests (tile visibility + tab bounce); `run_gateway_fs_redline` in `dev/ci.sh` MUST pass; `memory_retrieval_bench` migrates with the logic into the service. The retrieval harnesses split the same way per invariant 7: the memory scope belongs to `core/acowork-memory/tests/retrieval_eval.rs`, the conversation scope to `core/acowork-runtime/tests/retrieval_eval.rs` (offline variants) and `global_search_e2e.rs` (real HTTP + production model), and no memory-engine test is left under `acowork-runtime/tests/`; the shared metric kernel lives in `core/acowork-eval` (dev-dependency-only).

## 6. Implementation phases

| Phase | Scope | Done when |
|---|---|---|
| P1 | acowork-memory lib+bin service (MCP + REST + pipeline migration) | Service boots standalone; integration tests green |
| P2 | Gateway: `MemoryConfig` / `memory_supervisor` / `memory_proxy` / catalog injection / status exposure | `url.is_some()` ⇔ injection tests green; panel path backend switched |
| P3 | Runtime deletion surface (full §2.2 table) + chat_router dynamic resolution + thin integration | `cargo build/clippy/test` green; acowork-memory absent from runtime dependencies |
| P4 | Desktop: gating + global-search direct access | End-to-end check: service off → panel gone |
| P5 | Node migration channel (`memory_migration` pass + `acowork-node migrate-memory` + the service's `/admin/import`) + doc closeout (this ADR settled; module-design/protocols/AGENTS.md updated) | Pass unit tests cover import / skip-existing / oversize refusal / bounded retry / sentinel; `dev/ci.sh` green including `run_gateway_fs_redline`; migration drill over existing instances passes |

P1/P2 may run in parallel; P3+P4 ship in one batch (avoiding a half-builtin/half-service intermediate); P5 last.
