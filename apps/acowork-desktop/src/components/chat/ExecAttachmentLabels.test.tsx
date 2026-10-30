// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";

import { ExecAttachmentLabels } from "./ExecAttachmentLabels";

/**
 * ADR-100 D12: an execution view's composer sends its run's attached items as
 * labels only. The chat must show them (a turn that was a file and nothing
 * else still has to paint a head row) and must never imply they can be opened.
 *
 * These chips are the Chat surface's half of one grammar with the execution
 * session: the frame comes from `AttachmentCapsule` and the glyph from
 * `attachmentGlyph`, the same two pieces `AttachmentChipRow` draws with. What
 * the pin below guards is the one thing that must never come back — printing
 * the ADR-046 discriminant at the user. A row used to read
 * "ATTACHED_FILE chat_prompt.rs", which is a wire tag wearing a chip.
 */
describe("ExecAttachmentLabels", () => {
  it("paints one label per attached item", () => {
    render(
      <ExecAttachmentLabels
        labels={[
          { filename: "src/main.rs", kind: "attached_file" },
          { filename: "notes.md", kind: "file_upload" },
        ]}
      />,
    );
    expect(screen.getByText("src/main.rs")).toBeTruthy();
    expect(screen.getByText("notes.md")).toBeTruthy();
  });

  it("is inert — no button, no link, nothing to resolve", () => {
    render(<ExecAttachmentLabels labels={[{ filename: "diff.patch" }]} />);
    expect(screen.queryByRole("button")).toBeNull();
    expect(screen.queryByRole("link")).toBeNull();
  });

  it("draws a glyph for the arm instead of spelling its name out", () => {
    const { container } = render(
      <ExecAttachmentLabels labels={[{ filename: "spec.pdf", kind: "file_upload" }]} />,
    );
    // The arm still drives the rendering — it just is not text any more.
    expect(container.querySelector("svg")).toBeTruthy();
    expect(screen.queryByText(/attached_file|file_upload/i)).toBeNull();
  });

  it("shows the line range of a selection and the size of an upload", () => {
    render(
      <ExecAttachmentLabels
        labels={[
          {
            filename: "src/lib.rs",
            kind: "attached_selection",
            startLine: 12,
            endLine: 25,
          },
          { filename: "spec.pdf", kind: "file_upload", format: "pdf", sizeBytes: 12345 },
        ]}
      />,
    );
    expect(screen.getByText("L12-L25")).toBeTruthy();
    // The shared capsule spelling (`12.1 KB`, one decimal), not the router's
    // prompt spelling (`12KB`, integer) — one file, two surfaces, two widths.
    expect(screen.getByText("12.1 KB")).toBeTruthy();
  });

  it("paints nothing when the row carried no exec items", () => {
    const { container } = render(<ExecAttachmentLabels labels={[]} />);
    expect(container.innerHTML).toBe("");
  });
});
