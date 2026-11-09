# Chat Transcript Turn Header — Unified Feedback Design

> Version: v1.0 | Updated: 2026-11-08
>
> Related: [ADR-106](../../en/ADR-106-chat-execution-reference-projection.md) (D9 presentation contract; this design proposes a revision to it), ADR-049 (single `ProcessingPhase` vocabulary), ADR-105 (group default responder), ADR-055 (cross-machine reachability), Design 27 (Intent messaging & group chat), Design 29 (Chat Router)
>
> The Chinese document ([docs/design/zh/33-chat-turn-header.md](../zh/33-chat-turn-header.md)) is the source of truth.

## 1. Executive Summary

The execution session shows the assistant avatar + name + a phase status line **immediately** after the user sends. The chat session (1:1 and group) shows only a centered generic waiting line at that moment; the avatar + name appear only once the round's reference row and block projection land. The latency is structural: the chat transcript's content plane runs on the disk projection + doorbell (ADR-106 D3/D5), while the waiting window (the D9 claim) was deliberately made "the platform's own voice" — nameless, centered, bubble-less.

This design upgrades the waiting window's presentation from a centered generic line to a **Turn Header**: right after sending, the transcript tail renders the *expected responder's* avatar + name + status line; when the round's execution block or an agent answer row arrives, the claim ends and the block's own avatar header (already existing) takes over at the same anchor — content "overwrites" the state. No new status vocabulary: reuse `getProcessingPhase()`. The name is not a guess — it is the delivery decision's own input (@-mentions / the group default responder / the 1:1 peer), and server-side receipts and closing rows are what falsify it.

The change is purely presentational: no new network endpoints, no proto changes, no storage changes. It requires a revision chapter to ADR-106 D9 (draft in §9.1).

## 2. Context

### Facts (verified in code)

