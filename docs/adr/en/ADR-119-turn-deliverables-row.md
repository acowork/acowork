# ADR-119 Turn Deliverables Row: Files and Images a Turn Produced, Summarised as One Transcript Row

- **Status**: Accepted (Runtime + Desktop implemented, verified on Windows; Decisions 4 and 7 amended 2026-11 — see those sections; **2026-11 mechanism rework: the turn-end system row is retired, produced facts ride on each tool_result, display policy moves to the frontend** — see the revision section below)
- **Date**: 2026-11-05
- **Decider**: 大鱼
- **Related**: ADR-046 (attachments as parts of the user turn), ADR-107 (message-ref parts), ADR-118 (desktop child-webview in-app browser), ADR-024 (merge metadata into the session index)

## Revision (2026-11): produced facts ride on each tool_result, display policy moves to the frontend

The turn-end `deliverables` system row is **retired**. This revision supersedes the "standalone system row + runtime adjudication" mechanism in Decisions 1/2/3/5/6; Decision 4 (image bytes land on the node, the channel carries coordinates only) and Decision 7 (clicks reuse the existing viewer) are unchanged.

**New mechanism**:

- **The backend supplies facts only.** When a tool result is persisted (`persist_and_emit_tool_results`), the produced declarations of that tool_call are mapped to coordinates and stamped onto **that `tool_result`'s own `metadata.produced`**: `[{workspace_id, rel_path, mime, bytes, created}]`. `turn_deliverables::to_coordinates` only maps paths to coordinates and filters noise (MIME-decidable, size ceiling); it no longer dedups or caps. Image byte landing (Decision 4) is kept.
- **The facts never enter the model context.** The restorer rebuilds model messages from `content` / `tool_call_id` / `tool_name` only; `metadata.produced` has zero effect on the LLM input — it is a fact annotation for the frontend, not reasoning material.
- **The live channel carries the same facts.** The `record_complete` event gains `metadata_json` (an additive proto field) mirroring that JSONL row's metadata; the desktop merges it into `toolData`, so **the live projection equals the restored one** — the banner is derivable within the turn with no extra HTTP fetch. (The old mechanism relied on a persist-only system row creating a count divergence that drove a refresh; that retired with the row.)
- **The frontend derives uniformly.** [deriveDeliverables.ts](../../../apps/acowork-desktop/src/lib/deriveDeliverables.ts) is one pure function: given all `tool_result`s of a turn, it yields that turn's deliverables — files/images read `metadata.produced` (`created` filter, dedup, per-kind cap of 8, images first), docs read the MCP result envelope. When/where/what to display is entirely frontend logic.
- **Two row shapes, one derivation (added 2026-12).** A history page is read through the `groups=stub` projection: a contiguous explore run collapses into one `explore_stub` and the fact-carrying `tool_result` rows disappear with it. The fold therefore **forwards** those rows' artifact facts verbatim ([artifact_facts.rs](../../../core/acowork-runtime/src/agent/artifact_facts.rs): a `FACT_KEYS` allow-list, copied untouched, projection-only and never persisted, no tool names), and the frontend reads `exploreStub.facts` inside the same derivation above. Without the forwarding, one turn reconstructs as two different conversations — the reloaded history page simply has no deliverables row.
- **New deliverable category: doc-service online documents.** A successful (`status=applied`) `doc_write`/`doc_move` result envelope is parsed by [deliverableDocs.ts](../../../apps/acowork-desktop/src/lib/deliverableDocs.ts) into a doc card (deduped by doc_id, last write wins); clicking opens a **read-only** Markdown preview in a file tab (`fileEditorStore.openDocPreview`, fetched by doc_id through the Gateway doc proxy, reusing `DocMarkdownView`). Doc recognition is a separate adapter and does not pollute the tool-agnostic file-deliverable path.
- **File-type icons.** File chips now use the workspace's SetiIcon (reusing `fileIcons.getFileIcon`) instead of a uniform FileText.

**Compatibility**: `deliverables` rows already persisted in old sessions are still read by `parseDeliverableFiles`; new sessions no longer write the row.

**The falsifiability criterion still holds**: the file-deliverable path (`turn_deliverables.rs` / `deliverableFiles.ts` / `ChatDeliverablesRow.tsx`) does not branch on tool names; doc recognition is isolated inside the `deliverableDocs.ts` adapter.

## Background

One turn can run a hundred tool calls. What the user reviews is the **result**, not the hundred steps. Today's transcript structure makes that hard:

- every tool call / result / thought folds into an `explore_group` block, **collapsed by default**;
- one screen routinely shows a dozen `Explored (N steps)` blocks interleaved with assistant text;
- so any per-tool-block surface is either inside the fold (invisible) or on every block (noise).

Two independent needs land in the same place:

1. **Browser automation**: when an agent debugs a page through a third-party MCP such as Playwright, screenshots are its only output form. Without a way to show them, "what the agent did in a remote browser" stays invisible to the user.
2. **General output**: reports, charts, PDFs and data files an agent writes are equally evidence of "what this turn did".

