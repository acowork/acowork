// @vitest-environment jsdom
/**
 * The turn's deliverables row — wiring only.
 *
 * The parsing rules (what a row means, what a malformed entry degrades to) live
 * in `lib/deliverableFiles` and are tested there. This file pins the four
 * things only the component can get wrong:
 *
 *   1. an image renders as a thumbnail and a non-image as a chip;
 *   2. a click opens the **editor** viewer through the chat-side rule — an
 *      image in preview mode, everything else in Monaco. `openFile` for an
 *      image would render base64 in the editor instead of the picture;
 *   3. a thumbnail whose bytes cannot be fetched degrades to a chip instead of
 *      a broken-image hole — and a thumbnail that is merely *interrupted*
 *      (unmount, StrictMode double-mount, a row recycled by the virtual list)
 *      must degrade to nothing at all: the bytes still land;
 *   4. the fetch goes through the editor preview's own hook, so a thumbnail and
 *      the full-size tab share one cached download.
 *
 * The blob lib is mocked at the **network** boundary, not the hook: the hook is
 * the thing whose cancellation behaviour the regression in (3) depends on.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import React from "react";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";

const fetchWorkspaceFileBlobUrl = vi.fn();
const fetchWorkspaceFileBlob = vi.fn();
const writeText = vi.fn();
const openFile = vi.fn(async (..._args: unknown[]) => {});
const openPreview = vi.fn(async (..._args: unknown[]) => {});
const openDocPreview = vi.fn((..._args: unknown[]) => {});

vi.mock("../../lib/workspaceFileBlob", () => ({
  fetchWorkspaceFileBlobUrl: (...args: unknown[]) =>
    fetchWorkspaceFileBlobUrl(...(args as [])),
  fetchWorkspaceFileBlob: (...args: unknown[]) => fetchWorkspaceFileBlob(...(args as [])),
}));
vi.mock("../../stores/fileEditorStore", () => ({
  useFileEditorStore: { getState: () => ({ openFile, openPreview, openDocPreview }) },
}));
vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
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

/** An image artifact as the Runtime names them: readable stem, content tag. */
const shot = {
  workspace_id: "__agent_home__",
  rel_path: "artifacts/s-1/logo-1a2b3c4d.png",
  mime: "image/png",
  bytes: 2048,
};
const report = {
  workspace_id: "ws-1",
  rel_path: "docs/report.md",
  mime: "text/markdown",
  bytes: 512,
};

/** A row carrying an online-document deliverable (`metadata.docs`). */
function docRow(docs: unknown[]): ChatMessage {
  return {
    id: "row-2",
    type: "system",
    content: "",
    timestamp: 0,
    metadata: { type: "deliverables", files: [], docs },
  } as ChatMessage;
}

const doc = {
  docId: "doc-1a2b",
  path: "项目管理/p-1/评审/spec.md",
  name: "spec",
  version: 3,
};

/**
 * A download that behaves like the network: it resolves on a later task, and
 * rejects with `AbortError` if the caller aborts first. A promise that settles
 * synchronously cannot expose a cancellation bug, because there is nothing left
 * to cancel.
 */
function mockSignalAwareDownload(result: { blobUrl: string; mime: string }) {
  fetchWorkspaceFileBlobUrl.mockImplementation(
    (_agentId: string, _ws: string, _p: string, opts?: { signal?: AbortSignal }) =>
      new Promise((resolve, reject) => {
        const timer = setTimeout(() => resolve(result), 0);
        opts?.signal?.addEventListener("abort", () => {
          clearTimeout(timer);
          reject(new DOMException("signal is aborted without reason", "AbortError"));
        });
      }),
  );
}

