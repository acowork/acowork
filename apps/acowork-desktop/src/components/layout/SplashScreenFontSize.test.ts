/**
 * The SplashScreen recovery view must sit on the SAME type/control scale as
 * the rest of the app (the Settings card-column convention).
 *
 * Why this file exists:
 *   The timeout-recovery view was hand-styled ahead of the token tiers: the
 *   Gateway-address field was a raw `<input>` spelling `px-3 py-1.5 text-sm`
 *   with hardcoded `zinc` colours, and the retry action was a
 *   `bg-zinc-200 px-5 py-2 text-sm` button. Both read a notch larger than the
 *   equivalent controls everywhere else (`StyledInput` owns the `text-xs` +
 *   `--ui-input-py` pair; filled action buttons take vertical padding from
 *   `--ui-btn-py`), so the splash looked off next to the main UI. This pins
 *   the recovery view to the shared scale so it can't drift back.
 *
 *   Same source-scan approach as `onboardingFontSize.test.ts`: jsdom resolves
 *   no cascade, so the class string IS the spec.
 *
 * What this pins:
 *   1. the recovery view hosts the shared `GatewayAddressPicker` instead of
 *      hand-rolling an address field — that control owns the `text-xs` +
 *      `--ui-input-py` pair (and, since ADR-115, is the ONE place a user can
 *      change servers, so every mount point must share it);
 *   2. no rem-frozen / oversized text steps (`text-lg`+), which would ignore
 *      the global `--ui-font-size` scale (see globalFontSize.test.ts);
 *   3. filled ACTION buttons take vertical padding from `--ui-btn-py`, not a
 *      fixed `py-N`. The CandidateChooser rows are deliberately excluded: they
 *      are compact list buttons (a tighter fixed hit-target, `text-left` +
 *      `justify-between`), not form actions — the `candidate rows use the
 *      global accent button style` case owns their look.
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { describe, expect, it } from "vitest";

const SRC = readFileSync(
    resolve(dirname(fileURLToPath(import.meta.url)), "SplashScreen.tsx"),
    "utf8",
);

describe("SplashScreen recovery view shares the app type/control scale", () => {
    it("hosts the shared address picker instead of a hand-rolled field", () => {
        const raw = SRC.match(/<input[\s\S]*?\/>/g) ?? [];
        expect(raw, `use GatewayAddressPicker: ${raw.join("\n")}`).toEqual([]);
        // And the shared control is actually wired in.
        expect(SRC, "GatewayAddressPicker must be imported").toContain(
            "GatewayAddressPicker",
        );
    });

    it("stays within the token tiers (no rem-frozen text-lg+ steps)", () => {
        const oversized =
            SRC.match(/className="[^"]*\btext-(lg|xl|2xl|3xl|4xl)\b[^"]*"/g) ?? [];
        expect(oversized, `sizes must stay at text-sm or smaller: ${oversized}`).toEqual(
            [],
        );
    });

    it("takes action-button padding from --ui-btn-py, not a fixed py step", () => {
        // Scan every class string rather than slicing JSX tags — an
        // `onClick={() => …}` arrow puts a stray `>` before `className`.
        const classes = SRC.match(/className="[^"]*"/g) ?? [];
        const filled = classes.filter((c) => /btn-(solid|accent)/.test(c));
        // Exclude the CandidateChooser list rows: compact hit-targets, not
        // form actions (see the file header).
        const actions = filled.filter((c) => !/text-left|justify-between/.test(c));
        const offenders = actions.filter((c) => /\bpy-(1|1\.5|2|2\.5|3)\b/.test(c));
        // Vacuous-pass guard: the recovery view has at least one action button.
        expect(actions.length, "no action buttons found — the scan stopped matching").
            toBeGreaterThan(0);
        expect(offenders, `action buttons must use py-[var(--ui-btn-py)]: ${offenders}`).
            toEqual([]);
        // The retry / start-local button is the accent action on the shared step.
        expect(
            actions.some((c) => /btn-accent/.test(c) && /py-\[var\(--ui-btn-py\)\]/.test(c)),
            "the primary action must be a btn-accent on --ui-btn-py",
        ).toBe(true);
    });
});
