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
    /** How retrieval matched this hit (ADR-095 D4). Absent means the source
     *  did not report it — treated as **unknown**, never as "text". The
     *  Runtime validates this against an enum whitelist; an unrecognized
     *  value is dropped rather than rendered. */
    matchedBy?: MatchedBy;
}

/** Wire values of `acowork_core::retrieval::MatchedBy`. */
export type MatchedBy = "vector" | "text" | "both";

export interface CandidateInput {
    title: string;
    snippet: string;
    /** Retrieval evidence from the `/search` payload (ADR-095 D4). Absent on
     *  file/git/doc/project, which have no fusion engine. */
    matchedBy?: MatchedBy;
    /** In-source fused rank, 0-based. The list is already in this order. */
    fusedRank?: number;
    /** Dedup key: `session_id` (conversation) / `node_id` (memory). */
    dedupKey?: string;
}

export interface CandidateSet {
    candidates: SummaryCandidate[];
    /** Citation id → the hit it stands for, for the click dispatcher. */
    byId: Record<string, SearchSource>;
    /** Per-source ordinal of each candidate, so a click can map back to
     *  the row index in that source's hit list (rN order is deterministic). */
    indexOf: Record<string, number>;
}

/** Sources with no fusion engine: they keep their own order and a flat
 *  per-source budget (ADR-095 D5 — "evidence from a source that cannot tell
 *  how it matched is not demoted, it is just not promoted"). */
const FLAT_SOURCES: readonly SearchSource[] = ["file", "git", "doc", "project"];
/** Sources backed by the hybrid engine, so they carry retrieval evidence. */
const FUSED_SOURCES: readonly SearchSource[] = ["memory", "conversation"];

/** At most this many candidates from one conversation, so a single verbose
 *  session cannot monopolize the summary (session diversity, design 28 §4.4). */
export const MAX_CANDIDATES_PER_DEDUP_KEY = 2;

/**
 * Budget the six hit lists into the summary candidate payload.
 *
 * Deterministic and threshold-free by design (ADR-095 D5): the ordering keys
 * are the *in-source rank* the engine already computed and the evidence
 * category, never an uncalibrated score. A fused score is not comparable
 * across sources, so nothing here sorts by `score`.
 *
 * Rules:
 * - file/git/doc/project: unchanged — source order, first
 *   `MAX_CANDIDATES_PER_SOURCE`.
 * - memory/conversation: server order (`fusedRank`), but within the per-source
 *   budget a hit matched by **both** sources takes a slot ahead of a
 *   single-source one, and a vector-only hit goes last. Rationale: two
 *   independent signals agreeing is the only cross-source-comparable evidence
 *   we have; vector-only matches are where an embedding miss shows up.
 * - conversation/memory additionally cap at
 *   `MAX_CANDIDATES_PER_DEDUP_KEY` rows per dedup key.
 *
 * The per-source budget is a **ceiling, not a target**: if a source has fewer
 * eligible rows, fewer are submitted, and an empty result means the caller
 * must not call `/search/summary` at all.
 */
