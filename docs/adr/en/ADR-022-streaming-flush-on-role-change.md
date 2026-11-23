# ADR-022: Flush on Streaming Role Change — Making JSONL a Faithful Real-Time Record

> **Chinese source of truth**: [ADR-022](../zh/ADR-022-streaming-flush-on-role-change.md)
> **Terminology**: see [GLOSSARY.md](./GLOSSARY.md)

## Status

Adopted (see §12, amendment 2026-11-23: interleaved reasoning must not fragment the assistant line)

## Date

2026-07-01

## Decision Makers

架构讨论 (architecture discussion)

## Predecessor

ADR-021 (Unified Session Data Loading)

## Blast radius

- `core/acowork-runtime/src/providers/openai.rs` and other provider adapters (normalize raw provider chunks into structured `StreamEvent`)
- `core/acowork-runtime/src/agent/agent_core.rs` (StreamingLine lifecycle and flush semantics)
- `core/acowork-runtime/src/agent/loop_llm.rs` (consume only structured events)
- `core/acowork-runtime/src/agent/loop_tools.rs` (text-boundary flush before tool_call)
- `core/acowork-runtime/src/conversation.rs` (`line + char_offset` streaming read semantics)
- `apps/acowork-desktop/src/stores/chatStore.ts` (render strictly in JSONL line order + streaming placeholder)
- `apps/acowork-desktop/src/components/chat/ChatPanel.tsx` (drop role-parsing responsibility)

---

## 1. Context

ADR-021 unified frontend data loading onto HTTP Pull: the frontend reads the completed
lines from JSONL, reads the single incomplete `StreamingLine` from Runtime memory, and
polls with `(line_number, char_offset)` for new complete lines and for the delta of
the incomplete line. The direction is right — **JSONL is the persisted truth; the
StreamingLine is only a temporary representation of the next JSONL line before it is
complete**.

But it exposed an under-specified assumption in ADR-021:

> The role of a `StreamingLine` never changes during its lifetime.

That is false. A single LLM response naturally spans several semantic segments:
assistant body, reasoning / thought, assistant body again, tool_call, tool_result, and
the next round of assistant body.

If one `StreamingLine` may mix roles and the splitting responsibility is pushed to the
frontend, you get: JSONL lines containing both assistant and thought content; assistant
text displayed in the wrong place around tool_call; frontend patches such as
`parseThinkContent` / `stripThinkTags`; a streaming placeholder type that has to be
synced repeatedly; and JSONL order disagreeing with what the user sees. None of that is a
frontend display bug at the root — it is **an under-defined write boundary**.

## 2. Decision

Adopt **flush on role change**:

> Once the Runtime confirms that the next semantic segment has a role different from the
> current `StreamingLine.role`, it MUST first flush the current non-empty `StreamingLine`
> as a complete JSONL line, and only then open a new `StreamingLine`. Every message line
> in JSONL carries exactly one clear role.

Precisely:

1. **JSONL is the persisted truth** — the frontend renders complete lines strictly in JSONL line order.
2. **A StreamingLine is an incomplete line** — it represents only the next single-role line not yet written to JSONL.
3. **A role never changes in place** — within a `StreamingLine` lifetime the role is fixed; a role change happens only through a flush boundary.
4. **The frontend does not infer roles** — no think-tag parsing, no role correction, no message reordering.
5. **Provider adapters own raw-protocol normalization** — the Runtime main loop consumes only structured `StreamEvent`s and never guesses roles from UI hints or mixed text.

## 3. Key definitions

**JSONL complete line** — a `ConversationEntry` already written to the conversation
JSONL file. Append-only; one role per line; line order is display order; once written, the
frontend never re-splits, re-orders, or re-fixes it.

**StreamingLine** — the incomplete line in Runtime memory:

```rust
pub struct StreamingLine {
    pub line_number: usize,
    pub role: String,
    pub accumulated_content: String,
    pub started_at: String,
}
```

`line_number` is the JSONL line number it will become once flushed; `role` is the only role
allowed on this incomplete line; `accumulated_content` holds only that role content. After
the flush the object MUST be removed from `StreamingStateMap` or replaced with an empty
object for the new role.

**Role segment** — a contiguous, already-normalized piece of model output. The role is
constant within a segment; role transitions happen between segments. Given

