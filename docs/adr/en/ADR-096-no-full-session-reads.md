# ADR-096: No Full Session-File Reads on Request Paths — Write-Time Derived State and Bounded Reads

- **Number**: ADR-096
- **Status**: Accepted
- **Date**: 2026-10-20
- **Deciders**: ACowork Architecture Group
- **Related**: ADR-092 (user-chat bounded reads; this ADR generalizes its discipline), ADR-024 (conversation meta / token usage), ADR-082 (SQLite vector + FTS storage), ADR-084 (user standalone process), ADR-076 (chat doorbell); design doc 27 §4.6

## Summary

Chat history and execution-session history grow without bound. The review that produced ADR-092 measured ~69 ms for a whole-file `read_messages` at 100k lines; the same read-amplification class existed on the *other* session surface too — the agent execution conversation (`acowork-runtime`) and the request paths that listed sessions, counted rows, resolved idempotency ids, and fetched the latest compaction summary.

This ADR states an implementation-wide iron rule and the derived-state model that satisfies it:

1. **No request path and no tool read may take a full read of a session file** — neither a whole-file `read_to_string` nor a per-line full scan/parse done only to derive a row count, a sequence number, an idempotency id, a tail preview, or a compaction summary. This covers both `messages.jsonl` (user chat) and `{sid}.jsonl` (execution conversation).
2. **Every statistic is served from write-time derived state** — session meta (`message_count`, `last_message_preview`, `last_compaction_summary`), an in-memory log index (spans with byte offsets and a data-row counter), or a small SQLite side table (`conversation_state`).
3. **A full pass is legitimate only as a once-per-process first-touch reconcile / open-calibration / migration / startup-backfill** — low frequency, not on the real-time path, never per request and never per append.

The wire contract is unchanged: `total`, `offset`, paging and preview semantics stay byte-for-byte identical, so the frontend is not touched.

## Context

### 1. The two session surfaces

- **User chat** (`core/acowork-user/src/chat_sessions.rs`): `chat-sessions/{id}/messages.jsonl` + `session.json`. Served to Desktop/Mobile over HTTP through the Gateway; the empty-payload doorbell (ADR-076 §Decision 8) turns every event into a read.
- **Execution conversation** (`core/acowork-runtime/src/conversation.rs`): `conversations/{sid}.jsonl` + a SQLite `SessionMeta`. Read on user pagination, on incremental poll (`read_messages_since*`), by the handoff path (latest compaction summary), and indexed by a background tailer (`conversation_index.rs`).

ADR-092 already fixed the user-chat *read* hot path (seq identity + reverse tail-chunk seek + `message_count`). What remained across both surfaces were the *auxiliary* full reads that had nothing to do with serving a page: counting, dedup lookup, preview projection, list fan-out, watermark recovery.

### 2. The residual full-read sites (before this change)

| Surface | Site | Why it scanned | Cost class |
|---|---|---|---|
| user chat | `append_message` ×2 per write | recount rows + search `message_id` for idempotency | O(history) per append |
| user chat | session list (`chat_api`, `attach_group_previews`, `list_project_groups`) | read the last message to build a preview | O(sessions × history) per list call |
| user chat | `read_messages` | `total` derived by counting lines; window parsed after a whole-file read | O(history) per page |
| exec conv | `read_messages_paginated` forward offsets | parse every line to skip to an offset | O(history) per page |
| exec conv | `read_messages_since*` | `enumerate()` from line 0 to skip to the cursor | O(history) per poll |
| exec conv | latest compaction summary (T2 handoff) | forward scan of the whole file | O(history) per handoff |
| exec conv | index `sweep` (every 2 s) + `index_session` | `read_to_string` of the whole log to find appended lines | O(history) per tick, all sessions |
| exec conv | `recover_watermarks` on open | `MAX(message_index)` full-table scan + `NOT IN` dedup pass | O(rows) per open |

None of these is a *content* requirement. All of them are *statistics* that a writer already knows at the moment it appends.

## Decision

### D1 — `total` / row counts come from derived state, never from counting

- User chat: `read_messages` returns `total` from `session.json`'s `message_count`, maintained incrementally by the write path.
- Execution conversation: `read_messages_paginated` takes `total` from the writer's `committed_lines` for an active session, or `SessionMeta.message_count` for a cold one. `message_count` is calibrated by a single open-time line-count pass (D4), then kept by increments.

### D2 — Window reads are reverse tail-chunk seeks

Both surfaces read a page by seeking backwards from EOF in chunks (64 KiB start, grow on demand to cover over-long lines) and deserializing only the lines that fall inside the window. A forward offset `[K, K+limit)` is converted to tail-relative (`skip total−K−limit` lines) and served by the same reverse reader. Corrupt-line skipping and participant normalization are preserved; the leftover legacy header line is stripped once at calibration, not re-tested per line.

### D3 — Incremental poll uses a byte-offset cursor, not line skipping

The writer records a line→byte-offset index in memory (kept in sync with `committed_lines`). `read_messages_since` / `read_messages_since_cursor` convert the cursor row to a byte offset, seek, and read only the appended bytes. The `enumerate()`-from-0 skip loop and the `count_jsonl_lines` fallback are removed.

### D4 — First-touch reconcile / open-calibration is the only permitted full pass

