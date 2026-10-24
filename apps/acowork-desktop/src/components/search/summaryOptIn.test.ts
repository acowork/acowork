/**
 * summaryOptIn.test.ts — the LLM summary must never be called
 * automatically.
 *
 * Background: summarizing spends tokens, so ADR-093's opt-in flow lands
 * every search on `phase: "done"` and waits for a click on the summary
 * area's trigger. The earlier implementation had `runSearch` await
 * `runSummaryStage` unconditionally whenever the (default-on) toggle was
 * set, which meant every keystroke-submitted search silently burned a
 * model call.
 *
 * Why this is a source-text test rather than a render test. The property
 * is a call-graph fact — "the retrieval function must not invoke the
 * summary function" — and SummaryView.test.tsx renders the presentational
 * half in isolation, with no access to `runSearch`. Mounting the whole
 * dialog would require stubbing ten stores (agents, git, chat, workspace,
 * docs, pm, memory, settings, search) purely to observe that a promise is
 * never awaited; that harness would be far more fragile than the
 * assertion itself, and would break on unrelated store edits.
 *
 * So: read the module source and assert the call graph directly. This is
 * narrow on purpose — it pins one call, not the file's shape.
 */

import { readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";

const SRC = readFileSync(join(__dirname, "GlobalSearchDialog.tsx"), "utf8");

/** Body of `runSearch`, i.e. the retrieval stage that must never reach
 *  for the summary stage. */
function runSearchBody(): string {
    const start = SRC.indexOf("const runSearch = useCallback(");
    expect(start, "runSearch not found — did the search stage move?").toBeGreaterThan(-1);
    // The callback ends at its dependency array — the first bare `[…],`
    // line followed by the closing `);`. Located structurally rather than
    // by the exact member list, because that list is edited by unrelated
    // work (ADR-100 appended `memoryServiceReady`) and a literal match
    // here would break on those edits without protecting anything.
    const rel = SRC.slice(start);
    const end = rel.match(/\n\s*\[[^\]]*\],\n\s*\);/);
    expect(end, "runSearch dependency array not found").not.toBeNull();
    return rel.slice(0, end!.index);
}

describe("retrieval stage never triggers the summary stage", () => {
    // The regression this whole flow exists to prevent.
    it("runSearch does not call runSummaryStage", () => {
        expect(runSearchBody()).not.toContain("runSummaryStage");
    });

    // Landing on "done" is what leaves the pane waiting for a click.
    // If the run instead advanced to "summarizing", the trigger would
    // render as "Cancel" and the LLM call would begin unprompted.
    it("runSearch parks the phase on done", () => {
        const body = runSearchBody();
        expect(body).toContain('setPhase("done")');
        expect(body).not.toContain('setPhase("summarizing")');
    });

    // The frozen snapshot is what makes the later click safe: regenerate
    // must reuse the candidates this run produced, never re-issue
    // retrieval. Freezing it is therefore mandatory in the same place the
    // old automatic call used to live.
    it("freezes the candidate snapshot for the later click", () => {
        const body = runSearchBody();
        // Matched on the call shape rather than the function name: the
        // candidate selector has been renamed as the retrieval work
        // evolved, and that rename must not be able to silently gut this
        // assertion.
        expect(body).toMatch(/\w+SummaryCandidates\(rowsRef\.current\)/);
        expect(body).toContain("lastSummaryReqRef.current");
    });
});

describe("the opt-in trigger lives in the summary area only", () => {
    // The input-box Sparkles toggle implied a default-on cost and had to
    // go; the affordance now belongs next to the text it produces.
    it("no summary toggle remains in the input box", () => {
        expect(SRC).not.toContain("summary-toggle");
        expect(SRC).not.toContain("toggleSummary");
    });

    // `summaryEnabled` guarded the old default-on path. With it gone the
    // only thing that decides whether to call the model is a click.
    it("no summaryEnabled state survives", () => {
        expect(SRC).not.toContain("summaryEnabled");
    });
});
