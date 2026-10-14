# Mobile App

> Version: v1.3 | Updated: 2026-10-12
> Status: design confirmed (v1.1 added Startup & Connection §7, Realtime Event Stream §8, edge states §7.5; v1.2 revises §8 into a dual-channel model — relay deployments use precise MQTT-over-WSS subscriptions, polling becomes the local/LAN channel; v1.3 mounts the same WS bridge on the MAIN LAN listener so both shapes have equal rights, and adds the global presence connection); engineering in development

---

ACowork Mobile App is a Tauri v2 based mobile client, positioned as a **portable operating terminal** for ACowork. It is not a port of the Desktop App — it is a second terminal with a **reduced feature set and a different shape**.

## 1. Positioning and Responsibilities

### 1.1 In one sentence

**Mobile App = an IM-shaped conversation control terminal**: it carries "conversations with Agents and colleagues" using the interaction paradigm of mainstream IM software (WeCom / DingTalk), does basic conversation control in v1, and takes on no platform-management duties.

### 1.2 Relationship to Desktop App

| Dimension | Desktop App | Mobile App |
|-----------|-------------|------------|
| Positioning | Primary workbench — create/debug/publish Agents | Portable terminal — receive and send messages anywhere |
| Shape | Left/center/right multi-column | Bottom tab bar + full-screen hierarchy (IM shape) |
| Navigation | Single nav bar + left list | One independent stack per tab + iOS push semantics |
| Sessions | An Agent may have **multiple Session Tabs open in parallel** | An Agent has multiple sessions, **switched serially** (see §5) |
| Agent management | Create/install/clone/publish | Not in v1 |
| Debugging | DevMode protocol, Git status bar | Not in v1 |
| Local Gateway | Embeds/manages a Gateway process | **No local Gateway process** (see §11.1) |

**Key insight**: the two Apps share the same server-side state (same Agents, same Sessions, same permission model), but **their client state is independent**. A session opened on Desktop can be continued on the phone; a session created on the phone is visible on Desktop (subject to permissions). Mobile is never a "remote display" for Desktop — it is a peer terminal.

### 1.3 What Mobile App does not do (v1)

| Not doing | Why |
|------------|-----|
| DevMode debugging protocol | Requires local filesystem and process-level control; a phone has neither |
| Git status bar | Depends on live workspace fs watching (ADR-078); no local workspace on mobile |
| Agent create/install/clone/publish | Occasional developer operations — stay on desktop |
| Provider / API Key / MCP / Embedding management | Harness's job, involves secrets; a phone should not hold them |
| Rich-text editor | Document editing needs a large screen and a physical keyboard |
| Harness / Extensions navigation | Developer concepts; ordinary users do not need them |
| Local Gateway process management | Mobile does not run a Gateway (§11.1) |
| Push notifications | v1 has no background channel: a mobile WebView is suspended seconds after backgrounding (§11.3), which freezes even the relay MQTT connection — there is no "delivered while closed". Vendor push (APNs/FCM) needs its own server and certificate system — a v2 evaluation item. Foreground freshness is carried by the §8 dual channel and refresh-on-resume |
| Registration / invites | Account registration stays on Desktop and the invite chain (ADR-076 §Decision 6); mobile only **logs in** and, when registration is open, points the user to Desktop |
| QR pairing | Needs a Desktop-generated one-time pairing code; v1 uses manual address entry (§7.2), pairing is v1.1 |

> **Note**: the table above means "not in v1", not "never". Multi-user collaborative document editing (Yjs, ADR-079) is evaluated after v2.

## 2. Information Architecture (IA)

### 2.1 Bottom Tab Bar (fixed, four items)

```
┌─────────────────────────────────┐
│  ← Back      ADR-085…      ⌄  ⋯ │  ← detail page: tab bar slides away
│                                  │
│         (content)                │
│                                  │
├─────────────────────────────────┤
│  ┌──────┬──────┬──────┬──────┐  │
│  │ Chat │Tasks │ Docs │Config│  │  ← root pages only
│  └──────┴──────┴──────┴──────┘  │
└─────────────────────────────────┘
```

| Tab | Desktop counterpart | Notes |
|-----|--------------------|-------|
| Chat | AgentList + UserList + ChatPanel | Primary entry point; ~80% of daily use |
| Projects | ProjectsView | Read-only board + task transitions |
| Documents | DocsView | Directory tree + document reading + review |
| Settings | SettingsPage | Secondary settings pages |

**Harness and Extensions do not enter mobile navigation.** The settings page keeps greyed-out "desktop only" entries as placeholders, so users know the features exist and are available elsewhere — honesty beats silence.

### 2.2 Navigation model: one independent stack per tab

```
Tab stacks (in-memory; depth preserved across tab switches)
chat:     [Conversation list] → [Chat detail]
projects: [Project list] → [Board] → [Task detail]
docs:     [Document tree] → [Document content] → [Review detail]
settings: [Settings root] → [Profile|General|Appearance|Gateway]
```

| Rule | Behavior |
|------|----------|
| Enter a detail page | The bottom tab bar **slides away entirely** (iOS / WeChat behavior) |
| Switch tab | Each tab keeps its own stack depth; leaving a chat and returning lands you in the same chat |
| Tap the current tab again | Return to that tab's root (clear the stack) |
| Back | Pop one level; the tab bar slides back in |

**Why the tab bar hides on detail pages**: neither iOS nor WeCom shows a bottom bar on a second-level page. Showing both leaves the user facing two navigation exits — bottom tabs and the top-right back button — with no guidance on which to use. Hiding it leaves exactly one back path (top-right, or the right-swipe gesture).

### 2.3 Chat home = a unified conversation stream

Desktop has two separate lists (AgentList and UserList). Mobile merges them into a **single IM conversation stream**:

```
┌─────────────────────────────────┐
│  🔍 Search agents or contacts     │
├─────────────────────────────────┤
│  AGENTS (7)                      │
│  ┌───┬──────────────────────┐   │
│  │ ● │ Architect      14:32  │   │
│  │   │ ADR-085 state machine…│ 2 │
│  └───┴──────────────────────┘   │
│  ┌───┬──────────────────────┐   │
│  │ ● │ Senior Eng.    13:05  │   │
│  │   │ cargo test all green…│   │
│  └───┴──────────────────────┘   │
│  CONTACTS (4)                    │
│  ┌───┬──────────────────────┐   │
│  │ ● │ Alice            15:10│  │
│  │   │ embedding, 4 dims…   │ 3 │
│  └───┴──────────────────────┘   │
└─────────────────────────────────┘
```

