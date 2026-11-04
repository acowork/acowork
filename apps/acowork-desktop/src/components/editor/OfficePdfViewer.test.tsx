/**
 * OfficePdfViewer — the four outcomes of a local Office → PDF conversion.
 *
 * The component owns one decision and nothing else: which renderer is on
 * screen, and what the user is told about it. So the stubs are the two
 * things it delegates to — the conversion hook and `PdfViewer` — and the
 * assertions are purely about the routing plus the banner copy:
 *   - ready        → the PDF viewer, fed by the external bytes, with the
 *                    quiet print-layout note; the JS fallback is NOT mounted;
 *   - converting   → the loading body (again, no fallback flash);
 *   - unavailable  → the fallback viewer + "approximate layout" + the
 *                    install hint, and NO error styling;
 *   - failed       → the fallback viewer + the reason + a working retry.
 *
 * The chat hand-off props are the routing this file can still check without a
 * real viewer: they reach the PDF branch and never the fallback one.
 * Rust side of the contract (engine probing, soffice, COM) is not tested
 * here — this file only pins the UI's reaction to its answers.
 */
import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, fireEvent, cleanup } from "@testing-library/react";

const h = vi.hoisted(() => ({
  convState: {
    status: "idle" as string,
    pdfBytes: null as Uint8Array | null,
    error: null as string | null,
    retry: vi.fn(),
  },
  convCalls: [] as unknown[],
  pdfProps: [] as Array<Record<string, unknown>>,
}));

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({
    t: (key: string, params?: Record<string, unknown>) =>
      params ? `${key} ${JSON.stringify(params)}` : key,
  }),
}));
vi.mock("../../hooks/useConvertedPdf", () => ({
  useConvertedPdf: (ref: unknown, opts: unknown) => {
    h.convCalls.push({ ref, opts });
    return h.convState;
  },
}));
vi.mock("./PdfViewer", () => ({
  default: (props: Record<string, unknown>) => {
    h.pdfProps.push(props);
    return <div data-testid="pdf-viewer" />;
  },
}));
// The frame's escape hatches re-address the Gateway route; irrelevant here.
vi.mock("../../lib/workspaceFileBlob", () => ({
  saveWorkspaceFileToDisk: vi.fn(() => Promise.resolve(true)),
  openWorkspaceFileExternally: vi.fn(() => Promise.resolve()),
}));

import OfficePdfViewer from "./OfficePdfViewer";

const file = {
  agentId: "a1",
  workspaceId: "ws1",
  relPath: "docs/paper.docx",
  fileName: "paper.docx",
  previewSeq: 4,
  size: 2048,
  mtimeMs: 111,
};

const fallback = <div data-testid="fallback-viewer" />;

beforeEach(() => {
  cleanup();
  h.convCalls.length = 0;
  h.pdfProps.length = 0;
  h.convState = { status: "idle", pdfBytes: null, error: null, retry: vi.fn() };
});

describe("OfficePdfViewer", () => {
  it("shows the native PDF viewer with the converted bytes when ready", () => {
    const bytes = new Uint8Array([0x25, 0x50, 0x44, 0x46]);
    h.convState.status = "ready";
    h.convState.pdfBytes = bytes;
    render(<OfficePdfViewer {...file} fallback={fallback} />);

    expect(screen.getByTestId("pdf-viewer")).toBeTruthy();
    expect(screen.queryByTestId("fallback-viewer")).toBeNull();
    expect(h.pdfProps[0]).toMatchObject({
      agentId: "a1",
      workspaceId: "ws1",
      relPath: "docs/paper.docx",
      fileName: "paper.docx",
      previewSeq: 4,
      external: { state: "ready", bytes, retry: h.convState.retry },
    });
    // And nothing is stacked above it. The print-layout note used to occupy a
    // row of the document on every ready preview, to explain a distinction the
    // user never had to make: the degraded paths say what they could not do,
    // which is the only case where the layout is in question.
  });

  it("passes the tab's file version to the converter", () => {
    h.convState.status = "converting";
    render(<OfficePdfViewer {...file} fallback={fallback} />);
    expect(h.convCalls[0]).toEqual({
      ref: {
        agentId: "a1",
        workspaceId: "ws1",
        relPath: "docs/paper.docx",
        fileName: "paper.docx",
        size: 2048,
        mtimeMs: 111,
      },
      opts: { reloadSeq: 4 },
    });
  });

  it("keeps the fallback unmounted while converting (no renderer flash)", () => {
    h.convState.status = "converting";
    render(<OfficePdfViewer {...file} fallback={fallback} />);
    expect(screen.getByText("fileEditor.officePdf.converting")).toBeTruthy();
    expect(screen.queryByTestId("fallback-viewer")).toBeNull();
    expect(screen.queryByTestId("pdf-viewer")).toBeNull();
  });

  it("treats the very first render like the converting state", () => {
    h.convState.status = "idle";
    render(<OfficePdfViewer {...file} fallback={fallback} />);
    expect(screen.getByText("fileEditor.officePdf.converting")).toBeTruthy();
    expect(screen.queryByTestId("fallback-viewer")).toBeNull();
  });

  it("degrades to the fallback with an explanatory banner when no engine exists", () => {
    h.convState.status = "unavailable";
    render(<OfficePdfViewer {...file} fallback={fallback} />);
    expect(screen.getByTestId("fallback-viewer")).toBeTruthy();
    expect(screen.queryByTestId("pdf-viewer")).toBeNull();
    expect(screen.getByText("fileEditor.officePdf.engineMissing")).toBeTruthy();
    expect(screen.getByText("fileEditor.officePdf.installHint")).toBeTruthy();
    // Not an error: no retry affordance on a machine that cannot convert.
    expect(screen.queryByText("fileEditor.docViewer.retry")).toBeNull();
  });

  it("degrades with the failure reason and a working retry when conversion failed", () => {
    h.convState.status = "failed";
    h.convState.error = "soffice exited with code 1";
    render(<OfficePdfViewer {...file} fallback={fallback} />);
    expect(screen.getByTestId("fallback-viewer")).toBeTruthy();
    expect(
      screen.getByText("fileEditor.officePdf.convertFailed: soffice exited with code 1"),
    ).toBeTruthy();
    fireEvent.click(screen.getByText("fileEditor.docViewer.retry"));
    expect(h.convState.retry).toHaveBeenCalled();
  });

  it("forwards the chat hand-offs only to the PDF viewer", () => {
    // The panel wires the hand-offs once and this wrapper only routes them:
    // the JS fallback renderers have no print layout to offer, so promising
    // "send the agent the way this looks when printed" there would be a lie.
    const onAddRegionToChat = vi.fn();
    const onAddDocumentToChat = vi.fn();
    const handOffs = { addToChatDisabledTitle: "fileEditor.noChatTarget", onAddRegionToChat, onAddDocumentToChat };
    h.convState.status = "ready";
    h.convState.pdfBytes = new Uint8Array([0x25, 0x50, 0x44, 0x46]);
    render(<OfficePdfViewer {...file} {...handOffs} fallback={fallback} />);
    expect(h.pdfProps[0]).toMatchObject({ ...handOffs, fileName: "paper.docx" });
    // The tab's own addressing, not the wrapper's leftovers.
    expect(h.pdfProps[0]).not.toHaveProperty("fallback");
    expect(h.pdfProps[0]).not.toHaveProperty("size");
    cleanup();

    h.pdfProps.length = 0;
    h.convState.status = "unavailable";
    render(<OfficePdfViewer {...file} {...handOffs} fallback={fallback} />);
    expect(h.pdfProps).toHaveLength(0);
  });
});
