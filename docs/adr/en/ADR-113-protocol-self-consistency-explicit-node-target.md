# ADR-113: Protocol Self-Consistency — Launch Relationships Stay Out of the Protocol, Installs Must Name an Explicit `node_id`

- **Number**: ADR-113
- **Status**: Implemented
- **Date**: 2026-11-12
- **Decision maker**: Nicholas (architecture criterion and scope)
- **Related**: ADR-055 (remote Runtime / Node topology), ADR-059 §15.1 (`node.{id}` is Optional), ADR-075 (node_id = UUID v4), ADR-087 (node owner and `can_manage`), ADR-009 (Gateway boundary)
- **Supersedes**: the `gateway_managed` passages in ADR-055 §6.3.4 / §6.11 / §6.12, ADR-075 D5 / D9 / implementation checklist, and ADR-087 D2; `docs/runbooks/single-machine-remote-topology.md` updated in sync
- **Chinese source of truth**: [docs/adr/zh/ADR-113-protocol-self-consistency-explicit-node-target.md](../zh/ADR-113-protocol-self-consistency-explicit-node-target.md) (maintained in the same commit)

---

## Summary

**In one sentence**: who started a Node, in what order, and on which machine is not protocol state. Every install / ensure / create request must carry an explicit `node_id`; the Gateway decides only from protocol-visible facts — the node exists, is online, its control plane is ready, and the caller may manage it — and no longer infers a default target.

**In one sentence (why)**: as soon as `node_id` may be omitted, the Gateway must fill the default using knowledge outside the protocol (who launched me, hostname, a `local_node.json` anchor, a reuse window). Those fillers necessarily break under a distributed topology, producing `Gateway 409: the Gateway's own-machine node is not online` while the Node process is in fact running.

**Architecture criterion (non-negotiable)**: replace "who started the Node" with anything — Gateway / Desktop / CLI / remote / not started at all — and protocol behaviour must be identical. Any code whose result changes under that substitution is an architecture violation. **A self-consistent protocol never needs to know where a process came from.**

---

## Background (evidence)

1. **Reproduction**: build release Desktop → clear the user's `.acowork` directory → reinstall → launch → manually `install agent` with a local `.agent` package → `Gateway 409`, whose text asks the caller to wait for `/api/bootstrap` READY or pass an explicit `node_id`; yet `acowork-node` is running.

2. **Direct trigger**: `resolve_local_node_id()` in `core/acowork-gateway/src/http/agents.rs`, called when the `POST /api/agents/install` multipart carries no `node_id`. On failure it returns a plain `ApiError::conflict`. That is not a structured `dependency_not_ready`, so Tauri's `submit_with_retry` never retries and the user sees a one-shot 409.

3. **Why "the process is running" does not mean "the local node_id can be resolved"**: `NodeRegistry` is populated only from MQTT retained `status` / `info`, never from the process table. The old `local_node_id()` required `gateway_managed && online && hostname == system_hostname()`. Change the launch path (Desktop, CLI, an upgraded old binary) and that conjunction breaks.

4. **bootstrap READY and node availability are different facts**: `BootstrapOrchestrator::recompute()` determines READY from Required subsystems only. Production static registration has `vault` / `mqtt` / `publisher` as Required, `embedding` as Optional, and `node.{node_id}` as a dynamically registered Optional (ADR-059 §15.1). `ensure_local_node()` neither registers with the bootstrap registry nor calls `mark_ready`. READY therefore permits the local Node to be absent, offline, not `NodeReady`, or failed to spawn. **Making the node Required again would break remote topologies** — a remote node is allowed to never come online.

5. **Desktop also has its own start-local-Node entry point**: `apps/acowork-desktop/src-tauri/src/commands/gateway.rs::create_local_node`, which spawns without `--gateway-managed`. So `gateway_managed` was not even self-consistent inside local mode: it merely recorded "who pressed start".

