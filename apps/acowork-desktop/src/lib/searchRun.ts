/**
 * searchRun — ADR-093 global-search run coordinator (pure logic, unit-tested).
 *
 * One "search run" fans the query out to all six sources in parallel,
 * reports each source as it lands (tabs fill progressively), and then
 * feeds a bounded candidate list to the Runtime's `POST /search/summary`
 * LLM endpoint. Cancellation is generation-scoped: a new query (or the
 * ⟲ re-run button) aborts the parent signal, which tears down every
 * in-flight child request and the summary request together.
 *
 * Design doc 27 §4.1/§4.4/§5. This module holds no React state — the
 * dialog owns state; this owns the mechanics (deadline arithmetic,
 * candidate budgeting, citation parsing) so they are testable with fake
 * adapters.
 */

/* ─── Sources ───────────────────────────────────────────────────────── */

import { with503Retry } from "./httpRetry";
import { log } from "./logger";

export type SearchSource = "file" | "git" | "doc" | "project" | "memory" | "conversation";

export const SEARCH_SOURCES: readonly SearchSource[] = [
    "file",
    "git",
    "doc",
    "project",
    "memory",
    "conversation",
] as const;

export type SourceStatus = "ok" | "error" | "timeout";

export interface SourceResult<T> {
    status: SourceStatus;
    data?: T;
}

/** Design doc 27 §5: retrieval deadline — absolute, never reset per source. */
export const SEARCH_DEADLINE_MS = 8_000;
/** Summary deadline (frontend half; the Runtime enforces its own 20 s too). */
export const SUMMARY_DEADLINE_MS = 20_000;

export interface RunSearchOptions<T> {
    /** One adapter per source; each must honor its `signal`. */
    adapters: Record<SearchSource, (signal: AbortSignal) => Promise<T>>;
    /** Generation-scoped parent: abort it to cancel the whole run. */
    parentSignal: AbortSignal;
    /** Fired as each source settles, in completion order (not tab order). */
    onSource: (source: SearchSource, result: SourceResult<T>) => void;
    deadlineMs?: number;
}

/**
 * Run all six source adapters concurrently under one absolute deadline.
 *
 * Semantics (design 27 §5):
 *  - a source that lands before the deadline reports "ok" immediately via
 *    `onSource` — the other tabs keep filling as they arrive;
 *  - at the deadline, still-pending sources are aborted and report "timeout";
 *  - if the parent is aborted first, the returned map is partial and
 *    `parentSignal.aborted` is true — the caller must discard the run.
 */
export async function runSearchSources<T>(
    opts: RunSearchOptions<T>,
): Promise<Record<SearchSource, SourceResult<T>>> {
    const { adapters, parentSignal, onSource } = opts;
    const deadlineMs = opts.deadlineMs ?? SEARCH_DEADLINE_MS;
    const results = {} as Record<SearchSource, SourceResult<T>>;

    const deadline = new AbortController();
    const timer = setTimeout(() => deadline.abort(), deadlineMs);
    const onParent = () => deadline.abort();
    parentSignal.addEventListener("abort", onParent);

    const settled = await Promise.allSettled(
        SEARCH_SOURCES.map(async (source) => {
            // Child signal = deadline ∪ parent (either aborts the fetch).
            const child = new AbortController();
            const link = (sig: AbortSignal) => {
                if (sig.aborted) child.abort();
                else sig.addEventListener("abort", () => child.abort(), { once: true });
            };
            link(deadline.signal);
            link(parentSignal);
            try {
                const data = await adapters[source](child.signal);
                results[source] = { status: "ok", data };
            } catch (e: unknown) {
                const aborted = (e as Error)?.name === "AbortError" || child.signal.aborted;
                // Parent-abort also reads as "timeout"; the caller's
                // generation check discards those results anyway.
                results[source] = aborted ? { status: "timeout" } : { status: "error" };
            }
            // Progressive reporting — late arrivals after a parent abort are
            // dropped by the caller's generation check, not here.
            onSource(source, results[source]);
        }),
    );
    // allSettled never rejects given the try/catch above; the loop keeps
    // TypeScript honest about unhandled rejections from adapters that throw
    // synchronously before the promise chain.
    void settled;

    clearTimeout(timer);
    parentSignal.removeEventListener("abort", onParent);
    return results;
}

/* ─── Candidates (bounded prompt payload, design 27 §4.4) ───────────── */

/** Runtime contract (search_summary.rs): ≤30 candidates, id shape
 *  `[A-Za-z0-9_-]{1,32}`, title ≤200, snippet ≤500. */
export const MAX_CANDIDATES_TOTAL = 30;
export const MAX_CANDIDATES_PER_SOURCE = 5;
const MAX_TITLE_CHARS = 200;
const MAX_SNIPPET_CHARS = 500;

export interface SummaryCandidate {
    scope: SearchSource;
    /** Whitelisted citation id assigned here (`r1`..`r30`) — NOT the hit's
     *  own id, which may contain `/` `:` `.` (file paths, session ids). */
    id: string;
    title: string;
    snippet: string;
}