Each row shows: avatar (with a working dot), name, online status, **most recent conversation summary**, timestamp.

The avatar corner dot carries exactly one meaning: **working** (a green dot while `AgentSummary.status === 'busy'`, nothing at all when idle).
There is no read/unread badge — with no push channel in v1 a per-agent "unread" count has no
reliable semantics, and a dot that lies is worse than no dot. Only the deterministic working
signal is kept, matching the desktop sidebar.

**The summary comes from the last message of the most recently active session** — not a fixed Agent-level string. IM semantics demand it: what the user wants to know is "where did we leave off".

## 3. Gestures and Navigation Interaction

| Gesture | Scope | Behavior |
|---------|-------|----------|
| Swipe left → | Chat detail page | Open the Agent settings drawer (Desktop RightPanel) |
| Swipe right ← | Detail page | Go back one level (pop) |
| Swipe right ← | Root page | **No action** |
| Vertical swipe | Anywhere | Yields to page scrolling (does not trigger navigation) |
| Horizontal swipe | Detail page top (horizontal list areas) | Yields to the child component |

**Detection rule**: a horizontal gesture is recognized only when `|dx| > |dy|` and `|dx| > 6px`; the commit threshold is 38% of screen width or sufficient velocity (<300ms). The chat page must resolve between "swipe left opens drawer" and "swipe right goes back" based on the **actual direction** — this is the branch most easily implemented wrong.

## 4. Chat Detail Page

### 4.1 Layout

```
┌─────────────────────────────────┐
│  ←     ADR-085 Lifecycle SM  ⌄  ⋯ │  ← title = session switcher
│        gpt-5 · 24 messages       │
├─────────────────────────────────┤
│  Today 14:02                     │
│                                  │
│              ┌─────────────┐    │
│              │ help me look│    │  ← my message (right)
│              └─────────────┘    │
│  ┌──────────────────────────┐   │
│  │ ⚡ file_read  412ms       │   │  ← tool-call card
│  │ docs/adr/zh/ADR-085…      │   │
│  └──────────────────────────┘   │
│  ┌─────┐                        │
│  │ Arch│ two ambiguities: 1)  │  │  ← Agent message (left)
│  └─────┘ rollback 2) RESTART… │   │
│                                  │
│  ┌──────────────────────────┐   │
│  │ ⚠ Confirm: run shell     │   │  ← approval card
│  │ cargo test -p …          │   │
│  │        [Deny]  [Allow]   │   │
│  └──────────────────────────┘   │
├─────────────────────────────────┤
│ [⚙] [Send a message…      ] [🌐] [🙂] [↑] │
└─────────────────────────────────┘
```

### 4.2 Agent settings drawer (swipe left)

Corresponds to Desktop's RightPanel, with six sections: Status / Workspace / Memory / Tools / Config / Sessions.

Mobile uses **native-feeling components** (inset group lists, Switch, Segmented, Stepper, select dropdowns, Action Sheet) rather than scaled-down Desktop controls.

**The workspace section offers Add to Chat only — no file preview.** Mobile has no Monaco, and file content is better read by the Agent inside the conversation. This boundary also honors [ADR-009 §5](../../adr/zh/ADR-009-gateway-workspace-isolation.md): mobile never touches the filesystem directly, going only through the Gateway reverse proxy.

### 4.3 Message elements

| Element | Mobile treatment |
|---------|------------------|
| User / Agent message bubbles | Kept, with Markdown rendering |
| Tool calls | Collapsed card, collapsed by default (narrow screen); data comes from the `GET /messages` reload, not an event stream |
| Approval card | Allow/deny buttons kept (`POST .../approval`); card detail is tiered by channel — full (tool/risk/reason) over WS, generic over polling (§8.6) |
| AskQuestion card | relay/WS channel renders the question card (`ask_question` event + `POST …/answer`); the polling channel degrades to a "handle this on Desktop" hint (§8.6) |
| Streaming output | **Not in v1** (§8): a status indicator carries "working", content arrives complete |
| Code blocks | Horizontal scroll, **no syntax highlighting** (no Monaco) |
| Image / file attachments | Read-only display, no previewer |
| Think / Compaction cards | Collapsed to a one-line summary |

## 5. Multi-Session Model (core design)

> The first interaction prototype suggested "one active session per Agent". That was **rejected**. Multiple sessions per Agent is a business fact; Mobile is only one of several operating terminals — dropping multi-session would make the product unusable.

### 5.1 Why one session per Agent is not acceptable

An Agent being driven by several topics at once is normal: the architect is writing an ADR in one session, reviewing someone else's PR in another, and debugging a production issue in a third. Desktop carries this with **parallel Session Tabs**. A 390pt-wide phone screen cannot fit N side-by-side tabs — but that is a reason **"tabs are the wrong container"**, not a reason **"multiple sessions are not needed"**.

### 5.2 Container: the nav-bar title is the session switcher

```
┌─────────────────────────────────┐
│  ←      ADR-085 Lifecycle SM   ⌄  ⋯ │
│        gpt-5 · 24 messages       │  ← tap here
└─────────────────────────────────┘
              ↓ tap
┌─────────────────────────────────┐
│      Architect · Sessions (8/12)  │
│  Loaded the latest 8; scroll for more│
├─────────────────────────────────┤
│ ✓ ADR-085 Lifecycle Review    🌐  │
│   ADR-085 state machine review…  │
│ ─────────────────────────────────  │
│   Relay Degradation List     🌐 RO│
│   node-local degradation table…  │
│ ─────────────────────────────────  │
│   ADR-084 User Process        🔒  │
│   ADR-084 finalized, awaiting…   │
│ ─────────────────────────────────  │
│   Vault Key Rotation         🌐 RO│
├─────────────────────────────────┤
│        ＋ New session              │
│        Manage all sessions         │
│        [   Cancel   ]              │
└─────────────────────────────────┘
```

