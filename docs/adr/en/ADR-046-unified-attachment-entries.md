# ADR-046: Unified Attachment Entries (File Upload / Image Upload / Add to Chat)

> **Chinese source of truth**: [ADR-046](../zh/ADR-046-unified-attachment-entries.md)
> **Terminology**: see [GLOSSARY.md](./GLOSSARY.md)

## Status

Draft

## Date

2026-07-25

## Decision Makers

大鱼 (Dayu)

## Predecessors

- [ADR-021](./ADR-021-unified-session-data-loading.md) — unified session data loading path
- ADR-024 — per-session meta file + JSONL header removal
- [ADR-034](./ADR-034-mqtt-http-boundary.md) — MQTT / HTTP boundary (HTTP remains the document upload channel)

**Trigger**: a user uploads an image for the Agent to analyze. The analysis is correct, but
the conversation JSONL contains no trace of the upload at all; reopening the session
later loses the file, and the chat history shows no file icon. The three attachment
kinds (file upload, image upload, Add to Chat) each take a different, non-persisted
code path, and the `user` message gets polluted.

---

## 1. Problem

### 1.1 Current state (fact)

| Attachment kind | Entry point | Persistence result |
|---|---|---|
| File upload (PDF/DOCX/PPTX/XLSX) | `upload_document` HTTP → `<work_dir>/sessions/{sid}/documents/<doc_id>` + a `documents.json` sidecar index | JSONL gets one `metadata.type="document_upload"` system entry, but `filename` / `format` / `size_bytes` / `path` are all null — `gateway_loop` forwards only a `document_ids` string array and drops the metadata |
| Image upload (PNG/JPG) | frontend base64 → `content_parts.image_url` → `ChatMessage::user_multimodal` (**memory only**) | JSONL records **nothing**; the base64 is discarded after crossing MQTT |
| Add to Chat (file / selection / folder) | frontend `addAttachedContext` → `params.attached_context` → `build_attached_context_blocks` folds it into `enriched_content` | JSONL records **nothing**; it only lives in that one LLM request prompt |

### 1.2 User-visible consequences

1. After uploading a PDF and restarting the session, JSONL has a `document_upload` entry but all metadata is null
2. After uploading an image and restarting, the image is gone entirely
3. After Add to Chat on a selection/file and restarting, the context is lost
4. Chat history never shows an attachment icon (no separate entry type → no render branch)
5. After a restart the `user` message is folded back and complicated by `mergeDocumentUploads`, so it no longer matches the original input

## 2. Decision

### 2.1 Unified entries

**Every attachment is a standalone system entry in JSONL**, and the `user` message keeps
only the user own words. The attachment kind is distinguished by `metadata.type`:

```jsonl
{"id":"...","ts":"...","role":"system","content":"Uploaded file: report.pdf","metadata":{"type":"file_upload","document_id":"0123456789ab-3","filename":"report.pdf","format":"pdf","size_bytes":12345}}
{"id":"...","ts":"...","role":"system","content":"Uploaded image: screen.png","metadata":{"type":"image_upload","document_id":"0123456789ac-7","filename":"screen.png","format":"png","size_bytes":987654,"width":1920,"height":1080}}
{"id":"...","ts":"...","role":"system","content":"Attached: src/main.rs","metadata":{"type":"attached_file","abs_path":"/abs/path/src/main.rs","name":"main.rs"}}
{"id":"...","ts":"...","role":"system","content":"Attached: src/main.rs (L10-L25)","metadata":{"type":"attached_selection","abs_path":"/abs/path/src/main.rs","name":"main.rs","start_line":10,"end_line":25}}
{"id":"...","ts":"...","role":"system","content":"Attached folder: src/","metadata":{"type":"attached_folder","abs_path":"/abs/path/src","name":"src"}}
```

The `user` message always contains only the user's original words — except when there is
an image, where the base64 is injected into the LLM through `content_parts` for that one
request but is NOT written to JSONL (the image file itself is already on disk).

