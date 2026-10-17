/**
 * SummaryView.test.tsx — the summary call is opt-in, and its trigger never
 * floats over the summary body.
 *
 * Two independent guarantees are asserted here.
 *
 * 1. Cost safety. Summarizing spends LLM tokens, so the dialog lands every
 *    search on `phase: "done"` without calling the model. This pane is the
 *    only thing that can start that call, and it must be inert while the
 *    six-source fan-out is still running — the candidate snapshot is
 *    frozen only when retrieval settles, so a click mid-flight would
 *    summarize an incomplete set. A regression that re-enables the
 *    automatic call is caught by the dialog's own gate, not here; what is
 *    caught here is a trigger that fires early or offers no way to stop.
 *
 * 2. Placement. The trigger lives in the footer's right slot. It used to be
 *    an `absolute right-2` corner button with no `top`, which — per the
 *    flexbox static-position rule — resolved to the pane's TOP-right and
 *    painted over the scrolling summary text and the scrollbar's hit area.
 *    design 27 §4.6.5 requires the bottom-right.
 *
 * jsdom has no layout engine, so placement is asserted structurally: the
 * trigger must be a normal-flow descendant of the footer, and the pane must
 * contain no absolutely-positioned node at all.
 *
 * Run: `npx vitest run src/components/search/SummaryView.test.tsx`
 */

import { act, render, screen, fireEvent } from "@testing-library/react";
import { describe, expect, it, vi, beforeEach } from "vitest";
import { SummaryView, type SummaryPhase } from "./GlobalSearchDialog";

/** Captured so the copy test can assert what actually reached the
 *  clipboard; CopyButton's own fallback path is covered in its test file. */
const writeText = vi.fn();

beforeEach(() => {
    writeText.mockReset();
    writeText.mockResolvedValue(undefined);
    Object.assign(navigator, { clipboard: { writeText } });
});

// Same asset mocks as GlobalSearchDialog.test.ts — the module pulls in
// presentational icon modules that jsdom cannot load meaningfully.
vi.mock("../common/SetiIcon", () => ({ SetiIcon: () => null }));
vi.mock("../workspace/FileTree/fileIcons", () => ({ getFileIcon: () => null }));

const BASE = {
    summary: "",
    error: null as string | null,
    sourcesDone: 0,
    summaryMeta: null,
    canSummarize: true,
    allowedIds: new Set<string>(),
    onSummarize: () => {},
    onCancel: () => {},
    onCitation: () => {},
};

function renderView(phase: SummaryPhase, props: Partial<typeof BASE> = {}) {
    return render(<SummaryView phase={phase} {...BASE} {...props} />);
}

/** Every element in the summary pane that opts out of normal flow. */
function floatingNodes(container: HTMLElement): Element[] {
    return Array.from(container.querySelectorAll("*")).filter((el) =>
        (el.className || "").toString().includes("absolute"),
    );
}

describe("SummaryView — opt-in trigger (no automatic LLM call)", () => {
    it("offers a Summarize trigger after a search lands", () => {
        renderView("done");

        const btn = screen.getByTestId("summary-start");
        expect(btn.hasAttribute("disabled")).toBe(false);
        expect(btn.textContent).toContain("Summarize");
    });

    it("starts the summary only on click, never on its own", () => {
        const onSummarize = vi.fn();
        renderView("done", { onSummarize });

        // Nothing fires merely by reaching `done` — that is the whole
        // point of the opt-in flow.
        expect(onSummarize).not.toHaveBeenCalled();

        fireEvent.click(screen.getByTestId("summary-start"));
        expect(onSummarize).toHaveBeenCalledTimes(1);
    });

    // The user must not be able to start a summary on a result set that
    // is still arriving.
    it("disables the trigger while retrieval is in flight", () => {
        const onSummarize = vi.fn();
        renderView("searching", { canSummarize: true, onSummarize });

        expect(screen.getByTestId("summary-start").hasAttribute("disabled")).toBe(true);
        fireEvent.click(screen.getByTestId("summary-start"));
        expect(onSummarize).not.toHaveBeenCalled();
    });

    it("disables the trigger when the sources produced no candidates", () => {
        renderView("done", { canSummarize: false });

        expect(screen.getByTestId("summary-start").hasAttribute("disabled")).toBe(true);
        expect(screen.getByText(/No results available to summarize/)).toBeTruthy();
    });

    it("shows no trigger before a search has been run", () => {
        renderView("idle");
        expect(screen.queryByTestId("summary-start")).toBeNull();
        expect(screen.queryByTestId("summary-cancel")).toBeNull();
    });
});