| Design point | Rationale |
|--------------|-----------|
| Title shows `session.title` | Not `agent.name` — users relate to topics, not bots |
| Each row shows 🌐/🔒 | Visibility marker (§6) |
| Read-only rows show a "read-only" badge | States clearly that writing is unavailable |
| ✓ marks the current session | Positional awareness |
| Scrollable list + "8/12" pagination hint | Mirrors `agentStore.fetchSessions(agentId, page)` |
| "Manage all sessions" at the bottom | Jumps to the drawer's Sessions section (bulk management) |

### 5.3 Switching sessions is an atomic transition

Switching is **not** a local page change; it must follow the full flow (mirroring Desktop's `chatStore.openSession`):

```
User taps a session row
      │
      ├─→ ① Frontend switches the current session + reloads message history
      ├─→ ② POST /api/agents/{id}/sessions/{sid}/open (ADR-038 activation, idempotent)
      └─→ ③ In parallel: fetch session config + state
```

**Read-only sessions skip ②.** Activation is per-session global lifecycle state: a viewer has no right to activate (close is write-gated on purpose, and the owner cannot see that anyone is holding it), and read-only viewing does not need activation — history comes from `GET /messages`. read-only freshness rides the same channel as writable sessions (§8 dual channel — the subscription test is "readable", not "writable").

### 5.4 Session management (drawer → Sessions section)

| Capability | Supported | Notes |
|------------|-----------|-------|
| List | ✅ | Paginated, "8/12" form, can load more |
| Create | ✅ | Defaults to Private (§6.4) |
| Switch | ✅ | Equivalent to §5.2 |
| Delete | ✅ | Writable sessions only (owner/admin) |
| Rename | ❌ v1 | Requires desktop |
| Toggle visibility | ✅ | The 🌐/🔒 control in the composer |

## 6. Multi-User Permission Model

> The full derivation is in [ADR-076](../../adr/zh/ADR-076-multi-user-account-system.md) §决策 4. This section defines only how mobile **consumes** it.

### 6.1 The backend's `can_write` is the single source of truth

```rust
// core/acowork-runtime/src/conversation.rs
pub struct SessionListView {
    pub session_id: String,
    pub visibility: Option<SessionVisibility>,  // public / private
    pub can_write: bool,                        // ← the only authority
}
```

**Mobile must consume `can_write` as-is and must never re-derive write permission from the `visibility` label.** Public means everyone can read, but normally only the owner can write. A 🌐 public session can perfectly well be read-only.

The predicate (isomorphic to Desktop's `isReadOnlySession`):

```ts
// Read-only applies only when can_write === false; a missing field means writable.
// The degradation direction is "let the backend reject" rather than "lock every control":
// an optimistically created session, or an older Runtime that omits the field,
// should not leave the user facing a row of dead buttons.
function isReadOnly(canWrite: boolean | undefined): boolean {
  return canWrite === false;
}
```

### 6.2 Visibility / writability matrix

| scope \ session | public | private |
|---|---|---|
| admin | read + write | read + write |
| owner | read + write | read + write |
| other user | **read (read-only)** | **invisible** (404) |
| local (headerless) | read + write | read + write |

Mapping to mobile UI:

| State | Mobile behavior |
|-------|----------------|
| Writable | Normal composer + 🌐/🔒 toggle |
| Read-only (`can_write === false`) | Read-only banner on top + approval/AskQuestion cards hidden + composer replaced by a read-only line |
| Invisible (private, not owner) | The session never appears in the list (the backend filters before pagination) |

### 6.3 Concrete handling of the read-only state

```
┌─────────────────────────────────┐
│ 🔒 Read-only · shared by Alice; you cannot send messages │
├─────────────────────────────────┤
│           (message list, read-only)│
├─────────────────────────────────┤
│ 🔒 Shared by Alice · read-only   │  ← replaces the textarea
└─────────────────────────────────┘
```

| Control | When read-only | Why |
|---------|----------------|-----|
| textarea | **not rendered** | A placeholder that looks typeable is a worse experience |
| Send / tools / attachments | not rendered | No write path exists |
| Approval / AskQuestion cards | **hidden** | They would make a viewer decide on the owner's behalf |
| Visibility icon | **disabled, not hidden** | A viewer still needs to know what they are looking at |
| Change visibility / delete | Menu items disabled + reason sub-label | Not "click, then get a toast" |

**Why the visibility icon is disabled rather than hidden**: this matches Desktop's existing `SessionVisibilityToggle` rule — a viewer must be able to see that what they are reading is a private session. Hiding the icon would strip them of that information.

### 6.4 Default visibility for new sessions

New sessions default to **Private**, matching ADR-076's `create_frontend_session`, which writes `Some(Private)` alongside `user_id`. Mobile submits private up front, avoiding any "newly created but publicly readable by bob" window.

## 7. Startup and Connection

> This section covers "from install to first message". Mobile has **no local Gateway
> process** (§11.1), so connection and authentication are a precondition of every
> feature and must be an explicit state machine, not error-handling scraps.

### 7.1 Startup state machine

```
Cold start
  │
  ├─ no saved Gateway address ───────────→ [Connect screen] (enter address)
  │                                          │ probe GET /api/status
  ├─ address saved ──→ probe /api/status ─fail→ [Disconnected screen] (last address + retry/change)
  │                  │ok
  │                  ├─ requires_setup=true ─→ [Restricted notice] ("Gateway not initialized — finish setup on Desktop")
  │                  ├─ no valid token ─────→ [Login screen]
  │                  └─ token present ──────→ main UI (validity proven by the first 401 triggering refresh)
  │
[Login screen] ── POST /api/auth/login ──ok──→ persist token pair → main UI (realtime channel established lazily per §8.0: subscribing/polling starts only when a chat detail opens)
                     └──fail──→ inline form error (wrong credentials / unreachable)
```

**Principle**: every state has exactly one labeled exit; no "tap and see". `/api/status` is public (no token) and returns `auth_mode / registration_open / requires_setup / version` — one probe per launch is enough.

### 7.2 Connect screen (first launch / change address)

| Element | Rule |
|---------|------|
| Address input | Single-line URL; `http://host:port` (LAN) and `https://<gw-id>.<relay-domain>` (relay device domain — the same value Desktop uses as its Gateway URL); pasted values trimmed. **The scheme picks the protocol and port of the realtime channel** (§8.0): https → `wss://{authority}/mqtt`, http → `ws://{authority}/mqtt`; both are the MQTT-over-WS primary |
| LAN discovery | **No mDNS/auto-scan in v1.** On multi-NIC machines the advertised address often is not the reachable one (container bridge / WSL / VPN adapters broadcast wrong IPs), and scanning adds a needless permission surface on mobile. v1 substitutes "Desktop shows its LAN IP + manual entry" (Settings → Gateway); pairing codes in v1.1 remove even this manual step |
| Validation | `GET /api/status` must succeed before saving; failures show the concrete cause (DNS / timeout / not an ACowork Gateway) and are **not persisted** |
| Version display | After a successful probe show the Gateway version so users confirm the right instance |
| `auth_mode=local` | "This Gateway runs in local mode; mobile requires multi-user mode" — login is blocked |
| Change entry | Settings → Gateway → server address (same screen reused) |

### 7.3 Login screen

| Item | Rule |
|------|------|
| Fields | Username + password; `EnterNext` flow; submit button enters loading, no double submit |
| Errors | 401 → inline "wrong username or password" (not distinguishing "user not found", matching backend semantics); network error → inputs kept + retry |
| `registration_open=true` | Footer hint "please register on Desktop" (v1 mobile does not register, §1.3) |
| Lockout | Backend policy (ADR-076); mobile only surfaces the error text, no local counters |
| Forgot password | v1: "ask an admin to reset on Desktop"; the invite chain stays on Desktop |

### 7.4 Token lifecycle (same shape as Desktop)

```
access_token (15 min)  ── Authorization: Bearer on every request ──┐
refresh_token        ── only for POST /api/auth/refresh ──────────┘
persisted via tauri-plugin-store (native) / localStorage (browser dev)
```

| Rule | Behavior |
|------|----------|
| 401 → refresh ladder | Any 401 → exchange refresh_token for a new pair → **replay the original request once**; refresh also 401 → clear tokens → login screen (Gateway address kept) |
| Concurrency | Simultaneous 401s trigger a single refresh; others await the same promise (Desktop `authFetch` pattern) |
| Proactive refresh | **Not done.** Passive refresh inside a 15-minute window suffices; pre-emptive refresh invents a second time model |
| Logout | Settings → Gateway → sign out: `POST /api/auth/logout` + clear local tokens → login screen |
| Plaintext risk | Tokens live in WebView storage, not the system Keychain — a known v1 compromise; hardened storage is a v1.1 item |

### 7.5 Disconnect and reconnect (global)

| State | Presentation |
|-------|--------------|
| Request failure (non-401) | A top **offline banner** ("cannot reach Gateway") persists until the next successful request; page data keeps the last snapshot and is never cleared |
| Send failure | Red `!` at the bubble corner + "Retry" (resends the same message_id; the Runtime is idempotent) |
| History load failure | Error card + "Reload" in the list area — never rendered as an "empty session" |
| Resume to foreground | One refresh of the session list + current session messages (§8.4); no background tasks |

## 8. Realtime Event Stream (v1: dual channel)

> **Decision (revised in v1.2, revised again in v1.3)**: the realtime channel is
> chosen by **connection health**; the event-handling policy is unified across
> channels — **both the relay and the LAN shape subscribe precisely over
> MQTT-over-WS** (the same channel and the same rights Desktop has), and
> **foreground-session polling is the degraded channel** (while the WS is not up,
> was refused, or is down). Whatever the channel, a "content changed" signal
> triggers the **same full HTTP reload**, and only final results are rendered.
>
> **Revision record**: v1.1 decided "no MQTT in v1, polling everywhere" on the
> premise that "a mobile WebView cannot reach MQTT". That premise holds only for
> local/LAN topologies. Under relay, `wss://<gw-id>.<relay-domain>/mqtt` is exposed
> on the public internet through the outbound tunnel
> ([24-cloud-relay](./24-cloud-relay-remote-access.md)), and the broker's strict
> listener natively accepts the `user:{name}:mobile:{id}` shape
> (`core/acowork-gateway/src/mqtt/broker.rs::remote_client_shape`) — Mobile uses the
> same user token, the same remote listener and the same ACL as Desktop. There is
> no "Mobile can't subscribe MQTT" constraint. v1.2 therefore promoted the relay
> shape to WS but left the LAN shape on polling, on the grounds that "the main
> listener has no bridge". v1.3 builds that bridge: the same WS↔TCP byte pipe is
> now mounted on the MAIN LAN listener
> (`core/acowork-gateway/src/relay/remote_listener.rs::lan_mqtt_bridge`), so the LAN
> shape has the same rights as relay. The bridge changes no authentication: the
> CONNECT inside it is still verified by the strict listener against a user access
> token, a Gateway without the account system refuses it (fail closed) and the
> client falls back to polling by itself.

### 8.0 Channel selection

```
Connect-screen probe (§7.2)
  │
  ├─ http(s):// baseUrl ──────────────────→ [WS primary] {ws|wss}://{host}/mqtt
  │        │ connect failure / CONNACK refused / reconnect limit exceeded
  │        └──────────────────────────────→ [polling fallback] (§8.3) + "realtime degraded" status line
  │
  └─ no bridge reachable (historical shape: main listener without `/mqtt`) → [polling]
```

| Rule | Value |
|------|-------|
| Test | The scheme only decides the WS **protocol and port** (`https` → `wss://{authority}/mqtt`, `http` → `ws://{authority}/mqtt`, the same derivation as Desktop's `relay_mqtt_wss_url`). Both shapes have a bridge, so what selects the channel is now "is the WS healthy" |
| Seam | `src/lib/realtime.ts` exposes `startWatching/stopWatching`; the upper layer (chatStore) is channel-agnostic. The polling loop becomes an internal fallback of `realtime.ts`, no longer a direct chatStore responsibility |
| Exclusivity | The two channels never drive reloads simultaneously; while WS is healthy polling is off (battery + traffic), and polling takes over the moment WS drops |
| Refusal degrades | A Gateway without the account system fails the CONNECT closed (a close, no CONNACK); the client treats it as a reconnect-backoff case and polling carries freshness meanwhile — the LAN shape therefore loses nothing when accounts are off |

### 8.1 MQTT-over-WS (primary channel, relay and LAN alike)

| Item | Rule |
|------|------|
| Endpoint | `wss://{relay device domain}/mqtt` (relay) or `ws://{host}:{port}/mqtt` (LAN main listener), both derived from the saved baseUrl — no new configuration surface. The WebSocket handshake must offer the `mqtt` subprotocol (both bridges echo it per the rumqttc convention; verified live) |
| client_id | `user:{sub}:mobile:{device_uuid}`. `device_uuid` is generated on first launch and persisted (same store as tokens); `{sub}` = the access token's `sub` claim (the **user_id UUID**, not the login name) — the broker cross-checks and drops impersonation with a close, no CONNACK. **The session channel and the presence channel are two connections and their client ids MUST differ**: MQTT 3.1.1 keys a session by client id, so a duplicate CONNECT takes the other socket over. The presence channel appends `-presence` to the device segment (`remote_client_shape` forbids a fourth colon, so `:presence` is not an option) |
| CONNECT auth | username=`{sub}` (informational; the broker decides on client_id + password), password=**current access token**. MQTT 3.1.1 never re-authenticates a live connection: after a token rotation (15 min) the new password rides the next reconnect — Desktop's `MqttCredentials::refresher` semantics: every reconnect reads the freshest token from authStore, triggering the single-flight refresh first if it is near expiry |
| Subscription set (**foreground session only**) | `acowork/agents/{id}/sessions/{sid}/state` (retained: status + message_count — the pushed equivalent of the polling snapshot)<br>`…/messages/done`, `…/messages/error`, `…/messages/stopped`<br>`…/messages/tool_approval_needed` (retained QoS1, carries tool details)<br>`…/messages/ask_question` (retained QoS1, `question_json`) |
| Presence subscription (**global second connection**) | `acowork/agents/+/status` (QoS1) — one wildcard filter covers the whole agent fleet. The payload is `DataEnvelope{agent_status = 9}` (`AgentStatus`: online / instance_id / state / detail), mapped onto **the same vocabulary the HTTP directory uses** (`alive` / `lifecycle`), so a push and a poll can never disagree about what the dot means (`src/lib/presence.ts`). The topic is Retained with a heartbeat plus an LWT (`online=false`), so subscribing replays every agent's last verdict and a transition that happened while the device was unreachable is never lost. Lifecycle: opened on login (phase `ready`), closed on logout/expiry. **It has no polling fallback by design** — the directory poll already carries presence; if the bridge is unreachable the list simply keeps the last polled verdict |
| User-chat wake-up subscription (**same background connection, the SAME SUBSCRIBE packet**) | `acowork/users/{me}/chat/changed` (QoS1, **empty payload**, non-retained) — the doorbell `acowork-user` rings after every send and every read ([ADR-076 §Decision 8](../../adr/en/ADR-076-multi-user-account-system.md)). It deliberately names nothing: the embedded broker has no subscription-level ACL, so a payload saying which thread changed would publish the social graph. One reaction only: re-read over HTTP (§8.8). Both filters ride ONE SUBSCRIBE packet — the packet-id space is per connection, and two round trips would leave a state where presence subscribed but the doorbell silently did not, with nothing logging it. `chatTopic()` is re-derived per CONNECT, so an account change needs no teardown |
| The background channel is granted **per filter** | SUBACK return codes line up with the filter order, and `0x80` is the only failure code MQTT 3.1.1 defines. "presence granted + chat `0x80`" is a legal combination (a Gateway with the account system off refuses the mailbox only): presence alone keeps the connection alive, a refused chat half reports `onChatChannel(false)` so the store arms its 5 s fallback poll (§8.8) instead of leaving the thread silently stale. The state is reported **only on a change** — re-reporting it on every reconnect would stop and re-arm that timer each time |
| **Never subscribed** | `messages/chunk`, `tool_call`, `tool_result`, `reasoning_*` — every incremental/process event. "Final results only" is honored at the subscription layer, not by receiving-and-discarding |
| Payload | protobuf `SessionMessage` (`core/acowork-core/proto/mqtt_payload.proto`). The WebView uses a hand-written minimal wire decoder reading only the needed fields (oneof discriminator + a few string/uint fields); **no protobuf runtime dependency** |
| Exits | leave the screen / switch session / background → UNSUBSCRIBE the old set (the connection may stay open); session expired → DISCONNECT |
| Retained trap | `tool_approval_needed`/`ask_question` are retained — a fresh subscribe may deliver an **already-handled card from a past turn**. Whether a card exists is judged by `state.status` (render the approval card only when `waiting_approval`); the event payload only **enriches the card**, it never proves pending-ness |

### 8.2 Event semantics → unified reload

| Event | Action |
|-------|--------|
| `state` (status/count change) | `message_count ≠ loadedCount` → full HTTP reload (§8.3 criterion); status drives the status line and card existence |
| `done` / `stopped` | trigger one reload (idempotently merged with the state criterion — never a double fetch) |
| `error` | reload + error-state rendering |
| `tool_approval_needed` | if `state.status=waiting_approval`: render the **full approval card** (tool name, action, risk level, reason, timeout); the decision still goes over `POST …/approval` (HTTP — the anti-forgery channel) |
| `ask_question` | render the **question card** (`question_json`); answers go over `POST …/answer` (HTTP) |
| List-level events (`sessions/created` etc.) | not subscribed in v1 — list freshness rides foreground entry / pull-to-refresh (§8.5) |

### 8.3 Polling (local/LAN channel + WS fallback)

```
Enter chat detail / after sending
  │
  ├─ every 2s  GET /api/agents/{id}/sessions/{sid}   (read live_state.status + meta.message_count)
  │     status ∈ {Thinking, LlmStreaming, ToolExecuting, …} → "working" indicator, keep polling
  │     status = WaitingApproval{request_id}          → render approval card (polling variant: generic card, §8.6), keep polling (decision is HTTP)
  │     status = Paused / Errored                      → render the matching card; Errored triggers one history reload
  │     meta.message_count ≠ locally loaded count      → GET /messages?tail=… full reload → update the count baseline
  │
  └─ exits: leave the screen / switch session / app backgrounded → stop immediately
```

> **The freshness signal is `message_count`, not a status transition.** A rule
> watching only active→idle edges misses any turn that starts and finishes
> between two ticks (a fast reply) — the history would never reload.
> `meta.message_count` is the backend's authoritative count; comparing
> "server count ≠ the count this view was loaded at" is a stateless test that
> catches every change between any two ticks. The transition remains as an
> additional trigger (for rendering the status line), but reload correctness
> depends only on the count. The WS channel's `state` push carries the same
> count — the criterion is isomorphic across channels.

| Rule | Value | Why |
|------|-------|-----|
| Interval | 2 s (`POLL_INTERVAL_MS`) | Sufficient for "final result" UX; 1 s doubles traffic with no perceived gain |
| Concurrency | **one global loop**, watching only the foreground session | Parallel per-session polling is a client-side traffic storm |
| Backoff | Design target: consecutive failures → double the interval (2→4→8→16→30 s cap), reset on any success. **Not implemented in v1** (fixed 2 s retry + offline banner today); scheduled for v1.1 | Without backoff, an outage becomes hammering |
| Idle | Idle + count unchanged + no in-flight action → fully stopped | No "something in progress" means no periodic traffic |
| 404 semantics | Session unknown to the Runtime (never opened / restarted) = idle; **not** counted as a network failure, no banner | 404 is a business state, not a connectivity fault; misreading it causes false disconnects |
| Identity | Same Bearer chain as every request (§7.4) | Polling gets no second auth model |

### 8.4 Rendering: final results only (shared by both channels)

- Any channel seeing `meta.message_count` change (or a `done` event) → `GET /api/agents/{id}/sessions/{sid}/messages?tail=N` reloads the tail; render whole (Markdown, collapsed tool cards, scrollable code — all static forms of §4.3 kept).
- **No incremental assembly, no typewriter effect.** "Agent is working" is carried by the status indicator (nav subtitle + progress row above the composer); content arrives complete.
- Pagination stays as the existing endpoints (`offset/limit/tail`), first screen tail=50.

### 8.5 List state freshness

| Data | Refresh moments |
|------|-----------------|
| Conversation list (§2.3) | once at startup, pull-to-refresh, resume-to-foreground, after session create/delete |
| Working dot | Follows the agent directory refresh (once at startup + on foreground + pull-to-refresh + after session add/remove). No read/unread count is kept: without push, per-agent "unread" cannot be judged reliably, so v1 drops it outright |
| (Removed) read/unread badge | v1 did render a local `message_count`-based unread dot; in practice the red/green flip-flop was more distracting than informative and inconsistent with desktop, so the whole thing was removed |
| Agent online status | pushed by the presence connection (§8.1): online/offline/error flip within milliseconds; the directory poll (once at startup + resume + pull-to-refresh) stays the baseline and the race corrector — a late-arriving poll can repaint a row the push just lit as offline, and the next heartbeat/verdict resets it |

### 8.6 Channel capability matrix (recorded honestly so "polling-variant semantics" never calcifies into the global truth)

| Capability | WS channel (relay and LAN) | Polling channel (fallback / no bridge) |
|-----------|--------------------|--------------------------------------|
| Status line | `state` push | 2 s snapshot |
| Approval card | full details (tool / risk / reason / timeout) | generic card: "The Agent requests running a tool — details on Desktop" |
| AskQuestion | question card + `POST …/answer` | degraded hint: "handle on Desktop" |
| Decision / answer flow-back | HTTP on both channels (identical) | same |
| Latency | sub-second | ≤ 2 s |

### 8.7 Known gaps (not v1 blockers; evaluate before public-internet exposure)

- **Per-user subscription authorization**: the remote ACL is currently the `sessions/+/messages/#` wildcard ([ADR-076 §5.5](../../adr/en/ADR-076-multi-user-account-system.md)) — subscribing to another user's session events is protocol-feasible today. Mobile's subscription discipline (foreground readable session only) is client behavior, not authorization. Desktop is in the same water at the same depth; topic-level authorization is a backend item, scheduled as a v1.1 cross-team item.
- **The presence wildcard is unauthorized in the same way**: `agents/+/status` has no topic ACL on the strict listener either, so ANY valid user token sees the lifecycle verdict of every agent on that Gateway (online/offline/failed, including the startup-failure reason). That matches what Desktop already sees locally; filtering agent presence by ownership is part of the same topic-level authorization item.
- **Traffic-storm defense**: the broad-subscription defense rests on the client discipline "subscribe to the foreground session only"; relay-side per-connection rate limiting (24 §5.3) backstops misbehavior.
- **The chat doorbell is unauthorized in the same way**: `acowork/users/{uid}/chat/changed` is named per account, but with no topic ACL any valid token can subscribe another account's doorbell. The payload is empty, so nothing about the content leaks — what leaks is the existence fact that this account is active. Filtering by `user_id` client-side is discipline, not authorization; topic-level authorization covers this too.

### 8.8 User↔user chat: wake-up and the refresh that must not move the viewport (same rule on Desktop)

User chat is not an agent session: there is no "turn in progress" to poll, and content appears exactly when the peer decides. Freshness therefore arrives only as a wake-up plus a re-read. The rule is shared by both clients (Desktop `src/stores/userChatStore.ts`, mobile `src/stores/userChatStore.ts`) — **both must read the same window out of the same server data, so a change on one side alone is a defect**.

| Rule | Value | Why |
|------|-------|-----|
| Signal | `acowork/users/{me}/chat/changed`, **empty payload**, QoS1, non-retained | A payload naming the thread or the peer would publish the social graph into a broker with no ACL (§8.7). The doorbell only says "re-read" |
| Reaction | Always an HTTP re-read: `GET /api/users/{me}/chats` plus the open thread's `GET …/messages` | The HTTP response is the only copy of the truth; MQTT carries no content, so a lost signal needs no replay machinery |
| Coalescing | Signals arriving while a re-read is in flight fold into "one more round afterwards" instead of stacking | One message emits two doorbells (the message plus the read cursor) as a matter of course |
| Connect edge | One proactive re-read on the MQTT connected edge | Non-retained signals published before the subscribe took effect are gone for good; the edge is the only correction point |
| **Refresh strategy (the core)** | A page is always "the newest N rows, oldest first". With `d = total − knownTotal`: `d=0` → touch nothing; `0<d≤page.length` → append only `page.slice(page.length-d)`; anything else (`d<0`, `d>page.length`, missing `total`) → replace the window | The reader may be parked at the top, the middle or anywhere in a long history; a signal every few seconds must not keep yanking the viewport. The head of the array keeps its identity ⇒ React does not remount the rendered bubbles ⇒ zero movement |
| Missing count | A missing `total` must be normalized to `null` before it reaches the rule — never `undefined`/NaN in a comparison | NaN falls through every branch and degrades into "append the whole page again": the transcript doubles on every signal |
| New-message pill | On a window replacement the unseen count grows by the delta the server reported; `d=0` leaves it alone; **the first page never counts** (an empty window → 0) | The 50 rows arriving with the thread are not "messages I missed" — otherwise the pill flashes "50 new messages" the moment the thread opens |
| Read receipt | Cleared only on the pinned (near-bottom) transition, driven by the scroll event, not a render | Scrolling back to the bottom is itself the "finished reading" signal; a render is not a movement |
| Fallback poll | Armed at 5 s (`POLL_MS`) only while the wake-up channel is unavailable (bridge down / chat filter refused); stood down completely once the channel is up | Push and poll must never both drive the reload (the §8.0 principle). On Desktop the timer belongs to the session, on mobile to "thread on screen && channel down" |
| Login / logout | Only a **session change** resets the store; an MQTT connect is not a logout | Treating "the bridge came up" as a session change empties the thread the reader is looking at |
| Switching thread | The known count resets with the thread and never travels into the new one | `total` is per thread: carrying the old thread's count into a new one makes the first page read as a negative delta and forces a repeated window replacement |

Verified by `apps/acowork-desktop/src/stores/userChatStore.test.ts` (planPage / signal-driven reload / coalescing / push-poll exclusivity), `apps/acowork-desktop/src/lib/stickToBottom.test.ts` (the pin gate), `apps/acowork-desktop/src/stores/chatStore.userChatPush.test.ts` (Tauri fanout + account filter), `apps/acowork-mobile/src/__tests__/user-chat-push.test.ts`, `apps/acowork-mobile/src/__tests__/stick-to-bottom.test.ts`, `apps/acowork-mobile/src/__tests__/presence.test.ts`.

## 9. Projects and Documents

| Module | Hierarchy | v1 degradation |
|--------|-----------|----------------|
| Projects | List → board → task detail | **Board columns degrade to a horizontal status Chip filter** (no drag-and-drop board on a narrow screen) |
| Documents | Directory tree → document content → review detail | Read-mostly; no rich-text editing |

All detail pages support right-swipe to go back one level.

## 10. Settings

Four top-level entries, each opening a secondary settings page:

| Top level | Content | v1 status |
|-----------|---------|-----------|
| Profile | Account, avatar, role | ✅ |
| General | Language, notifications, defaults | ✅ |
| Appearance | Theme (light/dark/system), accent color | ✅ |
| Gateway | Connection address, mode | ⚠️ **Degraded** (see §11.1) |

## 11. Runtime and Platform Constraints

### 11.1 Mobile has no local Gateway process

Desktop embeds and manages a local Gateway process, hence settings for "start/stop/restart Gateway". **Mobile has no such process** — it connects over HTTP/MQTT to an already-running Gateway (usually the user's own machine, or a relay deployment).

Therefore in v1:
- Start / stop / restart Gateway → **disabled**
- Gateway address configuration → **kept** (this is mobile's most critical connection setting)
- Gateway status display → ✅ kept

### 11.2 Technology Choices

| Item | Choice | Rationale |
|------|--------|-----------|
| Framework | Tauri v2 | Same stack as Desktop; shares HTTP/MQTT clients and type definitions |
| Frontend | React + TypeScript | Same as Desktop; can reuse pure logic such as `session-control.ts` |
| Styling | CSS Modules / vanilla CSS | Mobile feel needs fine-grained control; Tailwind's Desktop breakpoint model does not fit |
| Components | Custom mobile component set | Inset lists / Switch / Segmented / Stepper / Action Sheet |
| State | Reuse the Desktop store pattern (Zustand) | Same concepts, low migration cost |

**The frontend codebase is NOT shared with Desktop.** The two Apps have entirely different information architecture, components, and interaction paradigms; forcing a shared codebase accumulates a large number of conditional branches. What is shared is the **protocol-layer type definitions** (`SessionInfo`, the `can_write` semantics).

### 11.3 Background execution constraints

Mobile OSes make no promise about background execution: a WebView is suspended within seconds of backgrounding, the WS connection freezes with it, and it may be dead by resume. Therefore:

- Both §8 channel exits hang on `visibilitychange` (background → stop polling / UNSUBSCRIBE; foreground → restart plus one immediate reload);
- No correctness decision ever relies on the connection being alive — the first act on resume is always the HTTP snapshot (state + messages); the event stream is only an accelerator;
- Background delivery (push) is out of v1 scope (§1.3) — a platform constraint, not a feature trade.

## 12. Relationship to Existing Documents

| Document | Relationship |
|----------|--------------|
| [01-overview.md](./01-overview.md) | Concretizes §5 "deep mobile adaptation" |
| [14-desktop-app.md](./14-desktop-app.md) | Mobile is a subset terminal of Desktop; shared backend contract |
| [21-pm-project-management.md](../../design/zh/21-pm-project-management.md) (zh) | Data source for the Projects tab |
| [20-doc-online-document.md](../../design/zh/20-doc-online-document.md) (zh) | Data source for the Documents tab |
| [ADR-009](../../adr/zh/ADR-009-gateway-workspace-isolation.md) | Basis for the Add-to-Chat-only workspace boundary |
| [ADR-076](../../adr/zh/ADR-076-multi-user-account-system.md) | **Authoritative** definition of session visibility and writability |
| [ADR-085](../../adr/zh/ADR-085-agent-lifecycle-state-machine.md) | State preconditions for session activation |
| [24-cloud-relay-remote-access.md](./24-cloud-relay-remote-access.md) | The relay topology and the `wss://<gw-id>.<domain>/mqtt` entry (§8.1 primary channel exists because of it); Mobile shares Desktop's token, listener and ACL |

## 13. Design Decision Log

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Shape | IM style, not a direct three-column translation | Mobile is "messages anywhere", not "managing many panels" |
| Bottom tabs | 4 (no Harness/Extensions) | Developer concepts do not belong in an ordinary user's navigation |
| Conversation list | Agents + contacts merged into one stream | The IM mental model is "find someone to talk to", not "pick an Agent" |
| Navigation stacks | One independent stack per tab | Preserves context across tab switches, matching IM habits |
| Tab bar on detail pages | Hidden | Avoids two coexisting navigation systems |
| Multi-session container | Nav-bar title + Action Sheet | A narrow screen cannot fit a side-by-side tab bar |
| Session summary source | Last message of the most recently active session | IM semantics |
| Write-permission source | Backend `can_write`, never derived locally | Public ≠ writable; client-side derivation creates a second source of truth |
| Read-only textarea | Not rendered | A fake typeable placeholder is a worse experience |
| Read-only visibility icon | Disabled, not hidden | A viewer needs to know what they are looking at |
| New session visibility | Defaults to Private | Matches ADR-076's create path |
| Workspace | Add to Chat only, no preview | No Monaco; the Agent reading files is more natural |
| Board | Degrades to Chip filters | Drag-and-drop is unusable on a narrow screen |
| Gateway process management | Disabled in v1 | Mobile runs no local Gateway process |
| Frontend code reuse | Protocol-layer types only | IA and components differ entirely; sharing would accumulate conditionals |
| Startup & connection | Explicit state machine: connect screen → `/api/status` probe → login screen | No local Gateway; "install to first message" must precede every feature |
| Startup & connection | Explicit state machine: connect screen → `/api/status` probe → login screen | No local Gateway; "install to first message" must precede every feature |
| Authentication | Reuse ADR-076 `/api/auth/*` (login/refresh/logout); 401→refresh→replay once | Same contract as Desktop, no second auth model; proactive refresh would invent a second time model |
| Registration / pairing / forgot-password | Not in v1; point to Desktop | Low-frequency and tied to the invite chain (ADR-076 §Decision 6) |
| Realtime event stream | Dual channel: relay → precise MQTT-over-WSS subscriptions (primary); local/LAN and degraded → foreground polling (2 s, one global loop); both converge on the same HTTP reload | Channel follows the deployment topology; product policy (foreground-only subscriptions, final-results-only rendering) does not vary by channel. v1.1's "polling everywhere" premise ("WebViews can't reach MQTT") overlooked the relay's public WSS entry — fixed in v1.2 |
| History-reload trigger | Stateless `meta.message_count` comparison (transitions are only an additional trigger) | Fast replies starting and finishing between ticks make transition observation unreliable; the count is backend-authoritative and every change moves it |
| Streaming output | Not in v1; status indicator + complete arrival | Consistent with the polling model; the typewriter effect justified the chunk channel, which is cut |
| AskQuestion | question card over the WS channel; "handle on Desktop" hint over polling | Event-channel availability decides the capability; inventing an HTTP question-polling surface would be worse than honest tiering |
| Push notifications | Not in v1 | No background channel; vendor push needs its own server + certificates; v2 evaluation |
| Read/unread badge | **Not built in v1**; only the working dot remains | No push means no true unread; the cognitive cost of red/green flip-flopping exceeds its information value |

## 14. Deliverables

| Phase | Deliverable | Status |
|-------|-------------|--------|
| Interaction design | [`docs/prototypes/mobile-im-v1.html`](../../prototypes/mobile-im-v1.html) | ✅ v1.1 (adds first-launch / connect / login / offline / send-failure screens); v1.2 pending: WS-channel full approval card, question card, "realtime degraded" status line |
| Architecture decision | [ADR-086](../../adr/zh/ADR-086-mobile-app-im-ia-and-multi-session.md) (zh) | ✅ Done |
| Engineering scaffold | [`apps/acowork-mobile`](../../../apps/acowork-mobile) | ✅ Done (Vite + React 19 + Tauri v2) |
| Protocol-layer reuse | Shared type-definition crate | Planned |

### 14.1 Scaffold Technology Choices

| Area | Choice | Rationale |
|------|--------|-----------|
| Shell | Tauri v2 (standalone crate, as Desktop) | Reuses the core crates; webview first paint and memory beat RN |
| Frontend | Vite + React 19 + zustand + TypeScript strict | Same stack as Desktop, but **no shared code** |
| Styling | Plain CSS + design tokens, no UI framework | The mobile-native component set is "few and fixed"; a framework's defaults all have to be overridden anyway |
| Native layer | Near-empty shell | No local Gateway, no tray, no LSP sidecar — see the "Gateway process management" row in §11 |
| Mobile crate shape | Empty `[workspace]` table, resolved standalone | Same as `apps/acowork-desktop/src-tauri`; mobile targets a different platform and must not inherit core's feature unification |

**Directory layout**

```
apps/acowork-mobile/
  src/
    lib/       types.ts (wire-format subset) · session-write-access.ts (permission gate) · api.ts (HTTP transport)
    stores/    navStore (four independent tab stacks) · agentStore (directory + sole owner of the session list) · chatStore (view state + atomic openSession)
    components/ ui.tsx (ListSection/Row · Switch · Segmented · Stepper · Sheet) · EdgeSwipe.tsx
    screens/chat/  ChatListScreen · ChatDetailScreen
    routes.tsx  route registry
  src-tauri/    standalone crate, near-empty shell
```

### 14.2 Architectural Defect Found While Scaffolding

`agentStore` and `chatStore` each held their own `sessions` array. `session-write-access.ts` read the former; `chatStore.openSession` read the latter (always empty). So `can_write` could never be observed as `false` — read-only sessions would wrongly emit `open_session`, and the composer gate would fail open.

This is not a "sync both sides" fix: **two copies of one fact will always diverge**, and the divergence direction here is exactly a fail-open on the permission gate. The fix gives `agentStore` sole ownership of the session list; `chatStore` keeps only per-agent view state (current session, messages, loading) and reads `can_write` through `getSession()`. The defect was surfaced by a delete-case test in `session-access.test.ts`.

**Corollary: whatever field the permission gate reads must have exactly one owner.**

### 14.3 Prototype Verification Record

The interaction prototype was validated in two automated passes:

| Layer | Tool | Cases | Result |
|-------|------|-------|--------|
| Behavior | jsdom (real clicks + synthetic touch events) | 60 | 60 passed |
| Layout | WKWebView live `getBoundingClientRect` | 44 | 44 passed |

The layout layer cannot be skipped: jsdom returns 0 for every geometry value and would mask real overflow. The prototype verification surfaced 4 real defects (nav-bar subtitle never rendered, session switcher bursting the nav bar, session list never scrolling because `max-height` was too generous, session names rendered twice).