These two must stay decoupled in the implementation: the Runtime knows neither Playwright nor "browser automation". It knows one thing — **a tool's result carried image bytes**. Any MCP that returns an image gains the display for free; any tool that stops returning one disappears from the row without a line changing.

## Decisions

### Decision 1: one row per **turn**, never per step

The deliverables row is a `system` entry written when the turn ends (`metadata.type = "deliverables"`), placed **after** the turn's last assistant message (visually below it). Intermediate output is recorded but not rendered.

There is one reason, and it is measurable: **the number of rows must not grow with the step count**. Per-step rendering looks fine in a 3-step turn and is noise in a 111-step one; per-turn is one row either way.

### Decision 2: the rule is "tools declare, the Runtime adjudicates", not after-the-fact probing

Turning a file into a deliverable needs two roles, neither optional:

- **The tool declares** (`ToolContext::record_produced_file`): only the writer knows whether the path existed before it wrote. That fact cannot be guessed — `mtime` is fooled by checkout and copy, a whole-tree snapshot has uncontrolled IO cost on a remote node, and parsing the tool's result text means guessing at three different path notations.
- **The Runtime adjudicates** (`turn_deliverables::collect`): context-free rules — is it `created`, does it fall under a known workspace root, does it hit a noise directory, is the MIME identifiable, is it within the size and count caps, has `(workspace_id, rel_path)` already been seen.

Adjudication lives in the Runtime and declaration in the tool so that adding a file-writing tool is one act: declare. Every rule has exactly one home and no tool can quietly route around it.

### Decision 3: `created` is the only "output" semantics; a rewrite is not

`created = the path did not exist before this write`.

- Rewriting an existing source file is **process**, not output — nobody wants every version of their own `src/main.rs` in the deliverables row.
- Several writes to one path in a turn: the first is `created=true` and lands; the rest are `created=false` and dedupe keeps the first.
- Created in a previous turn, edited in this one: not this turn's output, which is exactly the intent.
- `file_edit` can only edit a file that exists (`old_text` must match), so it does **not** report — a call site that can only ever report `created=false` has no reason to exist.

### Decision 4: image bytes stay on the node; only a path crosses the message channel

A `data:` URI image in a tool result's `content_parts` is decoded by the Runtime, written to `<agent_home>/artifacts/<session_id>/<stem>-<content tag>.<ext>`, and recorded as a `ProducedFile`.

> **Amended (2026-11)**: the first edition used `<tool_call_id>-<index>.<ext>`. That was deterministic and meaningless to a human — `call_01a2095c7dba7734ae7a-0.png` in the chip, in the editor tab and in the OS file manager — and reading the same picture twice produced two deliverables. Both halves of the name are now computed by the Runtime from facts it already holds: `<stem>` is the file the call was *about* (the `path` argument of `image_read`; a tool that points at no file falls back to its own name, then to `image`), `<content tag>` is a digest prefix of the bytes. Determinism improved rather than weakened: same file at the same detail ⇒ same path, same bytes ⇒ one entry in the row; same stem with different bytes (a different source, or a different detail) ⇒ different names, no overwrite. The stem is always sanitised — last segment only, non-alphanumerics collapsed into one space, Windows reserved names prefixed, 48 characters — because that path comes from the model.

The desktop **never receives the bytes**: it takes `(workspace_id, rel_path)` through the existing workspace file route (`GET /workspace-files/{agentId}/{workspaceId}/{path}`, ADR-055 L2-7), and the byte cache is the LRU in [workspaceFileBlob.ts](../../../apps/acowork-desktop/src/lib/workspaceFileBlob.ts). This is the **same flow** as previewing a file in the right-hand workspace panel, not a second one.

The consequences are the good kind: base64 never rides MQTT, a remote node does not pay transfer for something the user may never look at, and thumbnail and full image share one cache entry.

An `http(s)` image part is neither decoded nor downloaded: that would make a deliverables row depend on someone else's server staying up.

### Decision 5: the write point converges on the turn's single exit

