# 27 — Global Search LLM Summary: Module Interfaces, Concurrency, and Cancellation

> **Version**: v0.1 (design draft, pending review)  
> **Date**: 2026-10-14  
> **Related ADRs**: [ADR-093](../../adr/en/ADR-093-global-search-llm-summary-citations.md), [ADR-081](../../adr/en/ADR-081-global-search.md), [ADR-009](../../adr/en/ADR-009-gateway-workspace-isolation.md), [ADR-064/070](../../adr/en/ADR-064-pm-standalone-process.md)  
> **One-sentence decision**: Keep existing per-source search APIs and UI navigation actions; add a bounded parallel query coordinator in the Desktop, publish each source's results as they arrive, and add only a Runtime summary endpoint using the existing Provider `chat()` API. Validated local citation IDs map to clickable navigation.

---

## 1. Goals and Non-Goals

### 1.1 Goals

1. Search files, docs, projects, conversations, memory, and Git in parallel for one query; keep each source list and ranking independent.
2. Update a tab as soon as its source returns and expose source-level status; a slow or failed source/LLM call must not block other sources or list interaction.
3. Generate one cross-source summary from bounded, truncated hit summaries; citations may refer only to hits submitted in the current request.
4. Define cancellation and stale-response isolation for new queries, agent/workspace changes, dialog close, and deadlines.
5. Prefer existing APIs, clients, AbortSignal conventions, hit mappers, navigation actions, and Runtime Provider interfaces; fill gaps without reimplementing source search.

### 1.2 Non-Goals

- No six-source aggregation or LLM call in the Gateway; no change to doc/PM/Runtime data ownership.
- No cross-tab merge/rerank; no changes to source-specific ranking or retrieval algorithms.
- No search queue, cache, persistent index, LLM SDK, background worker, or streaming SSE.
- No model-generated URLs/HTML; no full files, docs, conversations, or locator/payload sent to the model.
- MVP adds no new Git search algorithm: reuse Desktop's existing `git log(limit=100)` + `filterGitCommits` semantics.

## 2. Existing Modules and Reuse Decisions

| Capability | Existing location / behavior | Reuse and minimum change |
|---|---|---|
| Global-search UI, source lists, scope hit types | `apps/acowork-desktop/src/components/search/GlobalSearchDialog.tsx` | Keep the component and hit state; add per-query coordinator state and summary area, not a new global-search store |
| Debounce / abort / stale-response guard | Same component has 300 ms debounce, `AbortController`, and `seqRef` | Consolidate into one `SearchRun` lifecycle; one child controller per source; use `seq` to isolate old runs |
| Doc / project APIs | `src/lib/doc-api.ts::searchDocs` and `src/lib/pm-api.ts::searchPm` already accept `AbortSignal` | Reuse as-is; do not rewrite request clients |
| Runtime memory/conversation API | `/api/agents/{id}/search?scopes=...`; existing response mappers produce `MemoryHit` / `ConversationHit` | Continue using the endpoint and mappers; one request per scope with an independent signal |
| File search | Existing Workspace Runtime API + fetch in the dialog | Reuse URL, `toFileHits`, and existing open action; add run signal and centralized timeout handling |
| Git search | `useGitStore.fetchLog`, internally `httpGet` + 503 retry; does not currently accept caller signal; UI gets up to 100 commits then filters client-side | Reuse store, filter, and commit DTO; add optional signal through `fetchLog`/`httpGet`, ensuring abort is not retried |
| Navigation/locate | `openFileHit`, `openDocHit`, `openProjectHit`, `openConversationHit`, `openMemoryHit`, `openGitHit`, `activate` | Reuse or extract one typed dispatcher; summary citations must not copy navigation implementations |
| Runtime LLM | `Provider::chat(ChatRequest)`, `ChatMessage`, and the call pattern in `episode_distill` | Reuse the Provider trait, existing AgentCore/SessionManager model resolution path, and request types; add a small search-summary prompt/parser, not compaction or distillation prompts |
| Gateway Runtime proxy | `proxy_routes` + `proxy_to_runtime_with_method` | Add one transparent POST route forwarding body/status/content type; no business logic or separate HTTP client |