---

## Decisions

### D1 — `local` is a launch relationship, not a protocol branch

The only meaning of `local` is "the Desktop started the Gateway, and the Gateway incidentally starts one local Node". That incidental Node start is **optional, not mandatory**: the Gateway may start first and the local Node later; the launcher may be CLI, Desktop, or Gateway. Apart from that relationship, all three behave through one protocol and one set of mechanisms.

**MUST NOT** use `gatewayMode` / `gateway_managed` / spawn argv / the process table as protocol inputs.
**MUST NOT** build one mechanism for local and another for remote.

### D2 — Explicit install targets (fixing the root cause)

| Entry point | Contract |
|---|---|
| `POST /api/agents/install` (multipart) | `node_id` **required**; missing/blank → 400 `missing required field: 'node_id' — installs must name an explicit target node; query GET /api/nodes for available nodes` |
| `POST /api/agents/ensure` (multipart) | same |
| `POST /api/agents/{id}/publish/install-locally` | body `node_id: String` required |
| `acowork-gateway` node-scoped CLI subcommands | `--node <node_id>` **required**; missing → error pointing at `acowork-gateway nodes list` |
| Tauri `install_agent` / `install_bundled_agent` / `install_from_hub` / `ensure_local_agent` / `create_agent` | `node_id: String` (no longer `Option<String>`), validated by `require_node_id()` |

`resolve_local_node_id()` is deleted. `resolve_agent_node_id()` reads only a `node_id` already recorded in the inventory; when none is recorded it errors and asks for an explicit value, without falling back to the local node.

The Gateway's final gate performs only protocol-internal queries: **exists → online → `ensure_node_ready()` (NodeReady) → permissions**. Having no node is a legitimate "no usable target", not a 409 and not a reason to wait on bootstrap.

**Boundary**: `POST /api/agents/{id}/upgrade` is not in this table. An upgrade acts on an **existing instance**, whose target node is uniquely determined by that instance's recorded `node_id` in the inventory — a protocol-internal fact, not an inferred default and not launcher-dependent. By contrast, no path that creates a new instance may omit `node_id`.

### D3 — Launcher identity is removed from the protocol entirely

`gateway_managed` expressed a process-lifecycle fact ("who launched me") but had entered:

- `NodeInfo.gateway_managed` (protobuf `mqtt_payload.proto` field 12)
- `NodeIdentity.gateway_managed` (`identity.json` persistence)
- `NodeInfoState.gateway_managed` (Gateway NodeRegistry cache)
- `NodeResponse.gateway_managed` (`GET /api/nodes`)
- Desktop `NodeInfo.gateway_managed` (`lib/types.ts` / `gateway-api.ts`)

All are deleted. The proto keeps `reserved 12; reserved "gateway_managed";` and the field number is never reused. A legacy `identity.json` field is ignored by serde, so no migration is needed on upgrade.

`local_node_id()` remains a **Gateway-local deployment query**, used only for local deployment checks, loopback download optimisation, and the local scope of a claim. It is decided purely from protocol-visible facts (`online` + `hostname`) and **must not** serve as an install / ensure default.

### D4 — Launcher-local process marker `--spawn-marker`

`--gateway-managed` is renamed `--spawn-marker`. It exists solely so a supervisor can scan local process argv and reap orphan children it started:

- consumed and discarded immediately by the Node CLI (`let _ = spawn_marker;`)
- **not** written to `identity.json`
- **not** part of `NodeConfig`
- **not** published in `NodeInfo`
- **not** an input to any protocol branch

Upgrade compatibility: `cleanup_orphaned_local_nodes()` matches both the new `--spawn-marker` and the legacy `--gateway-managed` string so children spawned by older binaries are still reaped. Starting, reaping, and orphan cleanup are **local transactions of the respective launcher**, not protocol.

### D5 — `control_ready` exposes readiness as protocol-visible fact