describe("SummaryView — the trigger becomes Cancel while generating", () => {
    it("replaces the start trigger with a cancel button in summarizing", () => {
        renderView("summarizing");

        expect(screen.getByTestId("summary-cancel")).toBeTruthy();
        expect(screen.queryByTestId("summary-start")).toBeNull();
    });

    // Stopping an in-flight LLM call must cost one click, at the same
    // spot the start affordance occupied.
    it("keeps the cancel affordance in the same footer slot", () => {
        const onCancel = vi.fn();
        const { container } = renderView("summarizing", { onCancel });

        const footer = screen.getByTestId("summary-footer");
        expect(footer.contains(screen.getByTestId("summary-cancel"))).toBe(true);
        expect(floatingNodes(container)).toHaveLength(0);

        fireEvent.click(screen.getByTestId("summary-cancel"));
        expect(onCancel).toHaveBeenCalledTimes(1);
    });
});

describe("SummaryView — trigger placement (design 27 §4.6.5)", () => {
    // Regression guard for the exact defect: an absolutely-positioned
    // button with only `right` set resolves to the container top, which is
    // how the trigger ended up on top of the body text.
    it.each<[SummaryPhase, string]>([
        ["done", "the summary body"],
        ["error", "an error"],
        ["cancelled", "a cancellation"],
    ])("keeps the trigger in the flow when phase is %s", (phase) => {
        const { container } = renderView(phase, {
            summary: "Body text with a citation [[r1]] in it.",
            error: "Provider 503",
        });

        const footer = screen.getByTestId("summary-footer");
        const trigger =
            phase === "summarizing" ? screen.getByTestId("summary-cancel") : screen.getByTestId("summary-start");

        expect(footer.contains(trigger)).toBe(true);
        expect((trigger.className || "").toString()).not.toContain("absolute");
        expect((footer.className || "").toString()).not.toContain("absolute");
        expect(floatingNodes(container)).toHaveLength(0);
    });

    it("renders the footer after the scrolling body, not inside it", () => {
        const { container } = renderView("done", { summary: "Some summary body." });

        const body = screen.getByTestId("summary-text");
        const footer = screen.getByTestId("summary-footer");

        // Sibling rows: the footer must not be a child of the scroll
        // container, otherwise scrolling would carry the trigger along.
        expect(body.contains(footer)).toBe(false);
        expect(body.compareDocumentPosition(footer) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
        // The body is the only scroll owner in the pane.
        expect((body.className || "").toString()).toContain("overflow-y-auto");
        expect((footer.className || "").toString()).toContain("shrink-0");
        expect(container.querySelectorAll(".overflow-y-auto")).toHaveLength(1);
    });

    it("keeps the footer present when the Runtime returned no model or usage", () => {
        // The row must always render, so the trigger holds a stable
        // position instead of vanishing between runs.
        renderView("done", { summary: "Body.", summaryMeta: null });

        expect(screen.getByTestId("summary-footer")).toBeTruthy();
        expect(screen.getByTestId("summary-start")).toBeTruthy();
        expect(screen.getByTestId("summary-meta").textContent).toBe("");
    });
});

describe("SummaryView — regenerate after a summary exists", () => {
    it("relabels the same trigger as Regenerate once a summary exists", () => {
        renderView("done", { summary: "A finished summary." });

        // One slot, not two buttons: both labels mean "run the summary
        // against the frozen snapshot".
        const btn = screen.getByTestId("summary-start");
        expect(btn.textContent).toContain("Regenerate");
        expect(screen.getAllByRole("button").filter((b) => b.className.includes("accent"))).toHaveLength(1);
    });

    it("re-runs on click after a summary exists", () => {
        const onSummarize = vi.fn();
        renderView("done", { summary: "A finished summary.", onSummarize });

        fireEvent.click(screen.getByTestId("summary-start"));
        expect(onSummarize).toHaveBeenCalledTimes(1);
    });

    it("still reports model and token usage inside the footer", () => {
        renderView("done", {
            summary: "Body.",
            summaryMeta: {
                model: "agnes-3.0-flash",
                usage: {
                    prompt_tokens: 1300,
                    completion_tokens: 514,
                    cache_read_tokens: 0,
                    total_tokens: 1814,
                },
            },
        });

        expect(screen.getByTestId("summary-model").textContent).toContain("agnes-3.0-flash");
        expect(screen.getByTestId("summary-tokens").textContent).toContain("1.3k");
        expect(screen.getByTestId("summary-tokens-total").textContent).toContain("1.8k");
    });

    it("offers the trigger again after a failure so the user can retry", () => {
        const onSummarize = vi.fn();
        renderView("error", { error: "Summary timed out", onSummarize });

        expect(screen.getByText("Summary timed out")).toBeTruthy();
        const btn = screen.getByTestId("summary-start");
        expect(btn.textContent).toContain("Summarize");
        fireEvent.click(btn);
        expect(onSummarize).toHaveBeenCalledTimes(1);
    });
});

describe("SummaryView — copyable and selectable body", () => {
    // `body` sets `user-select: none` for the Tauri window drag region, so
    // the summary prose needs an explicit opt-in to be selectable by hand.
    // CompactionCard/MessageBubble carry the same classes for the same
    // reason; without them the summary is unreadable-but-unselectable.
    it("opts the body out of the global user-select: none", () => {
        renderView("done", { summary: "Body text." });

        const cls = screen.getByTestId("summary-text").className;
        expect(cls).toMatch(/(^|\s)select-text(\s|$)/);
        // An I-beam cursor makes the affordance visible before the user
        // discovers it by dragging.
        expect(cls).toMatch(/(^|\s)cursor-text(\s|$)/);
    });

    it("copies the raw summary text, citation markers included", async () => {
        // The chips are a rendering affordance; the clipboard receives what
        // the user actually sees in the prose, markers and all.
        renderView("done", {
            summary: "First point [[r1]].",
            allowedIds: new Set(["r1"]),
        });

        const btn = screen.getByTestId("summary-copy");
        await act(async () => {
            fireEvent.click(btn);
        });
        expect(writeText).toHaveBeenCalledWith("First point [[r1]].");
    });

    // No prose yet means nothing to copy; a Copy button that reports
    // success on an empty clipboard is worse than no button.
    it.each<[SummaryPhase]>([["idle"], ["searching"], ["summarizing"], ["done"]])(
        "shows no copy affordance in phase %s before a summary exists",
        (phase) => {
            renderView(phase, { summary: "", canSummarize: true });
            expect(screen.queryByTestId("summary-copy")).toBeNull();
        },
    );

    it("sits beside the trigger in the footer, not floating over the body", () => {
        const { container } = renderView("done", { summary: "Body." });

        const footer = screen.getByTestId("summary-footer");
        expect(footer.contains(screen.getByTestId("summary-copy"))).toBe(true);
        expect(floatingNodes(container)).toHaveLength(0);
    });
});

describe("SummaryView — unchanged behaviour", () => {
    it("renders citation chips as buttons that report their id", () => {
        const onCitation = vi.fn();
        renderView("done", {
            summary: "First point [[r1]] and second [[r2]].",
            allowedIds: new Set(["r1"]),
            onCitation,
        });

        // Only the allowed id becomes a chip; the unknown marker stays text.
        const chip = screen.getByTestId("summary-text").querySelector("[data-citation-id]");
        expect(chip?.getAttribute("data-citation-id")).toBe("r1");
        fireEvent.click(chip as Element);
        expect(onCitation).toHaveBeenCalledWith("r1");
    });

    it("keeps the summary body as the only scrollable region", () => {
        renderView("done", { summary: "Body." });
        expect(screen.getByTestId("summary-text")).toBeTruthy();
    });
});
