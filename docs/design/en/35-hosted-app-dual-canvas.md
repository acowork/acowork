# Design Doc 35: Dual canvas for hosted apps (user surface / agent working state / chat preview)

**Version**: 1.0 (2026-11-19)
**Status**: Accepted, not yet implemented (first `visual` app: FigCraft)
**Authoritative decision**: [ADR-118](../../adr/en/ADR-118-hosted-app-dual-canvas.md)
**Depends on**: [ADR-116](../../adr/en/ADR-116-hosted-application-framework.md) · [Design doc 34](./34-hosted-application-framework.md)

> The Chinese document is the source of truth. This English counterpart is kept in the same commit by convention.
>
> This is the **working-state contract** on top of ADR-116. Process, reverse proxy, tickets and catalog remain as in doc 34. This document specifies how the user canvas, the agent working state and the chat preview stay separate, and the checklist a new app must follow.

---

## 1. Positioning

A hosted app presents up to three surfaces. Any of them may be absent; they MUST NOT be mixed:

| Surface | Who uses it | Where state lives | Entry |
|---|---|---|---|
| **User canvas** | the signed-in user | that user's Desktop webview (and the user's files under the app data directory) | left nav → `AppFrame` → `/api/apps/{id}/ui/` |
| **Agent working state** | one agent instance in one chat | a session **inside the app process**, keyed | Runtime → `{mcp.gateway_path}` → `POST /mcp` |
| **Chat preview** | the user wanting to see what the agent drew | a read-only projection; source of truth remains the agent working state | chat card → durable pane iframe → `/ui/` with a canvas query |

DOC / PM are **library**: the agent writes a shared durable store; the user sees the result by opening that resource; a live third-surface canvas is unnecessary. FigCraft and peers are **visual**: the agent's document lives in a session; without opening the preview the user does not see it live.

Platform chat is **not co-edit by default**. Co-edit happens only when the app's own UI hosts a chat / agent (only then may MCP attach to the user canvas).

---

## 2. Architecture

```
Desktop                                              Gateway                         App process (one per app_id)
┌─────────────────────────────┐                     ┌──────────────┐                ┌─────────────────────────────┐
│ Left-nav AppFrame           │  ticket + /ui/      │ app::proxy   │                │ User static bundle          │
│  = user canvas (no MCP)     │────────────────────►│              │───────────────►│ does not hold agent sessions│
│                             │                     │              │                │                             │
│ Chat transcript: canvas card│                     │ app::mcp_    │  X-MCP-Actor   │ Agent session table         │
│  (no iframe)                │                     │  proxy       │  X-MCP-Conv.   │  key = (actor, conversation)│
│                             │                     │              │───────────────►│  Headless / equivalent      │
│ Editor pane: preview AppFrame│ ticket + /ui/?canvas│              │                │  POST /mcp → that session   │
│  = read-only viewer         │────────────────────►│              │───────────────►│  GET /api/canvas/… project  │
└─────────────────────────────┘                     └──────────────┘                └─────────────────────────────┘
        ▲                                                                                    │
        │  theme / locale postMessage                                                        │
        └────────────────────────────────────────────────────────────────────────────────────┘
                              Agent Runtime uses MCP only; it never touches the user iframe
```

One process, many sessions. Canvas isolation does not mean more processes.

---

## 3. `app.toml` additions

Every section from doc 34 stays. Add:

```toml
[ui.canvas]
# library = shared durable store (DOC / PM). Opening a resource shows agent writes.
# visual  = in-session canvas (FigCraft and peers). MUST offer the chat preview channel.
# none    = no user-webview working state.
kind = "visual"

# For visual: UI path of the preview iframe, relative to this app's /ui/.
# The app projects the canvas session id in the query into a read-only view.
preview_path = "/?canvas={canvas_id}"
```