`NodeResponse` in `GET /api/nodes` gains `control_ready: Option<bool>`, derived from `bootstrap_registry.is_ready(SubsystemId(format!("node.{}", n.node_id)))` — i.e. the Node control plane announced `NodeReady`. Serialisation stays snake_case (`control_ready`); tests pin that `gateway_managed` / `gatewayManaged` / `controlReady` are absent.

The field is **tri-state, not a boolean**:

| wire value | meaning |
| --- | --- |
| `true` | registry present, `node.{id}` is Ready |
| `false` | registry present, `node.{id}` not Ready |
| *absent* | no bootstrap registry wired (MQTT disabled / minimal server) — readiness **unknown** |

Absence must not be collapsed to `false`. `ensure_node_ready()` is deliberately fail-open in exactly the no-registry configuration, so asserting `false` there would make `GET /api/nodes` and the install gate contradict each other, and the Desktop would hide every online node as unusable. `#[serde(skip_serializing_if = "Option::is_none")]` makes an unknown readiness indistinguishable on the wire from a pre-113 Gateway — both correctly mean "let the Gateway's own gate decide".

ADR-059 §15.1 is **unchanged**: `node.{id}` stays an Optional dynamic subsystem. Readiness is a fact queried per node, not a precondition of global READY.

### D6 — The upper layer consumes "usable nodes", not "launch status"

A launch success/failure is a one-shot event bound to a launcher; the replayable, launcher-independent state is "the current set of online / ready / manageable nodes". `apps/acowork-desktop/src/components/nodes/nodeAvailability.ts` is the **single** definition:

```ts
usableNodesOf(nodes) = nodes.filter(
  (n) => n.online && n.control_ready !== false && n.can_manage !== false,
);
```

`control_ready` / `can_manage` are optional for compatibility with older Gateways and with Gateways that have no readiness registry; when absent, the final gate remains the Gateway's `ensure_node_ready()` plus Tauri's structured retry. `normalizeNode()` therefore only maps actual booleans through and preserves absence as `undefined` (D5).

- `GET /api/nodes` is the single source of truth; `gatewayMode` does not participate (`settingsReads === 0` guard in `AgentLibraryDialog.test.tsx`).
- `AgentLibraryDialog` and `OnboardingFlow` share `usableNodesOf()` and a shared `NoNodeBanner`; `AgentList` passes an explicit `nodeId` into `CreateWizard`.
- The empty state uniformly says "no usable node" and gives a protocol-internal reason: `none` / `notReady` / `notManageable` / `blocked` (`noNodeDetailOf()`).
- Every topology gets two **parallel** recovery options, with no mode branch: ① `create_local_node` to start a Node on this machine; ② a guide for starting a Node elsewhere.
- **Launch retry and install retry are decoupled**: a start action does not wait for the Node to come online and does not chain an install; an install retry is another user click, always with an explicit `node_id`.

### D7 — The remote guide gives a real, copyable command

On expansion, `NoNodeBanner` calls the Tauri `build_remote_node_command`:

- `GET /api/status` → `mqtt_port`
- `POST /api/nodes/enrollment-tokens` → a real enrollment token (TTL 3600s)
- gateway base URL → `suggested_host`

It produces `acowork-node start --gateway <host>:<port> --token <token>`, with an editable host, a copy button, and a loopback hint. The token is minted only when the user expands the guide, never when the empty state renders. Guide state and token lifetime are owned by the banner and reset on unmount.

### D8 — i18n contract

`noOnlineNode` is renamed to `noAvailableNode`. Added: `noNodeDetailNotReady`, `noNodeDetailNotManageable`, `noNodeDetailBlocked`, `remoteNodeLoading`, `remoteNodeLoadFailed`, `remoteNodeHost`, `remoteNodeHostHint`, `remoteNodeCopy`, `remoteNodeCopied`, `remoteNodeTokenHint`. `zh-TW.json` / `ja.json` / `ko.json` gain a complete English `agentHub` fallback so all 5 locales have valid placeholders.