> Runtime `/search` currently aggregates agent-owned memory/files/Git/conversations; Desktop file search uses `/workspaces/search`, while docs and PM are separate processes. The new coordinator follows each tab's current API; aggregation ownership does not move.

## 3. Module Topology and Dependency Direction

```text
GlobalSearchDialog (React view + existing hit state)
  ├── SearchRunCoordinator (pure coordination logic, initially colocated)
  │     ├── FileSearchAdapter ──────────────── Runtime /workspaces/search
  │     ├── GitSearchAdapter ───────────────── Gateway /git/log -> Runtime
  │     ├── DocSearchAdapter ───────────────── doc client -> Gateway -> doc
  │     ├── ProjectSearchAdapter ────────────── PM client -> Gateway -> pm
  │     ├── MemorySearchAdapter ────────────── Gateway /agents/{id}/search?scopes=memory
  │     └── ConversationSearchAdapter ───────── Gateway /agents/{id}/search?scopes=conversation
  ├── SummaryCandidateBuilder (typed hit -> bounded DTO + local ID map)
  ├── POST /api/agents/{id}/search/summary
  │      Gateway transparent proxy
  │        └── Runtime http::global_search::summarize_search
  │              ├── request validation + prompt construction + output validation
  │              └── existing AgentCore/SessionManager Provider::chat
  └── SummaryRenderer (plain text + allowlisted citation buttons)
         └── existing typed hit dispatcher -> existing navigation actions
```

The coordinator lives only for one dialog/query generation. It owns no retrieval business logic; source adapters are thin closures/functions that call existing APIs. Initially keep it in the dialog; extract to `searchRun.ts` only if testability warrants it. Add the Runtime handler to `http/global_search.rs` or a neighboring `search_summary.rs`, whichever best matches the existing file's size; do not add a crate.

## 4. Module Interfaces

### 4.1 Desktop `SearchRunCoordinator`

Initially keep the coordinator as pure async logic in `GlobalSearchDialog.tsx`. A run takes an immutable input snapshot:

```ts
type SearchScope = "file" | "git" | "doc" | "project" | "memory" | "conversation";
type SourceStatus =
  | { state: "waiting" | "searching" }
  | { state: "ok"; count: number }
  | { state: "empty" }
  | { state: "skipped"; reason: string }
  | { state: "error"; message: string }
  | { state: "timeout" };

interface SearchRunInput {
  query: string;
  agentId: string;
  workspaceId?: string;
  caseSensitive: boolean;
  wholeWord: boolean;
  generation: number;
}
```

Design-level pseudocode:

```ts
startSearchRun(input, callbacks): SearchRunHandle
interface SearchRunHandle {
  cancel(reason: "superseded" | "closed" | "scope_changed"): void;
}
```

- `callbacks.onSourceState(scope, status)`: update source status at start/settlement.
- `callbacks.onSourceHits(scope, hits, metadata)`: update existing hit state when a source completes; accept only the current generation.
- `callbacks.onSummaryState(state)`: `waiting | generating | done | error | skipped`.
- `callbacks.onSummary(result, citationMap)`: commit summary and local citation map atomically for the generation.
- `callbacks.isCurrent(generation)`: use existing React `seqRef` to prevent late callbacks.

Do not replace each source API with a generic adapter framework. Six named tasks with `Promise.allSettled` (or per-source settled callbacks) are sufficient; each adapter returns its existing hit type. Normalize only at the summary boundary.

### 4.2 Six Source Adapters

