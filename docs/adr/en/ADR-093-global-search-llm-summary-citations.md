# ADR-093: LLM Synthesis and Clickable Citations for Global Search

- **Number**: ADR-093
- **Status**: Draft (pending review)
- **Date**: 2026-10-14
- **Decision makers**: ACowork Architecture Group
- **Related**: ADR-009 (Gateway / Runtime workspace boundary), ADR-064/070 (zero business logic in Gateway), ADR-081 (six-source global search), ADR-082 (memory and conversation storage)

## Summary

Global search needs more than six independent result lists: users need a cross-source summary that quickly answers what was found, with citations that open the corresponding hits. Today, the Desktop mostly searches only the active tab and has no cross-source summary.

This ADR decides that the Desktop will query all six applicable sources in parallel when the query changes, keep each tab's list and existing ranking independent, and update lists and progress as each source responds. The Desktop will submit bounded hit summaries to the current agent's Runtime, which uses the existing Provider to generate a cross-source summary. The summary will cite hits by restricted result IDs; the Desktop will render only IDs validated against the current result set as clickable controls and map them to existing typed open/locate actions. Summary failures do not affect search lists; slow retrieval or LLM calls must not block the UI.

## 1. Context and Problem

ADR-081 established six sources: files, docs, projects, conversations, memory, and Git. The existing [GlobalSearchDialog.tsx](../../../apps/acowork-desktop/src/components/search/GlobalSearchDialog.tsx) mostly searches only the active tab, so one query does not currently cover all sources. Runtime's [global_search.rs](../../../core/acowork-runtime/src/http/global_search.rs) aggregates memory, conversations, files, and Git, while docs and projects are served by separate processes. Existing scores are not comparable across sources.

Existing result rows already have typed open/locate actions: open a file at its matching line, open a document, open a project/task, locate a conversation message, and load a memory node. Git currently opens the Git log view. The model must not generate executable URLs, and its output must not be treated as UI markup.

## 2. Decisions

### D1 — Parallel six-source retrieval; summary is an additional view, not a seventh ranking

When the query changes, the Desktop will query the applicable file, doc, project, conversation, memory, and Git sources in parallel for the current agent/workspace. Sources unavailable due to missing agent, workspace, or index capability will be skipped with an explanation under existing semantics. Each tab's result list updates independently and retains its existing ranking and interactions; **there is no cross-tab ranking and no change to source-local ranking**.

Data ownership and identity remain unchanged: docs/projects continue using their existing Gateway reverse-proxy APIs; files/Git/conversations/memory continue using the owning Runtime APIs. The Gateway may add only a transparent reverse proxy where needed; it owns no aggregation, summarization, or ranking logic.

### D2 — Runtime invokes the existing LLM Provider for the summary

When all source requests finish, or the retrieval deadline is reached, the Desktop submits the query and bounded returned candidates to the current agent's Runtime summary endpoint. A candidate contains only the ID, scope, title, and truncated snippet for this request; full files, docs, and conversations are not uploaded, and locators/payloads are not sent to the model. Runtime reuses the existing Provider `chat()` capability; no new model SDK or Gateway LLM logic is introduced.

Initial limits: at most 20 hits per source for UI; at most 5 hits per source, 500 Unicode characters per snippet, and 30 total candidates for the summary. Extra hits remain available in tab lists but are not sent to the summary. Model output is capped at 800 tokens. If the input still exceeds the Provider's acceptable context after truncation, the summary request fails and degrades as in D5; the query itself is not truncated.

Candidate content is untrusted data: the prompt must explicitly instruct the model to ignore instructions embedded in hits and perform only summarization and citation. The model cannot modify search data, invoke tools, or produce navigation URLs.

### D3 — Summary citations are validated IDs, not links

The Desktop assigns a per-query unique ID (for example, `r1`) to each summary candidate and retains an `ID → original typed hit` mapping. Runtime asks the model to return constrained JSON, with references in the summary body written as `[[r1]]`. The UI parses the body as plain text and renders only markers whose IDs belong to the submitted candidate set as accessible button/link-styled controls. Unknown, malformed, or repeated IDs must not navigate and must never be rendered as HTML.

On click, the Desktop resolves the hit via its local mapping and invokes the existing typed action rather than opening a model-provided URL. Files/docs/projects/tasks/conversations/memory use their existing target actions. Git uses the existing Git log action; if the current implementation cannot locate a specific commit, the citation only switches to the Git tab and focuses the matching hit, without claiming that a nonexistent commit-detail view is opened.