| Field | Default | Notes |
|---|---|---|
| `ui.canvas.kind` | `library` (when there is a webview) or `none` | Undeclared built-in PM/DOC count as `library`; MCP-only services as `none` |
| `ui.canvas.preview_path` | absent | Required for `visual`. `{canvas_id}` is replaced by the platform with a URL-safe session key |

`[mcp].auto_inject = true` remains the switch into `acowork/global/mcps` (doc 34). A `visual` app that agents should call MUST turn it on.

---

## 4. Agent working state

### 4.1 Key

```
canvas_id = app_id + actor_id + conversation_id
```

- `actor_id`: `X-MCP-Actor`, already verified by the Gateway as an installed instance_id.
- `conversation_id`: `X-MCP-Conversation`, the current chat session id, sent by the Runtime on every MCP HTTP request.
- Missing actor: refuse write tools (same spirit as DOC's anonymous read-only; the exact read-only set is the app's).
- Missing conversation: refuse write tools. MUST NOT reuse "the only session in the process".

Same agent, two chats = two working states. The user's left-nav canvas **does not** have this key and does not enter this table.

### 4.2 Authority and lifetime

- Source of truth: in-process in the app (MAY also persist under `app-data/{id}/canvases/{canvas_id}/`; the key is still the one above).
- Created on the first write tool for that key.
- Destroyed by the app's own GC (idle timeout, session-ended notification — the latter is a later platform capability and does not block this version). A process restart MAY drop unexported visual sessions; apps that need to keep them persist themselves.
- `tools/call` MUST NOT require the preview iframe to be online.

### 4.3 MCP HTTP

As in doc 34: a single `POST /mcp`, stateless Streamable HTTP subset. The app dispatches JSON-RPC to the `Server` / tool table **for that request's key**, not a process-wide singleton.

Gateway `/api/apps/{id}/mcp` (and the declared `gateway_path`) remains reverse-proxy + actor checks. Tool lists, screenshots and exports are the app's.

### 4.4 Relationship to library apps

DOC / PM "working state" is a document / task in the library, addressed by resource id, not an anonymous canvas. They:

- keep using `X-MCP-Actor` to sign writes (already shipped);
- do **not** need a canvas per chat;
- do **not** need a preview iframe;
- the existing "open this document / task" from chat already covers "see the result".

A new library app follows the same path: shared store + the user opens a resource. Do not implement visual preview "for consistency".

---

## 5. User canvas

- Entry, ticket, sandbox, identity injection: unchanged from doc 34.
- Desktop MAY unmount the iframe when leaving that nav item (if the user's document lives only in WASM, draft recovery is the app's problem, not a platform MCP problem).
- MUST NOT register that iframe as an MCP Backend.
- Theme: §7.

---

## 6. Chat preview (`kind = visual` only)

### 6.1 Transcript card

When this conversation has a `tools/call` against that app's MCP (success is enough), chat inserts (or updates) a canvas card that is **not** a live iframe in a windowed row:

- app name, optional title, latest `screenshot` / export thumbnail (placeholder if none);
- action: "Open canvas".

The card is an ordinary message / attachment and scrolls with the windowed transcript. **No iframe inside the card.**

### 6.2 Durable iframe

"Open canvas" mounts the preview in the **file-editor column** (the same slot as opening a workspace file; if no file is open, expand that column). Requirements:

1. Reuse `AppFrame` (same sandbox: no `allow-same-origin`, `allow-downloads` present).
2. `src` = Gateway `/api/apps/{id}/ui/` + expanded `preview_path` + ticket in the fragment (ticket still never goes in the query).
3. `canvas_id` is a query or the app's agreed preview parameter; it names the agent session, not a user id.
4. The iframe stays mounted until the user closes the preview or switches to another conversation's preview. Switching chat sessions unmounts it or replaces it with that conversation's canvas.
5. The preview MAY open automatically after the first successful MCP tool for that app in the turn; idle chat MUST NOT preload WASM.

### 6.3 How the preview follows working state

The preview is a projection. Apps pick one or both:

| Mode | How | When |
|---|---|---|
| Snapshot | After each write tool, refresh document bytes / PNG; the preview page polls or `GET /api/canvas/{canvas_id}/snapshot` | Cheapest |
| Injected control | The adapter injects a script into the served `index.html`: add `?control`, listen for `acowork` theme, feed snapshots into the page's existing file-open API | When the preview should look like the real editor |

MUST NOT register the preview iframe as the only MCP Backend. It MAY also attach for display; a disconnect MUST NOT fail tools.

### 6.4 Why not in the bubble

`VirtualRowList` only mounts rows near the viewport. FigCraft-class WASM in a row would restart on scroll and fork from the in-process source of truth. The preview MUST sit in a durable pane **outside** the transcript.

---

## 7. Theme and locale

ADR-116 already has `AppFrame` `postMessage` on load / `ready`:

```json
{ "channel": "acowork", "type": "theme", "theme": "light|dark", "accent": "#…", "locale": "zh-CN" }
```

This contract adds two rules:

1. Desktop MUST push again when the `documentElement` light/dark class changes (both the user canvas and an open preview iframe need it).
2. App side: the hosted adapter's injected script listens and maps onto the app's own theme commands (FigCraft: `view.themeLight` / `view.themeDark`, or `ui.set { theme }`). Default `system` follows the OS and **will not** follow the ACowork shell; a visual app inside the iframe SHOULD use the value the shell pushed.

Apps MAY ignore accent. Locale is best-effort.

**Do not** patch upstream `index.html` or the UI crate for theme; injection happens when the adapter serves `/ui/`.

---

## 8. Handoff and co-edit

**Handoff (default)**: the agent writes a resource the app can open (`.figcraft`, a document id, an exported PNG). The user opening it on the left-nav canvas is user behaviour; afterwards it is the user's document.

**Co-edit (explicit, later)**: the app UI embeds chat, and the canvas the user has open is the one being edited. Only then may that user canvas be attached. The path "platform left-nav + platform chat" never co-edits automatically.

---

## 9. Hosted adapter (where changes go)

In the upstream repo (or fork), ACowork-specific code concentrates in one binary / crate, e.g. `apps/{name}-acowork`:

| Responsibility | Lives here | Does not live here |
|---|---|---|
| ADR-116 CLI (`--host` / `--port` / `--port-file` / `--gateway-health-url`) | adapter `main` | upstream desktop `main` |
| `GET /health`, `GET /ui/*`, `POST /mcp` | adapter | upstream |
| Session table keyed `(actor, conversation)` | adapter | a process-wide upstream MCP singleton |
| Preview snapshot API | adapter | upstream |
| Inject control / theme / canvas script into `index.html` | adapter response path | upstream `index.html` source |
| `packaging/acowork/app.toml` | adapter packaging dir | upstream release config |

The adapter MAY **depend on** the upstream MCP / engine crate (e.g. `figcraft-mcp::Server::handle`) and implement its own `Backend` pointing at a Headless session. Do not rewrite the upstream tools table to speak HTTP.

Path adaptation (no real filesystem on the Web: read `open_file`'s path as bytes, then the engine's `{name, data}`) also lives in the adapter Backend.

---

## 10. Platform-shell obligations

An app that follows this document can be hosted. The rows below are **ACowork Desktop / Runtime** duties, so each app does not invent its own chat UI.