- User chat: a per-process "reconciled" set (keyed by session path, guarded per session). Before a session's *first* write in a process, one full pass calibrates `message_count` (covering the crash window where a line landed but the meta save failed), rebuilds the idempotency id set, and back-fills a missing `last_message_preview` (this doubles as the migration for old data). Subsequent appends are pure increments and O(1) id lookups.
- Execution conversation: opening a session does one byte scan counting only `b'\n'` (no deserialization) to calibrate `message_count`; after that everything is incremental. At most once per process per session.

### D5 — Idempotency ids are an indexed set, not a scan

User chat keeps a small per-session sidecar `ids.index` (`message_id\tseq` per line) plus an in-memory `HashSet` populated at first-touch. The write path checks memory (O(1)); the sidecar is rebuilt once at first-touch and compacted past a size ceiling (old ids have no retry semantics beyond the window).

### D6 — List surfaces project stored fields, touching no log

`last_message_preview` becomes a write-time persisted field of `session.json` (its doc is re-scoped from "read-path projection" to "write-time derived state"). The session list, group-preview attachment, and project-group listing return this stored value directly; `list_project_groups` walks the existing `chat-sessions.projects.json` index instead of scanning session directories. A full `scan_all_sessions` survives only on the startup / backfill path.

### D7 — Compaction summary lives in `SessionMeta`

`append_compaction_event` stores the summary text in `SessionMeta.last_compaction_summary` alongside the existing offset. The handoff read is O(1). For a session whose meta predates the field, the first read does one legacy scan and self-heals the value back into meta; a session with no meta row returns `None` rather than scanning per request (a live session always has meta).

### D8 — The conversation indexer keeps a `conversation_state` tail pointer

- New SQLite table `conversation_state(session_id, next_line, byte_offset)`, updated in the same transaction as the indexed write.
- `sweep` compares `fs::metadata().len()` against the persisted `byte_offset`: an untouched log is skipped with zero content reads; a grown log is read only from `byte_offset` forward. `index_session` no longer does a whole-file `read_to_string`.
- `recover_watermarks` on open reads `conversation_state` (O(#sessions)) instead of a full-table `MAX`/`NOT IN` scan. The junk/dedup cleanup moves to a low-frequency background maintenance pass (the write path is already an idempotent upsert, so correctness does not depend on it). Upgrading an old library (indexed rows but no state row) triggers a one-time rebuild per session as a background migration.

### D9 — Write-path cost stays O(1); reconcile is not per-append

The writer commits an appended line by pushing its known `(offset, len)` span directly into the log index — O(1), no re-open, no rescan. It falls back to a full reconcile only on an offset desync or an external truncation (rare). A per-append re-open-and-rescan was explicitly rejected: it slowed the async writer enough to race the raw-file test reads and re-introduced an O(history) cost on the write hot path.

### D10 — Enforcement: a red-line guard, not a convention

`dev/ci.sh` gains `run_session_full_read_redline`. It scans the two log-owner files and fails on a `read_to_string` / `count_jsonl_lines` in any function body that is not a whitelisted first-touch / calibrate / migrate / scan / sidecar / meta loader, skipping the test module. The ceiling only tightens as call sites are removed.

## Consequences

**Positive**

- Read and write cost no longer grows with history length: page reads are O(page + window bytes), increments/polls are O(appended bytes), counts/previews/summaries are O(1), list fan-out reads no logs, indexer sweep is O(appended bytes) and open is O(#sessions).
- The "no full session read" property is enforced by a CI red line and covered by unit tests (page forward/tail equivalence over 10k synthetic lines, byte-cursor since reads, compaction meta round-trip, `conversation_state` reopen recovery), so it cannot silently regress.
- The frontend and the wire contract are untouched.

**Negative**

- The write path now maintains several derived-state stores (session meta fields, the in-memory log index, `ids.index`, `conversation_state`); each is new state whose correctness is pinned by a first-touch reconcile / open calibration and per-session locks.
- Crash-window consistency shifts from "calibrate on every write" to "calibrate once per process at first touch"; a crash between a line landing and its meta save is healed at the next process's first touch, not the next append.
- The SQLite schema gains `last_compaction_summary` on `sessions` and the `conversation_state` table (migration 5).

## Alternatives Considered

1. **Keep the per-append full recalibration** (pre-change) — rejected: two O(history) passes per write is exactly the amplification the rule forbids, and it scaled with the largest threads.
2. **Migrate chat to SQLite now** — rejected: ADR-092 Decision 7 defers this until content search or multi-process writes require it; the derived-state model gives O(1) statistics on flat files without that cost.
3. **Per-request lazy count with a cached total** — rejected: any cache keyed to "count when needed" re-opens the O(history) path on the first miss per process; the count belongs on the write path where it is free.
4. **Reconcile the log index by re-opening and rescanning the file on each append** — rejected as D9 (write-hot-path regression + reader race); the direct span push is O(1) and equally correct under the committed-lines invariant.

## References

- ADR-092 (user-message identity and bounded reads — this ADR generalizes its read discipline to every auxiliary session read on both surfaces)
- ADR-024 (conversation meta + token usage; the `SessionMeta` derived fields live here)
- ADR-082 (SQLite vector + FTS store; hosts `conversation_state`)
- ADR-084 (user standalone process), ADR-076 (chat doorbell — why list/read paths are high-frequency)
- Design doc 27 §4.6 (serialized chat session writes — now also the write-time preview contract)
