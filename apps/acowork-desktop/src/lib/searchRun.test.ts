/**
 * ADR-093 searchRun coordinator tests — fake adapters, no network, no React.
 * Run: npm test (vitest).
 */
import { describe, it, expect } from "vitest";
import {
    runSearchSources,
    selectSummaryCandidates,
    parseSummarySegments,
    SEARCH_SOURCES,
    type CandidateInput,
    type SearchSource,
    type SourceResult,
} from "./searchRun";

function emptyAdapters(
    impl: (source: SearchSource, signal: AbortSignal) => Promise<unknown>,
): Record<SearchSource, (signal: AbortSignal) => Promise<unknown>> {
    return Object.fromEntries(
        SEARCH_SOURCES.map((s) => [s, (signal: AbortSignal) => impl(s, signal)]),
    ) as Record<SearchSource, (signal: AbortSignal) => Promise<unknown>>;
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

describe("runSearchSources", () => {
    it("reports every source ok and fires onSource per arrival", async () => {
        const seen: [SearchSource, SourceResult<unknown>][] = [];
        const parent = new AbortController();
        const results = await runSearchSources({
            adapters: emptyAdapters(async () => "hits"),
            parentSignal: parent.signal,
            onSource: (s, r) => seen.push([s, r]),
            deadlineMs: 1000,
        });
        expect(seen).toHaveLength(6);
        for (const s of SEARCH_SOURCES) {
            expect(results[s]).toEqual({ status: "ok", data: "hits" });
        }
    });

    it("marks sources still pending at the deadline as timeout", async () => {
        const parent = new AbortController();
        const results = await runSearchSources({
            adapters: emptyAdapters(async (source, signal) => {
                if (source === "doc") {
                    // Never resolves unless aborted — the deadline must kill it.
                    await new Promise((_, rej) => {
                        signal.addEventListener("abort", () => rej(new DOMException("AbortError", "AbortError")));
                    });
                }
                return "fast";
            }),
            parentSignal: parent.signal,
            onSource: () => {},
            deadlineMs: 30,
        });
        expect(results.doc.status).toBe("timeout");
        expect(results.file.status).toBe("ok");
    });

    it("classifies a non-abort failure as error", async () => {
        const parent = new AbortController();
        const results = await runSearchSources({
            adapters: emptyAdapters(async (source) => {
                if (source === "project") throw new Error("pm down");
                return "ok";
            }),
            parentSignal: parent.signal,
            onSource: () => {},
            deadlineMs: 1000,
        });
        expect(results.project.status).toBe("error");
    });

    it("parent abort tears down in-flight adapters", async () => {
        const parent = new AbortController();
        let sawAbort = false;
        const p = runSearchSources({
            adapters: emptyAdapters(async (_source, signal) => {
                await new Promise((resolve, reject) => {
                    const t = setTimeout(resolve, 5000);
                    signal.addEventListener("abort", () => {
                        clearTimeout(t);
                        sawAbort = true;
                        reject(new DOMException("AbortError", "AbortError"));
                    });
                });
                return "x";
            }),
            parentSignal: parent.signal,
            onSource: () => {},
            deadlineMs: 5000,
        });
        await sleep(10);
        parent.abort();
        await p;
        expect(sawAbort).toBe(true);
    });
});

describe("selectSummaryCandidates", () => {
    const rows = (n: number, prefix: string): CandidateInput[] =>
        Array.from({ length: n }, (_, i) => ({ title: `${prefix}-${i}`, snippet: `s${i}` }));

    const fused = (
        n: number,
        prefix: string,
        evidence: (i: number) => Partial<CandidateInput>,
    ): CandidateInput[] =>
        Array.from({ length: n }, (_, i) => ({
            title: `${prefix}-${i}`,
            snippet: `s${i}`,
            ...evidence(i),
        }));

    it("caps per source at 5 and numbers ids r1.. in fixed source order", () => {
        const hits = {
            file: rows(10, "f"),
            git: rows(3, "g"),
            doc: [],
            project: rows(2, "p"),
            memory: rows(5, "m"),
            conversation: rows(1, "c"),
        } as Record<SearchSource, CandidateInput[]>;
        const set = selectSummaryCandidates(hits);
        expect(set.candidates.map((c) => c.id)).toEqual([
            "r1", "r2", "r3", "r4", "r5", // file (capped at 5)
            "r6", "r7", "r8", // git
            "r9", "r10", // project
            "r11", "r12", "r13", "r14", "r15", // memory
            "r16", // conversation
        ]);
        expect(set.byId.r16).toBe("conversation");
        expect(set.indexOf.r16).toBe(0);
        expect(set.indexOf.r5).toBe(4);
    });

    it("caps the total at 30 candidates", () => {
        const hits = Object.fromEntries(
            SEARCH_SOURCES.map((s) => [s, rows(10, s)]),
        ) as Record<SearchSource, CandidateInput[]>;
        const set = selectSummaryCandidates(hits);
        expect(set.candidates).toHaveLength(30);
    });

    it("truncates overlong titles/snippets by code points", () => {
        const hits = {
            file: [{ title: "字".repeat(300), snippet: "x".repeat(900) }],
            git: [], doc: [], project: [], memory: [], conversation: [],
        } as Record<SearchSource, CandidateInput[]>;
        const [c] = selectSummaryCandidates(hits).candidates;
        expect(Array.from(c.title).length).toBeLessThanOrEqual(200);
        expect(Array.from(c.snippet).length).toBeLessThanOrEqual(500);
        expect(c.title.endsWith("…")).toBe(true);
    });

    it("prefers both-source evidence inside a source budget", () => {
        // Server order puts the vector-only hit first; selection must still
        // promote the hit two independent signals agreed on.
        const hits = {
            file: [], git: [], doc: [], project: [],
            conversation: [],
            memory: fused(3, "m", (i) =>
                i === 0
                    ? { matchedBy: "vector", fusedRank: 0 }
                    : { matchedBy: "both", fusedRank: i },
            ),
        } as Record<SearchSource, CandidateInput[]>;
        const set = selectSummaryCandidates(hits);
        expect(set.candidates.map((c) => c.title)).toEqual(["m-1", "m-2", "m-0"]);
        // ...and each citation still points at its own rendered row.
        expect(set.indexOf).toEqual({ r1: 1, r2: 2, r3: 0 });
    });

    it("orders vector-only matches after text and both", () => {
        const hits = {
            file: [], git: [], doc: [], project: [], memory: [],
            conversation: fused(3, "c", (i) => ({
                matchedBy: (["vector", "text", "both"] as const)[i],
                fusedRank: i,
            })),
        } as Record<SearchSource, CandidateInput[]>;
        const set = selectSummaryCandidates(hits);
        expect(set.candidates.map((c) => c.title)).toEqual(["c-2", "c-1", "c-0"]);
    });

    it("treats missing evidence as unknown and never demotes it", () => {
        // A provider that cannot report matched_by must not lose its slot to a
        // vector-only match: an observability gap is not a relevance signal.
        const hits = {
            file: [], git: [], doc: [], project: [], memory: [],
            conversation: fused(2, "c", (i) =>
                i === 0 ? {} : { matchedBy: "vector" },
            ),
        } as Record<SearchSource, CandidateInput[]>;
        const set = selectSummaryCandidates(hits);
        expect(set.candidates[0].title).toBe("c-0");
        expect(set.candidates[0].matchedBy).toBeUndefined();
    });

    it("caps candidates per dedup key so one session cannot monopolize", () => {
        const hits = {
            file: [], git: [], doc: [], project: [], memory: [],
            conversation: fused(6, "c", (i) => ({
                dedupKey: i % 2 === 0 ? "session-A" : "session-B",
                fusedRank: i,
            })),
        } as Record<SearchSource, CandidateInput[]>;
        const set = selectSummaryCandidates(hits);
        expect(set.candidates).toHaveLength(4);
        expect(set.candidates.map((c) => c.title)).toEqual(["c-0", "c-1", "c-2", "c-3"]);
    });

    it("forwards matchedBy only when the source reported it", () => {
        const hits = {
            file: [{ title: "f", snippet: "s" }],
            git: [], doc: [], project: [], memory: [],
            conversation: [{ title: "c", snippet: "s", matchedBy: "both" }],
        } as Record<SearchSource, CandidateInput[]>;
        const set = selectSummaryCandidates(hits);
        expect("matchedBy" in set.candidates[0]).toBe(false);
        expect(set.candidates[1].matchedBy).toBe("both");
    });

    it("submits fewer than the budget when sources are thin (ceiling, not target)", () => {
        const hits = {
            file: rows(1, "f"), git: [], doc: [], project: [],
            memory: rows(2, "m"), conversation: [],
        } as Record<SearchSource, CandidateInput[]>;
        const set = selectSummaryCandidates(hits);
        expect(set.candidates).toHaveLength(3);
    });
});

describe("parseSummarySegments", () => {
    it("splits whitelisted citations and demotes unknown ids to text", () => {
        const allowed = new Set(["r1", "r3"]);
        const segs = parseSummarySegments("见 [[r1]] 与 [[r9]] 及 [[r3]]。", allowed);
        expect(segs).toEqual([
            { kind: "text", text: "见 " },
            { kind: "citation", id: "r1" },
            { kind: "text", text: " 与 [[r9]] 及 " },
            { kind: "citation", id: "r3" },
            { kind: "text", text: "。" },
        ]);
    });

    it("keeps malformed syntax as plain text", () => {
        const segs = parseSummarySegments("[[bad id]] [[r1]]", new Set(["r1"]));
        expect(segs[0]).toEqual({ kind: "text", text: "[[bad id]] " });
        expect(segs[1]).toEqual({ kind: "citation", id: "r1" });
    });
});