| # | Layer | Duty |
|---|---|---|
| P1 | Runtime | MCP HTTP requests carry `X-MCP-Conversation: {session_id}` |
| P2 | Gateway | `app::mcp_proxy` forwards that header (do not strip it as forged identity) |
| P3 | Desktop `AppFrame` | Re-push theme / locale when the theme class changes |
| P4 | Desktop chat | `kind=visual` and this conversation called that app's MCP → canvas card + open editor-pane preview |
| P5 | Desktop layout | Preview uses `AppFrame`, `src` expanded from `preview_path`; it MUST NOT overwrite left-nav `hostedAppId` (user canvas and preview MAY coexist: one in `view=app`, one in chat's file column) |

The user opening FigCraft on the left and opening an agent preview in chat are two `AppFrame` instances, two tickets, two URLs. Coexistence is allowed.

---

## 11. New-app checklist

When adding a hosted app, tick in order:

**Doc 34 (hosting)**

1. `app.toml`: `id` / `process` / `mcp` / `ui` / `permissions` / `package`
2. Generic process: `/health` + `/ui/` + `POST /mcp`
3. Bind `127.0.0.1` only; go through the Gateway proxy
4. Set `mcp.auto_inject` according to whether agents should see the catalog entry
5. Desktop does not import the app's front end

**This document (working state)**

6. Choose `ui.canvas.kind`
7. `library`: MCP writes the shared store, signed with `X-MCP-Actor`; the user opens the resource to see the result
8. `visual`: in-process session table keyed `(actor, conversation)`; preview URL; adapter injects theme
9. MCP does **not** attach to the left-nav user iframe
10. Do not put a webview in a chat bubble
11. Glue stays in the hosted adapter
12. Multi-user: missing session → error, never "whatever is currently attached"

**FigCraft mapping (first visual app)**

- User canvas: the packed `/ui/` WASM, left nav.
- Agent working state: `figcraft-mcp` Headless (or equivalent Session), tabled by key; `auto_inject = true`.
- Preview: chat card + editor-pane `AppFrame`; injected script feeds snapshots / `?control`.
- Theme: injected listener on `acowork` theme → `view.themeLight` / `view.themeDark`.
- Do not change `crates/mcp`, `crates/engine`, or `apps/figcraft-web/index.html`.

---

## 12. Security

Continuing doc 34:

- `canvas_id` in the preview URL is not a credential. The page still needs a ticket; the app server authorises with `X-App-User` + the canvas key: only "this preview is the agent document in this user's current chat" may return a snapshot. Practical shape: Desktop opens the preview with a session ticket; the app checks that ticket's user against the conversation that owns the canvas — or binds the snapshot to the user the ticket was minted for, with `canvas_id` in the query required to be a session that user may see (the platform MAY later encode the canvas into the ticket domain; until then the app at least requires a logged-in user).
- The MCP path still strips `X-App-User` and friends; an agent cannot read someone else's user canvas by asserting a user id.
- Preview sandbox matches the user canvas: no `allow-same-origin`.

The first implementation MAY simplify: snapshot endpoints require a valid ticket, and the `canvas_id`'s `actor` is an installed agent on this Gateway. Cross-user invisibility then rests on "you do not know the canvas_id". Tightening authorisation is a follow-up; this document MUST NOT describe "guess the id, see someone else's document" as a feature.

---

## 13. Verification (when implementing)

1. User A opens the app on the left and draws; user B's agent `tools/call` MUST NOT appear on A's left-nav canvas.
2. Same user, two chats, same agent: two working states; opening each preview MUST NOT write across.
3. Close the preview iframe, `tools/call` still succeeds, reopen preview, latest snapshot is there.
4. Scrolling chat or collapsing explore blocks MUST NOT unmount the preview-pane iframe.
5. Switching ACowork light/dark, the user canvas and an open preview follow the shell, not the OS.
6. With `auto_inject = false` the catalog has no server; with `true`, the global MCP list contains `{mcp.gateway_path}`.
7. The adapter's diff against upstream `index.html` is empty (injection is only in the HTTP response).

---

## 14. Boundaries

- Session-ended notification and canvas GC protocol: not done. Apps MAY use their own idle timeout.
- Binding the preview ticket to a canvas: the tightening in §12.
- In-app co-edit: out of this version.
- memory panel UI: still excluded.
- Mobile: Desktop first in this version; whether mobile mounts a preview pane is separate. Red lines 1–3 (user canvas, agent authority in-process, no iframe in the transcript) still apply.
