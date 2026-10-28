# ADR-098: Unified Standard-Group Model — Converging Every Chat Surface onto Kind-Parameterized `chat_sessions`

- **ID**: ADR-098
- **Status**: Accepted
- **Date**: 2026-10-21
- **Deciders**: ACowork Architecture Group
- **Related**: ADR-076 (multi-user account system), ADR-092 (user-chat identity and bounded reads), ADR-096 (no full session reads on the request path), ADR-097 (in-conversation history search — DMs/groups reuse its search surface); design doc 27 §4.4 / §5.4.4 / §6

## Summary

Converge every desktop "people talking" chat surface onto **one kind-parameterized generic container**, `chat_sessions`: no more per-surface storage and per-surface human components for "one-to-one / group / project group". Instead there is **one standard group plus five kind specializations**. The human surface is rendered uniformly by `GroupChatPanel`; differences are driven solely by `kind`. Messages, search (ADR-097), attachments and locate are all shared — zero new storage.

All five kinds are specializations of the standard group:

| kind | meaning | member source | member governance |
| --- | --- | --- | --- |
| `user_user` | the smallest group of two people (DM) | session participants | none (1:1, no add/remove) |
| `user_group` | user-created multi-human / multi-agent group | session participants | any-participant add/remove (new in this ADR) |
| `project_group` | project channel | PM project members, synced server-side via `sync_group` | PM-mediated, read-only |
| `user_agent` | 1:1 human↔agent | session participants | none (1:1, cannot add) |
| `agent_agent` | 1:1 agent↔agent | session participants | system-mediated |

## Context

- **Pre-convergence fragmentation**: `user_user` once rode a separate `userChatStore` / `InboxPanel`, `project_group` rode `GroupChatPanel`, `user_agent` rode `AgentChatView` — three human surfaces, two storage semantics, with search / attachments / unread reimplemented on each.
- **P0 (landed and verified)**: `user_user` migrated to `chat_sessions(user_user)`; the DM reuses `GroupChatPanel` + `chatSessionStore` + `ChatHistoryDialog`; `userChatStore` / `InboxPanel` were retired. The DM thereby gains in-conversation search for free (attaching `activeGroupId` is enough to hit `/api/chat-sessions/{sid}/search`).
- **The backend was already kind-agnostic**: `chat_sessions` has long been a generic container, `SessionKind = { UserUser, UserGroup, ProjectGroup, UserAgent, AgentAgent }`; `create_session` already accepts `kind=user_group` (participant accounts→Human, the rest→Agent, requiring only that the caller is themselves a participant).

## Decision

### D1 — `chat_sessions` is the only container; `kind` is a display / behavior tag only

No kind gets its own storage. `kind` decides only: the sidebar row icon, which governance path the member panel takes, and whether the @ picker is enabled (off for `user_user`). Message read/write, the pagination spine, the search index, attachments and unread are fully homogeneous across the five kinds.

### D2 — creating a `user_group` adds zero new backend

