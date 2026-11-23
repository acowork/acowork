# Design Doc 35: Dual canvas for hosted apps (user surface / workspace file / preview)

**Version**: 1.2 (2026-11-21; formats are routed by `ui.canvas.extensions`, plus save-back and the page host mode)
**Status**: Accepted, implemented (FigCraft / WordCraft / GridCraft / DeckCraft / PdfCraft / VectorCraft / LightCraft)
**Authoritative decision**: [ADR-121](../../adr/en/ADR-121-hosted-app-dual-canvas.md)
**Depends on**: [ADR-116](../../adr/en/ADR-116-hosted-application-framework.md) · [Design doc 34](./34-hosted-application-framework.md)

> The Chinese document is the source of truth. This English counterpart is kept in the same commit by convention.
>
> This is the **artifact contract** on top of ADR-116. Process, reverse proxy, tickets and catalog remain as in doc 34.

---

## 1. Positioning

Same as an agent **generating an image**:

1. Create a file in that agent's workspace;
2. Edit, save, preview, edit again on that file;
3. When done, leave it in the workspace, upload to DOC, or send it to another agent / the user.

**Always start from a file.** Not from "the in-memory canvas bound to this chat".

Three surfaces may be absent; they MUST NOT treat the UI the user is editing as the agent's file:

| Surface | Who uses it | Source of truth | Entry |
|---|---|---|---|
| **User canvas** | the signed-in user | the document the user opened in the app | left nav → `AppFrame` → `/api/apps/{id}/ui/` |
| **Agent artifact** | an agent | **a file in that agent's workspace** | Runtime MCP → app `open_file` / `save_file` / edit tools |
| **Preview** | the user watching the agent | read-only open of that workspace path | chat file card → editor-pane viewer (images: `ImageViewer`; `.figcraft`: app iframe) |

`library` (DOC / PM): the artifact key lives in the app's own store; opening the resource shows it — a "file" that does not sit in the agent workspace. `visual` (FigCraft and design files that need a dedicated viewer): the artifact key is a workspace path.

Platform chat is **not co-edit by default**.

---

## 2. Architecture

```
Agent workspace (on the Node)          App process (one per app_id)
┌──────────────────────────┐           ┌──────────────────────────────┐
│  ridgeline.figcraft      │◄─ open/ ─►│ Headless / engine: that file │
│  cover.png               │   save    │ editor cache, not authority  │
└────────────┬─────────────┘           │ POST /mcp carries path       │
             │                         └──────────────────────────────┘
             │ existing workspace file API
             ▼
Desktop chat card → editor pane previews that path
  png  → ImageViewer
  figcraft → AppFrame /ui/?file=… (read-only projection)
Left-nav AppFrame = the user's own document; MCP does not attach
```

One app process MAY have several workspace files open (cached by path). Isolation is the **agent workspace directory**.

---

## 3. `app.toml` additions

Keep every section from doc 34. Add:

```toml
[ui.canvas]
# library = artifact in the app's own store (DOC / PM).
# visual  = artifact is an agent workspace file; preview uses this app's iframe.
# none    = no dedicated viewer (Monaco / ImageViewer already handle it).
kind = "visual"

preview_path = "/?file={workspace_file}"

# Workspace extensions this app owns (no leading dot). Desktop uses the
# list to pick the preview iframe. Declaring docx / xlsx / pptx / pdf
# takes those over from the built-in viewers when the app is enabled.
extensions = ["figcraft"]
```

`kind` is the **preview channel**, not "whether there is a file". Images are `none` (ImageViewer exists); a `visual` app that declared the extension uses its iframe; a DOC document is `library`.

---

## 4. Agent artifact = workspace file

### 4.1 Primary key

```
file_id = (instance_id, workspace_id, relative_path)
```

Chat, preview and MCP `path` all cite it. `conversation_id` MUST NOT be the primary key.

### 4.2 Lifecycle (same as generating an image)

| Step | What happens |
|---|---|
| Create | Agent writes a new file in the workspace (FigCraft: `save_file` / `file.save` to a workspace path; or `export` then `file_write` bytes). An empty file still counts |
| Edit | MCP `open_file` that path (if not open) → edit tools → `save_file`. Disk is authority |
| Preview | User opens the card; the viewer reads workspace file bytes |
| Edit again | MCP on the same path; preview re-reads or takes a snapshot |
| Handoff | File is already in the workspace: copy, open in left-nav FigCraft, import to `doc`, send the path |

An in-process open document is a cache of that path. After restart, `open_file` again. Unsaved edits MAY be lost; the app SHOULD save after any tool that should show up in preview.

### 4.3 Paths and roots