```text
assistant: "我先看一下文件。"
thought:   "需要定位配置读取路径。"
assistant: "接下来调用搜索工具。"
tool_call: grep(...)
```

these are four role segments, which MUST produce at least three message JSONL lines
plus one tool_call JSONL line.

## 4. The criterion for a "new role"

This is the core clarification of this ADR.

### 4.1 A new role may only come from a provider-normalized StreamEvent

The Runtime main loop MUST NOT infer a new role from: the current `StreamingLine.role`;
the frontend message type; the last role in the JSONL history; a half-typed tag in the
accumulated text; `finish_reason`; or a hardcoded provider-name branch.

A new role is determined only by a structured event emitted by a provider adapter:

| Provider-normalized event | role / boundary | Runtime action |
|---|---|---|
| `StreamEvent::Content(text)` | `assistant` | if the current streaming role is not `assistant`, flush first, then append the text |
| `StreamEvent::ReasoningContent(text)` | `thought` | if the current streaming role is not `thought`, flush first, then append the text |
| `StreamEvent::ToolCallStart` | non-text boundary | flush the current text line; then write tool_call as a JSONL line |
| `StreamEvent::ToolCallChunk` | tool_call argument delta | not part of the StreamingLine; only accumulate tool_call arguments |
| `StreamEvent::Finished` | end-of-response boundary | flush the current text line; merge usage / finish_reason / tool_calls |
| `StreamEvent::Error` | error boundary | flush the current text line, then return the error |
| user Stop / Pause | control boundary | flush the current text line, then stop or pause |

So a "new role" in the Runtime is not an arbitrary string parameter but a finite set
derived from `StreamEvent` types. It is recommended to tighten the boundary with an
internal enum:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamingRole {
    Assistant,
    Thought,
}

