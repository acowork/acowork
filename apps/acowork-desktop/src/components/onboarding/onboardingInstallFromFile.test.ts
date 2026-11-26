/**
 * Onboarding's "install from file" picker must ask for the package extension
 * the app actually ships (D6: `.agent` → `.bot`).
 *
 * Why this file exists:
 *   The OS file dialog is the one place where a wrong extension is invisible
 *   to every other test: nothing throws, nothing logs, the user simply cannot
 *   see or select the `.bot` package they downloaded, and the wizard's only
 *   recovery path is "install a bundled agent". The filter is a plain object
 *   literal inside an async click handler, so it is asserted from the source
 *   — the same idiom `onboardingProviderReuse.test.ts` uses for the provider
 *   step, which is likewise untestable without mounting the whole wizard.
 *
 *   The `name` of the filter is a hard-coded English literal (it is *not* an
 *   i18n key): it is the label in the dialog's file-type dropdown, so it is
 *   user-visible text and belongs to this rename.
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { describe, expect, it } from "vitest";

const SRC = readFileSync(
  resolve(dirname(fileURLToPath(import.meta.url)), "OnboardingFlow.tsx"),
  "utf8",
);

describe("onboarding install-from-file", () => {
  it("asks the picker for .bot packages", () => {
    expect(SRC, "the picker must offer the .bot extension").toContain(
      'filters: [{ name: "Bot Package", extensions: ["bot"] }],',
    );
  });

  it("no longer asks for the retired .agent extension", () => {
    expect(SRC.match(/extensions:\s*\["agent"\]/g) ?? []).toEqual([]);
    expect(SRC, "the old filter label").not.toContain('name: "Agent Package"');
  });
});