| Adapter | API and eligibility | Signal / per-source limit |
|---|---|---|
| File | `GET /api/agents/{id}/workspaces/search?q=&workspace_id=&max_results=20&match_file_names=true`; keep current case/whole-word options | One signal; skip without agent; at most 20 results, aligned with ADR-093 UI cap. `match_file_names` makes the same endpoint return content hits and file-name hits together (ADR-081 D2) |
| Git | Reuse `useGitStore.fetchLog(agentId, workspaceId, "", 100, 0)` + `filterGitCommits(query)` | Add optional `AbortSignal` to existing `fetchLog`/`httpGet`; AbortError exits without 503 retry |
| Doc | `searchDocs(query, 20, signal)` | Signal already supported; identity/permissions remain as enforced by current Gateway API |
| Project | `searchPm(query, 20, signal)` | Signal already supported |
| Memory | `GET /api/agents/{id}/search?scopes=memory&mode=hybrid&limit=20` | Reuse existing fetch/mapper; skip without agent |
| Conversation | Same Runtime endpoint with `scopes=conversation&mode=hybrid&limit=20` | Same; `not_indexed` is skipped/no hits, not a network failure |

**Limits**: Start at most six source requests at a time; do not add a second concurrency pool. This fixed small fan-out needs no semaphore. The run deadline controls request lifetime; any per-fetch deadline must not exceed the remaining run deadline. Source lists retain their existing order; Git commits retain log order after filtering.

### 4.3 Summary Candidates and Citation Map

Add a pure helper; do not copy hit locators into the request:

```ts
interface SummaryCandidate {
  id: string;       // r1..rN, unique per run
  scope: SearchScope;
  title: string;     // max 200 Unicode scalar values
  snippet: string;   // max 500 Unicode scalar values
}
interface CitationMap { [candidateId: string]: RowHit }
```

The builder takes completed, visible hits from all six sources. It takes the first five from each source in that source's existing order, then caps the total at 30 in a fixed scope order; it must not compare scores across sources. At most five per source and 30 overall. IDs are unique within the run. A hit duplicated across APIs may remain two citations; no global dedupe is required.

Privacy/data boundary: send only title/snippet/scope/id; never serialize `RowHit`, `payload`, `path`, `sessionId`, `nodeId`, doc/task ID, absolute path, or full commit hash. Do not put hit text in URL/query parameters.

### 4.4 Desktop → Gateway → Runtime Summary API

#### Gateway route

```http
POST /api/agents/{instance_id}/search/summary
Content-Type: application/json
```

Add the route to `core/acowork-gateway/src/http/proxy.rs::proxy_routes`. The handler extracts only agent ID, body bytes, and headers, then forwards to Runtime `/search/summary` through the existing `proxy_to_runtime_with_method`. Reuse the Runtime registry, error forwarding, and hop-by-hop header rules. Do not parse candidates, log request bodies, or retry POST.

#### Runtime request

```json
{
  "query": "database migration",
  "candidates": [
    {"id":"r1","scope":"file","title":"migration.rs","snippet":"..."},
    {"id":"r2","scope":"doc","title":"Migration Guide","snippet":"..."}
  ],
  "partial": false,
  "timed_out_scopes": [],
  "failed_scopes": []
}
```

`scope` is restricted to the six enum values; `id` matches ASCII `[A-Za-z0-9_-]{1,32}`; trim query, require non-empty, max 512 Unicode scalar values; require non-empty candidates, ≤30; title ≤200 and snippet ≤500 Unicode scalar values; total JSON body ≤64 KiB. `partial=true` means the Desktop used partial results (8-second retrieval deadline expiry or a source failure); `timed_out_scopes` and `failed_scopes` list timed-out and failed sources separately, both restricted to the six scope enum values (validated Runtime-side as well), and are metadata/model context only — no user-executable fields. Oversize/invalid input returns 413 or 422 (consistent error envelope); never silently truncate and claim full input.

#### Runtime response

```json
{
  "summary": "Found ... [[r1]] ... [[r2]]",
  "citations": ["r1", "r2"]
}
```

Use existing Axum `Json`/Serde conventions; response JSON ≤16 KiB; `summary` non-empty and ≤4000 Unicode scalar values; citations are a deduplicated subset of request IDs, at most 30. The frontend parses `[[id]]` in the body; `citations` may support consistency checks/diagnostics but is not a navigation authority. The server validates citations against the input allowlist. Unknown IDs should be stripped during parsing or cause parse failure; do not expose raw model JSON to the UI.