### 2.2 Unified file storage

**All uploaded files (PDF/DOCX/image/etc.) land in `<work_dir>/files/<doc_id>.<safe_ext>`**,
a sibling of `conversations/`. The extension comes from the `safe_extension` allowlist:
allowlisted formats keep their real extension, anything else falls back to `bin`.

```
<work_dir>/
├── conversations/
│   ├── <sid>.jsonl          # attachment metadata lives in the JSONL system entries
│   └── meta/<sid>.json
└── files/
    ├── 0123456789ab-3.pdf   # PDF — allowlist preserves the extension
    ├── 0123456789ac-7.docx  # DOCX — allowlist preserves the extension
    ├── 0123456789ad-1.png   # image — allowlist preserves the extension
    └── 0123456789ae-9.bin   # unknown format (exe/html/…) → falls back to bin
```

**Allowlist** (full list in `attachment_impl.rs::safe_extension`):

| Category | Kind | On-disk extension |
|----------|------|-------------------|
| Document | PDF | `pdf` |
| Document | DOCX | `docx` |
| Document | PPTX | `pptx` |
| Document | XLSX | `xlsx` |
| Image | PNG | `png` |
| Image | JPG / JPEG | `jpg` |
| Image | GIF | `gif` |
| Image | WebP | `webp` |
| Fallback | anything else | `bin` |

The allowlist logic MUST stay in lockstep with `detect_format` in the `doc_reader` tool
(`tools/builtin/doc_reader/mod.rs`). If a `docx` lands as `.bin`, `doc_reader` fails to
dispatch on the extension and reports "unsupported document format".

**Deleted**: the `sessions/<sid>/documents/` directory and the `sessions/<sid>/documents.json`
sidecar index, together with `load_documents_index` / `save_documents_index` /
`documents_dir` / `compute_doc_id`. File metadata no longer has a sidecar — **JSONL is the
single source of truth**.

**Add to Chat (`attached_file` / `attached_selection` / `attached_folder`) is not written
to disk**; only its address is recorded in JSONL. These are workspace files that already
exist on disk, so copying them is pointless and the only persistence cost is the path string.

### 2.3 Write timing

| Kind | Trigger | Write |
|---|---|---|
| `file_upload` | after `upload_document` HTTP returns, the frontend puts `{document_id, filename, format, size_bytes}` into `params.document_ids` | the backend `session_task.rs` calls `conversation.append_message_with_id("system", "Uploaded file: ...", metadata, doc_id_as_msg_id)` once it receives the `documents` param |
| `image_upload` | **new** `upload_file` HTTP endpoint (accepts any file, `content_type` inferred from the extension by default). The frontend may pass `width`/`height` back (desktop always does, a CLI might not); the backend schema tolerates them and omits them from JSONL when absent | same as above |
| `attached_*` | the frontend appends one system entry per item to JSONL in current order (before the user message) at send time | reuse `append_message_with_id("system", "Attached: ...", metadata, msg_id)` |

### 2.4 LLM input rework

**Delete** the `enriched_content` assembly in `session_task.rs:807-1100`: the
`<attached_document filename="...">...</attached_document>` doc_reader pre-extraction
blocks, the `[Attached context:] - file: \`path\`` block concatenation, and the
`The following workspace files were attached by the user...` block.

**Kept**:

- images → `content_parts` is injected into the current user message at `agent_loop.run()` (already the multimodal path, unchanged)
- files / selections → surfaced in `context_builder` as **structured references** (each `attached_*` becomes one `- file: \`path\` (L10-L25)` line so the LLM knows to fetch the content itself with `read_file`)

### 2.5 Rendering layer

`MessageBubble.tsx` replaces the single `document_upload` branch with five:

```tsx
if (message.type === "system" && message.metadata?.type === "file_upload")
  return <AttachmentChipRow kind="file"     filename={...} format={...} size={...} />
if (message.type === "system" && message.metadata?.type === "image_upload")
  return <AttachmentChipRow kind="image"    filename={...} format={...} size={...} documentId={...} width={metadata.width} height={metadata.height} />
if (message.type === "system" && message.metadata?.type === "attached_file")
  return <AttachmentChipRow kind="file"     name={...} absPath={...} onClick={openInEditor} />
if (message.type === "system" && message.metadata?.type === "attached_selection")
  return <AttachmentChipRow kind="selection" name={...} startLine={...} endLine={...} onClick={openInEditor} />
if (message.type === "system" && message.metadata?.type === "attached_folder")
  return <AttachmentChipRow kind="folder"   name={...} absPath={...} onClick={revealInTree} />
```

The unified `AttachmentChipRow` component uses icons `FileText` (file), `Image` (image —
thumbnail blob fetched from `GET /files/<doc_id>` then rendered via `URL.createObjectURL`),
`Hash` (selection) and `Folder` (folder). Single click: file/selection calls
`useFileEditorStore.openFile(agentId, workspaceId, relPath)`; folder calls `requestLocate`
plus `requestShowWorkspacePanel`.

**image_upload width/height**: when present in JSONL (desktop always sends it) they are
used as CSS dimension hints; when absent (a future lightweight CLI scenario) the `<img>`
`onLoad` reads the real dimensions as a fallback. **The rendering layer tolerates both paths.**

### 2.6 Deletions

| Location | Removed |
|---|---|
| `core/acowork-runtime/src/http/server.rs` | `UploadDocumentBody`, `upload_document`, `read_document`, `delete_document`, `DocumentEntry`, `DocumentsIndex`, `documents_dir`, `documents_index_path`, `load_documents_index`, `save_documents_index`, `compute_doc_id` |
| `core/acowork-gateway/src/http/proxy.rs` | `proxy_upload_document`, `proxy_list_documents`, `proxy_read_document`, `proxy_delete_document` + their routes |
| `core/acowork-runtime/src/agent/loop_memory.rs` | the whole `write_document_entries` function |
| `core/acowork-runtime/src/agent/session/session_task.rs` | the whole `build_attached_context_blocks` function; the `enriched_content` assembly at `session_task.rs:807-1100`; the doc_reader pre-extraction logic; the manual `file_summary` assembly on image upload |
| `core/acowork-runtime/src/startup/gateway_loop.rs` | parsing `documents: Vec<serde_json::Value>` → parse a rich object array `document_ids: Vec<{document_id, filename, format, size_bytes, [width, height]}>` instead (`width`/`height` are `Option<u32>`, absent means None; matched to the type field to write `file_upload` / `image_upload` respectively) |
| `apps/acowork-desktop/src/stores/chatStore.ts` | the `mergeDocumentUploads` function, the `document_upload` branch in `convertConversationEntry`, the `optimisticDocs` merge, the logic that strips the `documents` field onto the user message |
| `apps/acowork-desktop/src/components/chat/ChatPanel.tsx` | the `upload_document` Tauri command in `handleFileUpload` → replace with a generic `upload_file` (auto-detecting file vs image); `pendingFiles` stays as the uploading temporary state but is refilled from the persisted JSONL system entries right after sending; `optimisticDocs` is removed |
| `apps/acowork-desktop/src/components/chat/MessageBubble.tsx` | the existing single `document_upload` branch → replaced by the five `metadata.type` branches |

### 2.7 HTTP endpoints (kept and extended)

| Endpoint | Purpose |
|---|---|
| `POST /sessions/{sid}/files` | upload any file (PDF/DOCX/PPTX/XLSX/PNG/JPG/...). `multipart` form `{file, content_type?, width?, height?}` (`width`/`height` optional — desktop always sends them, a CLI may omit them). Returns `{document_id, filename, format, size_bytes, [width, height]}` (the server echoes back whatever width/height it received; they are absent when not sent). **Saved to `<work_dir>/files/<document_id>.<safe_ext>`**. The backend does no image recognition, does not read the header, and does not depend on the `image` crate — it fully trusts the metadata the frontend passes |
| `GET /files/{document_id}` | download the file (auth passes through the Gateway proxy; the Tauri side uses this to load thumbnails). The implementation also falls back to reading `<document_id>` (the extensionless legacy filename) so historical data stays readable, though new writes no longer use the legacy path |

