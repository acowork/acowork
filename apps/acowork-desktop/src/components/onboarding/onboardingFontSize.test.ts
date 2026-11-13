/**
 * The onboarding wizard must sit on the SAME type/control scale as
 * Settings — the app's card-column convention.
 *
 * Why this file exists:
 *   The wizard was written before the `--ui-font-size` / `--text-*` token
 *   tiers settled, and still spelled its sizes as one-off steps: step
 *   titles `text-lg`, subtitles and body rows `text-sm`, hero `text-2xl` /
 *   `text-4xl`, buttons `py-2` / `py-2.5`, raw `<input>`s with `px-3 py-2
 *   text-sm`. Every one of those is a notch above what the same card reads
 *   like in Settings (labels `text-xs`, card titles `text-11`, inputs via
 *   `StyledInput`, buttons `px-3 py-[var(--ui-btn-py)] text-xs`), so the
 *   wizard looked like a zoomed-in copy of the settings panel and grew taller
 *   than the viewport at the larger font steps.
 *
 *   `text-2xl` / `text-4xl` are the worse half of the problem: they resolve
 *   to Tailwind's frozen `rem` defaults, not to a `--text-*` token, so they
 *   ignore the global font-size setting entirely (see globalFontSize.test.ts,
 *   which exists for that exact class of bug).
 *
 * What this pins (source scan — jsdom resolves no cascade, so the class
 * string IS the spec, same approach as globalFontSize.test.ts):
 *   1. no text step above `text-lg`, and none of the frozen `2xl+` defaults;
 *   2. filled buttons take their vertical padding from `--ui-btn-py`
 *      instead of a fixed `py-N`;
 *   3. text fields go through `StyledInput`, which owns the `text-xs` +
 *      `--ui-input-py` pair;
 *   4. step 2 hosts the shared `GatewayAddressPicker` and has no residual
 *      `gatewayMode` control (ADR-115: the mode is derived from the address,
 *      so the wizard must not offer a second, conflicting way to set it).
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { describe, expect, it } from "vitest";

const SRC = readFileSync(
  resolve(dirname(fileURLToPath(import.meta.url)), "OnboardingFlow.tsx"),
  "utf8",
);

describe("onboarding shares the settings type/control scale", () => {
  it("stays within the token tiers (no frozen 2xl+ steps)", () => {
    const frozen = SRC.match(/className="[^"]*\btext-(2xl|3xl|4xl|5xl)\b[^"]*"/g) ?? [];
    expect(frozen, `rem-frozen sizes ignore --ui-font-size: ${frozen}`).toEqual(
      [],
    );
    // `text-lg` / `text-xl` are reserved for the welcome hero — step 2
    // onward is pure card content, which stays at `text-sm` or smaller.
    const hero = SRC.slice(0, SRC.indexOf("/** Step 2"));
    const rest = SRC.slice(SRC.indexOf("/** Step 2"));
    expect(hero.match(/\btext-(lg|xl)\b/g), "the hero keeps its two steps").
      toHaveLength(2);
    const oversized = rest.match(/className="[^"]*\btext-(lg|xl|2xl|3xl|4xl)\b[^"]*"/g) ?? [];
    expect(oversized, `card content must stay at text-sm or smaller: ${oversized}`).
      toEqual([]);
  });

  it("takes filled-button padding from --ui-btn-py, not a fixed py step", () => {
    // Scan every class string in the file rather than trying to slice JSX
    // opening tags — an `onClick={() => …}` arrow puts a stray `>` before
    // the `className` attribute, so a tag-based split would silently read
    // no classes at all.
    //
    // Ghost links ("skip") are not matched: their padding is a hit target,
    // not a surface, so it is not the token's job.
    const classes = SRC.match(/className="[^"]*"/g) ?? [];
    const filled = classes.filter((c) => /btn-(solid|accent)|bg-zinc/.test(c));
    const offenders = filled.filter((c) => /\bpy-(1|1\.5|2|2\.5|3)\b/.test(c));
    // Vacuous-pass guard: the wizard is button-heavy.
    expect(filled.length, "no filled buttons found — the scan stopped matching").
      toBeGreaterThan(5);
    expect(offenders, `buttons must use py-[var(--ui-btn-py)]: ${offenders}`).toEqual(
      [],
    );
  });

  it("has no raw <input> re-declaring the field style", () => {
    // The only hand-rolled field left is the agent-list checkbox, which
    // carries no text and is sized in fixed units on purpose.
    const raw = SRC.match(/<input[\s\S]*?\/>/g) ?? [];
    const fields = raw.filter((tag) => !/type="checkbox"/.test(tag));
    expect(fields, `use StyledInput: ${fields.join("\n")}`).toEqual([]);
  });

  // ADR-115: the wizard's step 2 no longer has a local/remote/relay radio —
  // `gatewayMode` is derived from the address, so the whole mode row (and the
  // qualifier-vs-name layout problem it carried) is gone. What must hold now
  // is that the step hosts the SAME address control as every other mount
  // point, so the user cannot get a different (or missing) way to change
  // servers depending on which screen they are on.
  it("hosts the shared address picker, not a per-step mode control", () => {
    expect(SRC).not.toContain('name="gatewayMode"');
    expect(SRC).toContain("GatewayAddressPicker");
    expect(SRC).toContain('from "../connection/GatewayAddressPicker"');
  });
});