Errors: 400 malformed JSON; 413 oversized body; 422 invalid field/ID/scope; 503 AgentCore/Provider not ready; 504 Provider deadline; 502 Provider error or invalid output. Error bodies must not contain prompts, API keys, or complete candidate text.

### 4.5 Runtime Provider Selection and Summary Module

Place the handler in `core/acowork-runtime/src/http/global_search.rs` or adjacent `search_summary.rs`; register `POST /search/summary` in `server.rs` and declare a module in `http/mod.rs` only if needed. Existing `HttpState.agent_core` and `session_manager_slot` are already shared; do not add another Provider registry.

Module responsibilities:

1. `validate_summary_request`: trust-boundary validation and byte/character/count caps.
2. `build_search_summary_prompt`: fixed system directive plus structured JSON candidate data. Treat candidates as quoted data, ignore embedded instructions, only summarize and cite supplied IDs; no tools.
3. `resolve_search_summary_llm`: reuse SessionManager's current `(model, provider)` resolution (`current_model_and_provider()`) and existing Provider construction/AgentCore active-provider path. If needed, add one narrow accessor returning a `(Arc<dyn Provider>, model)` snapshot; do not duplicate the model-priority rules in the HTTP handler. Release SessionManager/AgentCore sync locks before network I/O. If the HTTP layer cannot obtain the Provider instance matching the selected provider, first expose the narrowest existing provider-factory reuse point; do not silently substitute the distiller/compaction default model without ADR review.
4. `call_search_summary_llm`: build one `ChatRequest`, `tools: None`, temperature `Some(0.1)` or provider default, `max_tokens: Some(800)`, `thinking_mode: Some("disabled")`, then call `Provider::chat`. Do not enter the session tool loop or write to conversation/session token totals; this is independent search inference, not user chat.
5. `parse_and_validate_summary`: do not fall back to `reasoning_content` when usable content is absent; require valid JSON schema, non-empty summary, and allowlisted citations; do not trust model markup.

Do not reuse `compact_with_llm`'s compaction prompt or `<summary>` quality gate: the semantics differ. Reuse its Provider request conventions, explicit thinking disable, and fail-closed handling for empty/invalid output.

### 4.6 Summary Renderer and Navigation Reuse

#### 4.6.1 Dialog Zone Adjustments

Keep the existing `Zone 1 / 2 / 3` concept but split the PreviewPane height in half and share the zone with the new summary area:

```
Zone 1  border-b  [AgentPickerChip] │ 🔍 input  [Aa][][⟲] │ (height unchanged)
Zone 2  border-b  Status bar (source/summary stage, X/6)      (new, 32px)
Zone 3  border-b  [SummaryView · generating/done/error]      (new, upper half)
        │         [PreviewPane]                              (lower half)
Zone 4  border-b  [file][git][doc][project][conversation][memory] (unchanged)
Zone 5  flex-1    Active tab hit list                        (unchanged)
Footer  border-t  [Close]                                    (unchanged)
```

The current `h-[35vh] max-h-[300px] min-h-[140px]` PreviewPane becomes a two-row `flex-1` split: the upper half hosts `SummaryView` and the lower half hosts `PreviewPane`, with a `min-h-[120px]` floor per pane. Reduce `max-h` to `240px` so the two panes never overflow the dialog. The 70vh dialog cap is preserved. `Zone 2` and `Zone 3` collapse to `0` when summary is disabled (empty query or feature off) to avoid consuming typing space.

#### 4.6.2 Summary Renderer

`SearchSummaryView.tsx` is a new dedicated component — JSX and state are non-trivial, so a separate file is clearer than inlining. State machine:

| State | Display | Clickable elements |
|---|---|---|
| `idle` (query empty) | Hidden (collapsed) | — |
| `waiting_for_results` (within 8 s) | "Searching, N/M sources returned" + six source dots (gray/green/red) | none |
| `generating` (summary request in flight) | "Generating summary from N/M sources…" + inline spinner | "Cancel summary" button (top-right) |
| `partial_done` (summary done, ≥1 source timed out) | Body + "received N/M sources; M timed out" | citation buttons + "Regenerate" button |
| `done` | Summary body | citation buttons + "Regenerate" button |
| `skipped_no_results` | "No hits to summarize" | none |
| `error` (Provider 503/timeout/invalid) | "Summary unavailable (reason)" + retry button | "Regenerate" button |
| `cancelled` (user-initiated) | "Summary cancelled" | "Regenerate" button |