export interface CandidateInput {
    title: string;
    snippet: string;
}

export interface CandidateSet {
    candidates: SummaryCandidate[];
    /** Citation id → the hit it stands for, for the click dispatcher. */
    byId: Record<string, SearchSource>;
    /** Per-source ordinal of each candidate, so a click can map back to
     *  the row index in that source's hit list (rN order is deterministic). */
    indexOf: Record<string, number>;
}

/**
 * Budget the six hit lists into the summary candidate payload.
 * Per source: first `MAX_CANDIDATES_PER_SOURCE` rows; total capped at
 * `MAX_CANDIDATES_TOTAL` in fixed source order (file, git, doc, project,
 * memory, conversation) so the ids are stable for one run.
 */
export function buildSummaryCandidates(
    hitsBySource: Record<SearchSource, CandidateInput[]>,
): CandidateSet {
    const candidates: SummaryCandidate[] = [];
    const byId: Record<string, SearchSource> = {};
    const indexOf: Record<string, number> = {};
    for (const source of SEARCH_SOURCES) {
        const hits = hitsBySource[source] ?? [];
        for (let i = 0; i < Math.min(hits.length, MAX_CANDIDATES_PER_SOURCE); i++) {
            if (candidates.length >= MAX_CANDIDATES_TOTAL) break;
            const id = `r${candidates.length + 1}`;
            candidates.push({
                scope: source,
                id,
                title: truncate(hits[i].title, MAX_TITLE_CHARS),
                snippet: truncate(hits[i].snippet, MAX_SNIPPET_CHARS),
            });
            byId[id] = source;
            indexOf[id] = i;
        }
    }
    return { candidates, byId, indexOf };
}

function truncate(s: string, max: number): string {
    // Unicode-safe: slice by code points, not UTF-16 units.
    const cps = Array.from(s);
    return cps.length <= max ? s : cps.slice(0, max - 1).join("") + "…";
}

/* ─── Summary request ───────────────────────────────────────────────── */

export interface SummaryResponse {
    summary: string;
    citations: string[];
}

/**
 * `POST /api/agents/{agentId}/search/summary` (Gateway transparent proxy →
 * Runtime `POST /search/summary`). 503-retried via the shared helper
 * (Gateway boot window); an abort is never retried (with503Retry throws
 * AbortError straight through). Scope status lists are split by kind —
 * timed-out vs failed — so the model is told what actually happened
 * (Runtime contract: search_summary.rs `SearchSummaryRequest`).
 */
export async function requestSearchSummary(params: {
    baseUrl: string;
    agentId: string;
    query: string;
    partial: boolean;
    timedOutScopes: SearchSource[];
    failedScopes: SearchSource[];
    candidates: SummaryCandidate[];
    signal: AbortSignal;
}): Promise<SummaryResponse> {
    const url = `${params.baseUrl}/api/agents/${params.agentId}/search/summary`;
    const body = JSON.stringify({
        query: params.query,
        partial: params.partial,
        timed_out_scopes: params.timedOutScopes,
        failed_scopes: params.failedScopes,
        candidates: params.candidates,
    });
    const resp = await with503Retry(
        (sig) =>
            fetch(url, {
                method: "POST",
                headers: { "content-type": "application/json" },
                body,
                signal: sig,
            }),
        { tag: "searchRun.summary", logger: log, signal: params.signal },
    );
    if (!resp.ok) {
        let detail = "";
        try {
            detail = (await resp.text()).slice(0, 200);
        } catch {
            /* body already consumed or unreadable */
        }
        throw new Error(`summary request failed: status ${resp.status} ${detail}`);
    }
    return (await resp.json()) as SummaryResponse;
}

/* ─── Citation parsing ──────────────────────────────────────────────── */

export type SummarySegment =
    | { kind: "text"; text: string }
    | { kind: "citation"; id: string };

/** `[[id]]` citation syntax, same shape the Runtime validator accepts. */
const CITATION_RE = /\[\[([A-Za-z0-9_-]{1,32})\]\]/g;

/**
 * Split the LLM summary into plain-text and citation segments. Ids not in
 * the whitelist are demoted to literal text (defense in depth — the
 * Runtime already demotes unknown ids, but the frontend whitelist is the
 * one that knows the current run's candidate set).
 */
export function parseSummarySegments(summary: string, allowed: ReadonlySet<string>): SummarySegment[] {
    const segments: SummarySegment[] = [];
    let last = 0;
    for (const m of summary.matchAll(CITATION_RE)) {
        const id = m[1];
        if (!allowed.has(id)) continue;
        if (m.index! > last) segments.push({ kind: "text", text: summary.slice(last, m.index) });
        segments.push({ kind: "citation", id });
        last = m.index! + m[0].length;
    }
    if (last < summary.length) segments.push({ kind: "text", text: summary.slice(last) });
    return segments;
}
