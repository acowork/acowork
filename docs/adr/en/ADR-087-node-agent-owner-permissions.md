# ADR-087: The Node and Agent Owner Permission Model — Plugging the Hole Where "the Whole Machine Is Writable by Every User by Default"

> **Chinese source of truth**: [ADR-087](../zh/ADR-087-node-agent-owner-permissions.md)
> **Terminology**: see [GLOSSARY.md](./GLOSSARY.md)

**Status**: Draft (v2 revision; Q1/Q2/Q4 and the "single owner + multiple guests" model are decided, see §12 for the rest)
**Date**: 2026-10-28 (v2 revision: added D9 ownership cardinality and the guest model — no multiple owners; a guest is a use-tier authorization (not manage); Q1/Q2/Q4 land as decisions. v3 revision: the permission axis moves from two tiers to **three tiers manage/use/view** — manage narrows to "high-privilege / destructive / agent-defining", use covers every remaining write, view opens **metadata/definition reads**; "content reads" (workspace file content, git history, memory, global search) stay at use, not view (guards against the §7 option-G theft); start→use, stop→manage; the permissions roster is public at view)
**Decision Makers**: (TBD)

**Predecessors**:
- [ADR-076](./ADR-076-multi-user-account-system.md) (multi-user account system — this ADR extends the session-dimension isolation paradigm of §decision 4 (`user_id` + a single server-side decision point + `can_write` delivery) to the node and agent dimensions; the business semantics remain valid, and the implementation form is in ADR-084)
- [ADR-084](./ADR-084-user-standalone-process.md) (standalone user-domain process — the account authority lives in `acowork-user`; this ADR's owner is **Gateway-side authorization policy data**, not account data, so the two do not conflict)
- [ADR-075](./ADR-075-node-identity-uuid-and-node-name.md) (node_id = a UUID v4 stable routing key — owner is keyed on `node_id`, so renames/migrations are unaffected)
- [ADR-073](./ADR-073-agent-instance-identity-decomposition.md) (instance_id = UUID v4 — agent owner is keyed on `instance_id`, so multiple instances of a package each have their own owner)
- [ADR-055](./ADR-055-remote-runtime-node-topology.md) (§6.2 enrollment / §6.5 node-reported inventory are the existence authorities — this ADR only adds an "ownership authority", it does not change the existence authority)
- [ADR-009](./ADR-009-gateway-workspace-isolation.md) (Gateway red line: no direct access to Agent-private files — all authorization in this ADR happens at the **reverse-proxy entry point**, introducing no Gateway-side filesystem access whatsoever)
- [ADR-077](./ADR-077-system-agent-demotion-to-default-agent.md) (a default agent is an ordinary agent — it is likewise bound by the owner model, see §12 Q1)

---

## 1. Decision Summary

### 1.1 In one sentence

**Add a Gateway-side persistent `owner_user_id` (single owner) + a `guests` use-authorization list to each Node and Agent instance, with a three-tier gate: **manage = owner ∨ admin** (narrow — high-privilege / destructive only: install/uninstall/clone/stop/upgrade/debug, permission settings, agent-defining config writes, memory destructive writes, delete session); **use = owner ∨ guest ∨ admin** (broad — every write not in manage: start, chat, session create/modify, file/git operations, memory retrieval / global search); **view = use ∨ published (shared/public)** (read-only information offered as widely as possible, with session content additionally gated by the private/public second wall). The agent-level `visibility` (private/shared) decides **visibility only** (shared makes it *findable and read-only* for every logged-in user but does not open "use"), while the session dimension stays unchanged per ADR-076. Authorization is executed at a single point — the Gateway reverse-proxy entry (where `AuthContext` and the `instance_id→owner` mapping already exist) — and is fail-closed: an unregistered route defaults to manage, and ownerless resources are admin-manageable only.**

### 1.2 Key Decision Table (detailed rationale in §5)

| # | Decision | Conclusion |
|---|----------|-----------|
| D1 | Where owner is stored | **Two new Gateway-side persistent ownership tables** (`node_owners.json`, `agent_owners.json`, in the same directory and following the same pattern as `node_tokens.json`). **Not in the MQTT proto**: the inventory reported by Node remains the "existence authority" (ADR-055 §6.5); ownership is Gateway's policy data, and Node is not aware of users |
| D2 | Where the node owner comes from | **The enrollment token binds its creator**: in multi_user mode a new `POST /api/nodes/enrollment-tokens` (login required); the token record carries `owner_user_id`, and a successful enroll writes `node_owners.json`. A CLI-issued token is ownerless → that node is ownerless (admin-only). `gateway_managed` local nodes default to ownerless |
| D3 | Where the agent owner comes from | **Whoever installs, owns it**: on successful dispatch of `POST /api/agents/install` / `ensure` / `clone`, write to `agent_owners.json` keyed by `instance_id`, with owner = the caller's `AuthContext.user_id`. An admin can naturally install on any node (Q2 decided, see §12), so the resulting agent's owner = admin. (Installing requires the target node's manage list = owner ∨ admin; a guest is use-tier and cannot install.) |
| D4 | Three-tier gating | **manage = owner ∨ admin** (narrow): Node gating — install/uninstall onto that node, fs browse, rename, enroll ownership. Agent gating — stop/upgrade/clone/debug, agent-defining config writes (config/prompts/skills/model/workspace add-remove-modify/avatar/manifest), memory destructive writes, delete session, permission settings. **use = owner ∨ guest ∨ admin** (broad): every write not in manage — start, chat, session create/modify, file/git read+write, memory retrieval, global search, interactions; **content reads** (workspace file content / git history / memory / search) also sit at use. **view = use ∨ published**: agent definition/metadata reads only (list/detail/config-definition reads/status/avatar/permissions roster). See the D4 gate table |
| D5 | Execution point | **The Gateway reverse-proxy entry** (a routing policy table in `http/proxy.rs` + handler headers in `agents.rs` / `nodes_api.rs` / `fs_browse.rs`). Runtime does not gain an owner concept (it cannot get the ownership truth and should not); the `x-user-id` scope mechanism is retained as-is, used only for session filtering |
| D6 | Agent visibility and use | Agents gain `visibility`: `private` (only the use list = owner ∨ guest ∨ admin may see or use it) and `shared` (**every logged-in user may see it**, but **only the use list may use it** — opening a session to chat requires being authorized as a guest; sessions stay isolated per ADR-076). **`visibility` decides "can you see it", never "can you use it"; use always goes through the use list.** Defaults: instances installed/ensured/cloned by a user land as `private`; **the default agent preinstalled by onboarding lands as `shared`** (Q1 decided, see §12) — note this only makes it *visible* to everyone; whether one can chat still depends on being added to the guest list |
| D7 | Ownerless, transfer and cleanup (fail-closed) | In multi_user mode **fail-closed**: resources with `owner=None` are admin-manageable only; an admin can claim/transfer via `PATCH .../owner`. Local mode is a complete no-op (the same `AuthMode` switch as ADR-076 §decision 12) |
| D8 | Single source of truth on the server | `GET /api/agents` / `GET /api/nodes` deliver a server-computed `can_manage` / `can_use` boolean per record; Desktop/Mobile **only consumes the boolean for rendering and must not derive it itself** (continuing the discipline of ADR-086 invariant 1) |
| D9 | Ownership cardinality and collaboration | **Single owner + multiple guests**. The owner is unique (unique responsibility, a two-party transfer); a guest is a "**use authorization list**" member (authorized to use/chat, cannot configure) rather than ownership — it does not change ownership, cannot add/remove guests, cannot transfer, cannot change visibility, and **cannot manage** (config/workspace/files/lifecycle stay owner ∨ admin only); revocation immediately loses access and does not reclaim the ownership of agents installed in a guest's own name. Admin and the owner jointly maintain the list (see §5 D9) |

---

## 2. Background: Status Quo and Threat Model

### 2.1 Status Quo Inventory (code facts)

After ADR-076 elevated "user" to a first-class identity dimension, the **session dimension** already has complete isolation:

| Dimension | User binding | Authorization execution point | State |
|---|---|---|---|
| Session (conversation) | `SessionMeta.user_id` + `visibility` | Runtime `is_readable_by`/`is_writable_by` ([core/acowork-memory/src/session_meta.rs:258](../../../core/acowork-memory/src/session_meta.rs#L258)), with the scope coming from the `x-user-id` injected by Gateway | ✅ isolated |
| **Node** | ❌ none. `NodeInfo` ([mqtt_payload.proto:1188](../../../core/acowork-core/proto/mqtt_payload.proto#L1188)), `NodeInfoState` ([node_registry.rs:26](../../../core/acowork-gateway/src/mqtt/node_registry.rs#L26)), `NodeTokenRecord` ([enrollment.rs:197](../../../core/acowork-gateway/src/mqtt/enrollment.rs#L197)) all have no user field | none | ❌ |
| **Agent instance** | ❌ none. `AgentInfo` ([state.rs:28](../../../core/acowork-gateway/src/gateway/state.rs#L28)) and the install/enable endpoints have no owner field | none | ❌ |

In other words, the session dimension has already paid the modelling cost, but **the two dimensions that touch the machine have not been modelled at all**.

### 2.2 Attack / Misuse Scenarios (multi_user mode, any low-privilege account holding a valid login token)

1. **Default-broken cross-machine file read/write**: an ordinary user B calls `GET /api/agents/{A}/workspaces/tree` or `GET /api/agents/{A}/workspaces/file` on another user A's agent, and directly reads that machine's source code / `.env` / `~/.ssh`; further, `POST /api/agents/{A}/workspaces` creates a directory in A's workspace, and `PUT .../file` **writes content into A's workspace** (write-back into the code repo).
2. **Node-level reconnaissance**: `GET /api/fs/browse?target={node_id}` browses the entire disk of any node; `GET /api/nodes` leaks the whole cluster's hostname/OS/arch.
3. **Resource occupation and supply-chain position**: `POST /api/agents/install` with an arbitrary `node_id` installs a malicious/mining agent onto someone else's machine; `POST /api/agents/{id}/start` starts the process.
4. **Damaging others**: `stop` / `uninstall` / `PUT config` (change model, change prompt) / `DELETE workspace` on someone else's agent.
5. **Privilege-escalating snooping**: the `?target=` reverse proxy does not validate the caller's relationship to the node; `GET /api/nodes` leaks the whole cluster's hostname/OS/arch.

Scenario 1 is **broken by default**: it requires no misconfiguration at all — it holds as soon as a set-up Gateway is connected to a second user. This is precisely the motivation for this ADR.

### 2.3 Why session isolation does not block it

The session's `is_writable_by` only protects **conversation data**. The workspace configuration and file APIs have no scope concept at all on the Runtime side (`USER_SCOPE_HEADER` is consumed only by `http/session_control.rs`). An attacker does not need to touch someone else's session — doing work in their own session with the injected workspace, or calling the workspace API directly, has the same effect.

---

## 3. Goals and Non-Goals

**Goals**
1. In multi_user mode, the files and process resources of the machine a Node lives on are, by default, open only to that Node's owner (and admin).
2. An agent instance's configuration surface (config/prompts/skills/workspace/files/lifecycle) is by default open only to the agent's owner (and admin).
3. Preserve legitimate sharing needs: an agent can explicitly declare `shared` to let other users **see** it; **using** it (chatting) is authorized per-user through the guest list, and neither changes **owning** it.
4. Local mode (single-user self-use) has zero behaviour change; after upgrading to multi_user the migration path is explicit and fail-closed.
5. A single permission decision point and a single source of truth (booleans delivered by the server; the client does not derive them).

**Non-Goals**
1. **No fine-grained RBAC** (per-workspace ACL, per-tool authorization, team role matrices). The three tiers (manage/use/view) + two roles (owner/admin) cover all currently known scenarios; extend only when a third stable requirement appears (Rule of three).
2. **No change to the MQTT data-plane ACL**. The broker's `can_subscribe` remains Phase-1 permissive ([acl.rs:178](../../../core/acowork-gateway/src/mqtt/acl.rs#L178)); the cross-account retained-event fan-out problem (known, the root cause of e2e flakes) is handled by a follow-up ACL ADR. This ADR only tightens the **HTTP control plane**.
3. **No defence against Node itself misbehaving**. A Node is the user's own machine running the user's own processes; at the OS level it can already see everything on its own disk. What this ADR prevents is **other logged-in users** operating that machine through Gateway.
4. **No approval workflow** (request-approve-authorize-duration, etc.). Transfer and sharing are a single PATCH step performed proactively by the owner.

---

## 4. Terminology and Roles

| Term | Definition |
|---|---|
| **manage** | high-privilege operations that change the agent's definition, the machine's state, or are destructive: lifecycle (install/uninstall/clone/stop/upgrade/debug/dev-mode), agent-defining config writes (config/prompts/skills/model/workspace add-remove-modify/avatar/manifest), memory destructive writes, delete session, fs browse, enroll ownership, permission settings |
| **use** | every remaining operation that drives the agent to work (all writes not in manage): creating/opening **your own** session and chatting, start, workspace file and git read/write (incl. revert), memory retrieval, global search, interactions. **Content reads** (workspace file content, git history, memory, search) also sit at use — they read the owner's real data with no second wall behind them (see §7 option G) |
| **owner** | the **unique** user the resource belongs to (`user_id`, a UUID from the `acowork-user` account system). The responsibility anchor: adding/removing guests and changing visibility are owner ∨ admin; a change of the owner field itself is admin only (D7) |
| **guest** | a user granted the **use** tier on that resource by the owner or admin (the list lives in the ownership table, i.e. the "use/chat authorization list"). A guest is authorization, not ownership: may open their own session and chat, start the agent, read/write workspace files, and retrieve memory/search, but **cannot manage** (agent-defining config writes / workspace add-remove-modify / lifecycle / delete session / permission settings are owner ∨ admin only); no ownership operation power; revocation immediately loses access; no cascade (see D9) |
| **admin** | `Role::Admin`; can manage every resource, claim/transfer ownerless resources, maintain any resource's guest list; following ADR-076's "can see but cannot impersonate" discipline (write operations execute as the admin's own identity, and ownership does not thereby change) |
| **ownerless** | `owner=None`. Under multi_user = admin-only (fail-closed) |

The decision functions (the single implementation inside Gateway):

```text
can_manage(user, resource) := user.is_admin
                            ∨ (resource.owner = Some(user.id))
                            // guests are NOT in the manage tier: a guest is "use authorization", not "co-maintenance" (D9)
can_transfer(user, resource) := user.is_admin ∨ (resource.owner = Some(user.id))
                            // ownership operations (adding/removing guests / changing visibility) exclude guests;
                            // a change of the owner field itself is admin only (an extension of D7's "can see but cannot impersonate")
can_use(user, resource)    := can_manage(user, resource)
                            ∨ resource.guests.contains(user.id)
                            // use tier = authorization list (guests) ∨ owner ∨ admin; visibility never opens use
can_view(user, resource)   := can_use(user, resource)
                            ∨ resource.visibility.is_published()
                            // view tier = use list ∨ published (shared agent / public node)
```

---

## 5. Decision Details

### D1: Owner is Gateway-side policy data — not in the proto, not in Node

**Approach**: two new ownership tables under Gateway's `{data_dir}`, in the same directory and following the same "atomic-write JSON + in-memory mirror" pattern as `node_tokens.json` / `enrollment_tokens.json` ([enrollment.rs](../../../core/acowork-gateway/src/mqtt/enrollment.rs)):

```text
node_owners.json    # node_id (UUID v4, ADR-075) → { owner_user_id: Option<String>, guests: [user_id], visibility, created_at, claimed_by_enroll_token }
agent_owners.json   # instance_id (UUID v4, ADR-073) → { owner_user_id: Option<String>, guests: [user_id], visibility: Private|Shared, created_at }
```

**Why not stuff it into the `NodeInfo` / `InstalledAgentInfo` proto for Node to report**:
- Node is not aware of user accounts (the account authority is in `acowork-user`; Node only has a node token). Having Node carry/echo the owner means handing the policy truth to the data plane, and after a Gateway restart the ownership would be rebuilt from the retained inventory — **a tampered Node-side inventory could then change ownership**. The authorization foundation cannot be built on a channel that a lower-trust component can overwrite.
- Existence authority and ownership authority are separated: the inventory says "this instance is on that machine" (Node's authority), the owner says "who may operate it" (Gateway's authority). After uninstall the owner-table entry is cleaned up with a delay (see D7's cleanup policy).

**Cost**: two more local state files in Gateway; node migration/reinstall does not lose ownership (the key is a stable UUID — exactly the groundwork ADR-073/075 laid in advance).

### D2: The node owner comes from the enrollment token's creator

Status quo: enrollment tokens can only be issued by the CLI `acowork-gateway nodes token create` ([cli.rs:496](../../../core/acowork-gateway/src/cli.rs#L496)); issuance has no user context, and `EnrollmentTokenRecord` only has `consumed_by: Option<node_id>`.

**Decision**:
1. `EnrollmentTokenRecord` gains `owner_user_id: Option<String>`.
2. In multi_user mode a new `POST /api/nodes/enrollment-tokens {ttl}` (login required): the issued token has `owner_user_id = caller`. The Desktop "add device" wizard switches to this endpoint to obtain the token + the copyable `acowork-node start --token ...` command.
3. The successful enroll path (`decide_enroll` → Accept, [dispatch.rs:1114](../../../core/acowork-gateway/src/mqtt/dispatch.rs#L1114)) writes the token's `owner_user_id` into `node_owners.json`.
4. A CLI-issued token (ownerless) → the node is ownerless → admin-only. A `gateway_managed` local node (spawned by Gateway itself) defaults to ownerless: that Gateway machine belongs to the deployer (admin).
5. Re-enrolling the same node (reconnect/reinstall with identity not lost) does not change the owner; after `identity.json` is lost, **re-enrolling with a new node_id** counts as a new device (the token's creator is the owner).

**Why not "the first user to use it becomes the owner"**: the enroll action is itself the declaration of "connecting this machine to the cluster", so the token's creator is the most honest ownership signal, with no claiming needed afterwards.

### D3: Agent owner = the installer

`install_agent` ([agents.rs:1032](../../../core/acowork-gateway/src/http/agents.rs#L1032)) already has Gateway generate the `instance_id` (at install time, with the caller already authenticated), which is the natural anchor for writing ownership:

- `POST /api/agents/install`, `POST /api/agents/ensure` (declarative; when first creating the instance), `POST /api/agents/{id}/clone`: after successful dispatch the owner row is **staged** in an in-memory pending table keyed by the Gateway-minted `instance_id`; it is committed to `agent_owners.json[instance_id] = { owner: AuthContext.user_id, visibility: Private }` when the Node reports `ok` or the instance first appears in the retained inventory, and dropped when the Node reports `error` — a failed install leaves no orphan row (review M4). Clone is a synchronously-confirmed path and writes directly.
- When `ensure` hits an existing instance the owner is **not** changed (the idempotency semantics constrain existence, not ownership).
- Local mode has no `AuthContext` → write `owner=None`, which is fine since Local doesn't verify anything (D8).
- **Initial visibility**: install/ensure/clone default to `Private` when writing the table; the only exception is the default agent preinstalled by Desktop onboarding (the ADR-077 bundled package), which lands as `Shared` — a team deployment's out-of-the-box assumption is "anyone can *see* it" (note: `shared` opens visibility only; chatting still requires being added to the guest list, see D6), with the risk wording carried by the UI. The default agent's owner is still the first user who ran onboarding, and the owner may later switch it back to private.

### D4: Two-Tier Gating + the Permission Matrix

**Node gating** (ownership of the machine: node owner ∨ admin) and **Agent gating** (ownership of the instance) are layered. Three tiers: **manage = owner ∨ admin** (guests excluded); **use = owner ∨ guests ∨ admin** (the guest list *is* the use authorization); **view = use ∨ published (shared/public)**. Tiers are assigned by *minimum necessity*: only operations that change the agent's definition, the machine's state, or are destructive enter manage; writes that drive the agent to work enter use; read-only information goes to view as widely as safe. Table:

| Operation (Gateway HTTP route) | Gating tier | Required permission |
|---|---|---|
| `POST /api/nodes/enrollment-tokens` | open | login is enough (enrolling your own machine) |
| `GET /api/nodes` | visibility-filtered | `private`: the use list sees the row; `public`: every logged-in user sees id/name/online/`can_manage`, sensitive fields stay manage-only (D6, option B) |
| `PATCH /api/nodes/{id}` (rename), `.../visibility`, `.../guests`, `.../owner` | Node-manage / Node-transfer | manage list (owner ∨ admin); owner transfer is admin-only, guests have no ownership power (D9 R1) |
| `GET /api/nodes/{id}/permissions` | Node-view | **the ownership roster is public**: anyone who can see the node may read the roster (knowing *who* to ask for access is not a secret) |
| `POST /api/agents/install`, `ensure`, `clone` (choosing a target node) | Node-manage | the target node's manage list (installing onto someone else's machine requires machine permission first; a guest is use-tier and **cannot** install) |
| `DELETE /api/agents/{id}` (uninstall) | Node-manage ∧ Agent-manage | both lists must pass (uninstall touches both the machine and the instance) |
| `POST /api/agents/{id}/stop` / `restart` / `upgrade`, `debug/*` | Agent-manage | the agent's manage list (mutates/replaces the running agent; debug is high-privilege) |
| `POST /api/agents/{id}/start` | Agent-use | the use list — waking a stopped agent is "use", not "manage"; non-destructive |
| **agent-defining config writes**: `PUT config`/`prompts`/`skills`/`model`/`providers`/`mcp-*`/`builtin-tools`/`tools`/`shell-risk-rules`/`avatar-config`, `workspaces*` add/remove/modify, manifest upload | Agent-manage | the agent's manage list (rewrites what the agent *is*) |
| **agent definition/metadata reads**: `GET config`/`prompts`/`skills`/`model`/`providers`/`mcp-*`/`tools`/`avatar`/`avatar-file`/`manifest`/`cron`/`status`/`health` | Agent-view | the view list (use ∨ shared) — metadata describing "what the agent is" is offered broadly |
| **content read/write**: `GET/POST/PUT/DELETE /api/agents/{id}/files*`, `git*` (incl. diff/log reads, revert writes), `workspaces` tree/file reads | Agent-use | the use list — these read the owner's real working files (§7 option G: exposing content at view = the whole disk is theftable), so both directions stay at use; a pure viewer cannot read them |
| `GET /api/fs/browse?target={node_id}` | Node-manage | the node's manage list (reads the whole machine's filesystem, not one agent's workspace — sensitive) |
| `PATCH /api/agents/{id}/visibility`, `.../guests`, `.../owner` | Agent-transfer | the agent's ownership list (owner ∨ admin; guests have no ownership power, D9 R1) |
| `GET /api/agents/{id}/permissions` | Agent-view | **the ownership roster is public** (same as node — knowing who to ask) |
| `GET /api/agents`, `GET /api/agents/{id}` (list/detail/avatar/status/health) | visibility filtering | private: the use list may see; shared: all logged-in users may see (visible + read-only); **ownerless = admin-only** (D7 fail-closed) |
| session **reads** (`GET sessions`/`sessions/{sid}`/`messages`/`latest-session`/`stream`) | Agent-view ∧ ADR-076 | view list first; **content is then gated by the session's own private/public flag** (the second wall, Runtime `is_readable_by`) — a private session is absent to a viewer |
| session **writes** (create/open/close/messages/answer/approval/config/workspace-switch/stop/continue/compress) | Agent-use | the use list — opening your own session and chatting *is* the use tier; cross-session safety is the Runtime's per-session `is_writable_by` |
| `DELETE /api/agents/{id}/sessions/{sid}` | Agent-manage | the manage list (destroys the session and its files — destructive) |
| `GET /api/agents/{id}/memory/*`, `GET search`, `POST rag/query` | Agent-use | the use list — retrieval runs a query over the whole corpus and surfaces cross-conversation memory with no private/public wall, so **use, not view** |
| **memory destructive writes** (`POST memory/distill`, `rebuild-embeddings`, `PUT/DELETE memory/nodes*`) | Agent-manage | the manage list (rewrites the agent's knowledge base) |
| `POST /api/agents/{id}/interactions` | Agent-use | the use list — an activity stamp fired inside the chat-send path |

**Two boundary principles must be spelled out**:
1. **`shared` opens only view (visible + read-only metadata), never use.** To open a session and chat, one must be added to the guest list (use tier). A guest driving the agent with its mounted workspace reads/writes files at workspace permission and content may enter conversations — that is the real risk surface and the meaning of the use grant. A user not on the use list can see the list/detail/config metadata but **cannot chat, cannot change the agent definition, and cannot retrieve memory**. Docs and UI must state "shared = visible + read-only; chat and config changes require per-user authorization".
2. **Reads default to view, but "retrieval reads" are the exception and stay at use**: memory reads, global search, and rag/query run a query over the whole corpus and surface cross-conversation content with no per-item private/public wall, so they are use even though they are GETs; whereas config/prompts/skills/model/files/git *definition and metadata* reads are static and go to view.

### D5: The Execution Point Is the Gateway Reverse-Proxy Entry; Runtime Is Not Aware of Owner

**Approach**:
1. After `auth_middleware`, add a lightweight **authorization policy layer**: a static table mapping route patterns → `Permission::{None, NodeManage, NodeTransfer, AgentManage, AgentTransfer, AgentUse}` (Transfer = the ownership list owner ∨ admin, see D4/D9), assembled in [routes.rs](../../../core/acowork-gateway/src/http/routes.rs) (the same style of layer as `restricted_mode_middleware`). The handler takes the identity from `Extension<AuthContext>` (already present, [auth_middleware.rs:29](../../../core/acowork-gateway/src/http/auth_middleware.rs#L29)) and the ownership from the `agent_owners` / `node_owners` tables, producing a 403.
2. `instance_id → node_id` uses the existing `resolve_agent_node_id` ([agents.rs:561](../../../core/acowork-gateway/src/http/agents.rs#L561)); the `{id}` path parameter remains "an instance or package id" — the existing `resolve_agent_identity` resolves it first, then ownership is looked up.
3. Rejection semantics: `403 {"error":"forbidden","code":"not_authorized","resource":"agent|node","required":"manage"}` — it deliberately does not leak whether the resource exists (avoiding existence probing).
4. `?target=` reverse proxy paths also take the permission from the **target** node (not the agent in the path), otherwise switching the target bypasses the gate.
5. **No change to `x-user-id` injection**: session isolation (ADR-076) still flows through it; the owner model simply adds a second dimension on top.

### D6: The `visibility` Field for Agent / Node

- `agent_owners.json.visibility: Private (default) | Shared`, switched by `PATCH /api/agents/{id}/visibility` (ownership list: owner ∨ admin; guests cannot change it, see D9 R1); `GET /api/agents` entries carry `visibility` + the server-computed `can_manage`/`can_use`. The default rules are in D3 (the onboarding default agent is the exception landing as Shared, Q1 decided).
- `node_owners.json.visibility: Private (default) | Public`: `Public` refers **only to metadata** (name/online) being visible to everyone, so a team can know "this GPU machine exists"; **it opens no manage permission and does not open install** (install always requires the node manage list = owner ∨ admin). The only way to let someone install/maintain agents on this machine is for them to be the **node owner or an admin** (a node guest is use-tier and carries no machine-management right); the way to let someone **use** a given agent to chat is to **add them to that agent's guest (use) list**.

**List filtering (option B, revised 2026-10)**: a `private` node is **absent from `GET /api/nodes`** for callers outside the manage list — not merely field-trimmed. Rationale: such a caller has no action available on the row (install / fs browse / rename / LSP config are all Node-manage gated), yet keeping it would leak "this machine exists, is it online, how many agents does it run". Owner / guest / admin keep the row: the sidebar is the only place a node can be acted on, so hiding it would void the guest grant.

**Single decision entry point `ownership::can_view`**: agent list, agent detail and node list **must** share one function rather than each deriving visibility:

```text
can_view = is_admin ∨ can_use ∨ visibility.is_published()
```

- `is_admin` comes first — otherwise an ownerless resource (`owner=None`) would be invisible even to admin, and the D7 claim flow would deadlock.
- `can_use` includes guests: a guest must see the resources they were granted to use.
- A missing record / `owner=None` is **invisible to every non-admin** (the fail-closed extension of D7: if an ownerless resource is admin-only, it must not appear in any normal account's list).

**Two opposite fail-opens now fixed** (together they produced "the agent is listed but cannot be opened, and loads forever"):

| Site | Old implementation | Problem |
|---|---|---|
| `list_agents` | `rec.is_some_and(\|r\| r.visibility == Private) && !can_use` | filtered only rows that **existed** → every unrecorded agent leaked to all accounts |
| `get_agent_detail` | `rec.is_none_or(\|r\| …)` | a missing record counted as **visible** (the opposite of the list) → listed, then 404 on click |

The same rule implemented with two opposite defaults in two places is the classic shape of this class of drift; `can_view` exists so it cannot happen again.

**Third drift fix (the agent list bypassed the node cascade, 2026-10)**: `list_agents` once evaluated `can_view` on the **agent's own row only**, bypassing the `Node → Agent` view cascade defined in §4. The consequence: a `shared` agent hosted on a node the caller **cannot view** still appeared in `GET /api/agents`, yet opening it was 403/404'd at the node tier (detail/use go through the cascade), and client-side per-node grouping found no visible node to file it under. The fix: agent-list reads **must reuse `acl::evaluate(Op::View, Resource::Agent(instance_id))`** — the same decision source as detail/use — so an agent is listed only when both its own row and its hosting node's row pass the view tier; `auth=None` (Local mode) stays a full no-op. "Every listed agent has a viewable node" thus becomes a structural guarantee, and the mobile/desktop per-node grouping needs **no fallback bucket**.
- Entries with no historical `visibility` read as `Private` (fail-closed by default).

### D7: Ownerless Resources, Transfer and Cleanup (fail-closed)

| Situation | Rule |
|---|---|
| Upgrade migration: existing node/agent with no owner record | on first entering multi_user they are **not auto-claimed**. After an admin logs in they see all ownerless resources and claim via `PATCH .../owner`. Ordinary users instantly lose operating power over existing others' agents (which they should never have had) — this is the fix itself, not a regression |
| The owner's account is disabled/deleted | the resource falls back to ownerless (admin-only). Gateway does no cascading delete (the data is on the node; whether to delete it is the machine owner's/admin's decision) |
| The owner wants to hand over | `PATCH /api/agents/{id}/owner {user_id}` is callable by admin only (transfer = an authorization change; the owner themselves can only "share", not "change the master", avoiding unaudited ownership kicking) |
| After uninstall | the `agent_owners.json` entry is deleted when Gateway observes the retained inventory being cleared (dispatch.rs's remove path); orphan entries (whose instance has not been seen for 30 days) are WARNed about at startup and retained (better to keep the audit trail than leave dangling permissions) |

### D8: A Single Source of Truth on the Server + Local Mode no-op

**Single source of truth**: `GET /api/agents` / `GET /api/nodes` deliver, per record, the `can_manage` / `can_use` / `is_guest` / `visibility` computed by Gateway using §4's decision functions; Desktop/Mobile only consumes the booleans for rendering (disabling/hiding manage entries) and **must not** derive them from the `owner`/`guests` lists (continuing ADR-086 invariant 1). The 403 fallback and the boolean delivery must come from the same source (the same decision function, eliminating drift).

**Local mode no-op**: structurally identical to ADR-076 §decision 12: under `AuthMode::Local`, `auth_middleware` produces no `AuthContext` and the authorization layer passes straight through (the policy table exists but is not evaluated); the owner tables are still written (Local also records owner, keeping data for a future switch to multi_user). **No new configuration knob is added** — the single `AUTH_MODE` dial is already an existing decision, and adding an `owner_enforcement=off` style knob would be leaving the back door open on a security fix.

### D9: Ownership Cardinality = Single Owner + Multiple Guests (a guest is use authorization, not co-maintenance)

**Problems**: ① Should owner be split into further permission tiers (read-write vs read-only)? Should non-owners be entirely invisible? ② Are multiple owners allowed (an admin adding an owner for a node)?

**Decision 9a: the permission axis is three tiers (manage/use/view); guests sit in the use tier.**
This model **does** have a read-only tier (view), but view covers only **metadata / definition reads that describe the agent** (detail, status, config/prompts/skills/model reads, avatar, the permissions roster); **interfaces that read the owner's real data** (workspace file content, git history, memory, global search) do **not** fall into view — they sit at **use**, because they surface content on the owner's machine with no session-style private/public wall behind them (see the D4 gate table's "retrieval/content reads go to use" principle and §7 option G's update). A non-owner's two legal forms:
- **guest** (use authorization): opens **their own** session and chats, may `start`, read/write workspace files, retrieve memory/search — but **cannot manage** (agent-defining config writes / workspace add-remove-modify / lifecycle / delete session / permission settings are owner ∨ admin only);
- **viewer** (published visibility): a `shared` agent / `public` node is **visible + read-only metadata** to every logged-in user (list/detail/config-definition/avatar/permissions roster), **with no use and no manage** — they cannot chat, cannot touch any write, cannot read workspace file content / memory.
Plus one pre-existing exception: admin's `as_user` read-only view (ADR-076 §decision 4, retained as-is).

**Decision 9b: the owner is unique; manage cannot be delegated to a non-admin.**
Multiple owners (equal co-ownership) is rejected: who can transfer, who can revoke whom, who adjudicates conflicts — the semantics of ownership collapse, and "who is responsible for this machine" degrades from an anchor into a set, which is exactly the state this ADR aims to eliminate. Under a single owner + guest list: **a guest is use authorization (addable, removable, revocable, with no residue), the owner is ownership (unique, transferred under audit)**. This model **does not support delegating manage to a peer collaborator** — if several people must jointly configure/maintain a machine or instance, that goes through the admin role, not through guests; guests only solve "let more people *use* (chat)".

**The guest's three boundary rules**:

| # | Rule | Meaning |
|---|---|---|
| R1 | **Grants use, not manage/ownership** | a guest may only use (chat) and **cannot manage** (config/workspace/files/lifecycle/install/uninstall), cannot transfer the owner, cannot add/remove guests, cannot change visibility. Manage and ownership operations always have exactly the two parties owner ∨ admin (`can_transfer`, §4) |
| R2 | **No cascade** | a node guest is not a guest of the agents on that node. The node list answers "who may see/use this machine (not machine management)", the agent list answers "who may use (chat) this instance"; the two levels' lists are maintained independently |
| R3 | **Revocation immediately loses access; ownership is not reclaimed** | after being removed as a guest they immediately lose use; but agents they own in their own name are still theirs (ownership is independent of authorization). Uninstalling those agents requires that person / admin, or the target node's manage list |

**Guest list operations**: `PATCH /api/nodes/{id}/guests`, `PATCH /api/agents/{id}/guests` (full-replacement semantics, PUT-list style, so the ownership list is adjustable); `GET /api/agents|nodes` entries deliver `is_guest: bool` (folded into the `can_use` computation; the client does not query the list itself — D8's single source of truth is unchanged).

**Why guests have no sub-tiers (editor/viewer)**: a guest is a single use tier — "can you use (chat)" is binary, and config/files/lifecycle (manage) are never in a guest's grant, always owner ∨ admin only. Visibility (view) is solved globally by visibility (shared/public = visible to everyone), so there is no "chat-only vs can-configure" split to make within guests — the latter simply isn't part of being a guest.

---

## 6. Authorization Flow

```mermaid
sequenceDiagram
    participant C as Desktop / Mobile (logged-in user U)
    participant G as Gateway auth_middleware
    participant P as Gateway authorization layer (new)
    participant R as Runtime / Node
    C->>G: POST /api/agents/{id}/workspaces (Bearer token)
    G->>G: signature verification → AuthContext{user_id=U, role}
    G->>P: route match → Permission::AgentManage
    P->>P: agent_owners[id] → owner, guests, visibility
    alt U == owner ∨ U == admin (manage list; a guest is use-tier and is not in this list)
        P->>R: proxy forward (carrying x-user-id as-is)
        R-->>C: 200
    else not on the manage list
        P-->>C: 403 {code:"not_authorized", required:"manage"}
    end
```

---

## 7. Rejected Options

| Option | Reason for rejection |
|---|---|
| **A. owner enters the proto, reported/echoed by Node** | ownership truth is handed to the data plane (Node inventory can be rewritten by Node), and is overwritten by inventory after a Gateway restart; Node is not aware of the account system. See D1 |
| **B. Owner verification on the Runtime side (Gateway merely passes through)** | Runtime needs a full ownership-table sync + dual node/agent truth copies; Gateway is already the sole multi_user entry point and the existing authorization point (`AuthContext` lives there), so adding a second execution point violates the single source of truth |
| **C. Generic RBAC (a role × resource × action table)** | there are only two permission tiers and two roles today; a generalised framework has no near-term consumer (YAGNI), and client-side rendering complexity explodes |
| **D. per-workspace ACL (workspace-level sharing authorization)** | rule of three not yet triggered; workspace permission naturally follows agent permission (both mean "touching this machine's files"); subdividing waits for a real need |
| **E. Default shared, the owner only nominally in charge** | the opposite of the threat model — this ADR's motivation is precisely "public by default", so the default must be private and fail-closed |
| **F. Auto-assigning existing data to the first admin who logs in** | silently changing ownership with no audit; ownerless + explicit claim is safer, and the cost is just one extra claiming click by the admin |
| **G. Putting workspace file *content* reads into view** | Revised for the three-tier model: view covers only **metadata / definition reads** (config/prompts/skills/model/status/avatar/permissions roster); **interfaces that read the owner's real data** (workspace file content, git history, memory, global search) all sit at **use**, not open to a pure viewer of a shared agent. The rationale is unchanged — `GET /workspaces/file` at view would let any logged-in user drag the owner's working files off the machine; only the implementation changed from "reads and writes both at manage" to "content reads at use, metadata reads at view" (see the D4 table and D9a) |
| **H. A "non-owner read-only" tier that opens all content reads** | Revised: this model **does** have a view tier, but it is **not** "read any file on the machine". A viewer (shared/public, non-guest) sees only what the agent *is* (detail, config definition, avatar, permission roster), not the owner's working-file content, memory, or chat records (session content is separately gated by private/public). If "read-only" meant the whole disk, it would still be node-machine data exfiltration, so content reads must stay at use |
| **I. Multiple owners (equal co-ownership, an admin adding an owner for a resource)** | ownership semantics collapse: transfer/revocation/conflict adjudication has no arbiter, and "who is responsible for this machine" degrades from an anchor into a set. Collaboration needs are carried by the guest list; the real need behind "add an owner" maps into the model as "add a guest". See D9b |

---

## 8. Impact

**Positive**
- The default topology changes from "the whole machine is public to all users" to "the machine belongs to the person who enrolled it"; attack scenarios 1-5 are all closed.
- Sharing changes from "accidental default" to "explicit declaration" (agent visibility).
- Client-side permission rendering is unified to server booleans, eliminating frontend derivation (continuing ADR-086's invariant).

**Costs and risks**
- Two new Gateway local state files (the same pattern as the existing token tables, so the operational surface barely grows).
- On upgrade day: all existing agents become private to ordinary users — the release note must clearly state the "admin claims" step.
- Every agent route proxied through Gateway must enter the policy table; **missing one leaves a hole**. Mitigation: classify() is designed as "deny by default" (an unregistered `/api/agents/{id}/**` route, **any method**, falls to Agent-manage; real read routes must be explicitly registered as view/use), plus a test for that invariant (§9) and the `dev/ci.sh::run_permission_route_redline` enumeration gate.
- The MQTT data plane remains permissive: this ADR does not solve the known "other users' session events fan out to all localhost clients" problem, which needs a follow-up ACL ADR (§13 Q3).
- **Follow-up (not covered by this implementation)**: disabling or deleting an owner account does not automatically invalidate the owner rows of their nodes/agents. Current behavior is fail-closed (`can_manage` naturally 403s for a disabled account, leaving the resource effectively ownerless), but the explicit "deactivate ⇒ bulk-ownerless + audit" linkage is missing and must be designed together with the account-deletion flow (acowork-user).
- **Fixed (the visibility switch silently snapping back)**: the `ownerless ⇒ not Shared` normalisation in `OwnershipStore::upsert_with` ran *after* the caller's mutation, so setting an ownerless agent to `shared` made the handler answer `200 {"visibility":"shared"}` while the store kept `private`; the Desktop dialog's post-save `load()` then re-read the old value and the switch snapped back with no error. The fix has two halves, both necessary: ① `PATCH .../visibility` now rejects up front when the target is ownerless and the request is to publish it (agent `shared` / node `public`), returning `409` whose message gives the two-step recovery (claim via `PATCH .../owner` first, then set visibility); ② the Desktop permission dialog rolls its draft back to the server truth when a save is rejected, so a refused write no longer looks identical to a write that never happened. The normalisation itself is kept — it is a data invariant, not an authorization decision (`can_view = is_admin ∨ can_manage ∨ shared` already keeps an ownerless resource invisible to everyone else), and removing it would let an ownerless agent escape fail-closed through `visibility`. Regression tests: `patch_agent_visibility_rejects_shared_on_ownerless_row` / `node_visibility_public_persists_once_an_owner_is_claimed`, plus the frontend `reverts the switch to the server value…`. **UI path added (a gap in the first fix)**: the 409 message prescribed a two-step recovery, but step one required a hand-written PATCH request, and the dialog rendered the owner read-only with no claim control at all — so for every pre-existing resource (ownerless is the post-upgrade default) the recovery path was unreachable, which is a dead end with better wording. Added `patchAgentOwner`/`patchNodeOwner` wrappers plus an admin-only "take ownership" button in the dialog (`canClaim = isOwnerless && account.role === admin`); after claiming, the dialog reloads, and an owned resource no longer shows the button. The claim is a separate call (not part of Save), so the "claim first, then set visibility in a second step" path still exists — the button just shortens the route the 409 already prescribed into one click.

---

## 9. Verification Plan

1. **Unit tests (Gateway)**: the policy table as a pure function — the full route × role × ownership → allow/deny matrix; with emphasis on regressing "an unregistered route is denied by default".
2. **Structural invariant test (into `dev/ci.sh`, alongside `run_gateway_fs_redline`)**: enumerate all `/api/agents/{id}/**` and `/api/fs/browse` routes registered in `proxy.rs`/`agents.rs`/`fs_browse.rs`, and assert that each has an explicit tier in the policy table or falls into the default-deny bucket — **a new route that isn't registered turns CI red**.
3. **e2e (multi_user)**: alice installs an agent on node A and sets it private → bob `POST workspaces` 403 (write = manage) / `GET workspaces/file` 403 (content read = use, non-guest denied) / `GET fs/browse?target=A` 403; alice marks it `shared` without adding bob as a guest → bob sees the agent and may `GET config`/`GET model`/`GET permissions` (metadata/definition reads = view allowed) but `POST sessions` returns **403** (shared opens visibility + read-only, not use), and `GET files`/`GET memory` **403** (content reads at use); alice then adds bob as an agent guest → bob `POST sessions` 201, `GET files` 200, `GET memory` 200 (use allowed) and bob's session is still isolated from third parties other than alice per ADR-076; admin has full access + claim + transfer.
4. **guest semantics (D9's three rules asserted one by one)**: alice adds bob as an agent guest → bob `POST sessions` returns 200 (use allowed) but `POST workspaces` returns **403** (a guest has no manage) and `PATCH guests/visibility/owner` returns 403 (R1); bob added as a guest of node A → bob `POST install@A` returns **403** (a guest has no machine-management right; installing needs the node owner ∨ admin); after alice revokes bob's guest, bob's use calls (sessions) immediately 403 (no cached residue).
5. **Local mode regression**: under `AuthMode::Local` all behaviour is byte-identical to pre-upgrade (the no-op assertion).
6. **Migration rehearsal**: upgrading with existing data → an ordinary user's list only contains their own visible items; after an ownerless resource is claimed by the admin, functionality is restored.
7. **e2e (Desktop + Mobile)**: non-owner/non-admin manage entries are disabled per `can_manage=false`; a guest's (use authorization) chat path works smoothly while a non-guest sees a shared agent but cannot chat (use writes 403); the guest badge and list-management UI are usable.

---

## 10. Implementation Breakdown (by module, for scheduling)

| Step | Content | Involves |
|---|---|---|
| 1 | `node_owners.json` / `agent_owners.json` storage (atomic write + in-memory mirror, following enrollment.rs) | a new `ownership.rs` module under `acowork-gateway/src/gateway/` |
| 2 | enroll binds the owner (token record + `decide_enroll` writes the table); `POST /api/nodes/enrollment-tokens` | `mqtt/enrollment.rs`, `mqtt/dispatch.rs`, `http/nodes_api.rs`, `cli.rs` |
| 3 | install/ensure/clone write the agent owner; uninstall / retained-clear cleanup | `http/agents.rs`, `mqtt/dispatch.rs` |
| 4 | the authorization policy layer (deny by default + explicit tiers) + structured 403 errors; `can_manage`/`can_use` delivery | `http/routes.rs`, `http/proxy.rs`, `http/agents.rs`, `http/fs_browse.rs` |
| 5 | visibility + guests: `PATCH /api/agents/{id}/visibility`, `PATCH /api/agents/{id}/guests`, `PATCH /api/nodes/{id}/visibility`, `PATCH /api/nodes/{id}/guests` (ownership-list adjustable), `PATCH .../owner` (admin); list filtering includes the guest dimension; `can_manage`/`can_use`/`is_guest` delivery | the same as above |
| 6 | the ci.sh route-registration invariant test + integration/e2e | `dev/ci.sh`, `tests/` |
| 7 | Desktop: the device-add wizard switches to the HTTP enrollment token; AgentList/NodeList consume the booleans; the owner badge + claim UI; Mobile synchronously consumes `can_manage`/`can_use` (aligning with ADR-086 decision 11's "single source of truth on the backend") | `apps/acowork-desktop`, mobile |

Steps 1-4 are the minimal closed loop of the security fix (plug the hole as soon as it ships); 5-7 can follow.

---

## 11. Compatibility Red Lines

- The `NodeInfo` / `InstalledAgentInfo` proto fields are not changed (ownership does not enter the data plane); the MQTT topic structure is unchanged.
- No change to any session-dimension semantics (ADR-076 §decision 4 remains valid as-is); the `x-user-id` injection mechanism is untouched.
- No change to Gateway's red lines (ADR-009 §5): all authorization happens at the reverse-proxy entry, and Gateway still does not touch Agent-private files.
- No compatibility layer: under multi_user an unregistered route is directly 403, never silently allowed ("a silent fallback masks an insecure state" is itself the anti-pattern this ADR aims to eliminate).

---

## 12. Open Questions

| # | Question | Status |
|---|---|---|
| Q1 | **Ownership of the default agent (ADR-077)**: onboarding installs it as the first user → owner = that user, and by default other users cannot use it (unless added to the guest list). Should the "default agent installed by onboarding" default to `shared` (out-of-the-box **visibility** for teams)? | ✅ **Decided (2026-10-28): `Shared` by default**. The default agent preinstalled by onboarding lands as shared (visible to everyone), and the owner may switch it back to private; instances the user installs themselves keep the default private. **Note `shared` opens visibility only; chatting is still authorized per guest** (see D6) |
| Q2 | **Where an admin's install lands**: when an admin installs an agent onto user X's node, does it require X's manage authorization? Or can an admin naturally install on any node? | ✅ **Decided (2026-10-28): an admin may install on any node**. An admin naturally passes all Node-manage/Agent-manage gates, with no `admin_overrides_nodes` knob added (no near-term need → no config surface, YAGNI). Ownership is still recorded as the admin's own, and the node owner can later reclaim it via an admin transfer |
| Q3 | **MQTT data-plane ACL**: permissive subscribe causes cross-account event fan-out (the known e2e flake root cause). After the owner model lands, the ACL's subscription filtering rules (`user:{id}` may only subscribe to events for resources visible to them) should align with it | A separate ADR (orthogonal to this ADR's HTTP control plane; does not block this ADR) |
| Q4 | **Cost attribution of agent use**: when a guest uses my agent, it burns my provider key / quota. Should the budget (budget tracker) account per caller or cap per agent? | ✅ **Decided (2026-10-28): account per agent, not per user**. Usage belongs to the agent (hence to its owner's key/quota), and the budget tracker stays at agent granularity; caller-level apportionment / caps are revisited when a real need appears |
| Q5 | **Read endpoints such as `GET /api/agents/{id}/avatar`**: for a private agent, should a non-owner get a 404 or be allowed? | Recommend 404 (the list is already filtered, avoiding existence probing); decided at implementation time |

---

## 13. Reference File Index

| File | Relevance |
|---|---|
| [core/acowork-gateway/src/http/routes.rs](../../../core/acowork-gateway/src/http/routes.rs) | where the policy layer is assembled alongside `restricted_mode_middleware`; the main battleground for the policy table |
| [core/acowork-gateway/src/http/proxy.rs](../../../core/acowork-gateway/src/http/proxy.rs) | all `/api/agents/{id}/**` proxy routes; the policy table's main battlefield |
| [core/acowork-gateway/src/http/agents.rs](../../../core/acowork-gateway/src/http/agents.rs) | install/ensure/clone/start/stop/uninstall; the owner write points |
| [core/acowork-gateway/src/http/fs_browse.rs](../../../core/acowork-gateway/src/http/fs_browse.rs) | the `?target=` proxy; Node-manage gating |
| [core/acowork-gateway/src/mqtt/enrollment.rs](../../../core/acowork-gateway/src/mqtt/enrollment.rs) | the token record gains `owner_user_id`; the persistence pattern template for ownership.rs |
| [core/acowork-gateway/src/mqtt/dispatch.rs](../../../core/acowork-gateway/src/mqtt/dispatch.rs) | `decide_enroll`, the installed-inventory aggregation (the timing of owner binding / cleanup) |
| [core/acowork-gateway/src/mqtt/node_registry.rs](../../../core/acowork-gateway/src/mqtt/node_registry.rs) | the Node online view; the `can_manage` rendering input |
| [core/acowork-memory/src/session_meta.rs](../../../core/acowork-memory/src/session_meta.rs) | the reference for the session-dimension decision pattern (`is_readable_by`/`is_writable_by`) |

---

## Appendix A: Revision — a 409 is not a UI contract (2026-10)

**Trigger**: a multi_user deployment. Signed in as admin, right-clicked an agent →
Permissions → toggled visibility on → Save, and got
`409 this agent has no owner yet…` (`owner_user_id: null` is the default state of every
pre-existing resource — see the upgrade risk in §8).

**Diagnosis**: the 409 was not lying. `upsert_with`'s `ownerless ⇒ not published`
normalization really does silently roll that write back. But it **packaged a
predictable data state as a runtime error**, and it ignored who was asking: the very
admin who is the only role allowed to claim was the one the 409 blocked.
D8 already said the client renders server-computed booleans only, yet
`GET .../permissions` shipped just `can_attribute` — a **permission** answer, which is
`true` for an admin even on an ownerless row. The UI therefore enabled the switch as
usual and the user had to click to discover the write was impossible. Permission and
data state had been conflated; the 409 was the symptom.

**Revision** (a completion of D8, not a new decision):

1. `ownership::can_publish` / `ownership::is_ownerless` become shared decision functions.
   The normalization rule, the `permissions` payload, and the two `PATCH .../visibility`
   guards each rolled their own version of the same question; they now share one.
2. `GET /api/{agents,nodes}/{id}/permissions` gains `can_set_visibility` + `ownerless`.
   The client disables the switch and states why (`needsOwnerHint`) instead of letting
   the user find out by clicking.
3. The 409 stays, as the non-UI fallback only — it stops a direct HTTP caller from
   dressing the silent normalization up as success. Its message no longer tells the
   user to hand-write a PATCH; end users should not be asked to open devtools.
4. The dialog keeps its admin-only "claim ownership" action
   (`canClaim = isOwnerless && role === admin`).

**Explicitly rejected**: folding "and make it visible" into the claim. An ownerless row is
private by construction, so defaulting it to published silently widens who can reach the
agent. Saving a click is not worth an authorization change made on the admin's behalf;
claim and publish stay two explicit actions, matching D7's two-step recovery.

**Open**: migrating ownership for existing ownerless rows is still undesigned (§8 lists
it as an upgrade risk; this change only makes the recovery reachable and silent errors
impossible). The migration has to answer "who owns this machine / these agents", which
ADR-087 deliberately leaves to the deployer.

**Regression tests**: `permissions_reports_an_ownerless_row_as_unpublishable` /
`permissions_reports_an_owned_row_as_publishable` (server data state),
`disables the visibility switch instead of offering a write that 409s` /
`keeps an owned resource's switch enabled` / `claims ownership without also changing visibility` (UI).

## Appendix B: Revision — ownerless resources are no longer produced by construction (2026-11)

**Trigger**: Appendix A fixed *recovery* for rows that already lack an owner, but new ownerless
rows kept being manufactured. Seven production paths were identified:

| Path | Scenario | Prior behavior |
|---|---|---|
| N1 | Gateway auto-spawns the local node at startup | `create_token(3600, None)` → ownerless node |
| N2 | CLI `nodes token create` | no user context → ownerless token → ownerless node |
| N3 | bare enroll with `mqtt.auth_enabled=false` | no token to resolve → ownerless |
| N4 | re-enroll via `put_if_absent` | stays ownerless forever (frozen) |
| A1 | CLI `install` dispatched over MQTT | bypasses owner writing entirely |
| A2 | Local-mode install/ensure/clone | `ctx=None` (**correct**, see below) |
| A3 | onboarding default agent installed before login | no caller → ownerless |

**Principle**: owner binding happens at the **user-interaction layer**. An enrollment token is a
runtime-internal artifact — users must never fish it out of logs or config files and paste it
into a command line. Except for the one unavoidable scenario (starting the Gateway from a CLI on
a server — the operator is necessarily an admin), every other scenario closes the loop inside
the Desktop app.

**New rules** (revising D2 item 4 and D7 row 1):

1. **Two convergence points guard the backstop**: when an enroll is accepted, and when an agent
   first lands in the inventory (`commit_pending`), an unresolvable owner falls back to
   `default_owner` — the earliest-created admin `user_id`, resolved by the Gateway at startup via
   the user service (`acowork-user first-admin`). One guard covers every bypass.
2. **N1**: the enrollment token for the Gateway-spawned local node is bound to admin directly
   (server scenario — reasonable).
3. **N2/A1**: CLI `nodes token create` / `install` default to admin under multi_user.
4. **Joining a new machine (replacing token-fishing)**:
   - Desktop: the agent-list `+` menu gains "Create Local Node" → the Tauri command internally
     calls `POST /api/nodes/enrollment-tokens` (Bearer = signed-in user, so owner = that user),
     then spawns the bundled `acowork-node` (detached). The token never surfaces in any
     user-visible place. Idempotent: an already-enrolled machine skips the token step — the
     action degrades to plain "start".
   - Headless CLI: `acowork-node start` without identity and without `--token` prompts for
     username/password, calls `POST /api/auth/login` → `POST /api/nodes/enrollment-tokens` itself,
     and enrolls with the bound token.
5. **Claim endpoints**: `POST /api/nodes/{id}/claim`, `POST /api/agents/{id}/claim` — only
   ownerless rows can be claimed; the local node is relaxed to any signed-in user (the person is
   physically at the machine), remote nodes/agents stay admin-only.
6. **Legacy adopt**: at startup, if an admin resolves, all ownerless rows are adopted once
   (marker-gated, runs once); any ownerless rows still left afterwards produce a summarized
   startup WARN (node/agent counted separately).

**Local mode unchanged** (A2 is not a bug): single-machine loopback → `resolve_auth_mode` = Local,
the account system is off entirely, `owner=None` is the correct semantics — "the machine is the
owner" (D8 no-op). The local → multi_user transition is handed over by rule 6.

**D7 after revision**: ownerless narrows from "a possible birth state" to a **transient state** —
produced only when an owner account is disabled/deleted; every construction path binds an owner.

**Regression tests**: `enroll_with_ownerless_token_falls_back_to_default_owner`,
`installed_landing_without_staged_row_gets_default_owner` (convergence-point backstop),
`adopt_ownerless_binds_all_null_rows` (legacy handover), plus four claim-endpoint cases
(ownerless claimable / owned 409 / local relaxation / remote admin-only). Full Gateway suite: 619 passed.
