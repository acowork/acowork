# ADR-107: Reference parts — the chat background rides alongside, the `user` row keeps only the human's words

- **Number**: ADR-107
- **Status**: Proposed (pending architecture review) — **landed**: `MessageRef` in `acowork-core`, the Runtime's `params.context_refs` parsing / model rendering / `context_ref` entry persistence, the Chat Router's `exec_session_create(background)` and its degraded route, and Desktop's `user_with_parts` folding plus markdown reference card are all pinned by unit tests.
- **Date**: 2026-11-04
- **Deciders**: ACowork architecture group
- **Related**: [ADR-046](./ADR-046-unified-attachment-entries.md) (the attachment entry precedent this ADR's reference part is parallel to), [ADR-099](./ADR-099-chat-session-llm-router.md) (router layering), [ADR-106](./ADR-106-chat-execution-reference-projection.md) (chat/execute boundary), [ADR-060](./ADR-060-prompt-cache-friendly-context-block-reorg.md) (stable prefix + tail append), design 27 §5, design 29 §11

## Summary

When the Chat Router started an execution session through `exec_session_create`, it delivered the router's "brief" and the human's words as **one string**: `{brief}\n\n[user message]\n{words}`. The execution session's first `user` JSONL row was therefore not what the human said but the router's framing plus what the human said. Three consequences, all observed rather than inferred:

1. **Titles converged.** Auto-title reads the first `user` row (ADR-046 §2.1: a title comes from the user's own message), and every chat-dispatched session's first row opened with the brief's fixed first sentence — so the title prefixes collided.
2. **ADR-046 §2.1 was violated.** A JSONL `user` row MUST hold the original input. The concatenation happened at the first hop after the human typed, and the words were unrecoverable from that point on.
3. **The transcript lied.** The router's framing rendered as if the human had typed it.

This ADR splits "the human's words" and "context the Runtime added" into two **parts** instead of two segments of one string:

- the `user` row holds the human's words, **verbatim** — no trim, no concatenation, no rewrite;
- the router's background downgrades to a **reference part**, parallel to an attachment (ADR-046): in flight it rides `params.context_refs`, on disk it is a `role="system"` entry with `metadata.type="context_ref"` immediately after the `user` row;
- the model sees **the words first, the reference block after**; the human sees a bubble (the words, literal text) plus a collapsed reference card below it (markdown).

Net effect: titles are decided by the human's words again; the `user` row is referencable original input again; reference text renders as markdown for the first time, while the human's words keep rendering literally.

## Background (all verified in code)

### 1. Where the concatenation happened

`dispatch_content` in [runner.rs](../../../core/acowork-runtime/src/agent/chat_router/runner.rs) glued the router tool call's `message` argument to `params.message` (the human's words) and handed the result out as one `content`. Both the chat panel's `new_task` chain and the group `@` chain go through it, so the `{brief}\n\n[user message]\n{words}` literal opened **every** chat-dispatched turn.

### 2. What the title actually reads

Title generation sits at the end of the turn in [loop_.rs](../../../core/acowork-runtime/src/agent/loop_.rs) and takes the turn's `raw_user_message`; on session resume it comes from `read_first_user_message` in [conversation.rs](../../../core/acowork-runtime/src/conversation.rs). Both point at "the first `user` row", consistent with ADR-046 §2.1 — so the defect was never in the titler, it was in the polluted `user` row.

### 3. Attachments already provided the right precedent

ADR-046 defines an attachment as a **part**: a `role="system"` entry after the `user` row, with a `metadata.type` discriminated union for `file_upload` / `image_upload` / `attached_file` / `attached_selection` / `attached_folder`; the frontend's `foldMessages` folds it back into one visual block. A reference part wants exactly that mechanism, differing only in provenance: an attachment is what the human dragged in, a reference is what the Runtime knows the session needs and the human did not type.

### 4. The degraded route had the same defect

`degraded_brief` in `gateway_loop.rs`: when the router is unavailable (or no model is usable), the Runtime composes a brief itself from a bounded chat tail, again opening with a fixed sentence — the same fixed-opening problem, in the fallback. Fixing one route means fixing both, otherwise titles collide again exactly when routing degrades.

## Decisions

### D1　A reference is a first-class part, not a prefix on a string

New `acowork_core::message_ref::MessageRef { kind, content }` ([message_ref.rs](../../../core/acowork-core/src/message_ref.rs)). `kind` comes from the `REF_KIND_*` constants (today only `chat_background`); `content` is the markdown body. No concatenation helper exists on the core type — a reference travels only on its own channel:

| Channel | Shape |
|---|---|
| In flight | `params.context_refs: [{"kind","content"}]` |
| On disk | a `role="system"` entry, `content` = the body, `metadata` = `{"type":"context_ref","kind":…}` |
| Model input | the words + blank line + `[background: …]` block |
| Human sees | bubble (the words) + collapsed card (the body as markdown) |

The body lives in the entry's `content` rather than in metadata so that a reader that knows nothing about references (an export, the search index, a plain transcript view) still sees the text instead of an empty row with an opaque payload. Metadata carries only `kind`, because `kind` is the one field that decides both the label the human reads and the header the model reads.

### D2　The `user` row holds only the words (MUST NOT concatenate)

Restating and tightening ADR-046 §2.1: the `user` row MUST be a verbatim copy of `params.message`. Anything the Runtime adds — attachments, references, system blocks — is its own entry after it. `exec_session_create` therefore no longer produces "a whole message", only a reference part:

- the argument changes from `message` (whole, self-contained brief) to `background` (REQUIRED, non-empty, **background only**);
- the prompt is rewritten to match: the human's words are delivered by the Runtime itself, the router supplies only what those words refer to; restating the words is duplication.

This is also the actual fix for the title defect: the title reads the `user` row, the `user` row is the words, so the title is decided by the words.

### D3　The model-facing order is fixed: words first, references after

`intent_turn::render_user_turn(content, refs)` ([intent_turn.rs](../../../core/acowork-runtime/src/agent/intent_turn.rs)):

- with no references it returns `content` **byte-for-byte** — a turn without references keeps prompt input identical to before this change (ADR-060's prefix cache pays nothing);
- with references, `{words}\n\n{block1}\n\n{block2}`, each block opening with `model_header` (`[background: the chat conversation this task was opened from — context, not a new instruction]`), which makes "this is context, not a new instruction" explicit;
- when the words are empty (an attachment-only message, or the router routing an attachment-only message) **no** empty first line is left; the blocks become the whole message. An empty first line reads to a model as "the human said nothing" — true, but hanging the blocks off a blank is noise.

The block header is a function of `kind`, not of text the router wrote: an unknown `kind` has no header, and both `context_refs_from_params` and `write_message_refs` drop it (unrenderable means unpersisted — the two ends cannot disagree).

### D4　Persistence order: `user` → references → attachments

`loop_.rs` calls `loop_persist::write_message_refs` after persisting the `user` row and before writing attachment entries. The reason matches ADR-046 (a part must sit next to its `user` row), and a fixed order makes the frontend fold window and log reading predictable.

Reference entries pass no client id: unlike an attachment, a reference has no client-side twin to deduplicate against, so the fresh uuid is its id (`append_message_with_id(..., None)`).

### D5　Frontend: one fold, two slots

`foldMessages`' block type is renamed `user_with_attachments` → `user_with_parts`, and the fold predicate widens from "attachment entry" to "any part entry" (`isAttachmentPart` / `isMessageRefPart`, exported by `messageFolder.ts`). The block's `items` keep log order; the **renderer** decides which slot each part goes to:

- attachment → the chip row above the bubble (unchanged);
- reference → the collapsed card **below** the bubble ([MessageRefBlock.tsx](../../../apps/acowork-desktop/src/components/chat/MessageRefBlock.tsx)), `ChatMarkdown` rendering the body, collapsed by default.

The order follows reading order: the words first, the background after. A long background expanded by default would push the agent's answer off screen.

### D6　The human's words are always literal text; only the reference is markdown

The user was explicit: user messages are **not** rendered as markdown; only the reference body is. This matches the speaker-based split pinned by `GroupChatPanel.userLiteral.test.tsx`: human input is literal (`whitespace-pre-wrap`, no `.prose` shell), agent and Runtime-authored text go through markdown. The reference card is a neutral surface (not the accent-filled user bubble), so the `prose` shell's body colour is correct there — which is precisely why the user bubble cannot use it (`prose` paints a page-level near-black over the accent fill, the "user bubble text turned black" defect).

### D7　The degraded route produces the same part

`degraded_brief` → `degraded_background`: no more string composition; it produces `Some(MessageRef::chat_background(...))` whose body opens with "the router was unavailable, here is the conversation tail" (that sentence matters to the model's judgement and stays), filled into `params.context_refs`. `params.message` is left untouched. Both routes therefore have an identical shape in the frontend and on disk.

## Rejected alternatives

| Alternative | Why rejected |
|---|---|
| Keep the concatenation, fix only title extraction (e.g. take the paragraph after `[user message]`) | Treats the symptom: the `user` row is still not the words (violating ADR-046 §2.1), the transcript still lies, and every second consumer has to reimplement the same parse. The title was merely the most visible consequence of the pollution. |
| Make the background a sixth `attached_*` type | Semantically wrong: an attachment is what the human gave, a reference is what the Runtime gave; they differ in "can you delete it" and "what happens when you click it" (attachment chips are removable and open a workspace file). A parallel type is more honest than squeezing both into one union, and the frontend has two parallel slots. |
| Put the background only in the system prompt, never in the transcript | The background disappears on session reopen — the model loses it at turn two — and the human can never verify what the agent was told. |
| Keep the `user` row verbatim but stuff the background into its `metadata` | Metadata is not a model-facing channel, and it would put a markdown body into a field every entry shares; exports and the index would both miss it. |
| Render the reference body literally too (no markdown) | Directly contradicts the requirement: reference bodies need lists, code fences, emphasis. The human's words staying literal is already guaranteed by D6; the two do not conflict. |

## Consequences

**Upside**

- Titles are decided by the human's words again; chat-dispatched titles no longer converge on a fixed first sentence.
- The `user` row honours ADR-046 §2.1 again, so every consumer that reads "the first user row" (title, export, `@` reference, search) is correct again.
- Reference text renders as markdown while the human's words stay verbatim — both requirements met without contaminating each other.
- Prompt input for a turn without references is byte-identical (D3), so the prefix cache pays no extra miss.

**Downside / cost**

- One more part type, one more metadata discriminator, one more write per routed turn; the frontend gains a component and five locales of copy (`messageRef.chatBackground` / `messageRef.generic`).
- The `exec_session_create` wire argument is renamed (`message` → `background`); prompt, schema, drift guards and existing test assertions must move together — a one-time prefix-cache miss, expected.
- A turn with a background but no words (the human sent only an attachment) has an empty `user` `content`: the transcript gets a block with no bubble, expressed by the reference card and attachment chips. Acceptable (it is the truth), but the reading has to be understood in the UI.

**Rollback**: `context_refs` is an optional `params` field; absent means "no references", and an older Runtime simply ignores it (unknown params keys take no part in construction); the render side skips any unknown `kind`. So the rollback point is "make `dispatch_parts` glue the background back into `content`"; no other layer changes.

## Implementation map (code is the contract)

| Layer | Location | Contents |
|---|---|---|
| Data | [core/acowork-core/src/message_ref.rs](../../../core/acowork-core/src/message_ref.rs) | `MessageRef { kind, content }`, `REF_KIND_CHAT_BACKGROUND`, `model_header`, `is_empty`; 3 unit tests pin the wire shape / unknown kind / empty body |
| Parse | `intent_turn::context_refs_from_params` | Per-entry tolerance: empty bodies and unknown kinds are dropped, one bad part never costs the turn |
| Render | `intent_turn::render_user_turn` | Words first, blocks after; byte-identical when there are none |
| Persist | [conversation.rs](../../../core/acowork-runtime/src/conversation.rs) `MessagePartMeta` / `ContextRefMeta` / `MESSAGE_PART_TYPE_CONTEXT_REF`; `loop_persist::write_message_refs` | `{"type":"context_ref","kind":…}` + the body in `content` |
| Turn | `loop_.rs` | `turn_message_refs` persisted after the `user` row and before attachments; title input still `raw_user_message` |
| Router | [chat_router/runner.rs](../../../core/acowork-runtime/src/agent/chat_router/runner.rs) `dispatch_parts`, [chat_router/tool.rs](../../../core/acowork-runtime/src/agent/chat_router/tool.rs) schema, [chat_prompt.rs](../../../core/acowork-core/src/chat_prompt.rs) | `background` argument; a new session yields `(words, [ref])`, a continued one `(words, [])`; a background equal to the words is dropped |
| Fallback | [startup/gateway_loop.rs](../../../core/acowork-runtime/src/startup/gateway_loop.rs) | `degraded_background` yields a reference part; `params.message` untouched |
| Frontend | [messageFolder.ts](../../../apps/acowork-desktop/src/components/chat/messageFolder.ts) `user_with_parts`; [MessageRefBlock.tsx](../../../apps/acowork-desktop/src/components/chat/MessageRefBlock.tsx) | Folding + collapsed markdown card; `messageRefPart.test.tsx` pins 5 contracts (fold, literal words, collapsed default, markdown on expand, still rendered with no words) |