The caller is `X-MCP-Actor`. The Runtime supplies the **workspace root bound to that actor's current session** (recommended header `X-MCP-Workspace-Root`, an absolute path on the Node; or the adapter resolves it via a platform API).

The adapter MUST:

- set FigCraft (and peers) automation read/write roots to that root (or a declared subdirectory);
- refuse paths outside it;
- resolve relative tool paths against it.

If the root is missing, write tools fail with a clear error. MUST NOT write into the app process's own `data_dir` as a fallback.

### 4.4 MCP HTTP

As in doc 34: `POST /mcp`. `open_file` / `save_file` / `create_node` act on the **opened workspace file**. One process MAY keep several documents open by path (FigCraft already has a `document` parameter).

### 4.5 Library apps

DOC / PM MUST NOT be rewritten as "write workspace then upload". Their artifacts are library records. Chat "open this document" is already the preview. Workspace-file rules apply to **visual file types** and ordinary attachments.

---

## 5. User canvas

- Entry, ticket, sandbox: doc 34.
- The user opening the agent's `.figcraft` from the workspace tree into left-nav FigCraft is user behaviour.
- MUST NOT register the user iframe as an MCP Backend.

---

## 6. Preview

### 6.1 Transcript card

Same as image attachments: workspace-relative path, optional thumbnail, "Open". No iframe in the card.

### 6.2 Editor pane

Opening uses the existing "open workspace file" slot:

| Extension | Viewer |
|---|---|
| png / jpg / … with no app claiming them | `ImageViewer` (existing) |
| png / jpg / heic / raw / psd … (`lightcraft` enabled) | `AppFrame` on LightCraft |
| svg / svgz / ai / eps / dxf / cdr / emf … (`vectorcraft` enabled) | `AppFrame` on VectorCraft |
| docx / doc / odt / rtf / tex (`wordcraft` enabled) | `AppFrame` on WordCraft |
| xlsx / xls / xlsm / ods / csv / tsv / scjson (`gridcraft` enabled) | `AppFrame` on GridCraft |
| pptx / ppt / potx / ppsx / `deckcraft` (`deckcraft` enabled) | `AppFrame` on DeckCraft |
| pdf (`pdfcraft` enabled) | `AppFrame` on PdfCraft |
| `figcraft` (and extensions declared `kind = visual`) | `AppFrame` on that app's `preview_path`, file bytes handed to the app |

The pick is data-driven: `pickVisualApp(apps, fileName)` matches the file's
extension against the **enabled** apps' `ui.canvas.extensions` and takes the
lowest `nav_order`. An extension no installed app claims keeps its built-in
viewer (`ImageViewer`, the Office wrappers, `Monaco`) — installing an app is
what takes a format over, and uninstalling it gives the format back. Nothing
is hard-coded per extension in the Desktop.

`.figcraft` is added to `isWorkspacePreviewable`. Closing preview does not affect MCP.

### 6.3 Following edits

After `save_file`, the preview re-fetches bytes. The iframe MUST NOT be the only open engine copy.

### 6.4 Save back to the workspace file

A preview is not read-only: the user edits in the iframe and presses the app's
own Save. The bytes have to land on the workspace file, or the tab loses them.

The app's host contract decides how it hands them over — every Craft web shell
has one (page mode on `data-host="page"`): it emits a `<app>:save` message with
`{ name, bytes, saveAs, id? }` instead of downloading. The injected adapter
script forwards it to the Desktop:

```json
{ "channel": "acowork", "type": "saveFile", "name": "report.docx",
  "bytes": "<Uint8Array>", "saveAs": false, "id": "…" }
```

`AppFrame` writes it through `PUT /api/agents/{id}/workspaces/file` and answers
`{ "type": "saved" | "saveFailed", "id": "…", "error": "…" }`, which the adapter
turns back into the app's `<app>:saved` / `<app>:failed` message.

Two rules:

1. The write body carries `contentBase64`, not `content`. The text field would
   corrupt a `.docx`; the Runtime decodes the base64 and writes the bytes
   verbatim (and refuses an undecodable payload instead of truncating the file).
2. A save whose name or extension differs from the opened file (Save As,
   Export to PDF) is refused with a message — silently writing those bytes into
   the opened `.docx` would destroy it. Creating the sibling file is a
   follow-up; until then the app's own download path still works for copies.

Neither side is trusted blindly: the name comes from the app, the path from the
tab, and the Desktop only writes the path it opened.

### 6.5 Why not in the bubble

`VirtualRowList` unmounts rows. Images already use "card + open"; `.figcraft` does too.

---

## 7. Theme and locale

`AppFrame` `postMessage`s on load / `ready` and when the light/dark class changes. The adapter injects the listener; each app keeps its own light/dark command, so the adapter only carries the mapping:

| App | Command |
|---|---|
| FigCraft | `view.themeLight` / `view.themeDark` |
| WordCraft | `ui.theme` `{ value: "light" \| "dark" }` |
| GridCraft | `view.darkMode` `{ on: bool }` |
| DeckCraft | `view.dark` `{ on: bool }` |
| PdfCraft | `view.theme.light` / `view.theme.dark` |
| LightCraft | `view.theme` `{ theme: "paper" \| "charcoal" }` |
| VectorCraft | none — the drawing's own colours are the subject, the chrome has no light theme |

Do not use `system` inside the iframe (that follows the OS, not the ACowork shell). Do not patch upstream `index.html`; injection happens when the adapter serves `/ui/`.

### 7.1 Host mode: page, not parent

The ACowork iframe sandbox **includes** `allow-same-origin`, but its origin is the **Gateway** (`http://127.0.0.1:19876`), never the Desktop window's — so to the app the parent is still a stranger. Every Craft web shell refuses its `?host=parent` transport in that case. The adapter therefore opts the page into the shell's **page** transport instead:

- the injected script sets `data-host="page"` on the app's canvas **before** the wasm app starts (it reads the attribute once, at startup) — a `MutationObserver` on `documentElement` catches the canvas during parse;
- the app's host messages then travel as DOM `CustomEvent`s on that canvas (`<app>:ready`, `<app>:open`, `<app>:save`, `<app>:failed`), which need no origin at all;
- the adapter script bridges those events to the `{channel:"acowork"}` `postMessage` contract above.

VectorCraft has no host mode; its `window.vectorcraft` page API is used instead (`openFile(name, bytes)` for the open, the saved-file list for the save).

---

## 8. Handoff and co-edit

The file is already in the workspace. Handoff is that path: user copy / left-nav open / DOC import / send to another agent.

Co-edit: only after the app UI embeds chat. Platform chat never co-edits automatically.

---

## 9. Hosted adapter

| Responsibility | Adapter | Not here |
|---|---|---|
| ADR-116 CLI, `/health`, `/ui/`, `POST /mcp` | adapter | upstream desktop `main` |
| Actor → workspace root → automation roots | adapter | upstream |
| Open/save workspace files by path | adapter Backend + upstream `open_file`/`save_file` | anonymous Headless as authority |
| Feed workspace bytes into the preview iframe | adapter | upstream `index.html` |
| Theme injection | adapter response | upstream sources |

---

## 10. Platform-shell obligations

| # | Layer | Duty |
|---|---|---|
| P1 | Runtime | MCP requests carry `X-MCP-Actor` and the current workspace root (`X-MCP-Workspace-Root` or equivalent) |
| P2 | Gateway | Forward that header; workspace **bytes** still use the existing workspace file API |
| P3 | Desktop `AppFrame` | Re-push theme on class change |
| P4 | Desktop workspace / chat | `.figcraft` is previewable; chat card opens the editor pane |
| P5 | Desktop layout | Preview `AppFrame` MUST NOT overwrite left-nav `hostedAppId` |

---

## 11. New-app checklist

Doc 34 hosting rules, plus:

1. Choose `ui.canvas.kind` (preview channel, not "has a file")
2. `visual`: artifact in the agent workspace; MCP paths confined to that root; declare `extensions` + `preview_path`
3. `library`: write the app store; do not "unify" onto workspace files
4. MCP does not attach to the left-nav iframe
5. Preview is not a chat bubble
6. Glue stays in the adapter
7. Out-of-root paths error; no fall-through to "whatever is open"

FigCraft: left-nav WASM for the user; workspace `*.figcraft` + engine for the agent; editor-pane iframe to preview the file; do not change `crates/mcp`, `crates/engine`, or `apps/figcraft-web/index.html`.

---

## 12. Security

- Preview reads workspace files under existing workspace permissions.
- MCP paths stay inside `X-MCP-Actor`'s workspace root.
- Preview sandbox matches the user canvas.

---

## 13. Verification (when implementing)

1. After the agent creates `designs/a.figcraft`, the workspace tree shows it; left-nav FigCraft does **not** spontaneously show that document.
2. Close preview, MCP edit + `save_file`, disk updates; reopen preview shows the new content.
3. Paths outside the workspace are refused.
4. User B's agent writes its own workspace; user A's tree does not contain that file.
5. Scrolling chat does not unmount the editor-pane preview iframe.
6. ACowork light/dark: an open preview follows the shell.
7. Adapter diff against upstream `index.html` is empty.

---

## 14. Boundaries

- Final header name for the workspace root may match Runtime when implementing; the meaning does not change.
- In-app co-edit: out of this version.
- memory panel: still excluded.
- Mobile: Desktop first; "the artifact is a workspace file" still holds.