Parse `[[rN]]` in the body as React text segments; unknown markers are shown as plain text or removed. **Never use `dangerouslySetInnerHTML` or a Markdown link renderer**.

#### 4.6.3 Input Right-Hand Buttons

- `[Aa]` case-sensitive toggle: existing.
- `[]` whole-word toggle: existing.
- **New** `[⟲]` re-enter button: visible only when the query is non-empty. Click behavior:
  1. Increment the generation synchronously.
  2. Abort the previous run's source/summary signals.
  3. Keep focus on the input; retain the previous list state until new results arrive (avoid blank flashes).
  4. Re-arm the 300 ms debounce; the new run starts after debounce.
- The button is scoped to the current query; hidden when the input is empty.

#### 4.6.4 Summary Cancel Button

- Show "Cancel summary" in the summary area's top-right (`aria-label="Cancel summary generation"`) only in `generating`.
- Click behavior:
  1. Abort the summary signal, cancelling `POST /api/agents/{id}/search/summary`.
  2. Transition to `cancelled`; lists, status, and input remain fully intact.
  3. Do NOT cancel source signals (any source still in flight within the 8 s window continues to update lists).
  4. The user can tap "Regenerate" to retry.

#### 4.6.5 Regenerate Button

- Show "Regenerate" in the summary area's bottom-right in `partial_done / done / error / cancelled`.
- Reuses the same summary endpoint; does not re-issue retrieval; uses the frozen candidate snapshot. `partial` keeps the original timeout scopes.
- At most one auto "Regenerate" per generation; afterwards prompt the user to "modify the query and retry". Prevents a silent retry loop if the Provider is consistently failing.

#### 4.6.6 Citation Buttons

- Each valid citation renders as a `Button` with `aria-label="Open: <title>"` and `data-citation-id`.
- Click: look up `CitationMap` → call the typed dispatcher `activateSearchHit(hit)` → reuse `openFileHit / openDocHit / openProjectHit / openConversationHit / openMemoryHit / openGitHit`.
- The summary area does not switch tabs directly; if the citation's type differs from the active tab, the dispatcher decides internally whether to switch. File line, conversation message index, Memory node, and PM task IDs come only from the local `RowHit`; the model never supplies locators.

#### 4.6.7 Git Summary Citation

The current Git `openGitHit` opens the log, not a specific commit. For MVP, a citation click sets `tab` to `git` and sets `focusedIdx` to the row whose commit hash matches the hit. The user then presses Enter or double-clicks to trigger `openGitHit()`. If the existing UI cannot safely focus the commit (it is not in the most recent 100 log entries), show the Git tab/list without claiming a precise commit-detail navigation. A dedicated commit detail is a separate requirement.

## 5. Concurrency, Performance, and Reliability

### 5.1 One Generation, Fixed Fan-Out

- Keep the existing 300 ms debounce. Create a run only for non-empty trimmed query, open dialog, and eligible agent scope.
- Each run starts at most six fixed requests, using `Promise.allSettled`/per-source callbacks; no recursive fan-out, unbounded queue, or retry storm.
- Do not serialize sources; update hits/status immediately on each source callback.
- Show per-source waiting/searching/ok(count)/empty/skipped/error/timeout and overall `completed / applicable`; do not display a fake precise percentage.
- Show at most 20 hits per source; request `max_results=20` for files. Git still fetches up to 100 log entries for local filtering, but display/summary input is capped at 20, preserving newest-first order.

### 5.2 Retrieval Deadline and Partial Semantics

Use one absolute monotonic deadline `t0 + 8s` for the run, not a separate resettable 8s timeout per source. Sources may complete earlier. At the deadline:

1. Mark pending sources `timeout` and abort their signals.
2. Retain completed hits; a sibling failure must never clear them.
3. Immediately build candidates and send the summary request with `partial=true` and the timed-out scope list.
4. Do not automatically generate a second summary for late hits. A source that wins the cancellation race may update the list only if the generation is current and its source has not reached a terminal state; it does not alter the frozen summary input.

If all six sources settle before 8 seconds, summarize immediately. With no candidates, set summary state to `skipped: no_results` and do not call the LLM.

### 5.3 Fan-Out and Resource Caps

The fixed fan-out is six, so the concurrency limit is six; only one current generation exists per dialog. Abort all old source requests before starting a new query. Debounce controls typing bursts; per-source limits, candidate limits, and the Runtime body limit bound memory/context growth. Do not add polling or background retries per source.

The existing Git `with503Retry` must honor AbortSignal: AbortError is not retried. Other source clients keep their existing 503/retry behavior, but deadline abort has priority and retry loops must not extend work beyond the run deadline.

### 5.4 Summary Deadline and Fault Isolation

- Start an independent absolute 20-second deadline when summarization begins; lists are independent of it.
- Runtime wraps `provider.chat(...)` in `tokio::time::timeout(20s, ...)`. Timeout drops the chat future; the underlying HTTP Provider should best-effort close its request, but cannot guarantee the remote model server stops computation.
- Desktop also uses an `AbortController` for the summary signal; close/new query cancels the Gateway request. Do not automatically retry the LLM POST, to avoid duplicate spend and late summaries.
- Provider not ready/error/timeout/invalid JSON/empty output changes only summary status; retain all hits. Show a localized, non-technical recovery hint.
- Log durations, per-source states/counts, summary token usage (if returned), error class, and generation ID; do not log query/snippet/summary/API key. Generation ID is for log correlation only, not persistence.

### 5.5 Capacity Before Adding Backpressure Infrastructure

Worst-case summary input has 30 × (200 + 500) Unicode scalar values, plus JSON/prompt overhead, and must remain within a 64 KiB HTTP request. At most six retrieval requests plus one summary request are user-triggered per generation; generations replace one another in a dialog. Do not add a global semaphore in MVP: the explicit ceiling is six per active dialog. If multiple dialogs/high concurrent users require global limits, add a Runtime semaphore/Gateway rate limit based on measured concurrency rather than prebuilding a scheduler.

## 6. Cancellation Protocol and State Machine

### 6.1 Desktop State


The `Zone 2` status bar always displays the current generation's top stage (`idle / searching / generating / done / partial / error / cancelled`) and shows the source progress as `X/Y sources returned`. The status bar must not display precise percentages or block interaction.

`Zone 3` splits its height between the summary area and the preview pane. When the query is empty or summary is disabled, `Zone 2 + 3` collapse to `0` height and the preview pane reclaims the full height. See §4.6.1 for details.


```text
Idle
  └─ debounce expires → Searching(generation, source statuses)
       ├─ source settled → update source, remain Searching
       ├─ all settled or 8s → Summarizing(generation, frozen candidates)
       │    ├─ success → Done(summary, citations)
       │    ├─ no candidates → Skipped(no_results)
       │    ├─ 20s/provider/parse error → SummaryError (hits remain)
       │    └─ cancel → Cancelled (no error toast)
       └─ cancel/new query/close/scope change → Cancelled → new Idle/Searching
```

### 6.2 Cancellation Layers

1. **Generation scope**: one parent `AbortController` per query generation; source and summary child controllers propagate parent abort using `AbortSignal.any` or parent abort listeners. If WebView support is inconsistent, use a small local helper, no package.
2. **UI stale guard**: abort is best-effort resource release, not a substitute for `generation === seqRef.current`; every `then/catch/finally` UI write checks generation.
3. **Deadlines**: one run-level `setTimeout` handles the 8-second retrieval deadline; summary has its own 20-second timer. Individual fetch timeouts must not reset these absolute deadlines. Clear timers in `finally`/cancel.
4. **Gateway hop**: cancelling Desktop fetch stops waiting and dropping the reqwest proxy future best-effort cancels upstream work; cancellation affects only this HTTP request and is not a global Runtime process signal.
5. **Runtime LLM**: handler future drop / `tokio::time::timeout` drops the current `Provider::chat` future. There is no subprocess, disk write, or shared cross-request state to roll back. Provider cancellation relies on async future cancellation; remote service stop is best-effort.
6. **Backend searches that cannot cancel**: sources are ordinary HTTP GETs. Whether an in-progress file scan/database query stops depends on the underlying implementation. This feature guarantees that Desktop promptly stops waiting and stale results cannot overwrite the current query; propagate disconnect cancellation where supported. Do not add a database cancellation token.