The existing `POST /api/chat-sessions` (`kind=user_group`) already suffices: the server resolves identities through `typed_participants` (trusting not the body's roles) and forces the caller to be a participant. P1's **net-new is only the user-side member add/remove routes**.

### D3 — user-side member add/remove routes: any-participant authz, limited to `user_group`

Two human-facing routes are added in [chat_sessions_api.rs](../../../core/acowork-user/src/http/chat_sessions_api.rs):

- `POST /api/chat-sessions/{sid}/participants` (body `{ participant_ids }`)
- `DELETE /api/chat-sessions/{sid}/participants/{pid}`

Rules:

1. **Authz**: the caller must currently be a participant of the session (any-participant). A non-participant reuses the existing offload error convention mapped to `404` (existence hidden).
2. **Kind limit**: `user_group` only. `project_group` is refused (PM-governed); `user_user` / `user_agent` are 1:1 pairs that refuse adding — all `422`.
3. **Server-side identity**: new members are classified Human / Agent via `typed_participants`, never from the body. Adding is idempotent (an existing member is ignored).
4. **Floor**: a removal that would leave fewer than 2 participants is refused (a group keeps at least two members). You may remove yourself (= leave) or anyone else.
5. **Doorbell**: both routes publish `notify_chat_changed` to the session's human participants after a change, so other clients refresh the roster live (`chat_sessions::add/remove_participant` themselves do not notify; the handler does).

### D4 — frontend single `groups` bucket + `dms` bucket, split by `kind`

[projectGroupStore.ts](../../../apps/acowork-desktop/src/stores/projectGroupStore.ts):

- `fetchGroups`: `user_user`→`dms`; `project_group` ∪ `user_group`→ the unified `groups` (the `kind` feeds the row icon / member-panel path). The admin cross-account union only matters for `project_group`, merged with `mergeById` so a caller's own row keeps its unread context.
- User-side actions: `createGroup(title, memberIds)` (self is forced in and deduped → POST → upsert → `selectGroup`), `addMembers(sid, ids)`, `removeMember(sid, pid)` (syncs the local participants from the response on success; removing self drops the row and releases the middle pane).

### D5 — P2 (Scoped): the member panel dispatches by kind; AgentChatView is not physically merged

[GroupMembersPanel.tsx](../../../apps/acowork-desktop/src/components/chat/GroupMembersPanel.tsx) is a dispatcher that runs one hook then branches:

- `user_group` → [UserGroupMembersPanel.tsx](../../../apps/acowork-desktop/src/components/chat/UserGroupMembersPanel.tsx): the data source is the session's `participants`, exposing add / remove (any-participant).
- `project_group` (or a session still resolving) → keeps the PM-project read-only path (`ProjectGroupMembersPanel`).

`GroupChatPanel`'s header generically renders title + member count + history button for any non-DM kind; the @ picker is disabled only for `user_user`. The **physical merge of `AgentChatView` into `GroupChatPanel` is deliberately not done** (recorded as a future ticket).

### D6 — the convergence completion criterion

One human-surface component (`GroupChatPanel` + the kind-dispatched member panel) covers the `user_user` / `user_group` / `project_group` kinds, with all differences driven only by `kind`.

### D7 — a `project_group`'s `owner` is the PM project's owner (pushed by PM)

A `project_group` used to have no user-service owner, so only a platform admin could edit its `chat.md` (ADR-099). PM now pushes `Project::owner()` (`owner_id ?? created_by`) alongside the roster on every `ensure` / `sync_group`, and the session row caches it — the same shape as `title` / `participants` / `repo`: the upstream service owns the relation, the chat row caches the fact. The project owner therefore governs their channel's prompt exactly as they govern the project, and an ownership transfer reaches the chat domain on the same call (even when the outgoing owner stays on the roster).

Membership stays PM-mediated and read-only on the user surface (the table above is unchanged). `title` converges from PM for the same reason: a user-side rename of a `project_group` is refused, so PM's title is its only source.

## Consequences

- **Storage / component convergence**: three human surfaces collapse to one plus kind dispatch; search / attachments / unread are written once and shared across all kinds.
- **`user_group` becomes first-class**: users create their own multi-human / multi-agent groups and self-manage members under any-participant authz, no PM mediation.
- **Legacy projection surface retained (not deleted by this ADR)**: the `chat.rs` / `/api/users/{me}/chats` projection endpoint and the `user-chat-api.ts` attachment helpers **cannot be removed** — mobile is still their caller. Their **physical removal is gated on the mobile ticket** (P3 closeout), removed alongside it to avoid scope creep.
- **Legacy data migration obsolete**: the `user_user`→`chat_sessions` migration was completed in P0; no migration for the old inbox storage is needed.
- **AgentChatView merge deferred**: P2 takes the Scoped approach; the physical merge is left as a future ticket.

## Landing status (P0–P3)

- **P0**: desktop + backend complete and verified (`user_user` converged, `userChatStore`/`InboxPanel` retired; backend + desktop tests green).
- **P1**: backend user-side member add/remove routes + tests complete; frontend store actions + `NewGroupDialog` + sidebar entry complete.
- **P2 (Scoped)**: member-panel kind dispatch complete (`user_group` takes the participants add/remove path).
- **P3**: this ADR records the convergence; the physical removal of the `chat.rs` projection endpoint and the `user-chat-api.ts` lib is **gated on the mobile ticket**, not done this round.
