# ADR-097: In-Conversation History Search — Per-Session Disposable Search Index

- **Number**: ADR-097
- **Status**: Accepted
- **Date**: 2026-10-20
- **Deciders**: ACowork Architecture Group
- **Related**: ADR-096 (no full session-file reads on request paths — this ADR is the "content retrieval became a need" trigger it deferred to), ADR-092 (user-chat bounded reads and per-session single writer), ADR-082 (SQLite FTS storage and the trigram convention); design doc 27 §4.6

## Summary

The WeChat-style "browse this conversation by 文本 / 图片 / 文件" needs a queryable index. ADR-096 deferred "migrating chat to SQLite" until *content retrieval became a need* — this feature is that trigger. But the retrieval need must not in turn break the ADR-096 iron rule: search MUST NOT scan `messages.jsonl` on the request path.

This ADR establishes a **per-session, disposable, non-load-bearing** search-index model:

1. **The search index is decoupled from the paging spine.** A new file `{data_dir}/chat-sessions/{sid}/search.sqlite`; `lines.index` stays untouched and remains the load-bearing pager; `messages.jsonl` stays the single source of truth.
2. **Maintained incrementally at write time, read-only at query time.** `append_message` updates `search.sqlite` inside the existing per-session `session_lock`; `search_messages` reads only that sidecar and never opens `messages.jsonl`.
3. **Disposable and self-healing.** A corrupt/missing/lying sidecar degrades search alone — the worst case is a one-time rebuild, never an impact on chat reads or writes. That rebuild is the only sanctioned full read — the same first-touch exemption `ensure_index` uses, never per request and never per write.

## Background

- **Dependency status**: `acowork-user` previously had **zero** SQLite dependency ([chat_sessions.rs](../../../core/acowork-user/src/chat_sessions.rs) was fs/JSONL/`lines.index` only). This feature adds `rusqlite` (workspace-level 0.37, bundled, FTS5 + `trigram` tokenizer), same source and version as `acowork-sqlite`.
- **Cannot scan the log**: routing search through a full `messages.jsonl` read would directly violate the ADR-096 red line (`run_session_full_read_redline`). A separate, write-maintained query surface is therefore required.
- **Why not merge into `lines.index`**: `lines.index` is the load-bearing paging spine (zero-dependency, used by the hot read path every request). Folding disposable full-text search into the same file upgrades "search is broken" to "chat is broken" — the two differ in corruption semantics, rebuild cost, and lock domain, so they must stay separate.

## Decisions

### D1 — One disposable `search.sqlite` per session, not a single global DB

The index is scoped to the **session directory**: `chat-sessions/{sid}/search.sqlite`. A single global index DB for all of acowork-user was rejected:

- ADR-092's invariant is a **per-session single writer, not a global serial point**; one global DB would serialize every session's write onto one file lock, conflicting with that invariant.
- The session scope already *is* the query scope (this feature searches within a single conversation), so a per-session file aligns naturally with the query boundary.
- Delete lifecycle is consistent: `delete_session` removes the session dir including `search.sqlite` (directory-level cleanup), with no cross-DB refcounting. **The order is part of the contract**: release the cached handle (`search_index::forget`) before `remove_dir_all` — on Windows, SQLite files are opened without `FILE_SHARE_DELETE`, so an open handle makes the directory removal fail with a sharing violation and turns a “disposable sidecar” into “an undeletable session”.

### D2 — Schema and coordinate

- A single FTS5 table `search` (embedded content, `tokenize = 'trigram'`): primary column `body` (full-text); the rest — `seq / ts / from_id / kind / cat / attachments` — are all `UNINDEXED`; `rowid = line_pos`.
- `line_pos` is the same 0-based ascending physical-line coordinate `lines.index` assigns (blank lines occupy bytes but no position; a corrupt line occupies a position but yields no row), so the frontend converts a hit into a `read_messages` offset consistently across both indexes.
- `cat` is a bit mask: bit0 text (speech with a non-empty body), bit1 image (attachment mime `image/*`), bit2 file (other attachments); a row may carry several. The attachment JSON is stored in the `attachments` column so the file category can match on filename and the frontend can render thumbnails / file chips.
- `meta(indexed_lines)` holds a watermark: the number of physically indexed lines, used as the consistency check.

### D3 — Write path: best-effort incremental, a failure never rolls back the message

After `commit_row`, `append_message` calls `index_append(dir, line_pos, msg)`: when the watermark equals exactly `line_pos` it opens a transaction to `INSERT` and bump the watermark; any disagreement falls back to a one-time `rebuild`. **An index write failure only logs a warn and never bubbles up with `?`** — the log is the source of truth, so a durably appended line is a successful write regardless; a lagging sidecar heals on the next query/write first-touch rebuild. Search stays a disposable accessory to chat, never load-bearing.

### D4 — Rebuild is the only sanctioned full read, triggered by the watermark