### 6.3 Race Rules

- On new input, increment generation synchronously, then abort the old run; abort-related catch/finally handlers can no longer write current UI state.
- If the 8-second deadline races the final source completion, the first coordinator transition wins; `Searching → Summarizing` occurs once, and the candidate snapshot freezes once.
- If cancel and success race, only a current, non-cancelled generation may commit.
- Late sources after summary dispatch cannot mutate the summary citation map; summary and map commit atomically for one generation.
- AbortError is neutral cancellation: no failure toast and no clearing of hits; clear/reset behavior for a new query is an explicit UI policy.

## 7. Security and Permissions

1. `search/summary` inherits existing agent-instance routing and Gateway permissions; callers cannot choose provider, model, endpoint, or tools.
2. Runtime validates request bytes, query, scopes, IDs, counts, and field lengths; Gateway's global body limit remains active.
3. Snippets are untrusted user data in a JSON data region; the system prompt instructs the model to ignore embedded instructions; `tools=None`.
4. LLM inputs contain no locators; response IDs are allowlisted in Runtime and checked again in the frontend. Neither layer trusts model output.
5. Render summary as plain text; do not parse HTML or arbitrary Markdown links.
6. Log counts, duration, status, and error class only; never log search content or credentials.

## 8. Error and Degradation Matrix

| Failure | Source tab | Summary | Global UI |
|---|---|---|---|
| Any source 4xx/5xx/network error | Mark error; other sources continue | May summarize returned hits and report partial/failed sources | Other tabs remain usable/openable |
| One source or overall retrieval reaches 8s | Mark timeout; retain successes | Immediately summarize frozen partial candidates | Stop waiting on that source |
| Runtime summary 503/Provider unavailable | Lists work | Show “Summary temporarily unavailable” | Continue browsing lists |
| Provider 20s timeout | Lists work | Show retry hint; do not auto-retry | Input/lists remain unlocked |
| Invalid LLM format/unauthorized citation/empty text | Lists work | Reject summary; unknown IDs cannot navigate | Never expose raw model output |
| Query cancelled/agent or workspace changed | New generation is independent | Abort without toast | Stale results never overwrite current query |

## 9. Module-Level Implementation Order and File Boundaries

1. **Runtime API first**: add request DTO, validator, prompt/parser, Provider call, and route registration in `acowork-runtime/src/http/global_search.rs` or adjacent `search_summary.rs`; test allowlist and timeout behavior with a Mock Provider.
2. **Gateway proxy**: add `POST /api/agents/{id}/search/summary` in `core/acowork-gateway/src/http/proxy.rs`, reuse existing proxy entry point, verify body/status transparency.
3. **Desktop summary client/candidate helpers**: prefer the dialog or `src/lib/global-search-api.ts` only for request and pure DTO helpers; reuse the RequestInit signal convention from `doc-api`/`pm-api`.
4. **Abort-aware Git path**: add optional signal to `useGitStore.fetchLog`, pass it to existing `httpGet` and `with503Retry`, and test abort is not retried.
5. **Coordinator wiring**: consolidate the existing split `seqRef`/`AbortController` lifecycles into per-generation coordination without rewriting per-source hit mappers/state; add source and summary states.
6. **Summary UI + dispatcher**: add status area and accessible citation buttons; share one typed dispatcher between list and summary navigation.
6.1. **Layout first**: split `Zone 3` into two equal-height rows; add `Zone 2` status bar; add collapse logic; do this before integrating the summary endpoint so the dialog skeleton is testable on its own.
6.2. **Re-enter button** + **Regenerate button** land first as pure frontend logic; hidden on empty query.

