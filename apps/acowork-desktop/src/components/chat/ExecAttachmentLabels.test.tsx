// @vitest-environment jsdom
import { describe, expect, it, vi, beforeEach } from "vitest";
import { render, screen } from "@testing-library/react";

vi.mock("../../lib/openWorkspaceRef", () => ({ openAttachedRef: vi.fn() }));

import { ExecAttachmentLabels } from "./ExecAttachmentLabels";
import { openAttachedRef } from "../../lib/openWorkspaceRef";

/**
 * ADR-100 D12: an execution view's composer sends its run's attached items as
 * labels, and the chat must show them (a turn that was a file and nothing else
 * still has to paint a head row).
 *
 * Interactivity is decided by the data, not by the surface. Since D12 Rev.
 * 2026-11 a workspace arm carries `absPath`, so its chip opens the file through
 * the same dispatcher the execution session and the composer tray use
 * (`openAttachedRef`). The banner used to look exactly like the execution
 * session's clickable chip and do nothing — that is the regression these cases
 * pin. What must never come back: printing the ADR-046 discriminant at the user
 * (a row once read "ATTACHED_FILE chat_prompt.rs"), and opening something that
 * has nothing behind it (the upload arms, whose bytes belong to the run).
 */
describe("ExecAttachmentLabels", () => {
  beforeEach(() => {
    vi.mocked(openAttachedRef).mockClear();
  });

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

  it("is inert when the row has no path to open", () => {
    render(
      <ExecAttachmentLabels
        agentId="com.test.Agent"
        labels={[{ filename: "diff.patch", kind: "attached_file" }]}
      />,
    );
    expect(screen.queryByRole("button")).toBeNull();
    expect(openAttachedRef).not.toHaveBeenCalled();
  });

  it("opens a workspace arm through the one dispatcher the exec surface uses", () => {
    render(
      <ExecAttachmentLabels
        agentId="com.test.Agent"
        labels={[
          { filename: "src/main.rs", kind: "attached_file", absPath: "E:/ws/src/main.rs" },
        ]}
      />,
    );
    screen.getByRole("button", { name: "src/main.rs" }).click();
    expect(openAttachedRef).toHaveBeenCalledWith({
      item: { type: "attached_file", absPath: "E:/ws/src/main.rs", name: "src/main.rs" },
      agentId: "com.test.Agent",
      // The chat surface has no execution session of its own, so there is no
      // priority root: the resolver searches every workspace of the Agent.
      currentWorkspaceId: "",
    });
  });

  it("lands a selection on its first line, not at the top of the file", () => {
    render(
      <ExecAttachmentLabels
        agentId="com.test.Agent"
        labels={[
          {
            filename: "src/lib.rs",
            kind: "attached_selection",
            absPath: "E:/ws/src/lib.rs",
            startLine: 12,
            endLine: 25,
          },
        ]}
      />,
    );
    screen.getByRole("button", { name: "src/lib.rs" }).click();
    expect(openAttachedRef).toHaveBeenCalledWith({
      item: {
        type: "attached_selection",
        absPath: "E:/ws/src/lib.rs",
        name: "src/lib.rs",
        startLine: 12,
        endLine: 25,
      },
      agentId: "com.test.Agent",
      currentWorkspaceId: "",
    });
  });

  it("leaves an upload arm decorative even if a path rode along", () => {
    // Defence in depth: the Gateway never puts a path on an upload arm (its
    // bytes belong to the run), and a row that somehow carries one must still
    // not offer a click that would open a stranger's file.
    render(
      <ExecAttachmentLabels
        agentId="com.test.Agent"
        labels={[{ filename: "report.pdf", kind: "file_upload", absPath: "E:/ws/report.pdf" }]}
      />,
    );
    expect(screen.queryByRole("button")).toBeNull();
  });

  it("leaves a folder decorative — fileTab has no folder view", () => {
    render(
      <ExecAttachmentLabels
        agentId="com.test.Agent"
        labels={[{ filename: "src", kind: "attached_folder", absPath: "E:/ws/src" }]}
      />,
    );
    expect(screen.queryByRole("button")).toBeNull();
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
