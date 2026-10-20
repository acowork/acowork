# ADR-092: Stable Identity and Bounded Reads for User Messages — Restructuring the user chat Storage and Refresh Model

- **Number**: ADR-092
- **Status**: Accepted
- **Date**: 2026-10-12
- **Revision**: v2 (post-review fixes) — Decision 3's crash self-healing changed from "read-path renumbering" to an "append-time tail check" (F1); Decision 5 gains a per-user index lock (F2); Decisions 1/3/4 pin the physical-line-number basis and the counter initialization point (F3/F4); Background §1 layout sketch corrected (F5); Decision 3 lock scope narrowed, Decision 4 implementation notes added, Decision 5 rebuild cost noted (F6/F7/F8)
- **Deciders**: ACowork Architecture Group
- **Related**: ADR-076 (revises its Decision 8 §v2, items 3 and 4), ADR-084 (user standalone process), ADR-050 (chat-list data-driven refactor), ADR-082 (SQLite precedent); design doc 25 §8

## Summary

The three current defects of the user↔user chat subsystem (refresh misjudgment, lost concurrent updates, read amplification) share one root cause: **messages have no stable identity and reads have no bounded path**. Clients are forced to *guess what happened* from count deltas, and the server is forced to *answer any page* with a full-file scan. This ADR decides to add a server-assigned, per-thread monotonic sequence number (`seq`) to `ChatMessage`; to change the client refresh rule from the total-delta heuristic to a **seq suffix diff** (content-based, not count-guessing); to change the server's hot-path read to a **reverse tail-chunk seek**; to add a derived counter `message_count` to `conversation.json` guarded by a **per-thread write lock** covering every read-modify-write, with crash-window counter drift self-healed by an **append-time tail check** (v2 revision); and to fold push/poll edge handling into a single state machine. The doorbell's privacy contract (empty payload) and the "HTTP is the sole source of truth" contract are unchanged.

## Context

### 1. Current state (code facts)

Storage layout (`core/acowork-user/src/chat.rs`; v2 correction — the original sketch `pair_{min_id}__{max_id}/` was wrong; the actual layout is):

```
data_dir/
  users/{lo}/chats/{hi}/          # lo/hi = byte-sorted pair of the two user_ids (pair_dir); chat_id on the wire is `{lo}__{hi}`
    conversation.json   # participants, last_active_at, unread(BTreeMap), version
    messages.jsonl      # append-only, one message per line
    files/{id} + files/{id}.json  # attachment blobs and metadata (ADR-076 §Decision 9)
```

`ChatMessage` fields are `{ts, from, kind, body, attachments}` — **no id, no seq**. `Conversation` carries no message count. `read_messages` does `read_to_string` on the whole file, parses every line, then slices; `list_for` scans the entire fleet's pair directories on every call; `append_message` performs an unlocked load→modify→save on `conversation.json`.

### 2. Trigger: the doorbell pushed O(all-history) code onto the hot path

After ADR-076 Decision 8 §v2 introduced the empty-payload doorbell, message reads went from "5 s fallback polling" to "once per event". The code review that motivated this ADR confirmed two root causes:

- **Root cause ① (client, structural)**: the invariant "on the up edge ⇒ exactly one active re-read" (defined in v2 item 3 as the *only* correction point) is hand-written in three places: Desktop `chatStore`, Desktop `userChatStore`, Mobile `presence`. Comments declare the contract, but comments are not an enforcement mechanism — Mobile has already drifted (`reportChat(true)` lacks the catch-up wake, violating the ADR's own contract).
- **Root cause ② (data model, missing identity)**: with no stable message identity, clients can only guess via `d = total − knownTotal`. Every guess is a bug family: unseen counts inflate by a whole page length on the old-server path (no `total`), `ackedThrough` keyed by `ts` collides for same-second read receipts, the send echo and the wake re-read double-count, and `sameMessages` reference-equality is always false, triggering needless resyncs.

### 3. Measured numbers

100,000 messages / 12 MB `messages.jsonl` (Windows, release profile):

| Operation | Time |
|-----------|------|
| `read_messages` today (whole-file read + full parse) | ~69 ms |
| Line counting only (find newlines, no parse) | ~10.8 ms |
| Reverse tail-chunk seek over 64 KiB (Decision 4 below) | ~1.4 ms |

The JSONL append in `append_message` is itself O(1) (`O_APPEND` single write); the bottleneck is entirely on the read and metadata side.

### 4. Why the agent-session "chunking mechanism" does not save user chat

Chunked loading has three orthogonal layers: L1 client protocol chunking (fetch the tail by default, fetch another block when scrolling up), L2 server-side physical read chunking (each request touches only the bytes of that page), L3 the file itself being bounded. WeChat's chunking works because of L2 (per-conversation index seek by (thread, seq), independent of history length). Our agent sessions actually **lack L2** — `read_messages_paginated` (`core/acowork-runtime/src/conversation.rs`) also reads the whole file and slices — they stay healthy because L3 holds the line: compaction + `.jsonl.archive` + `prune_excess_sessions` bound the file, and reads happen only on user pagination.

User chat has neither precondition: the product promises "digging up old history", so history cannot be deleted (no L3); the doorbell makes every event trigger a read (high frequency). So the fix is to add L2, not to copy the surface of "agent sessions use this mechanism too".

## Decision

### Decision 1: `seq` — a server-assigned per-thread monotonic sequence number

- Add `seq: u64` to `ChatMessage` (one place on the wire, one in persistence). Assigned by `append_message` inside the **per-thread write lock** (Decision 3): `seq = message_count + 1` (`message_count` is first calibrated by the append-time tail check, see Decision 3 v2), starting at 1, strictly increasing.
- Legacy lines have no seq: `#[serde(default)]` reads them as 0, and `read_messages` backfills with the 1-based line number. The backfill is valid because legacy lines are appended in chronological order with non-decreasing `ts` — the line number is a legitimate seq. No rewrite migration.
- **The line-number basis is pinned to the PHYSICAL line number (v2)**: `read_messages` skips blank and corrupt lines today; backfill, `message_count`, and Decision 4's offset/line counting must all count **physical lines** (a corrupt line occupies its number), not parsed-message indexes — otherwise appended seqs and backfilled seqs drift apart whenever a corrupt line exists. Seq need not be dense: a corrupt line leaves a permanent hole, which is legitimate as identity. Side effect: Decision 2's gap check (`page[0].seq ≤ heldMaxSeq+1`) fires one safe resync at a corrupt-line boundary — pinned by a contract test.
- Wire surface: `GET …/messages` items carry `seq`; the `POST …/messages` echo response carries the assigned `seq`. `total` is kept but **demoted to an optional optimization** (display, "can I page up further"), no longer participating in correctness decisions.

### Decision 2: Client refresh rule = seq suffix diff (revises ADR-076 Decision 8 §v2 item 4)

`planPage(held, page)` becomes:

```
heldMaxSeq = max(seq in held)   // empty window = 0
append     = page.filter(m ⇒ m.seq > heldMaxSeq)

append empty                          → none (content-level: "re-parsed but unchanged" no longer misfires a resync)
no gap (held empty, or page[0].seq ≤ heldMaxSeq+1) → append; unseen is exactly append.length
gap                                   → resync; unseen = count of page items with seq > ackedSeq (still exact)
```

Accompanying rules:

- `ackedThrough: Map<peer, seq>` replaces the ts-keyed version — same-second receipt collisions vanish by construction. `markThreadRead` acknowledges up to `heldMaxSeq`.
- `send()` appends the server echo directly (it carries the assigned seq); the wake triggered by the sender's own doorbell then sees `seq ≤ heldMaxSeq` → none, **naturally idempotent**; the `messagesTotal` bookkeeping is deleted.
- Old-server degradation: when responses lack `seq` (Mobile may connect to an un-upgraded Gateway), fall back to the v2-item-4 total-delta rule, marked deprecated; Desktop ships with the Gateway and is unaffected. **Note: on the degraded path the count-guessing bug family persists unchanged — this is an explicit continuation of legacy behavior, not a fix**; identity cannot be conjured from nothing, and full elimination presupposes the server-side upgrade (Decision 1).
- The "no viewport displacement" discipline carries over unchanged: the page is always "latest N in ascending order", appends touch only the tail, references of rendered bubbles stay stable.

### Decision 3: `message_count` derived counter + per-thread write lock + append-time tail-check self-healing (v2 revision)

- Add `message_count: u64` to `Conversation`, on the **physical line count** basis (consistent with Decision 1). `read_messages` returns `total` straight from it in O(1), no line counting.
- **Initialization covers EVERY load path (v2 fix)**: `read_messages`, `append_message`'s load, and the `Conversation::new` rebuild branch after a failed load — whenever any path encounters `message_count = 0` with a non-empty `messages.jsonl`, it initializes once by counting physical lines (one O(N) per thread, O(1) thereafter). Missing the append path would hand seq=1 to the first post-upgrade message, colliding with legacy line 1.
- **Per-thread write lock**: a mutex registry keyed by pair directory. Every read-modify-write on `conversation.json` — `append_message`, `mark_read` — must complete under the same lock. The lost-update of unread counts found in review is fixed by the lock, not by write-ordering tricks. (v2 scoping: attachment sidecar writes do not modify `conversation.json` and are NOT under this lock; their visibility to append is provided by `resolve_attachments` reading inside the lock — an id whose upload has not finished simply fails to resolve, and the client retries.)
- **Crash-window self-healing = append-time tail check (v2 revision; replaces the former "read-path renumbering" scheme)**: JSONL append succeeds, meta save fails (`save` is an atomic tmp+rename write, so meta only ever reverts to the old value, never tears) → `message_count` lags the physical line count. The former scheme — "the read path detects a 0 or non-monotonic seq and renumbers by line number" — was proven untenable in review, for two reasons: ① anomaly detection on a tail window is **window-local** — once the window slides past the duplicate line, the shifted on-disk seqs are served looking monotonic and collide with the renumbered seqs already handed out: the same seq points to different messages across two reads, and `planPage`'s "append empty ⇒ none" silently swallows the new message; ② renumbering needs absolute line numbers, and absolute line numbers depend on the very `message_count` the crash broke — the repair path circularly depends on its repair target. The fix moves self-healing to the **write path**:
  - Inside the lock, before assigning seq, `append_message` **reverse tail-reads the last parsable line** (starting at 4 KiB, sub-millisecond) using Decision 4's mechanism: if `last_seq ≥ message_count`, set `message_count = last_seq`; then assign `seq = message_count + 1`.
  - Effect: **a duplicate seq never lands on disk**; the on-disk seq always equals the physical line number; counter drift from any crash is caught up on the next append. Legacy seq=0 lines yield `last_seq = 0`, which never triggers repair — naturally compatible with Decision 1's backfill.
  - The same tail read also verifies **that the file ends with a newline**: if a previous crash left a torn line, pad a `\n` before appending — otherwise the new message concatenates into the garbage line, losing two lines at once (a pre-existing defect in current code, hardened for free on the same tail read).
  - The read path's role narrows accordingly: it only backfills legacy seq=0 lines by physical line number (Decision 1); "non-monotonic" detection is kept as a **corruption alarm** (warn log) and does not fire in normal operation. The former "accepted risk: clients dedupe by seq" clause is deleted — after the fix, that window does not exist.

### Decision 4: Bounded reads — reverse tail-chunk seek

- `offset = 0` (the hot path; every doorbell wake lands here): read backwards from EOF in chunks (start 64 KiB, grow ×8 until ≥ limit+1 newlines or file head), parse only the tail ≤ limit lines; `total` comes from `message_count`. Measured ~1.4 ms @ 100k lines (vs ~69 ms full scan).
- `offset > 0` (scroll-up pagination): continue the reverse seek, skipping `offset + limit` lines by newline scanning — byte scan, no JSON parsing. Cost ∝ bytes traversed, matching the access distribution where "reaching message 50,000 is rare".
- Upper-bound guarantee: **no path performs O(all messages) parsing/allocation anymore**; the worst case (paging to the oldest page) degrades to an O(file bytes) newline scan.
- Implementation notes (v2): line counts and offsets are all on the **physical line** basis (consistent with Decisions 1/3); reverse chunking cuts UTF-8 multi-byte characters (Chinese bodies are the norm), so partial lines at chunk boundaries must be stitched before parsing — the JSONL writes only `\n`, so there is no CRLF ambiguity. A cross-chunk Chinese-message unit test is mandatory.

> **ADR-096 follow-up (contract note).** This decision's `read_messages` contract is unchanged in shape (reverse tail-chunk window read, `total` from `message_count`). ADR-096 generalizes the same "no full session read" discipline to the *auxiliary* reads ADR-092 left open, and adjusts two frequencies: ① the `message_count` / seq crash self-heal moves from an **append-time tail check per write** to a **once-per-process first-touch reconcile** (the first write in a process does one full pass; subsequent appends are pure increments); ② the idempotency-id membership moves from a per-append full scan to the first-touch pass plus an in-memory set backed by a small `ids.index` sidecar; ③ the list preview (`last_message_preview`) becomes a write-time persisted field of the session meta rather than a read-path projection. The read path itself is unaffected.

### Decision 5: `chats.index` — list_for from O(fleet) to O(self) (v2 revision: per-user index lock)

- Maintain `users/{uid}/chats.index.json` per user: a denormalized array of thread summaries (`chat_id, peer, last_active_at, last_message_preview, unread`). Updated on every change (append / mark_read), for both participants' indexes.
- **Index writes take a second lock keyed by uid (v2 fix)**: lock order is fixed as **thread lock → user lock** (a one-way hierarchy, no deadlock). The per-thread write lock alone is not enough: threads A-B and A-C hold **different** thread locks yet both read-modify-write `users/A/chats.index.json` — concurrent whole-file rewrites lose updates, producing a **silently stale yet intact-looking** index: the "corrupt ⇒ rebuild" fallback never fires, and "directories are truth" cannot save a cache that looks healthy. Index files reuse `save`'s tmp+rename atomic write, so `list_for` reads safely without a lock (old value or new, never torn).
- `list_for` reads only its own index file. Missing or corrupt index → fall back to a full directory scan and rebuild (the first-launch migration path). **Directories are the source of truth; the index is a disposable cache** — this positioning keeps it from ever becoming a second truth source requiring consistency maintenance. Rebuild cost is **O(fleet)** (threads where the user is the `hi` side are scattered under other users' directories and can only be collected by a full scan) — equal to one call of today's `list_for`, acceptable as a rare fallback (v2 note).

### Decision 6: Fold push/poll edge handling into one state machine (clients, both apps; revises the enforcement mechanism of ADR-076 Decision 8 §v2 item 3)

- Fold "up edge ⇒ exactly one active re-read" from three hand-written sites into `setChatPushChannel(up)` itself: `up` → stop fallback polling + emit one coalesced `wake()`; `down` → re-arm polling. Callers (the three edge sources) degrade to **reporting facts only**, no longer each deciding refresh behavior.
- Desktop `applyConnectionTransition`'s explicit wake is removed; Mobile `presence.reportChat`'s missing catch-up is fixed by structural convergence — not a one-line patch, but making drift unrepresentable.
- Both apps add **transition-table contract tests** pinning the invariant: `down→up` exactly one wake; `up→up` zero; `up→down` re-arms polling.
- The push/poll mutual-exclusion principle (v2 item 5) is unchanged; only its execution point moves into the state machine.
- Desktop and Mobile share no TS package today: during the transition keep two isomorphic implementations + mirrored contract tests; once a shared package exists, consolidate the freshness state machine and `planPage` into one implementation (this ADR records the intent, not a schedule).

### Decision 7: Evolution path (explicitly not now)

- **Rolling segments**: when a single thread's file exceeds a threshold (initial value 10 MB), seal the oldest segment as `messages.{n}.seg.jsonl`; reads span segments; **nothing is ever deleted** ("digging up old history" is a product promise — here L3 can only mean "goes cold", never "gets dropped"). Trigger: pagination p99 latency or file-size threshold after Decision 4 lands.
- **SQLite**: when **content search** or **multi-process writes** become requirements, migrate to `acowork-sqlite` following the ADR-082 precedent (FTS5 / indexes are the only answer to those query classes; flat files cannot provide any O(log n) answer). This ADR refuses to prepay that cost.

### Unchanged contracts

- The doorbell stays **empty-payload** (ADR-076 Decision 8 §v2 item 2: putting `chat_id` in the doorbell = publishing the social graph; the privacy decision is not shaken by this restructuring);
- **HTTP responses remain the sole source of truth**; signals carry no content and are not cached;
- Persistence stays JSONL + `conversation.json`, no rewrite migration.

## Consequences

**Positive:**

- All client refresh decisions are content-based (seq suffix diff); the four bug families (unseen inflation, same-second collisions, echo double-count, misfired resync) vanish by construction instead of being patched one by one;
- Read cost no longer grows with history length: O(page size) on the hot path, O(bytes traversed) when paging up;
- The edge invariant is guaranteed by mechanism (state machine + contract tests), not by comments; a fourth drift site cannot appear;
- Concurrent sends no longer lose unread counts; `list_for` no longer degrades with fleet size.

**Negative:**

- Wire model change: messages responses and the send echo gain `seq` (backward compatible: old clients ignore unknown fields; new clients must keep the legacy degradation branch);
- The write path gains a derived field (`message_count`), two lock registries (thread lock + per-user index lock), and one sub-millisecond tail read per append: the registries are new state that must be cleaned up with the pair-directory / user lifecycle;
- Two isomorphic TS implementations remain; contract tests are an anti-drift mechanism, not a deduplication fix.

**Risks:**

- ~~Seq reuse in the crash window (Decision 3)~~ — eliminated in v2 by the append-time tail check: duplicate seqs never land on disk, counter drift is caught up on the next append; the residual cost is one sub-millisecond tail read per append;
- `chats.index` diverging from directory scans — mitigated by positioning the index as a rebuildable cache: corruption triggers rebuild, no dual-truth-source sync.

## Alternatives Considered

1. **Patch-level fixes** (add one wake to Mobile, clamp the unseen inflation) — rejected: symptom-level; the invariant still has no enforcement mechanism, and the count-guessing model keeps producing new bug families. The immediate trigger of this ADR was exactly a patch proposal being rejected in review.
2. **Migrate to SQLite directly** — rejected: current needs (append + tail/paginated reads) are fully served by JSONL + bounded reads; search is the only requirement that genuinely needs indexes, and it does not exist yet. Prepaying migration and dual-write cost violates YAGNI; Decision 7 leaves the door open.
3. **Per-message UUID instead of seq** — rejected: UUIDs are unordered, so the suffix diff and `ackedThrough` would still depend on ts ordering, keeping the same-second collision; a per-thread monotonic integer is the minimal sufficient identity.
4. **Client-side dedup by content hash** — rejected: identical bodies in the same second are legitimate messages (two consecutive "OK"), so content cannot serve as identity.

## References

- ADR-076 Decision 8 §v2 (doorbell and refresh strategy; this ADR revises the enforcement of item 3 and the rule body of item 4)
- ADR-084 (user standalone process; same discipline for fire-and-forget doorbell publishing)
- ADR-050 (chat-list data-driven refactor)
- ADR-082 (memory storage migration to SQLite; the precedent for Decision 7)
- Design doc 25 §8