### D9 — Readiness state must be replayable, not a one-shot event

`status=online` and `NodeReady` are **state**, but the implementation published them exactly once per ConnAck (retained), and the 60 s `info` heartbeat did not carry them. On a cold start the Gateway's SUBSCRIBE may still be in flight when the node's first PUBLISH lands; once missed, nothing ever repairs it — the process is healthy and listening, yet `GET /api/nodes` does not contain it and `control_ready` stays `false` forever. Only a second node process (or a node reconnect) re-ran ConnAck and "accidentally" healed it.

This is the same criterion as D6, seen from the protocol side: **if the upper layer is to consume replayable state, the protocol layer must actually make that state replayable.**

Fix:

- `NodeControlPlane::run_loop` gains a presence re-assert: first at 1 s after connect, then every 5 s, re-publishing `status=online` and `NodeReady` (retained). It runs only while `state.connected` is true, so a queued `online` can never overwrite the `offline` published during shutdown.
- Both envelopes come from `node_ready_envelope()`, shared by the ConnAck path and the periodic re-assert so they cannot drift.
- Downstream is idempotent: `NodeRegistry::update_status_from_mqtt` only stamps `online_since` on a `false→true` edge, and `SubsystemReadinessRegistry::transition` returns early when the state is unchanged — re-asserting does not bump the bootstrap snapshot version or cause re-render storms.

Companion upper-layer fix (same criterion):

- New `useNodeTopologyRefresh(active)`: `AgentLibraryDialog` reads `GET /api/nodes` immediately on open and every 2 s thereafter; `OnboardingFlow` reuses the same implementation. **The moment a surface needs the node state, it starts reading the protocol continuously** instead of trusting a mount snapshot or a launch event.
- `create_local_node` dedup gives the protocol view a 1.5 s convergence window (only when `identity.json` already exists), so a cold-start race cannot spawn a second process for the same node id.

In one line: **protocol state may arrive late, but must never be lost; and the upper layer must not read it only once at mount.**

### D10 — A launcher must not leak its launch-chain console handles into the launched process

After the D9 fix, cold start still hung 100% of the time. Log forensics surfaced a second root cause: the Gateway's `spawn_node_child` configured only `stdout(null) / stderr(inherit)`, leaving stdin at the inherit default. In the dev chain Desktop (console subsystem) → Gateway (`CREATE_NO_WINDOW`, attached to a fresh hidden console but still holding the dev terminal's std handles) → node (inherits handles that belong to a *different* console). Every observation closes against this: process alive, 19 threads all waiting, zero MQTT connections, no identity.json, 0-byte file log with the file created, deterministic on every fresh environment, and "accidentally" healthy when Desktop spawns the node itself, when stdin is a pipe, or when identity already exists.

The fatal mechanism is that **`IsTerminal` is a handle-level check**: the node inherited the dev terminal's console input handle, so `interactive_enrollment_token` concluded "stdin is a TTY", a fresh machine (no identity) entered the ADR-087 interactive login, and `read_line` blocked forever on an input buffer of a console the node is not attached to. Not a race — a spawn-contract defect.

Fix (`spawn_node_child` and the Desktop `create_local_node` aligned to one identical stdio contract):

- `stdin/stdout/stderr` all explicitly `Stdio::null()`; on Windows add `CREATE_NO_WINDOW`. stdin=null makes `IsTerminal` deterministically false — whoever starts the node, whatever the launch chain's terminal topology, a fresh machine takes the headless enroll path (Gateway default-owner fallback). Same shape as D1: **interactive behavior must not depend on how the process was started**.
- The node's diagnostics channel is its own rolling file under `{home}/logs/`, not inherited stderr (cross-console stderr writes are undefined across Windows console hosts and empirically drag the file layer to 0 bytes too).
- Repro matrix: bash foreground, pipe-based reproducers, and a hidden-real-console reproducer (`Start-Process -WindowStyle Hidden`, replicating the dev handle topology) — after the fix, fresh-identity nodes enroll correctly and write file logs in all three; before it, the real chain hung 4 out of 4 runs.

