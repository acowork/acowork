# ADR-118: Dual canvas for hosted apps — user surface vs agent working state

**Status**: Accepted (2026-11-19; the user decided "the left-nav surface is the user's own canvas; in chat the agent operates on the agent's own canvas; chat must be able to open an iframe to see the result; co-edit happens only when the app UI itself hosts a chat")
**Date**: 2026-11-19
**Decider**: User

**Related**:
- [ADR-116](./ADR-116-hosted-application-framework.md) — hosted-app framework: process / proxy / catalog / iframe shell
- [Design doc 34](../../design/en/34-hosted-application-framework.md) — full ADR-116 contract
- [Design doc 35](../../design/en/35-hosted-app-dual-canvas.md) — full contract for this ADR: channels and new-app checklist
- [ADR-009](./ADR-009-gateway-workspace-isolation.md) — Gateway never reads an app data directory
- [ADR-069](./ADR-069-mcp-tool-level-optin.md), [ADR-073](./ADR-073-agent-instance-identity-decomposition.md) — MCP catalog + `X-MCP-Actor` = instance_id
- [ADR-104](./ADR-104-agent-manifest-declaration-surface.md) — `[[mcp_servers]]` declaration surface
- Windowed chat transcripts: [design doc 14](../../design/en/14-desktop-app.md) and the `VirtualRowList` rule (off-viewport rows are not in the DOM)

> The Chinese document is the source of truth. This English counterpart is kept in the same commit by convention.

---

## 1. Decision summary

### 1.1 One sentence

**A hosted app's user surface and its agent working state are two different things.** The left-nav webview is the signed-in user's canvas — pure user behaviour; MCP does **not** attach to it. When an agent in chat creates or edits that app's files, it mutates an agent session keyed by `(app_id, X-MCP-Actor, conversation_id)`. To see the result, chat shows a card; opening it mounts a read-only preview iframe in the **durable pane beside chat** — not inside a message bubble, and not the user's left-nav canvas.

### 1.2 Key decisions

| # | Decision | Outcome |
|---|---|---|
| 1 | User canvas | The left-nav iframe (`ui.mode = webview`) serves only the current user. Open / edit / save are user actions and have nothing to do with the agent |
| 2 | Agent working state | MCP `tools/call` mutates only the agent's own session. The source of truth lives in the app process, tabled by `(app_id, actor, conversation)`, **not** in a Desktop iframe |
| 3 | Process | Still one generic process per `app_id` (ADR-116). Isolation is the session table, not another process per user |
| 4 | Chat preview | The transcript holds a card (thumbnail + "Open canvas"). The iframe lives in the file-editor column (or an equivalent durable pane). Application WASM/webviews MUST NOT be placed in `VirtualRowList` rows |
| 5 | Preview ≠ backend | The preview iframe is a viewer of the agent working state. The agent can work with nobody watching; unloading the iframe MUST NOT fail `tools/call` |
| 6 | Handoff | For the user to edit the agent's result: the app exports a resource (file, document id); the user opens it on the left-nav canvas. After that it is the user's canvas; the agent does not touch it unless Decision 7 applies |
| 7 | Co-edit | MCP may attach to the user canvas only when the **app's own UI** embeds chat / an agent. Platform chat does not co-edit by default |
| 8 | App kind | `library` (DOC / PM): shared durable store; the user sees agent writes by opening the resource; no preview iframe required. `visual` (FigCraft and peers): the working state is a canvas and MUST offer chat preview. `none`: no user webview |
| 9 | Theme | Desktop `AppFrame` already `postMessage`s theme / locale / accent. The app page listens. The shell MUST push again when ACowork light/dark changes |
| 10 | Change concentration | ACowork glue lives in the app's **hosted adapter** (e.g. `figcraft-acowork`): MCP HTTP, per-actor sessions, preview URL, HTML injection. Do not patch the upstream engine / upstream MCP crate / upstream UI sources to meet the platform |
| 11 | Catalog | `mcp.auto_inject = true` uses the existing ADR-116 `catalog_apps()` → `acowork/global/mcps` path. The Gateway does not interpret tool semantics |