beforeEach(() => {
  writeText.mockReset();
  writeText.mockResolvedValue(undefined);
  Object.assign(navigator, { clipboard: { writeText } });
  mockSignalAwareDownload({ blobUrl: "blob:mock-url", mime: "image/png" });
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
    expect(screen.getByRole("img").getAttribute("alt")).toBe("logo-1a2b3c4d.png");
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
      "artifacts/s-1/logo-1a2b3c4d.png",
    ]);
    expect(fetchWorkspaceFileBlobUrl).toHaveBeenCalledTimes(1);
  });

  it("keeps the thumbnail when the effect double-mounts (StrictMode)", async () => {
    // Regression: the component used to hold one liveness flag for the whole
    // instance, so the aborted first run's `AbortError` arrived after the
    // second run had raised the flag again, was read as a failed download, and
    // latched the row into chips — for every image, on every dev start.
    render(
      <React.StrictMode>
        <ChatDeliverablesRow message={row([shot])} agentId="a1" />
      </React.StrictMode>,
    );

    await waitFor(() => expect(screen.getByRole("img")).toBeTruthy());
    expect(screen.queryByText("logo-1a2b3c4d.png")).toBeNull();
  });

  it("opens an image in the preview viewer, not in Monaco", async () => {
    // `openFile` opens edit mode; the editor's image branch needs preview mode,
    // so an image opened the wrong way shows the base64 read instead.
    render(<ChatDeliverablesRow message={row([shot, report])} agentId="a1" />);
    await waitFor(() => expect(screen.getByRole("img")).toBeTruthy());

    fireEvent.click(screen.getByRole("img"));
    expect(openPreview).toHaveBeenLastCalledWith(
      "a1",
      "__agent_home__",
      "artifacts/s-1/logo-1a2b3c4d.png",
    );
    expect(openFile).not.toHaveBeenCalled();

    // A Markdown document previews too — the same answer the workspace tree
    // gives it, so one file does not open differently per surface.
    fireEvent.click(screen.getByText("report.md"));
    expect(openPreview).toHaveBeenLastCalledWith("a1", "ws-1", "docs/report.md");
  });

  it("falls back to a chip when the bytes cannot be fetched", async () => {
    fetchWorkspaceFileBlobUrl.mockRejectedValueOnce(new Error("workspace file → 404"));
    render(<ChatDeliverablesRow message={row([shot])} agentId="a1" />);

    // The file is still openable — a failed thumbnail is not a failed file.
    await waitFor(() =>
      expect(screen.getByRole("button", { name: /logo-1a2b3c4d\.png/ })).toBeTruthy(),
    );
    expect(screen.queryByRole("img")).toBeNull();
    expect(screen.getByText("logo-1a2b3c4d.png")).toBeTruthy();
  });

  it("collapses a duplicated file into one entry", async () => {
    render(<ChatDeliverablesRow message={row([shot, { ...shot }])} agentId="a1" />);
    await waitFor(() => expect(screen.getByRole("img")).toBeTruthy());
    expect(screen.getAllByRole("img")).toHaveLength(1);
  });

  /**
   * The copy affordance exists to hand a reference to ANOTHER agent, so what it
   * writes is pinned: the doc's library path (the `ref` `doc_read` takes) and a
   * workspace-relative path — not the card's title, not a viewer URL.
   */
  it("copies a document's library path, not its title", async () => {
    render(<ChatDeliverablesRow message={docRow([doc])} agentId="a1" />);

    fireEvent.click(screen.getByTestId("deliverable-copy-link"));

    await waitFor(() => expect(writeText).toHaveBeenCalledWith(doc.path));
    // The copy is a sibling of the card: it must not also open the document.
    expect(openDocPreview).not.toHaveBeenCalled();
  });

  it("copies a workspace file's relative path", async () => {
    render(<ChatDeliverablesRow message={row([report])} agentId="a1" />);

    fireEvent.click(screen.getByTestId("deliverable-copy-link"));

    await waitFor(() => expect(writeText).toHaveBeenCalledWith(report.rel_path));
    // The copy is a sibling of the card: it must not also open the file.
    expect(openFile).not.toHaveBeenCalled();
    expect(openPreview).not.toHaveBeenCalled();
  });
});