- The execution session's instant feedback comes from subscribing to the local exec session's live state: `chatStore.agentStates[instanceId].sessionStates[sid].sessionStatus`, vocabulary per ADR-049 `ProcessingPhase` ([lib/types.ts:1615](../../../apps/acowork-desktop/src/lib/types.ts#L1615)).
- Chat side today: `AgentChatView.tsx:1260` (1:1) and `GroupChatPanel.tsx:1789` (group) render a centered `ChatTurnStatusLine` while `composerLocked` (this device's D9 claim is in window) — generic wording (group) or a named 1:1 string, but **no avatar, no phase**.
- `ChatExecutionBlock` already renders an avatar + name header (`AgentAvatar`, line 436); its header status word follows "live word wins, read-derived word is the fallback" (line 297), and the live subscription is viewport-scoped, never persisted, never in context, never in unread counts (ADR-106 D9, item 4).
- Claim mechanism: `chatSessionStore.armTurn(chatSessionId, rowId)`; the window starts at this device's `delivered` send receipt and ends at the first answering row or deadline (ADR-106 revision 2026-11-03); `archived` (no delivery target) and all-failed fan-outs arm nothing.
- Responder information already known client-side at send time: 1:1 = the peer agent; group with @ = the `targets` argument of `sendGroupText`; group without @ = the chat session's `responder` field (the same field GroupResponderCard reads/writes, stored server-side).
- Avatar anchoring machinery already exists (`avatarAnchor.ts` / `bubbleMetaAnchor.ts`); adjacent bubbles from the same speaker fold their headers, and a block always starts a new turn.
- Mobile's `AgentChatScreen.tsx` has the same waiting line; the transcript shape is unified with Desktop (ADR-106 P6).

### Assumptions

- The delivery decision's input (mentions / default responder) matches the actual delivery target within the claim window. Divergences (router re-routing, delivery failure) are expressed by server-side closing rows, and a closing row ends the claim — the turn header disappears with it, so a wrong name's lifetime is capped by the receipt by construction.

### Constraints

- ADR-106 D9's three invariants must survive: (1) terminal outcomes (aborted / failed / timeout / waiting) stay centered platform lines — a stop is not the agent speaking; (2) live status is a presentation-layer add-on only: not persisted, not in context, not in unread counts; (3) unreachable ≠ running.
- ADR-049: the single source of truth for status vocabulary is `getProcessingPhase()`; parallel enums are forbidden.
- ADR-055: the round's execution session may live on a remote node — live status being unavailable is the normal case, not an error.
- Gateway boundary red line: chat presentation data is fetched only via Runtime/Gateway HTTP, never by direct filesystem reads (this design adds no data plane at all).

## 3. Requirements

### Functional

- FR-1: In a 1:1 chat, right after the send's `delivered` receipt, the transcript tail shows the peer agent's avatar + name + role + status line (the role line was added by the 2026-11-08b amendment, §9.1).
- FR-2: In a group, the header shows the expected responder(s) per the resolution rules (avatar stack + name list for multiple targets, and no role line) + status line.
- FR-3: When the round's reference row arrives (block renders) or an agent answer row arrives, the turn header disappears and content takes over at the same avatar anchor — block header / bubble header — with no visual jump.
- FR-4: `archived` sends, all-failed fan-outs, and page reloads (no local claim) show no turn header — identical gating to today's waiting line.
- FR-5: Terminal closings (aborted / failed / timeout) remain centered platform lines, never avatar-wrapped.
- FR-6: Within the claim window, if the round's bound exec session is resident locally and resolvable (Phase 2, optional), the status line shows the real `ProcessingPhase` word; otherwise a generic "working" shimmer.

### Non-functional

- NFR-1: Presentation-layer change; no new network endpoints, proto fields, or storage writes.
- NFR-2: No virtualization regression — the turn header is a single synthetic node derived from the window snapshot, kept out of `messageFolder`'s block folding.
- NFR-3: Desktop and Mobile stay shape-unified (one contract, two hosts).

### Non-goals

- No message mirroring from exec into chat (ADR-106 retired the "copy" route).
- No changes to projection-pipeline freshness (earlier flushes, new doorbell triggers).
- No changes to routing/delivery semantics (Chat Router, default responder, fan-out criteria all untouched).

## 4. Alternatives Considered

| Option | Summary | Pros | Cons | Risk | Reversibility |
|--------|---------|------|------|------|---------------|
| **A (recommended)** | Turn header: avatar + name + status line during the claim window, taken over by the block header when content lands | Pure presentation; reuses claim / anchoring / vocabulary machinery wholesale; instant feedback | Needs an ADR-106 D9 revision; group naming depends on delivery-decision inputs | Low: closing rows cap a wrong name's lifetime | High: revert two call sites |
| B | Earlier projection: flush at round start + more doorbell triggers so blocks land sooner | No new presentation shape | Touches the neighborhood of D5's "terminal rounds are immutable" cache invariant; the send→route→announce chain latency can't reach zero; worse payoff cross-machine | Medium: cache & doorbell semantics change | Low |
| C | Mirror exec live-stream into the chat transcript | Richest feedback | Directly violates ADR-106's "reference, not copy" boundary (exactly the retired route) | High: architectural regression | — |

Why A: the perceived delay comes solely from the "claim window has no identity" segment; once a block lands, its presentation (avatar + name + live word) already matches the execution session. A targets precisely that segment and touches no data plane.

## 5. Recommended Architecture

```
user sends ──202 delivered──▶ armTurn(claim)
                              │
                              ▼
                 ┌─ ChatTurnHeader (synthetic row, transcript tail)
                 │   avatar(s) + name chain + status line (shimmer "working" / Phase-2 real word)
                 └── first answering row (ref row / agent msg / receipt) or deadline ──▶ claim ends
                              │
                              ▼
                 ChatExecutionBlock's own avatar header takes over at the same anchor (existing mechanism)
```

### Module: ChatTurnHeader (new, Desktop `components/chat/`)

- **Responsibility**: render the turn header — avatar (for several responders, this component's own overlapping stack; NOT the sidebar's square-tile `GroupAvatarStack`, which is a different visual for a different surface) + name chain + one status word; reuse `ChatEventText`'s live-shimmer grammar.
- **Dependencies**: agent directory (avatar/display-name resolution, same path both views already use); `getProcessingPhase` (Phase 2).
- **Public interface**: `<ChatTurnHeader identities={TurnIdentity[]} text={string} className?={string} />` — multiple identities stack; the name chain is joined by `Intl.ListFormat` for the active locale ("A、B" in zh/ja, "A, B" in en); Phase 2 adds a `phase` input here, and its absence renders the generic "working".
- **Failure modes**: identity unresolved (directory not ready) → degrade to today's centered generic line; never render a nameless avatar.

### Module: claim metadata extension (`chatSessionStore`)

- **Responsibility**: at `armTurn`, record the send-time-known "expected responder instance ids" into the claim-window state for views to derive the header from.
- **Dependencies**: callers already hold `targets` / peer id / `responder` read at the send site.
- **Public interface**: new read-only field on `InflightTurn`, `responders?: string[]` (cleared when the claim ends); the arm/close criteria are unchanged.
- **Failure modes**: field absent (legacy data / reload) → no header, identical to current behavior.

### Module: AgentChatView / GroupChatPanel call sites

- **Responsibility**: replace `composerLocked && <ChatTurnStatusLine …/>` with `<ChatTurnHeader>` per the resolution rules below; hole-class messages (`block.notFound` etc.) stay centered platform lines.
- **Responder resolution rules** (single function `resolveExpectedResponders`, shared by both views; it mirrors the server's `deliver_mentions` / `addressed_execution_round` rung for rung):
  1. 1:1 chat: the peer agent instance (one);
  2. group reply addressing a round: that round's agent alone — the server resolves `in_reply_to` against the whole log and ignores the body's `@`s once a round is named. When the addressed row is outside this surface's loaded window the fact is UNRESOLVED, not absent: name nobody (the centered line) rather than guess from the mentions;
  3. group with @: the `targets` list;
  4. group without @: the agent named by the chat session's `default_responder` field; no responder (archive path) → render nothing (that send arms no claim);
  5. multiple targets: list all (stacked avatars + the locale's name chain).
- **Failure modes**: responder read fails → generic-line fallback.

### Module: Phase 2 (optional) — real phase words during the claim window

- **Responsibility**: while a claim is in window and the round's bound exec session is resident locally, subscribe to its `sessionStates[sid].sessionStatus` and display the `getProcessingPhase` word, under the same rules as the `ChatExecutionBlock` header (live word wins, not persisted, viewport-scoped).
- **Dependencies**: round coordinates may not exist early in the claim (reference row not yet landed); resolution path = the D11 derived index "sessions bound to this chat" intersected with locally resident sessions; not found → fall back to the generic word.
- **Failure modes**: remote / not resident → generic "working" (same as Phase 1; not an error).

### Module: Mobile `AgentChatScreen`

- Same contract and shape as Desktop; identity resolution reuses Mobile's existing directory read path.

## 6. Data Flow

1. Send → `delivered` receipt → `armTurn(chatId, rowId, responderIds)`;
2. Views derive the turn header from the window snapshot (no new request);
3. (Phase 2) when resident locally, the existing MQTT `session_state` push updates the phase word — presentation-layer add-on, zero persistence;
4. First answering row / receipt lands → claim ends → header disappears with the snapshot; block header / bubble header renders;
5. Deadline → claim ends, no header (terminal outcomes, if any, are server-side closing rows).

## 7. Security and Operability

- Trust boundary: unchanged. The turn header is pure client-side presentation; names come from locally known delivery-decision inputs; the authoritative fact (who actually answered) is still stated by server-side rows.
- Permissions: none added; avatar/name resolution uses paths participants can already read (member profiles).
- Observability: the header shares the claim window's lifetime, already covered by existing `[chatSessionStore]` arm/resolve logs; no new log plane.
- Operational risk: a wrong name (re-route / failure) survives only send→closing-row — the same window as today's waiting line; no new persistent erroneous state.

## 8. Delivery Plan

| Phase | Goal | Validation | Rollback |
|-------|------|------------|----------|
| P0 | ADR-106 D9 revision chapter review (draft §9.1) | Architecture-group approval | No code merged |
| P1 | `ChatTurnHeader` component + 1:1 call site + armTurn metadata | Unit: visible right after `delivered`; gone on first answering row; invisible for archived/reload; terminal stays centered | Revert call sites (one commit) |
| P2 | Group call site + `resolveExpectedResponders` (@ / responder / multi-target stack) | Update `GroupChatPanel.waitLine.test.tsx` et al. + new cases | Same as P1 |
| P3 | Mobile `AgentChatScreen` alignment | Mobile shape snapshot tests | Independent revert |
| P4 (optional) | Real phase words in the claim window (local-residency subscription) | Behavior-parity cases against the `ChatExecutionBlock` header | Disable → back to P1 generic word |

P1/P2 are independently reviewable and mergeable; no phase spans modules.

## 9. Key Decisions

| Decision | Choice | Rationale | Rejected Alternatives | Trade-offs |
|----------|--------|-----------|-----------------------|------------|
| Status vocabulary | Reuse `ProcessingPhase` + one claim-state "working" | ADR-049 single truth; the claim state is "window exists", not a phase — no enum extension | New chat session-state enum | One less precise word in the shortest window, bought with zero drift |
| Group naming basis | Delivery-decision inputs (@ / responder), falsified by closing rows | The name is a decision fact, not a guess; D9's ban targeted inferring the responder from transcript shape | Keep the generic nameless line (status quo) | A later re-routed name may show for a very short window |
| Terminal presentation | Keep centered platform lines | D9 invariant: a stop is not the agent speaking | Avatar-wrap terminals too | Consistent with the exec session's own terminal lines anyway |
| Takeover mechanism | Reuse the block's own avatar header + avatarAnchor | Machinery exists; "overwrite" = claim end; no new folding logic | Persistent header that grows downward | Depends on the existing reference-row arrival timing |
| Phase 2 live words | Optional, local-only, viewport-only | Isomorphic to D9 item 4; cross-machine unavailability is the norm | Force full-chain immediacy | Cross-machine rounds keep showing the generic word |

### 9.1 ADR-106 D9 Revision Draft (for review; to be appended to the ADR's revision chapter on approval)

> **Revision (2026-11-08 · turn header)**: within "this device's send claim", D9's waiting line is upgraded from a centered generic line to a **turn header**: it renders the expected responder(s) (1:1 peer / @-mentions / group default responder — all delivery-decision inputs, not transcript-shape inference) as avatar + name + status word; the first answering row or the deadline hands over to the block's own avatar header at the same anchor. The "never guess the responder" ban narrows to "never infer the responder from transcript shape"; when delivery-decision inputs are unresolvable, degrade to the generic centered line. The three invariants (centered terminal rows, live-as-presentation-only, unreachable≠running) are kept verbatim. The criteria, window, and closing mechanism are unchanged (still the D9 claim).

> **Revision (2026-11-08b · the role line)**: the turn header's shape is fixed as **avatar + name + role + status word**. The role comes from the same agent-directory meta that `ChatExecutionBlock`'s header and a regular agent message header read, on the same tier (one step under the name). The reason is not "more information" but ADR-106 Decision 4's takeover invariant: the block header renders name + role, so a claim window showing only the name would have to *insert* a line under it at handover and shift the whole column — and the point of the turn header is "content overwrites state", not a re-layout. A multi-target fan-out renders **no** role line: a joined list of roles names no single responder, and the avatar stack already says "several".

> **Revision (2026-11-08c · the status word's position)**: the status word does not live in the identity column — it lives in the **content column**, on the row that starts with the same 40px spacer as the block body (the column where `ChatExecutionBlock`'s "Explored" row sits). The 2026-11-08b amendment fixed the element list (avatar + name + role + status word) but not which column the word belongs to, and the implementation put it as the identity column's third line: the `items-center` row grew with that column, lifting the name and role off the avatar's axis, and at handover the word would have had to jump sideways into the content column — a violation of the very invariant that revision was written for. The identity row therefore wears the block header's own classes, and the word becomes its own row behind a 40px spacer: identity row + one line in the content column, of which the takeover replaces only the latter.

## 10. Open Questions

- OQ-1: Multi-target groups — one stacked-avatar header vs. one header row per target. Stacking recommended (one send, one fact); to be settled at review.
- OQ-2: zh/en wording table for the claim-state "working" (and its relationship to the existing `chatView.notice.pendingTurn*` keys).
- OQ-3: Whether a ready client-side index exists for "sessions bound to this chat" (Phase 2 resolution); if not, whether a read-only query is worth adding (decide before P4).

## 11. Self-Review

- [x] Requirements covered (FR-1..6 each have a module landing point and validation)
- [x] No placeholders (interfaces, resolution rules, rollbacks are concrete clauses)
- [x] Interfaces explicit (component props, store field, single resolution function)
- [x] Tests and rollout defined (per-phase validation + independent rollback)
- [x] Contract change has a revision draft (§9.1) with a review gate ahead of code (P0)
- [ ] OQ-1..3 close at P0/P2/P4 reviews — they do not block P1