7. **Localization/tests**: add messages to all existing locales; first add small tests for pure coordinator logic and Runtime parser, then `GlobalSearchDialog.test.ts`/API route integration checks.

## 10. Verification Strategy

- Summary UI interaction (component tests):
  - Empty query → status bar and summary collapse; no spinner, no summary text.
  - Continuous typing within 300 ms debounce → only the last query triggers a run; generation increments; stale callbacks do not modify state.
  - Any source completion → corresponding tab updates immediately; status counter increments; other tabs keep their loading/empty state.
  - 8 s deadline with two sources still pending → mark them `timeout`; status shows `4/6 returned`; summary state is `partial_done`.
  - During summary generation: the top-right "Cancel summary" button is visible; clicking transitions to `cancelled`, lists and input are unchanged, the summary POST is cancelled in the network tab.
  - "Regenerate" is clickable once per generation; a second click in the same generation does not POST again and prompts "modify the query and retry".
  - Citation click: valid ID triggers the corresponding typed action; fabricated/duplicate/malformed IDs are inert; the literal `[[xxx]]` text fragments remain visible in the body (not promoted to buttons).
  - HTML tags, `<script>`, and `http://` links in the summary body are shown as plain text without any HTML/Markdown interpretation.
  - The `⟲` re-enter button on the right of the input is visible only when the query is non-empty; clicking aborts the old run, increments the generation, and keeps the prior list visible until new results arrive.
  - Summary and preview panes share height: `min-h-[120px]`, `max-h-[240px]`, total ≤ original zone height.


Use existing Vitest/Rust test facilities; do not add a test framework or fixtures. Minimum checks:

- Desktop fake adapters: immediate result, slow source, reject, AbortError, new generation, deadline, each source starts only once, late result cannot alter frozen deadline snapshot.
- Candidate builder: first five per source, total ≤30, Unicode-safe truncation (not UTF-8 byte truncation), no locator in serialized body.
- Citation renderer/dispatcher: valid ID invokes correct typed action; fabricated ID, repeated/malformed markers, and HTML characters cannot create arbitrary navigation.
- Runtime validator/parser: oversized/unknown scope/duplicate ID/unknown citation/bad JSON/empty summary rejected; valid JSON allowlisted.
- Runtime Mock Provider: success, Provider error, 20-second deadline, cancellation with no subsequent state effects; use a controllable pending future rather than waiting 20 seconds.
- Gateway route: correct Runtime path, transparent POST body, upstream status propagation; abort does not trigger retry.
- Manual local scenarios: PM/doc stopped, Runtime unavailable, no workspace, conversation not indexed, very slow Provider; lists become progressively usable, statuses are accurate, and input/tab switching/dialog close stay responsive.

## 11. Open Implementation Details

1. **Model-selection accessor**: `SharedAgentCore` and `SharedSessionManagerSlot` already exist in HTTP state; SessionManager has `current_model_and_provider()`. Trace where the active-session Provider instance is built and reuse its factory. If HTTP cannot obtain the same `(provider, model)` snapshot, add the narrowest getter; do not duplicate model selection. Do not default to a distiller model in place of the current chat model.
2. **Git hit focus**: the current dialog has no single-commit detail route between the result list and `openGitHit`; use Git-tab switching plus focused index for summary citations.
3. **8-second retrieval deadline**: fixed by ADR-093 for now; implement as one configurable constant for calibration, not hard-coded separately in each source client.

## 12. Outcome

- **Benefits**: reuse existing service search APIs and UI navigation; no Gateway aggregation; at most six fixed parallel requests; partial results stay usable; summaries/citations fail closed.
- **Costs**: up to five additional source requests and one Runtime LLM call per query; Git store gains a cancellation parameter; dialog gains coordination/source state.
- **Rollback**: disable summary coordination and restore active-tab triggering; remove Runtime/Gateway summary endpoint independently; source APIs, hit mapping, and navigation remain unchanged.
