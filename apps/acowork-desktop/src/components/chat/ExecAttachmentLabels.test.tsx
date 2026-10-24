// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";

import { ExecAttachmentLabels } from "./ExecAttachmentLabels";

/**
 * ADR-100 D12: an execution view's composer sends its run's attached items as
 * labels only. The chat must show them (a turn that was a file and nothing
 * else still has to paint a head row) and must never imply they can be opened.
 */
describe("ExecAttachmentLabels", () => {
  it("paints one label per attached item", () => {
    render(
      <ExecAttachmentLabels
        labels={[
          { filename: "src/main.rs", kind: "file" },
          { filename: "notes.md", kind: "upload" },
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

  it("names the ADR-046 arm when the sender said which one it was", () => {
    render(<ExecAttachmentLabels labels={[{ filename: "spec.pdf", kind: "upload" }]} />);
    expect(screen.getByText("upload")).toBeTruthy();
  });

  it("paints nothing when the row carried no exec items", () => {
    const { container } = render(<ExecAttachmentLabels labels={[]} />);
    expect(container.innerHTML).toBe("");
  });
});