### D4 — Visible progress, progressively available results, non-blocking interaction

The dialog adds a summary area separate from the lists; the summary does not replace tab lists. During a query:

- Show per-source states: waiting / searching / completed (count) / no results / skipped / failed / timed out. Update each tab immediately as its source returns, without waiting for other sources.
- Show aggregate source progress (for example, “3/6 sources completed”); slow sources have a deadline and cannot block indefinitely.
- Once sources return or the deadline fires, show an indeterminate “Generating summary” state. Lists remain switchable, scrollable, and actionable while the LLM is running.
- On new query, dialog close, or agent/workspace change, cancel old requests and discard late responses; stale summaries must not replace the current query state.
- Use asynchronous requests; never perform retrieval, parsing, or LLM work on the render/event thread.

Deadlines: wait at most **8 seconds** from dispatch for six-source collection; then start the summary with the results already returned, mark unfinished sources timed out, and cancel their requests. Wait at most **20 seconds** for the LLM summary. Empty results do not invoke the LLM. These deadlines may be revised after implementation based on real network/model timings; progress must not imply a precise percentage.

### D5 — Failure isolation and partial results

A source failure/timeout does not block other sources or the summary. The summary uses only successfully returned candidates, and the UI indicates its source coverage and any timeouts; if there are no candidates, show a no-hits state. If the LLM is unavailable, times out, returns invalid format, or returns an empty body, show a brief recoverable message and retain all returned lists; never present raw model output as the summary. Citation IDs must pass the D3 allowlist check.

### D6 — One summary request per query

Generate at most one summary per valid query; tab switches do not trigger duplicate LLM calls. Cancel the previous generation when the query changes. MVP excludes token streaming, persisted search history, cross-agent search, and model reranking; an indeterminate progress state is sufficient during the LLM phase.

## 3. Explicitly Out of Scope

- Merging or reranking hits across tabs; changing source-specific score semantics.
- Letting the LLM generate or execute URLs, HTML, code, tool calls, or search conditions.
- Sending full private files, docs, or conversations to Runtime; only bounded titles/snippets are submitted.
- Moving ownership of doc/project data into Runtime; the Gateway remains a reverse proxy only.
- Hiding or clearing retrieved results when summarization fails.

## 4. Alternatives

| Option | Decision | Reason |
|---|---|---|
| Search only the active tab, then summarize | Rejected | Cannot meet the requirement that the summary cover all tabs |
| Put six-source retrieval/aggregation in the Gateway | Rejected | Violates ADR-064/070's zero-business-logic boundary; Gateway does not own agent-private data |
| Let the model return arbitrary URLs or HTML links | Rejected | Links can be fabricated or incorrect, and untrusted model output would become executable UI content |
| Have the LLM rerank one mixed cross-tab list | Rejected | The user wants independent source lists; scope scores and semantics objectively differ |
| Wait for all sources and LLM before showing results | Rejected | Slow sources or models would block the primary interaction and prevent progressive feedback |

## 5. Consequences and Risks

- Each query triggers more simultaneous source requests; reuse debounce/cancellation and per-source/summary input limits to avoid resource amplification from fast typing.
- Sources span multiple services, so summaries after a deadline are partial, not a completeness guarantee; the UI must show coverage/timeouts.
- The LLM may return nonexistent citation IDs; frontend allowlist validation ensures invalid references cannot navigate.
- Git's current open action may not precisely locate a commit; MVP degrades to focusing the hit in the Git tab as described in D3.
- A summary is an inference result, not search truth or persisted data; original search hits remain the source for navigation.

## 6. Acceptance Criteria

1. A query starts all applicable sources in parallel; each source becomes browseable in its tab as soon as it returns.
2. Summary citations for hits from multiple sources are clickable and open/locate the mapped hit; fabricated IDs are not clickable.
3. Each tab retains its existing source ranking, and switching tabs does not trigger another summary LLM call.
4. Source failures do not stop other sources; after 8 seconds, summarization can start with returned hits.
5. The LLM wait is capped at 20 seconds; the UI remains interactive throughout, and failures leave lists usable with a clear status.
6. After query/agent/workspace changes or dialog close, stale responses cannot replace current state.
7. The LLM receives only bounded candidate summaries, not locators/payloads; the Gateway only reverse-proxies.

## 7. Rollback

The Desktop can hide the summary area and restore active-tab search triggering without affecting existing source search APIs. The Runtime summary endpoint and Gateway reverse proxy can be removed independently; no data migration or persistent format change is involved.