---

## Rejected alternatives

| Alternative | Why rejected |
|---|---|
| Add `LocalNodeHandle { node_id, pid, instance_id, state }` and a launch handshake, promoting the local Node to a first-class resource | Elevates "the Gateway is the creator" into protocol identity; local and remote necessarily diverge. Violates D1 |
| Make `node.{id}` Required so READY guarantees a usable local Node | A remote node is allowed to never be online; Required would deadlock the Gateway. ADR-059 §15.1's demotion was correct |
| Turn the 409 into a structured `dependency_not_ready` so Tauri retries | Fixes the symptom only: the retried request still has no `node_id`, so the Gateway still guesses. The root cause is that `node_id` is optional |
| Have the UI watch Node launch status when `gatewayMode === "local"`, prompting a retry on failure | Re-couples local; a launch event is bound to the launcher, not protocol state. The correct input is the usable-node set |
| Keep `gateway_managed` "for display only" | Any field on the wire will eventually be used for a decision. Delete, do not soft-deprecate |
| After Desktop starts a Node, automatically continue with the install | An implicit chain is Desktop inferring the target for the Gateway, and rebinds the flow to a launcher |

---

## Impact (code)

| Layer | Files | Change |
|---|---|---|
| Protocol | `core/acowork-core/proto/mqtt_payload.proto` | delete `gateway_managed = 12`, `reserved 12` |
| Node | `identity/mod.rs` / `config.rs` / `cli.rs` / `control/mod.rs` / `service.rs` | delete persisted/config fields; `--spawn-marker` consumed and discarded |
| Gateway HTTP | `http/agents.rs` / `http/publish_api.rs` / `http/nodes_api.rs` | require `node_id`; delete `resolve_local_node_id()`; drop launcher field from `NodeResponse`, add `control_ready` |
| Gateway other | `cli.rs` / `gateway/node_manager.rs` / `mqtt/node_registry.rs` | `--node` required; rename spawn marker + match legacy string; `local_node_id()` reduced to protocol-visible facts; `spawn_node_child` stdio all-null + `CREATE_NO_WINDOW` (D10) |
| Desktop Tauri | `commands/{agent,agent_library,create,hub,gateway}.rs`, `gateway_client.rs`, `lib.rs` | all commands take `node_id: String`; new `build_remote_node_command` |
| Desktop UI | `components/nodes/{nodeAvailability.ts,NoNodeBanner.tsx}`, `AgentLibraryDialog.tsx`, `OnboardingFlow.tsx`, `CreateWizard.tsx`, `AgentList.tsx`, `lib/{types.ts,gateway-api.ts,agentHub.ts}`, `stores/agentStore.ts` | shared usable-node definition and empty state; explicit `nodeId` end to end |
| Tests | `AgentLibraryDialog.test.tsx` / `CreateWizard.test.tsx` / Rust unit + E2E / proto golden | guards: `settingsReads === 0`, old Gateway compatibility, `control_ready` blocking, no launcher field on the wire |
| i18n | `en.json` / `zh-CN.json` / `zh-TW.json` / `ja.json` / `ko.json` | key rename and completion |

---

## Verification

