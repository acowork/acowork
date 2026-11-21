# ADR-119 Turn Deliverables Row: Files and Images a Turn Produced, Summarised as One Transcript Row

- **Status**: Accepted (Runtime + Desktop implemented, verified on Windows)
- **Date**: 2026-11-05
- **Decider**: 大鱼
- **Related**: ADR-046 (attachments as parts of the user turn), ADR-107 (message-ref parts), ADR-118 (desktop child-webview in-app browser), ADR-024 (merge metadata into the session index)

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

A `data:` URI image in a tool result's `content_parts` is decoded by the Runtime, written to `<agent_home>/artifacts/<session_id>/<tool_call_id>-<index>.<ext>`, and recorded as a `ProducedFile`.

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

### Decision 7: the click reuses the workspace viewer; no new viewer

Clicking a thumbnail calls `useFileEditorStore.openFile(agentId, workspaceId, relPath)` — literally the same action as clicking the same image in the right-hand workspace tree: open the editor's image viewer tab (zoom, pan, region-to-chat).

Zero new viewer components. That is a result of one route, one cache and one set of coordinates, not an adaptation layer built on top.

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