"One row per turn" has to be a property of the code, not a rule every call site remembers. The turn has four `ChunkEvent::Done` exits (normal, replay, error, interrupted), but beneath them `run()` / `replay()` both funnel through one function, `AgentLoop::close_turn_round` — the row is written there, and **before** `end_round` (the row must carry this round's id, and must sort after the assistant row it describes).

The same choke point buys a second guarantee: the sink is cleared at each turn entry, so last turn's files never leak into this turn's row.

### Decision 6: the row is standalone, not a part of the user turn

`metadata.type` now answers two different questions, which until now had one answer:

| Question | Attachments / refs | Deliverables row |
|---|---|---|
| Keep this row's metadata? | yes | **yes** |
| Fold into the user turn above? | yes | **no** |

So the registry splits into two sets: `isTurnPartMeta` (keep the metadata) covers all three kinds, and `isUserTurnPart` (fold into the user block) **excludes** the deliverables row. Adding the row to the existing sets would fold the thumbnails under the previous user message — semantically they belong to the assistant's answer, not to the human's request.

A rendering bonus follows for free: `system` is in `avatarAnchor`'s `SKIP_TYPES`, so the row grows no stray avatar and does not disturb the existing rule for later agent blocks.

### Decision 7: the click reuses the existing viewer, through the chat-side open rule

Clicking a thumbnail or a chip goes through the **same decision every other clickable file reference in chat already uses** (`pickOpenActionForPath`, shared with `openWorkspaceRef` chips and `<a>` links in messages): an image opens with `openPreview`, everything else with `openFile`. One editor tab (zoom, pan, region-to-chat), zero new viewer components.

> **Amended (2026-11)**: the first edition said "clicking calls `openFile`, literally the same action as clicking the same image in the workspace tree". That was never true: `openFile` opens a tab in **edit** mode, while the editor's image branch in `FileEditorPanel` requires `mode === "preview"` (the workspace tree itself calls `openPreview` for previewable files). So clicking an image in a deliverables row showed a wall of base64 in Monaco — images are in `BINARY_EXTENSIONS`, and the JSON content read returns base64. The fix is not a new rule but the existing one: chat-side entry points share `pickOpenActionForPath`.
>
> It is deliberately **not** the workspace tree's `isWorkspacePreviewable`, which also previews `.md`: a chat click on a Markdown or JSON file expects source (rendering a multi-MB file in the preview used to freeze the page). The division between the two rules is documented in the header of [filePreview.ts](../../../apps/acowork-desktop/src/components/editor/filePreview.ts).

One route, one cache and one set of coordinates also impose a requirement on the thumbnail itself: it must fetch through the editor preview's **`useWorkspaceFileBlob`**, not a hand-written `fetch` in the component. The first edition's effect kept its liveness flag on the component instance instead of per effect run, so under the StrictMode double-mount (and whenever the virtual list recycled the row) the first run's `AbortError` arrived after the second run had re-raised the flag, was read as a failed download, and latched the row into chips — every image degraded to a file banner.

## Out of scope for v1 (known holes, recorded deliberately)

| Hole | Today | Position |
|---|---|---|
| Files written by `bash` | not visible | not in v1. A later option is a `git status --porcelain` diff, but it drags in untracked noise and needs a filter design first |
| Files with no identifiable MIME (`.ts`, `.rs`, extensionless) | recorded, then dropped by adjudication | the row offers only what **opens**; a chip that opens a broken view is worse than no chip |
| Files over 50 MB | dropped | the row has to stay usable on a remote node; previewing huge files is a different problem |
| Intermediate images | recorded, then deduped / capped within the turn | the row's value is "what this turn produced", not "how many pictures it drew" |
| Per-kind caps | 8 images, 8 other files | the full set is still on disk; the row only has to be scannable |

## Failure modes

| Situation | Behaviour |
|---|---|
| No workspace resolver bound (a loop built without startup wiring, unit tests) | no row. Better no row than a fabricated `__agent_home__` that 404s |
| Session has no transcript writer | no row (`conversation` is `None`) |
| Image part is not valid base64 | skip that part, log `warn`, the rest of the parts and the turn are unaffected |
| Writing the artifact fails (disk full, permissions) | log `warn`, that file is not recorded, the turn ends normally |
| Metadata read fails | treated as "file does not exist", not recorded, not an error |

A deliverables row is never a reason for a turn to fail: every failure path costs a thumbnail, not the turn.

## Falsifiability (how this decision is accepted)

Replace the Playwright MCP with any third-party MCP that returns an image:

- Runtime: `git diff` is empty. The Runtime does not know tool names.
- Desktop: `git diff` is empty. Rendering keys off `metadata.type`, never a tool name.
- The only change: the new MCP's screenshots appear in the same row.

The reverse test: if anyone ever adds `if tool_name == ...` to `turn_deliverables` or `ChatDeliverablesRow`, this ADR has been violated.

## Incidental fix

Images returned by the built-in `image_read` tool previously reached the model's context but never the UI — the model saw what the user could not. This general rule fixes that too: its image parts take the same persist-and-record path, with no line added to `image_read`.

## Files

- [core/acowork-runtime/src/agent/turn_deliverables.rs](../../../core/acowork-runtime/src/agent/turn_deliverables.rs)
- [core/acowork-core/src/tools/traits.rs](../../../core/acowork-core/src/tools/traits.rs) (`ProducedFile`, `ProducedFileSink`, `ToolContext::record_produced_file`)
- [core/acowork-runtime/src/tools/builtin/file_write.rs](../../../core/acowork-runtime/src/tools/builtin/file_write.rs)
- [apps/acowork-desktop/src/lib/deliverableFiles.ts](../../../apps/acowork-desktop/src/lib/deliverableFiles.ts)
- [apps/acowork-desktop/src/components/chat/ChatDeliverablesRow.tsx](../../../apps/acowork-desktop/src/components/chat/ChatDeliverablesRow.tsx)
- [apps/acowork-desktop/src/components/chat/messageParts.ts](../../../apps/acowork-desktop/src/components/chat/messageParts.ts)