### 2.8 Extension allowlist (`<safe_ext>` resolution)

The `format` field of `upload_file` is a user-controllable lowercase string (it comes from
the extension picked by the desktop dialog filter). Concatenating it straight into
`<doc_id>.<format>` would let a hostile or accidental `format="exe"` land on disk where
Finder / Explorer treats it as an executable — that MUST be avoided.

`RuntimeAttachmentService::safe_extension` maps `format` explicitly onto an allowlist:

| Input `format` | On-disk extension |
|---|---|
| `pdf` | `pdf` |
| `png` | `png` |
| `jpg` / `jpeg` | `jpg` |
| `gif` | `gif` |
| `webp` | `webp` |
| anything else (including `""`, unknown, containing `/`, containing `..`) | `bin` |

**Key constraints**:

- the extension is always chosen by `RuntimeAttachmentService` itself and the user-supplied string is **never** trusted verbatim
- on-disk readability, replay, and `open -t` preview all follow this allowlist
- the `read_file` implementation accepts both `documents/<doc_id>.<safe_ext>` and the legacy `<doc_id>` filename (legacy fallback) so older developer-disk data stays readable

### 2.9 Outbound raster policy (added 2026-10)

The "no image recognition, no header reads, no `image` crate" rule in §2.7 constrains the **upload ingest** endpoint: `POST /sessions/{sid}/files` still stores the bytes untouched, not one bit changed.

