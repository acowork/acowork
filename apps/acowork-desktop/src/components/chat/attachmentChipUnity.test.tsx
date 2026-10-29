// @vitest-environment jsdom
/**
 * One attachment chip, two surfaces.
 *
 * The Chat surface and the execution surface used to render the same
 * attachment five different ways — the execution view drew a bordered capsule
 * with a FileText glyph, the Chat view drew a dashed pill that spelled the
 * wire's discriminant out ("ATTACHED_FILE chat_prompt.rs"), and the composers
 * each hand-copied the class string again. Nothing enforced the agreement, so
 * it drifted every time someone touched one side.
 *
 * These tests pin the three things that make it one grammar now:
 *   1. every chip's frame comes from `AttachmentCapsule` (one box),
 *   2. every glyph comes from `attachmentGlyph` (one vocabulary table),
 *   3. the wire's arm tag is never printed as text (it is data, not a label).
 *
 * Asserted against source as well as DOM: the class string IS the spec, and a
 * re-inlined copy in one component is invisible to a render test of another.
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { render } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { ExecAttachmentLabels } from "./ExecAttachmentLabels";
import { AttachmentChipRow } from "./AttachmentChipRow";
import { ATTACHMENT_KIND, attachmentGlyph, selectionRange } from "./attachmentGlyph";
import { attachedItemArmOf } from "../../lib/types";

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));

const __dirname = dirname(fileURLToPath(import.meta.url));
const sourceOf = (file: string) => readFileSync(resolve(__dirname, file), "utf8");

/** The capsule's own frame signature — padding and radius included. A chip
 *  that re-inlines it instead of composing the component matches this. */
const FRAME = "rounded-md border border-border-outer";

describe("one frame", () => {
  it.each([
    "AttachmentChipRow.tsx",
    "ChatAttachmentChip.tsx",
    "DocumentChip.tsx",
    "AttachedContextChips.tsx",
    "ExecAttachmentLabels.tsx",
  ])("%s composes AttachmentCapsule instead of copying the frame", (file) => {
    const source = sourceOf(file);
    expect(source).toContain("AttachmentCapsule");
    const ownsFrame = new RegExp(`["'\`][^"'\`]*${FRAME.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}`);
    expect(
      ownsFrame.test(source),
      `${file} re-inlines the capsule's frame — the copy is what let the two surfaces drift`,
    ).toBe(false);
  });
});

describe("one glyph vocabulary", () => {
  it("draws the same icon for one arm on both surfaces", () => {
    const { container: chatRow } = render(
      <ExecAttachmentLabels labels={[{ filename: "chat_prompt.rs", kind: ATTACHMENT_KIND.attachedFile }]} />,
    );
    const { container: execRow } = render(
      <AttachmentChipRow
        item={{ type: "attached_file", absPath: "E:/p/chat_prompt.rs", name: "chat_prompt.rs" }}
      />,
    );
    const path = (c: Element | null) => c?.querySelector("svg path")?.getAttribute("d");
    expect(path(chatRow)).toBeTruthy();
    expect(path(execRow)).toBe(path(chatRow));
  });

  it("keys the table by the wire tag, and falls back instead of vanishing", () => {
    expect(attachmentGlyph(ATTACHMENT_KIND.attachedFolder).Icon).toBe(
      attachmentGlyph("attached_folder").Icon,
    );
    // A row predating the `kind` field, and an unknown arm, both still get a glyph.
    expect(attachmentGlyph(undefined).Icon).toBe(attachmentGlyph("???").Icon);
  });

  it("spells a selection range once, for both surfaces", () => {
    expect(selectionRange(12, 25)).toBe("L12-L25");
    expect(selectionRange(12, 12)).toBe("L12");
    expect(selectionRange(undefined, undefined)).toBeUndefined();
  });

  /// The helper is not enough: every surface has to draw with it. A bare
  /// `12-25` surviving on one side is the two-spellings-of-one-selection split
  /// this pins out — and the router's `[attached refs]` line spells `L12-L25`
  /// too, so the transcript, the chip and the prompt say the same thing.
  it.each([
    ["L12-L25", 12, 25],
    ["L12", 12, 12],
  ])("renders %s on the Chat row and the execution chip alike", (spelling, start, end) => {
    const { container: chatRow } = render(
      <ExecAttachmentLabels
        labels={[{ filename: "src/lib.rs", kind: ATTACHMENT_KIND.attachedSelection, startLine: start, endLine: end }]}
      />,
    );
    const { container: execRow } = render(
      <AttachmentChipRow
        item={{ type: "attached_selection", absPath: "/srv/app/src/lib.rs", name: "src/lib.rs", startLine: start, endLine: end }}
      />,
    );
    expect(chatRow.textContent).toContain(spelling);
    expect(execRow.textContent).toContain(spelling);
    expect(execRow.textContent).not.toMatch(/\d+-\d+/);
  });

  /// The composer tray's glyph must be keyed by the arm the *sent* message
  /// carries, so the mapping is the send path's own. Two mappings is how a tray
  /// chip ends up drawing a folder for a file.
  it("keys the tray glyph by the arm the send path ships", () => {
    expect(attachedItemArmOf({ type: "file" })).toBe(ATTACHMENT_KIND.attachedFile);
    expect(attachedItemArmOf({ type: "selection" })).toBe(ATTACHMENT_KIND.attachedSelection);
    expect(attachedItemArmOf({ type: "directory" })).toBe(ATTACHMENT_KIND.attachedFolder);
  });

  it("never prints the arm tag as text", () => {
    for (const file of ["ExecAttachmentLabels.tsx", "AttachmentChipRow.tsx", "ChatAttachmentChip.tsx"]) {
      expect(
        sourceOf(file),
        `${file}: a wire tag rendered as a label is the regression this pins`,
      ).not.toMatch(/uppercase/);
    }
    const { container } = render(
      <ExecAttachmentLabels labels={[{ filename: "a.rs", kind: ATTACHMENT_KIND.attachedFile }]} />,
    );
    expect(container.textContent).toBe("a.rs");
  });
});