### 1.3 Invariants (red lines)

1. **MCP does not attach to the user webview by default.** The left-nav iframe is not the agent's backend.
2. **The agent working state's source of truth is the app process.** A Desktop iframe crashing, navigating away, or a virtualised row unmounting MUST NOT drop the document or make the next `tools/call` miss the session.
3. **The chat transcript does not host a live app iframe.** Rows are windowed; off-viewport rows are not in the DOM.
4. **A preview iframe is read-only onto that agent session.** It MUST NOT bind another user, another agent, another conversation, or the left-nav user canvas.
5. **When many users share one app process, the session key MUST include the actor.** A missing session returns a clear error; it MUST NOT silently fall through to "the only canvas currently attached".
6. **The Gateway still carries zero app business logic** (ADR-116 / ADR-009): it does not read app data directories, interpret tools, or render canvases.
7. **Co-edit is an in-app feature, not the platform-chat default.**

### 1.4 Explicitly out of scope

- Routing agent tools at the iframe the user currently has open on the left.
- Spawning another app process per user or per agent in order to "isolate canvases".
- Treating FigCraft (or similar) WASM as the only MCP backend.
- Forking the upstream engine, command table, or Web UI sources to plug into ACowork; glue stays in the adapter.
- Folding the memory right-hand panel into this contract (still excluded by ADR-116).

---

## 2. Context

ADR-116 answered how an app is hosted: process, reverse proxy, catalog, left-nav slot, sandboxed iframe. It did not answer **which copy of state an agent mutates**.

Attaching MCP to the user's currently open webview looks like "watch the agent draw", but:

- the user's own document is stolen by the agent;
- multiple users / Desktops collide on one attach point;
- the agent can work only while that iframe lives — the opposite of DOC, where `doc_write` works whether or not the Docs view is open.

The user's decision matches DOC: **opening the app UI is pure user behaviour; the agent uses MCP on its own working state; to see the result, chat offers a way to open a preview.** Co-edit waits until the app UI itself hosts a chat.

---

## 3. Identity and keys

| Channel | Identity | Use |
|---|---|---|
| `/api/apps/{id}/ui/*` + `/api/*` | app session ticket → `X-App-User` | user canvas, preview iframe pages and APIs |
| `{mcp.gateway_path}` | machine credential + `X-MCP-Actor` (instance_id) + `X-MCP-Conversation` (session id) | agent tools. The Gateway checks the actor is installed and strips client-asserted user identity (already true of the ADR-116 MCP path) |

Agent working-state key: `(app_id, actor_id, conversation_id)`. Two chats with the same agent = two documents. If `X-MCP-Conversation` is missing the app MUST refuse write tools (read-only probes MAY be allowed) and MUST NOT reuse "the only session in the process".

The Runtime forwards the current chat session id on MCP HTTP requests (header `X-MCP-Conversation`). Gateway `app::mcp_proxy` forwards that header as-is (it is not a spoofable identity header).

---

## 4. Consequences

**Upside**

- Later visual apps (boards, spreadsheets, prototypes) reuse the same split: user canvas, agent session table, chat card, durable preview iframe.
- Multiple users no longer fight over the user iframe; agents are isolated by actor × conversation.
- Upstream sync conflicts stay inside the adapter.

**Downside**

- The Runtime must send `X-MCP-Conversation` (a small new header).
- Desktop chat must notice "this app's MCP was called" and offer a card / open preview — that is a shell capability, not each app drawing its own chat UI.
- `library` vs `visual` must be declared in `app.toml` so DOC is not forced to pop a canvas iframe.

**Rollback**

- Turning off the app's `mcp.auto_inject` drops it from the catalog.
- If the preview pane is not built, the agent still works; the user only sees tool_result text / screenshots — a degradation that does not break red lines 1–3.

---

## 5. Delivery order

Steps and APIs are in [design doc 35](../../design/en/35-hosted-app-dual-canvas.md). The first `visual` app is FigCraft (the ACowork fork). PM / DOC stay `library` and are not required to ship a preview iframe.