**Outbound** (assembling this turn's attachments into a model request) is a different path, and its behaviour is fixed here:

| Fact | Rule |
|---|---|
| Delivered long side | 1568 px (`image_raster::DEFAULT_MAX_SIDE_PX`, the same value as `image_read`'s `high`: the largest raster OpenAI's `high` detail and Anthropic image input accept without rescaling server-side) |
| Decode memory budget | `image_raster::ATTACHMENT_DECODE_BUDGET` = 24 MP (~96 MiB RGBA). A source outside that budget is **neither decoded nor delivered raw** — it is skipped as `ImageSkip::TooLarge`; the file stays on disk and the prompt hint points at it, so `image_read` can still reach it |
| Image within the long-side cap | Delivered byte-for-byte, never re-encoded (a small picture is not worth the quality loss) |
| Delivered MIME | Comes from **magic-byte sniffing**, never from the upload's extension; a downscaled WebP/GIF is re-encoded as PNG, JPEG stays JPEG |
| Per-item payload | Still bounded by the 8 MiB inline budget; a PNG over it is re-encoded as JPEG at fixed quality first, and only skipped if that also fails |
| Determinism | Filter, JPEG quality and output encoding are constants (ADR-060 prefix cache: byte-stable across turns of the session) |
| What the model is told | On a downscale the prompt hint states both the delivered and the source size, so the model cannot believe it is looking at the original |

**Why this is needed**: an uploaded 4K screenshot used to be base64'd verbatim, so the model paid for its full pixel bill on **every** request of the session, while simply dropping an explicit human attachment is not acceptable either. Downscaling is the only answer that satisfies both "the human attached it, the model must see it" and "the model must not be billed for pixels it cannot use".

**Boundary**: this policy applies to image modality only. PDFs follow the document clause in §2.4, every other file is a structured reference, and no bytes of either enter the request.

## 3. Final directory structure

```
<work_dir>/
├── conversations/
│   ├── <sid>.jsonl       # pure conversation + attachment entries (system role + metadata.type)
│   ├── <sid>.jsonl.lock  # existing file lock
│   └── meta/
│       └── <sid>.json    # ADR-024 per-session meta
└── files/
    └── <doc_id>.<safe_ext>  # all uploaded files (extension allowlist in §2.8)
```

## 4. Compatibility

**None.** The project is still in development, so no compatibility code is kept and no
migration script is written:

- old `document_upload` entries are not recognized (discarded)
- the old `sessions/<sid>/documents/` directory is left for the user to handle manually (to keep the data, the user copies or moves it to the new location themselves)
- the old `documents.json` sidecar index is deleted outright
- the old `mergeDocumentUploads` / `optimisticDocs` / `documents`-inline-into-user-message logic is all removed

## 5. Risks and rollback

| Risk | Mitigation |
|---|---|
| On-disk filename collisions under heavy concurrent uploads | `<doc_id>` is the first 6 bytes (12-hex prefix) of `SHA-256(bytes)` plus bytes 6..8 (4-hex suffix) — **fully content-derived and stable across processes and restarts**. The birthday bound for a 48-bit prefix is ≈ 2²⁴ ≈ 16M blobs before ~50%, which is not a practical risk; if it ever happens the read path returns an `ambiguous on-disk match` error rather than guessing |
| The `files/` directory grows without bound over time | **out of scope for this round**, to be handled uniformly by a future document management feature |
| `MessageBubble` multi-branch regressions | one React test snapshot per each of the five `metadata.type` values |

## 6. Implementation breakdown

1. **Backend schema** (`conversation.rs` / `loop_memory.rs` / `session_task.rs`) — new metadata field types (`FileUploadMeta` / `ImageUploadMeta` / `AttachedFileMeta` / `AttachedSelectionMeta` / `AttachedFolderMeta`); replace `write_document_entries` to accept a rich object array; delete the `enriched_content` assembly.
2. **Backend storage** (`http/server.rs` + `proxy.rs`) — new `POST /sessions/{sid}/files` and `GET /files/<doc_id>` (**no DELETE**, left to a future document management feature); delete `upload_document` / `read_document` / `delete_document` / `documents.json`; change the on-disk path to `<work_dir>/files/<doc_id>`.
3. **Frontend send** (`chatStore.ts` / `ChatPanel.tsx`) — `sendMessage` gains an `attachedItems` parameter (from `sessionState.attachedContext`); the frontend only pushes `document_ids + attached_items` to the backend through params and the backend writes JSONL, which is simpler than having the frontend write system entries itself; `pendingFiles` / `pendingImages` stay as local upload state but are cleared right after sending; a unified `upload_file` Tauri command (auto-detecting PDF vs image).
4. **Frontend rendering** (`MessageBubble.tsx` / new `AttachmentChipRow.tsx`) — the five `metadata.type` branches; the `image_upload` thumbnail blob comes from `GET /files/<doc_id>` via `URL.createObjectURL`; the desktop frontend **always** sends width/height at send time (read with `new Image()`), and when JSONL carries them they are used as thumbnail CSS hints; a future CLI client may omit them and the rendering falls back to the real dimensions from `<img onLoad>` — **both paths are tolerated**.

## 7. Decision summary

- ✅ Reuse the `metadata.type` discriminant (same mechanism as `compaction`)
- ✅ `<work_dir>/files/` sits at the same level as `conversations`
- ✅ Files land on disk without a user-chosen extension (the kind lives in the JSONL metadata)
- ✅ `attached_folder` is not written to disk; only its address is recorded in JSONL
- ✅ No compatibility code and no migration script (old files are handled manually by the user)
- ✅ The user message always contains only the original words
- ✅ Image width/height are **optional** (`Option<u32>`): desktop always sends them, a future CLI may omit them; JSONL stores them tolerantly via `skip_serializing_if = "Option::is_none"`, and the rendering layer falls back to `<img onLoad>` when they are missing
- ✅ `files/` directory cleanup belongs to a future document management feature and is not implemented here
- ✅ `DELETE /files/<doc_id>` is not implemented; it is left to a future document management feature