impl StreamingRole {
    fn as_jsonl_role(self) -> &'static str {
        match self {
            StreamingRole::Assistant => "assistant",
            StreamingRole::Thought => "thought",
        }
    }
}
```

Even while the implementation still uses `&str`, the same constraint applies: only
`Content` and `ReasoningContent` may open a text streaming role.

### 4.2 Raw think markers are the provider adapter's responsibility

Some OpenAI-compatible providers do not use `delta.reasoning_content` and instead embed
thinking content inside `delta.content`, e.g.

```text
"我先说明一下<!think>这里需要分析路径willReturn然后调用工具"
```

Such a raw chunk MUST NOT reach the frontend and MUST NOT enter JSONL as mixed-role text.
The provider adapter must first normalize it into a structured event sequence:

```text
Content("我先说明一下")
ReasoningContent("这里需要分析路径")
Content("然后调用工具")
```

The main loop then handles it purely through the `StreamEvent → role` mapping of §4.1.

### 4.3 A half-typed marker must not trigger a role change

The adapter MUST handle cross-chunk markers when parsing raw `delta.content`:

```text
chunk 1: "abc<!thi"
chunk 2: "nk>reasoning willRet"
chunk 3: "urn answer"
```

Rules:

1. Only a fully recognized opening marker may switch from `Content` to `ReasoningContent`.
2. Only a fully recognized closing marker may switch back from `ReasoningContent` to `Content`.
3. The markers themselves are not user-visible content and are not written to JSONL.
4. An undecided partial marker stays in the parser scratch buffer and MUST NOT be forwarded to the main loop early.

This makes a "role change" a structured event boundary rather than a guess made while
scanning strings.

### 4.4 tool_call is not a StreamingLine role

`tool_call` is a complete JSONL line, not a text streaming line. Therefore:

- when `ToolCallStart` arrives, the current assistant/thought text line MUST be flushed first;
- tool_call argument deltas go into the tool_call accumulator;
- once the arguments are complete, a `role="tool_call"` JSONL line is written;
- the tool result is written as a `role="tool_result"` JSONL line.

This guarantees the usual model output order — assistant text, then tool calls — and
stops assistant text from hanging in an in-memory placeholder accumulating across
iterations:

```jsonl
{"role":"assistant","content":"我先搜索相关文件。"}
{"role":"tool_call","content":"..."}
{"role":"tool_result","content":"..."}
```

## 5. Runtime write model

### 5.1 Core invariants

1. `append_streaming_delta(role, delta)` may only append to a `StreamingLine` of the same role.
2. If the current line role differs from the target role, the caller MUST go through the transition helper first.
3. The transition helper is responsible for: writing a non-empty current line to JSONL; discarding an empty current line; creating or keeping the streaming line for the target role.
4. `flush_streaming_line()` is the only path that writes streaming text to JSONL.
5. Any path that calls `conversation.append_message()` directly, bypassing `flush_streaming_line()`, MUST also update `total_lines`, or the HTTP Pull `total_lines` becomes wrong.

```rust
fn transition_streaming_role(
    target: StreamingRole,
    conversation: Option<&ConversationSession>,
) {
    match current_streaming_line() {
        None => create_empty_line(target),
        Some(line) if line.role == target.as_jsonl_role() => {}
        Some(line) if line.accumulated_content.is_empty() => replace_empty_line(target),
        Some(_) => {
            flush_streaming_line(conversation);
            create_empty_line(target);
        }
    }
}
```

### 5.2 Structured event handling

The main loop should look like this:

```rust
match event {
    StreamEvent::Content(text) => {
        transition_streaming_role(StreamingRole::Assistant, conversation);
        append_streaming_delta(StreamingRole::Assistant, &text);
        notify_new_data_available();
    }
    StreamEvent::ReasoningContent(text) => {
        transition_streaming_role(StreamingRole::Thought, conversation);
        append_streaming_delta(StreamingRole::Thought, &text);
        notify_new_data_available();
    }
    StreamEvent::ToolCallStart(tool_call) => {
        flush_streaming_line(conversation);
        begin_tool_call(tool_call);
        notify_new_data_available();
    }
    StreamEvent::Finished(response) => {
        flush_streaming_line(conversation);
        merge_final_response_metadata(response);
    }
    StreamEvent::Error(error) => {
        flush_streaming_line(conversation);
        return Err(error);
    }
    StreamEvent::ToolCallChunk { index, arguments } => {
        accumulate_tool_call_arguments(index, arguments);
    }
}
```

### 5.3 tool_calls carried in the Finished event

Some providers never send the full `ToolCallStart` / `ToolCallChunk` sequence and instead
return complete `tool_calls` in the `Finished` response. The rules are unchanged:

1. `Finished` first flushes the current text streaming line;
2. tool_calls are then merged from the final response;
3. `prepare_tool_calls` writes the tool_call JSONL line;
4. the previous assistant line MUST NOT be rewritten to accommodate tool_calls.

## 6. JSONL and the frontend read model

**JSONL line order is the only display order.** The frontend displays messages from
JSONL ordered by file line number, plus an optional current StreamingLine placeholder at
its future `line_number`. The frontend does not: reorder by timestamp; move tool_call
after assistant; parse think tags inside assistant content; merge thought into assistant;
or modify an already-created JSONL row based on a type change.

**`line + char_offset` semantics.** The frontend sends `line_number` (the number of
complete JSONL lines it has already seen / the latest line position) and
`line_char_offset` (how far into the current streaming line it has read). The Runtime
returns `messages` (new complete JSONL lines after `line_number`), `streaming` (the delta
of the incomplete line after `line_char_offset`), and `total_lines`.

On a role transition:

```text
poll N:
  streaming line=12 role=assistant content="我先"

Runtime:
  Content continues -> assistant line grows
  ReasoningContent arrives -> flush line 12 assistant to JSONL, create line 13 thought

poll N+1:
  messages contains JSONL line 12 assistant
  streaming line=13 role=thought content="需要分析"