- `check:i18n`: all 1958 `t()` call sites resolve in en / zh-CN; all 5 locales have valid placeholders
- `tsc --noEmit`: zero errors in changed files (whole-tree errors are pre-existing missing `jszip` / `docx-preview` / `pdfjs-dist` / `xlsx` dependencies)
- `vitest`: 140 / 140 across the six affected test files; D9 adds `useNodeTopologyRefresh.test.ts` (3 / 3), which together with `AgentLibraryDialog.test.tsx` is 54 / 54; full suite `3321 passed / 6 failed`, 5 being pre-existing (4 editor test files cannot collect because `jszip` / `docx-preview` / `pdfjs-dist` / `xlsx` are not installed, plus 5 `GroupChatPanel.blockRef.test.tsx` cases about `@agent-a` vs `@Agent A`). The sixth, `DocEditor.dirty-on-open.test.tsx`, passes 2 / 2 when run alone — a timing flake under full-suite load, unrelated to this change
- `cargo test -p acowork-node --lib control::tests`: 30 / 30 (including the new D9 guard `node_ready_envelope_carries_the_announcing_node_id`)
- `cargo check --manifest-path apps/acowork-desktop/src-tauri/Cargo.toml` (lib + bin): passing (D9 dedup convergence window)
- `cargo test -p acowork-gateway --test node_control_plane_e2e`: 6 / 6 passing (including `spawn_marker_does_not_change_node_protocol_behaviour`; rebuild `acowork-node --bin acowork-node` first so the E2E binary knows the new CLI flag)
- `cargo check --manifest-path core/Cargo.toml -p acowork-core -p acowork-node -p acowork-gateway -p acowork-runtime --all-targets` and `cargo check --manifest-path apps/acowork-desktop/src-tauri/Cargo.toml --all-targets`: both passing
- Rust lib tests (proxy cleared): `402 + 904 + 212 = 1518` all passing; `node_proto_golden` 11/11 (the golden vector drops field 12's `60 01` bytes and now asserts `!fields.contains(&12)`)
- Rust guards: `identity_does_not_persist_a_spawn_marker`, `local_node_id_depends_only_on_online_and_hostname`, `local_node_id_rejects_a_node_on_another_machine`, `spawn_marker_does_not_change_node_protocol_behaviour`, `NodeResponse` serialisation regression; review added `unknown_control_ready_is_absent_not_false`, `list_nodes_marks_readiness_unknown_without_a_registry`, `list_nodes_projects_node_ready_from_the_bootstrap_registry` (`http::nodes_api::tests` 24 / 24)
- Review consistency check: `ensure_node_ready()` is fail-open when no bootstrap registry is wired, so `GET /api/nodes` no longer reports a hard `false` there — `control_ready` is omitted (unknown) and `normalizeNode()` keeps it `undefined`. Node list and install gate now agree (`http::agents::tests` 33 / 33)
- Remote-command safety guards: shell metacharacters filtered from the editable host, IPv6 bracketed, leading `-` / `.` stripped so clap cannot read the host as an option, copy timer cleared on unmount
- Environment note: when the session injects `HTTP_PROXY` with an empty `NO_PROXY`, five loopback HTTP unit tests (`proxy` / `lsp_admin_proxy` / `embedding_api` / `probe::runner`, none touched here) fail because their requests are intercepted. With proxy variables cleared they pass 5/5; unrelated to this ADR.
- D10: `cargo build -p acowork-gateway` passes; isolated E2E (fresh `ACOWORK_NODE_HOME`, dedicated ports, Desktop-identical `RUST_LOG` env, both pipe handles and hidden-real-console topologies) — the Gateway-spawned node reaches MQTT connected → enrollment → bootstrap complete within ~1 s, `identity.json` created, file log non-empty (4.9 KB / 3.9 KB); before the fix the same topology hung 4/4 runs. `cargo test -p acowork-gateway --lib` is 907 / 907 with proxy variables cleared.

---

## Rollback

Requiring `node_id` **tightens** the contract: an old client that omits it gets a 400 instead of a guessed target. If temporary compatibility is unavoidable, fix the client to supply `node_id` (selected from `GET /api/nodes`); **never** restore inference in the Gateway. `gateway_managed` is `reserved 12` and the field number must not be reused; upgrades simply ignore the legacy `identity.json` field.
