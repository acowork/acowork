/**
 * Onboarding step 3 must DELEGATE provider setup to Harness' components, not
 * re-implement them.
 *
 * Why this file exists:
 *   The wizard used to ship its own mini provider form: a `Dropdown` over
 *   `fetchProviders()`, one API-key input, one base-URL input, a
 *   `ModelMultiSelect`, a `invoke("add_key")` button — and below it a "Local
 *   Providers" card that was a `join(", ")` string. That card could not be
 *   clicked (it never called anything), there was no custom-provider path at
 *   all, and multi-account aliases / model discovery / capability editing /
 *   compact model existed only in Settings. So the first-run path was strictly
 *   weaker than the path every user takes afterwards, and the two copies
 *   drifted.
 *
 *   Step 3 now renders the same `ProviderPicker` + `AddProviderFlow` pair
 *   Settings → Harness uses, which owns keys, base URLs, local providers and
 *   custom providers. This pins the delegation so the hand-rolled form cannot
 *   creep back in.
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { describe, expect, it } from "vitest";

const SRC = readFileSync(
  resolve(dirname(fileURLToPath(import.meta.url)), "OnboardingFlow.tsx"),
  "utf8",
);

describe("onboarding delegates provider setup to harness", () => {
  it("renders the shared picker and the shared add-flow dialog", () => {
    expect(SRC, "step 3 must embed ProviderPicker").toContain(
      'import { ProviderPicker } from "../harness/ProviderPicker";',
    );
    expect(SRC, "step 3 must embed AddProviderFlow").toContain(
      'import { AddProviderFlow } from "../harness/AddProviderFlow";',
    );
    expect(SRC.match(/<(ProviderPicker|AddProviderFlow)[\s>]/g), "both mounted").
      toHaveLength(2);
  });

  it("does not write vault keys itself — AddProviderFlow owns the save", () => {
    // `add_key` from the wizard bypassed the key test, the alias rows and the
    // model-capability payload, which is how the old form silently stored an
    // half-configured provider.
    expect(SRC.match(/invoke\(\s*"add_key"/g) ?? [], "wizard must not add keys").
      toEqual([]);
    // And no hand-rolled model picker either — that lives in the dialog.
    expect(SRC, "ModelMultiSelect belongs to AddProviderFlow").not.toContain(
      "<ModelMultiSelect",
    );
  });

  it("re-reads the vault and the catalog after a successful add", () => {
    // Without the refresh the picker keeps listing the provider the user just
    // connected (it filters by configured count) and the summary strip stays
    // empty. Harness' own onSuccess does the same two reloads.
    const onSuccess = SRC.slice(SRC.indexOf("const handleAddFlowSuccess"));
    const body = onSuccess.slice(0, onSuccess.indexOf("};"));
    expect(body, "keys must be re-read").toContain("loadKeys()");
    expect(body, "providers must be re-read").toContain("loadProviders()");
    // The AppLayout shell reloads the shared model inventory on this signal.
    expect(body, "model inventory must be notified").toContain('"models-added"');
  });
});