```

The frontend then: merges the complete `messages` lines first; removes any streaming
placeholder that now corresponds to a complete JSONL line; then appends or updates the
new streaming placeholder. The visual order remains JSONL line order plus the current
incomplete line.

## 7. Typical scenarios

**assistant + thought + assistant + tool_call** — normalized events
`Content` / `ReasoningContent` / `Content` / `ToolCallStart` / `Finished` produce four
JSONL lines in that role order.

**raw content carrying a think marker** — the provider raw chunk
`"我先看<!think>分析路径willReturn然后搜"` is normalized by the adapter into
`Content("我先看")`, `ReasoningContent("分析路径")`, `Content("然后搜")`, yielding three
JSONL lines in the same order.

**thought only, no assistant** — `ReasoningContent("分析中...")` then `Finished(...)`
yields a single `{"role":"thought","content":"分析中..."}` line. No fake empty assistant
line is needed.

## 8. Migration strategy

**Phase 1 — tighten the Runtime / provider boundary.** Provider adapters normalize raw
think markers into `ReasoningContent` / `Content`; the main loop determines the role only
from `StreamEvent`; `StreamingLine.role` never changes in place and a role change must
flush; `ToolCallStart` / `Finished` / Stop / Error uniformly flush the current text line;
add unit tests for cross-chunk markers, role transitions, and the pre-tool_call text flush.

**Phase 2 — simplify the frontend.** Keep the `(line_number, char_offset)` polling; drop
the think-tag parsing and role correction for runtime output; the frontend only renders in
JSONL line order and manages the streaming placeholder lifecycle. Legacy compatibility for
mixed-role old JSONL is allowed only as a legacy display fallback and MUST NOT affect the
new write path.

**Phase 3 — delete the patch paths.** Remove `stripThinkTags` / `parseThinkContent` and
similar runtime patches; remove `lastStreamingLine`-style state that existed to support
in-place role changes; change `append_streaming_delta` to a typed role or add a debug
assertion so the role can never silently change again.

## 9. Test requirements

1. **provider parser: multiple segments in one chunk** — `a<!think>bwillReturnc` yields `Content(a) -> ReasoningContent(b) -> Content(c)`.
2. **provider parser: cross-chunk marker** — `a<!thi` + `nk>bwillRet` + `urnc` must not leak a partial marker, and JSONL must not contain markers.
3. **Runtime role transition** — `Content(a) -> ReasoningContent(b) -> Content(c)` produces three single-role JSONL lines.
4. **assistant text + tool_call** — `Content("我来查") -> ToolCallStart` MUST write the assistant line before the tool_call line.
5. **tool_calls inside Finished** — even with no `ToolCallStart`, the assistant/thought text MUST be flushed before entering `prepare_tool_calls`.
6. **frontend line order** — after merging `messages + streaming`, the display order MUST match the JSONL line order exactly, with no leftover duplicate bubble after a streaming placeholder flush.

## 10. Risks and mitigations

**Risk 1 — more JSONL lines.** Role boundaries produce more short lines. This is the
necessary cost of correct record semantics: a typical response adds only 1–3 lines and
append-only JSONL writes are cheap.

**Risk 2 — provider marker formats change.** Different providers may use different
thinking markers. The variation is confined to the provider adapter; the Runtime and the
frontend depend only on the structured `StreamEvent` and never see provider-private formats.

**Risk 3 — old JSONL files contain mixed-role lines.** Legacy files may already embed
think markers inside assistant lines. Compatibility is allowed only as a legacy display
fallback; the new Runtime write path MUST guarantee one role per line, and legacy data
MUST NOT be a reason to keep polluting the new architecture.

**Risk 4 — an empty StreamingLine creates an empty placeholder.** The transition helper may
create an empty line. When the HTTP response carries an empty streaming delta the frontend
MUST NOT create a visible placeholder, and flushing an empty line MUST NOT write to JSONL.

## 11. Conclusion

The essence of ADR-022 is not "one more think-parsing rule" but a boundary definition:

- **provider adapter** normalizes the raw provider protocol into structured events;
- **Runtime** maintains a single-role StreamingLine from those events, flushing on role change;
- **JSONL** stores complete single-role lines whose line order is the display order;
- **Frontend** renders in JSONL order, with streaming only an incomplete-line placeholder.

This pulls the current thought / tool / assistant misplacement problem out of the
"frontend patch chain" and back into a verifiable, testable, maintainable persistence model.

---

## 12. Amendment (2026-11-23): interleaved reasoning must not fragment the assistant line

**Status**: adopted
**Incident**: `com.acowork.ponytail` instance `8f7be9a6`, round `r:msg-d828f763` (MiniMax-M3.1-Flash, reproduced twice on 2026-11-23, 11:13 and 11:16).

### 12.1 Facts

The provider normalization layer can deliver a `ReasoningContent` delta **in the middle of an already-open assistant message** (interleaved `reasoning_content` over the OpenAI-compatible protocol). Read literally, §4 treated every such delta as a role change: the assistant line was finalized and flushed, and the next Content delta opened a new line. One sentence was cut mid-word (`"写"` | thought `" delete."` | `"盘链路我自己探一下，不再猜："` — the compound "写盘" split in half), and the frontend, faithfully following §6, rendered each record as its own bubble. **The fragmentation happens on the write side, not in display.**

The test: §4 assumes a role change marks a new semantic segment. An interleaved reasoning delta is not a new assistant segment — it is the previous reasoning stream resuming at an arbitrary byte offset of the text stream (`" delete."` is the continuation of the preceding thought "…yet be").

### 12.2 Decision

1. **Ownership where the lifetime is**: an interleaved thought segment's real lifetime is exactly the streaming line it was interleaved into — born with it, dies with it. It is therefore a field on `StreamingLine` (`interleaved: Option<ThoughtSegment>`, data model in conversation.rs), not a sibling field on `SessionCore`. "Drain / discard" thus becomes structural rather than a convention every call site must remember.
2. **Mapping owned by the persistence layer**: `SessionCore::on_reasoning_delta` is the single place that decides what a reasoning delta means — an open assistant line appends to its `interleaved` segment (deltas merge, keeping first startTime / last endTime); otherwise the existing thought-line path runs. The stream loop no longer decides roles and has no `continue` escape.
3. **Single drain exit**: `flush_streaming_line`, after writing the line's own record, atomically persists the line's segment as ONE `thought` record, AFTER the assistant record. Rationale: the assistant line's `line_number` was assigned at creation as the committed count then, and the §6 poll-cursor protocol assumes no committed line ever appears at or below an open streaming line's number; the line is removed from the map first, so appending the thought cannot renumber an open line (the partial-cut bug family). Every boundary (Finished / Error / Stop / Pause / `prepare_tool_calls` / `finalize_streaming_line`) just calls bare `flush_streaming_line` — the line-flush / segment-drain pairing no longer relies on memory.
4. **Single discard exit**: `remove_streaming_line` drops the segment together with the line — exactly the Path 2 contract (`persist_think_to_conversation` writes the whole accumulated reasoning, interleaved segments included, from the response). A safe default, with no "deliberately do not drain" exception to remember.
5. **Count semantics**: writing a thought record IS a streaming flush and must bump `streaming_flush_count`. Otherwise a whitespace-only assistant line carrying interleaved reasoning would leave the count at 0 → finalize wrongly takes Path 2 → persist_think writes the same reasoning a second time.
6. **A third operation**: stop paths where the caller persists the assistant text itself (`handle_stopped`) use `drop_line_persisting_thought` (persist the thought, drop the line text) — distinct from flush (writes the line text too) and remove (drops the segment too).
7. **Invariants unchanged**: every JSONL line still carries exactly one role; a StreamingLine still never changes role in place. The amendment only redefines what counts as a role transition: interleaved reasoning does not.

### 12.3 Canonical shape and impact

An interleaved round now persists as: `thought(leading) → assistant(merged full text) → thought(interleaved) → tool_call…`. The restorer's handling of the trailing thought (folded into the tool-call round's `reasoning_content`, dropped for text-only rounds) matches the shape the pre-amendment fragmented writes already produced — only the text is now whole. Live UI: during an interleaved pause the assistant bubble simply stalls briefly (no new deltas) instead of splitting; the thought record surfaces via polling at the boundary.

Implementation: `conversation::{StreamingLine.interleaved, ThoughtSegment}`; `SessionCore::{on_reasoning_delta, flush_streaming_line (atomic drain), remove_streaming_line (drops segment), drop_line_persisting_thought, persist_interleaved_segment}`. The independent buffer field and the four patch methods `buffer_interleaved_thought / flush_interleaved_thought / reset_interleaved_thought / flush_line_at_boundary` are deleted — drain / discard of the segment is guaranteed structurally by the line's two exits, so there is no "forgot to drain" state.

§9 test requirements add: `test_interleaved_reasoning_merges_assistant_reply`, `test_interleaved_thought_merges_segments_and_skips_whitespace`, `test_interleaved_segment_dies_with_line_on_path2_remove` (segment dies with the line, no cross-call leak), `test_whitespace_line_with_interleaved_thought_counts_as_flush` (count semantics, prevents Path 2 duplication) (session_core), `restore_folds_interleaved_thought_after_merged_assistant` (restorer, disk shape unchanged).