export function selectSummaryCandidates(
    hitsBySource: Record<SearchSource, CandidateInput[]>,
): CandidateSet {
    const candidates: SummaryCandidate[] = [];
    const byId: Record<string, SearchSource> = {};
    const indexOf: Record<string, number> = {};

    const push = (source: SearchSource, rowIndex: number, input: CandidateInput) => {
        const id = `r${candidates.length + 1}`;
        const c: SummaryCandidate = {
            scope: source,
            id,
            title: truncate(input.title, MAX_TITLE_CHARS),
            snippet: truncate(input.snippet, MAX_SNIPPET_CHARS),
        };
        // Only forward evidence that exists. Sending `matchedBy: undefined`
        // would serialize as absent anyway, but omitting the key explicitly
        // keeps the request byte-stable for tests.
        if (input.matchedBy) c.matchedBy = input.matchedBy;
        candidates.push(c);
        byId[id] = source;
        // Click-to-row needs the **list row index**, and the list is rendered
        // in server order — so the row is the original index, not the
        // selection order (design 28 §4.4).
        indexOf[id] = rowIndex;
    };

    for (const source of SEARCH_SOURCES) {
        if (candidates.length >= MAX_CANDIDATES_TOTAL) break;
        const hits = hitsBySource[source] ?? [];

        if (FLAT_SOURCES.includes(source)) {
            for (let i = 0; i < Math.min(hits.length, MAX_CANDIDATES_PER_SOURCE); i++) {
                if (candidates.length >= MAX_CANDIDATES_TOTAL) break;
                push(source, i, hits[i]);
            }
            continue;
        }

        if (!FUSED_SOURCES.includes(source)) continue;

        // Selection order: evidence class first, then the engine's own rank.
        // `Array.sort` is stable, so ties keep server order without a
        // secondary comparator inventing a tie-break the engine never had.
        const ranked = hits
            .map((h, i) => ({ h, i }))
            .sort((a, b) => evidenceClass(a.h.matchedBy) - evidenceClass(b.h.matchedBy));

        const perKey = new Map<string, number>();
        let taken = 0;
        for (const { h, i } of ranked) {
            if (taken >= MAX_CANDIDATES_PER_SOURCE) break;
            if (candidates.length >= MAX_CANDIDATES_TOTAL) break;
            if (h.dedupKey) {
                const seen = perKey.get(h.dedupKey) ?? 0;
                if (seen >= MAX_CANDIDATES_PER_DEDUP_KEY) continue;
                perKey.set(h.dedupKey, seen + 1);
            }
            push(source, i, h);
            taken++;
        }
    }

    return { candidates, byId, indexOf };
}

/**
 * Selection priority inside a fused source's budget. Lower runs first.
 *
 * `unknown` (absent `matchedBy`) is deliberately grouped with `text`: a source
 * that cannot report how it matched must not be penalized into never being
 * summarized — that would turn an observability gap into a quality regression.
 */
function evidenceClass(matchedBy?: MatchedBy): number {
    if (matchedBy === "both") return 0;
    if (matchedBy === "text") return 1;
    if (matchedBy === undefined) return 1;
    return 2; // "vector" only
}

function truncate(s: string, max: number): string {
    // Unicode-safe: slice by code points, not UTF-16 units.
    const cps = Array.from(s);
    return cps.length <= max ? s : cps.slice(0, max - 1).join("") + "…";
}

/* ─── Summary request ───────────────────────────────────────────────── */

/**
 * Mirrors the `UsageInfo` block the Runtime returns on
 * `POST /search/summary` (acowork-core `UsageInfo`). Kept as a
 * structural subtype here — the Desktop footer only needs prompt /
 * completion / total / cache_read; everything else is cost-accounting
 * detail not surfaced to the user.
 *
 * All fields are optional because the provider may omit usage (dev
 * stubs, certain free-tier APIs); the UI hides the token row when the
 * whole object is missing.
 */
export interface SummaryUsage {
    prompt_tokens?: number;
    completion_tokens?: number;
    total_tokens?: number;
    cache_read_tokens?: number;
}

export interface SummaryResponse {
    summary: string;
    citations: string[];
    /** Resolved model id actually used for the summary call. Empty
     *  string when the Runtime could not resolve one — the footer
     *  hides itself in that case. */
    model?: string;
    usage?: SummaryUsage;
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
        // camelCase in TS, snake_case on the wire (same convention as
        // timed_out_scopes above). `matchedBy` would deserialize as absent on
        // the Runtime side, silently dropping the evidence the whole point of
        // ADR-095 D4 is to surface.
        candidates: params.candidates.map((c) => ({
            id: c.id,
            scope: c.scope,
            title: c.title,
            snippet: c.snippet,
            ...(c.matchedBy ? { matched_by: c.matchedBy } : {}),
        })),
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
