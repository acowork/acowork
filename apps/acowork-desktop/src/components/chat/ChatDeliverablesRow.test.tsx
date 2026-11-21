// @vitest-environment jsdom
/**
 * The turn's deliverables row — wiring only.
 *
 * The parsing rules (what a row means, what a malformed entry degrades to) live
 * in `lib/deliverableFiles` and are tested there. This file pins the three
 * things only the component can get wrong:
 *
 *   1. an image renders as a thumbnail and a non-image as a chip;
 *   2. a click opens the **editor** viewer at the same coordinates the
 *      workspace tree uses — no second viewer, no bespoke preview;
 *   3. a thumbnail whose bytes cannot be fetched degrades to a chip instead of
 *      a broken-image hole.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";

const fetchWorkspaceFileBlobUrl = vi.fn();
const openFile = vi.fn(async (..._args: unknown[]) => {});

vi.mock("../../lib/workspaceFileBlob", () => ({
  fetchWorkspaceFileBlobUrl: (...args: unknown[]) =>
    fetchWorkspaceFileBlobUrl(...(args as [])),
}));
vi.mock("../../stores/fileEditorStore", () => ({
  useFileEditorStore: { getState: () => ({ openFile }) },
}));
vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));
vi.mock("../../lib/logger", () => ({
  log: { warn: vi.fn(), debug: vi.fn(), info: vi.fn(), error: vi.fn() },
}));

import { ChatDeliverablesRow } from "./ChatDeliverablesRow";
import type { ChatMessage } from "../../lib/types";

function row(files: unknown[]): ChatMessage {
  return {
    id: "row-1",
    type: "system",
    content: "",
    timestamp: 0,
    metadata: { type: "deliverables", files },
  } as ChatMessage;
}

const shot = {
  workspace_id: "__agent_home__",
  rel_path: "artifacts/s-1/call_1-0.png",
  mime: "image/png",
  bytes: 2048,
};
const report = {
  workspace_id: "ws-1",
  rel_path: "docs/report.md",
  mime: "text/markdown",
  bytes: 512,
};

beforeEach(() => {
  fetchWorkspaceFileBlobUrl.mockResolvedValue({
    blobUrl: "blob:mock-url",
    mime: "image/png",
  });
});

afterEach(() => {
  vi.clearAllMocks();
});

describe("ChatDeliverablesRow", () => {
  it("renders nothing for a row with no files", () => {
    const { container } = render(<ChatDeliverablesRow message={row([])} agentId="a1" />);
    expect(container.innerHTML).toBe("");
  });

  it("renders nothing without an agent id — nothing would be fetchable", () => {
    const { container } = render(<ChatDeliverablesRow message={row([shot])} />);
    expect(container.innerHTML).toBe("");
  });

  it("draws an image as a thumbnail and a document as a chip", async () => {
    render(<ChatDeliverablesRow message={row([shot, report])} agentId="a1" />);

    await waitFor(() => expect(screen.getByRole("img")).toBeTruthy());
    expect(screen.getByRole("img").getAttribute("alt")).toBe("call_1-0.png");
    // The document is listed by name, and nothing fetched it.
    expect(screen.getByText("report.md")).toBeTruthy();
    expect(fetchWorkspaceFileBlobUrl).toHaveBeenCalledTimes(1);
  });

  it("fetches a thumbnail exactly once, at the stored coordinates", async () => {
    render(<ChatDeliverablesRow message={row([shot])} agentId="a1" />);
    await waitFor(() => expect(fetchWorkspaceFileBlobUrl).toHaveBeenCalled());
    expect(fetchWorkspaceFileBlobUrl.mock.calls[0].slice(0, 3)).toEqual([
      "a1",
      "__agent_home__",
      "artifacts/s-1/call_1-0.png",
    ]);
    expect(fetchWorkspaceFileBlobUrl).toHaveBeenCalledTimes(1);
  });

  it("opens the editor viewer at the workspace coordinates on click", async () => {
    render(<ChatDeliverablesRow message={row([shot, report])} agentId="a1" />);
    await waitFor(() => expect(screen.getByRole("img")).toBeTruthy());

    fireEvent.click(screen.getByRole("img"));
    expect(openFile).toHaveBeenLastCalledWith("a1", "__agent_home__", "artifacts/s-1/call_1-0.png");

    fireEvent.click(screen.getByText("report.md"));
    expect(openFile).toHaveBeenLastCalledWith("a1", "ws-1", "docs/report.md");
  });

  it("falls back to a chip when the bytes cannot be fetched", async () => {
    fetchWorkspaceFileBlobUrl.mockRejectedValueOnce(new Error("404"));
    render(<ChatDeliverablesRow message={row([shot])} agentId="a1" />);

    // The file is still openable — a failed thumbnail is not a failed file.
    await waitFor(() => expect(screen.getByRole("button")).toBeTruthy());
    expect(screen.queryByRole("img")).toBeNull();
    expect(screen.getByText("call_1-0.png")).toBeTruthy();
  });

  it("collapses a duplicated file into one entry", async () => {
    render(<ChatDeliverablesRow message={row([shot, { ...shot }])} agentId="a1" />);
    await waitFor(() => expect(screen.getByRole("img")).toBeTruthy());
    expect(screen.getAllByRole("img")).toHaveLength(1);
  });
});