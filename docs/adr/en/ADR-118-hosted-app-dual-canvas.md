# ADR-118: Dual canvas for hosted apps — user surface vs agent workspace files

**Status**: Accepted (2026-11-19; the user decided "the left-nav surface is the user's own canvas; in chat the agent operates on a file in its own workspace, same as generating an image — write it, then preview and edit; chat must be able to open an iframe on that file; co-edit happens only when the app UI itself hosts a chat". Same-day revision: the agent's document is not conversation memory; the workspace file is the source of truth.)
**Date**: 2026-11-19
**Decider**: User

**Related**:
- [ADR-116](./ADR-116-hosted-application-framework.md) — hosted-app framework: process / proxy / catalog / iframe shell
- [Design doc 34](../../design/en/34-hosted-application-framework.md) — full ADR-116 contract
- [Design doc 35](../../design/en/35-hosted-app-dual-canvas.md) — full contract for this ADR
- [ADR-009](./ADR-009-gateway-workspace-isolation.md) — Gateway never reads an app data directory; workspace bytes go through the Runtime
- [ADR-069](./ADR-069-mcp-tool-level-optin.md), [ADR-073](./ADR-073-agent-instance-identity-decomposition.md) — MCP catalog + `X-MCP-Actor` = instance_id
- [ADR-104](./ADR-104-agent-manifest-declaration-surface.md) — `[[mcp_servers]]` declaration surface
- Windowed chat transcripts: [design doc 14](../../design/en/14-desktop-app.md) and the `VirtualRowList` rule

> The Chinese document is the source of truth. This English counterpart is kept in the same commit by convention.

---

## 1. Decision summary

### 1.1 One sentence

**Left nav is the user's app UI; the agent's document is a file in its workspace.** Same path as generating an image: create the file, then design, save and preview against that file, and only afterwards decide whether to upload it to DOC, send it on, or let the user copy it. MCP mutates that file — not an anonymous canvas that lives only in the conversation, and not whatever the user has open on the left. Chat preview = open that workspace file (`visual` types use the app's own iframe viewer).

### 1.2 Key decisions

| # | Decision | Outcome |
|---|---|---|
| 1 | User canvas | The left-nav iframe (`ui.mode = webview`) serves only the current user. Open / edit / save are user actions |
| 2 | Agent document | **The workspace file is the source of truth.** The agent creates `*.figcraft` (or the app's equivalent) in its own workspace; MCP open/save/edit target that path. Not a `(conversation)` in-memory session |
| 3 | Engine | The app process MAY hold a Headless / open document as an editor cache of that file. After a process restart, reopen from disk. The preview iframe is not the file's backend |
| 4 | Chat preview | Transcript holds a file card (same family as image attachments: path, thumbnail, "Open"). Opening it previews that workspace file in the **file-editor column**. A `visual` viewer is the app iframe. Application WASM MUST NOT go in a `VirtualRowList` row |
| 5 | Preview ≠ backend | Closing the preview MUST NOT fail the next MCP call. The agent keeps `open_file` / `save_file` on the workspace path |
| 6 | Handoff | The file is already in the workspace. Next: the user opens it (or a copy) in left-nav FigCraft, uploads to `doc`, or sends it to another agent. Once it is on the user canvas, the agent does not attach that UI unless Decision 7 applies |
| 7 | Co-edit | MCP may attach to the user canvas only when the **app's own UI** embeds chat / an agent. Platform chat does not co-edit by default |
| 8 | `library` vs `visual` | Both start from an openable artifact. `library` (DOC / PM): the artifact lives in the app's own store; opening the resource shows it. `visual` (FigCraft, design files): the artifact is an Agent **workspace file**; preview needs the matching viewer (FigCraft iframe / image viewer). The split is the **preview channel**, not "one has a file and the other does not" |
| 9 | Theme | `AppFrame` already `postMessage`s theme / locale / accent. Push again when ACowork light/dark changes |
| 10 | Change concentration | Glue lives in the **hosted adapter**: MCP HTTP, confine tool paths to the calling agent's workspace, project the workspace file into the preview iframe. Do not patch upstream engine / MCP crate / UI sources |
| 11 | Catalog | `mcp.auto_inject = true` → ADR-116 `catalog_apps()` → `acowork/global/mcps` |
| 12 | File isolation | Multi-user / multi-agent isolation is **each agent's workspace**, not an anonymous canvas table. FigCraft MCP automation roots = that actor's workspace root. Tools MUST NOT read another agent's workspace |

### 1.3 Invariants (red lines)

1. **MCP does not attach to the user webview by default.**
2. **The agent's document is a workspace file.** A process-only session with no file MUST NOT be the source of truth. The engine cache MAY be dropped; the file MUST NOT.
3. **The chat transcript does not host a live app iframe.**
4. **Preview opens that workspace path.** It MUST NOT bind another agent's file or unsaved state on the left-nav user canvas.
5. **File access is confined to the calling agent's workspace.** Missing or out-of-root paths error; MUST NOT fall through to "whatever is currently open in the process".
6. **The Gateway still carries zero app business logic.** Workspace bytes use the existing workspace file API.
7. **Co-edit is an in-app feature, not the platform-chat default.**

### 1.4 Explicitly out of scope

- Routing agent tools at the iframe the user currently has open on the left.
- Spawning another app process per user or per agent; isolation is the workspace directory.
- Treating FigCraft WASM as the only MCP backend (WASM is the viewer / user editor).
- Using "current chat session id" as the document primary key. The conversation holds a **reference to a workspace file**.
- Forking upstream engine, command table, or Web UI sources; glue stays in the adapter.
- Folding the memory right-hand panel into this contract.
- Forcing DOC/PM to write a workspace file first and then sync into the library — their artifact key is the resource id in the library.

---

## 2. Context

ADR-116 answered how an app is hosted, not **which copy of state an agent mutates**.

Two wrong directions:

1. **Attach the user iframe**: the user's document is stolen, Desktops collide, tools die when the iframe unmounts.
2. **Anonymous conversation canvas**: the document lives only in `(actor, conversation)` memory. That does not match every other agent artifact (images, code, Markdown), which are workspace files, and it cannot be handed to DOC, another agent, or the user without inventing an extra export.

The user aligned it with **image generation**:

1. The agent creates a file in its own workspace;
2. It edits, saves, previews, edits again on that file;
3. When done it can stay in the workspace, go to DOC, or be sent on.

FigCraft MCP already has `open_file` / `save_file` and automation roots. The workspace path is the document's identity.

---

## 3. Identity and keys

| Channel | Identity | Use |
|---|---|---|
| `/api/apps/{id}/ui/*` + `/api/*` | app session ticket → `X-App-User` | user canvas; preview iframe pages |
| Workspace file API | signed-in user + agent + `workspace_id` + rel path | preview / the user opening the agent's artifact (existing) |
| `{mcp.gateway_path}` | machine credential + `X-MCP-Actor` (instance_id) | agent tools |

**Document primary key**: `(agent_id / instance_id, workspace_id, relative_path)`. Chat and preview cite that path. The same file MAY be mentioned in several chats; two chats SHOULD use different files unless the agent explicitly opens the same path.

The Runtime MUST tell the app MCP the **workspace root bound to that actor's current session** (header or platform API; see design doc 35) so `open_file` / `save_file` land in the workspace, not an anonymous directory of the app process. FigCraft automation read/write roots are that root.

`conversation_id` is not the file's primary key. The tool argument `path` is.

---

## 4. Consequences

**Upside**

- Same as images, source, attachments: a workspace file → chat can cite it → the editor pane can preview it → it can be copied, uploaded, sent.
- Multi-agent isolation is workspace isolation; no extra anonymous-canvas table.
- In-process Headless is just the editor for that file; recover from disk after a crash.

**Downside**

- The adapter must resolve actor → workspace root and confine tool paths.
- Desktop must treat `.figcraft` (and later peers) as workspace-previewable, viewer = app iframe.
- DOC/PM stay on library resources; do not regress to writing PRDs as workspace `.md`.

**Rollback**

- Turning off `mcp.auto_inject` drops the catalog entry.
- If the viewer is not built yet, the file still appears in the workspace tree and screenshots still work — the "file is authority" red line holds.

---

## 5. Delivery order

Steps and APIs: [design doc 35](../../design/en/35-hosted-app-dual-canvas.md). First `visual` app: FigCraft — workspace `*.figcraft` + MCP open/save + editor-pane iframe preview. PM / DOC stay `library`.