`ensure(dir, log_line_count)`: after opening the handle, if `meta.indexed_lines ≠ the authoritative line count` (fresh / lying / crash window / copied or absent file) it rebuilds the whole sidecar from `messages.jsonl` once and bumps the watermark. This is the same-tier first-touch exemption as [ADR-096 D4](./ADR-096-no-full-session-reads.md) — at most once per session per process, never per request and never per write. On an open failure (corrupt) it deletes the sidecar plus `-journal` and retries once from a clean rebuild.

### D5 — Query: three categories share one cursor-paged read of the sidecar

`search_messages(data_dir, sid, category, q, before_line, limit)` → take `session_lock` → `ensure_index` for the authoritative total → `search_index::ensure(total)` → `query`:

- **Text**: when `q` has ≥ `TRIGRAM_MIN` (=3, matching `acowork-sqlite`) chars, an FTS `MATCH` (phrase); short / degenerate queries fall back to `body LIKE`; an empty `q` lists all `cat & text`.
- **Image**: `cat & image`; `q` is ignored.
- **File**: `cat & file`, `q` matches a filename substring in `attachments`.
- All ordered `rowid DESC`, with `before_line` as the toward-older cursor (`rowid < before_line`). Returns `hits / total_lines / next_before_line` (a full page yields the last row's `line_pos` as the next cursor, otherwise `None`).

### D6 — HTTP and permission: the same participation guard as reading messages

`GET /api/chat-sessions/{sid}/search`, params `category(text|image|file, defaults to text)`, `q`, `limit(defaults to DEFAULT_PAGE, capped at MAX_PAGE)`, `before_line`. The permission mirrors `read_messages_guarded`: a non-participant and an unknown session both surface as `not_a_participant` / not-found, mapped by offload to a hidden 404; an admin may search a project group they never joined. The Gateway's `user_proxy` reverse-proxies `/api/chat-sessions/{*rest}` with a wildcard, so **a new sub-path needs no Gateway change**.

### D7 — Enforcement: a search write failure must not affect messages, a query must not read the log

Fixed by unit tests rather than convention: ① append→search round-trip consistency; ② image/file mime correctly separated; ③ FTS match + short-word `LIKE` fallback; ④ `before_line` cursor continuation; ⑤ delete/corrupt `search.sqlite` → first-touch rebuild → results equal a per-line scan; ⑥ **a search write failure does not affect message persistence**; ⑦ non-participant rejected. `search_messages` never reads `messages.jsonl` (except the D4 rebuild), so it never trips the ADR-096 red line.

## Consequences

**Positive**

- In-conversation queries across the three categories work, and a clicked hit locates the message via `line_pos` → offset (the frontend jump is the §6 window-cursor rework in design doc 27).
- Search is a fully disposable side path: broken, missing, or lagging degrades search only, with zero impact on chat read/write; the hot read path still never touches `messages.jsonl`.
- Orthogonal to ADR-096: this feature does not merge into `lines.index`, does not change the paging spine, does not touch the wire contract; paging semantics stay byte-for-byte identical.

**Negative**

- `acowork-user` gains a `rusqlite` dependency and one derived-state store (`search.sqlite` + the `meta` watermark), whose correctness is held by watermark reconcile and the per-session lock.
- Crash window: a line durably appended but whose index write failed heals on the next `ensure`/`index_append`-triggered rebuild, not on that same write.
- This feature is single-conversation keyword/category retrieval only; semantic/pinyin retrieval and cross-conversation (Inbox user↔user) search are deferred (to `acowork-embed` and Phase 5 respectively).

## Alternatives Considered

1. **One global index DB for all of acowork-user** — rejected: violates ADR-092's per-session single writer (a global file lock serializes all session writes) and misaligns delete/scope (D1).
2. **Merge search into `lines.index`** — rejected: makes disposable search share life-or-death with the load-bearing pager, upgrading "search broken" to "chat broken"; the two differ in corruption semantics and lock domain.
3. **Scan `messages.jsonl` at query time** — rejected: directly violates the ADR-096 red line.
4. **Migrate `messages.jsonl` wholesale into a SQLite primary store** — rejected: ADR-096 alternative 2 already deferred that migration until "multi-process write becomes a need"; this is a read-only retrieval need met by a disposable derived index, not worth moving the source of truth or the paging spine.

## References

- ADR-096 (no full session-file reads — this ADR cashes in its reservation that "a small SQLite side table may be introduced when content retrieval becomes a need", reusing its first-touch rebuild exemption)
- ADR-092 (user-chat bounded reads, per-session single writer — the search write runs inside the existing `session_lock`, no new global lock)
- ADR-082 (SQLite vector + FTS storage, the `TRIGRAM_MIN` convention — this index reuses the same trigram token floor)
- Design doc 27 §4.6 (chat session storage and read discipline)
