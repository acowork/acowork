/**
 * DocxViewer — the wrapper-repair regression.
 *
 * docx-preview centers pages with `align-items: center`; in a pane
 * narrower than the page that clips the left margin beyond scroll reach
 * (the first-report bug: "字都截断了"). Mounting the viewer must hand
 * the rendered wrapper the safe-center / fit treatment — asserted here
 * against a stubbed renderAsync that reproduces the library's DOM shape.
 * jsdom has no layout, so the zoom branch measures 0 and stays out of
 * the way; the alignItems repair still runs (it is layout-independent).
 */
import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen } from "@testing-library/react";

const h = vi.hoisted(() => ({
  hookState: {
    blob: null as Blob | null,
    blobUrl: null as string | null,
    mime: null as string | null,
    loading: false,
    error: null as string | null,
    retry: vi.fn(),
  },
  renderAsync: vi.fn(),
}));

vi.mock("../../hooks/useWorkspaceFileBlob", () => ({
  useWorkspaceFileBlob: () => h.hookState,
}));
vi.mock("../../lib/workspaceFileBlob", () => ({
  saveWorkspaceFileToDisk: vi.fn(() => Promise.resolve(true)),
  openWorkspaceFileExternally: vi.fn(() => Promise.resolve()),
}));
vi.mock("../common/ToastProvider", () => ({ showToast: vi.fn() }));
vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({
    t: (key: string, params?: Record<string, unknown>) =>
      params ? `${key} ${JSON.stringify(params)}` : key,
  }),
}));
vi.mock("docx-preview", () => ({
  renderAsync: (
    _blob: Blob,
    container: HTMLElement,
    _style: unknown,
    _options: unknown,
  ) => {
    // Reproduce the library's output shape: wrapper > section, plus the
    // stylesheet it injects at document level.
    const wrapper = document.createElement("div");
    wrapper.className = "docx-wrapper";
    const page = document.createElement("section");
    page.className = "docx";
    wrapper.appendChild(page);
    container.appendChild(wrapper);
    return Promise.resolve();
  },
}));

import DocxViewer from "./DocxViewer";

beforeEach(() => {
  h.renderAsync.mockClear();
  h.hookState = {
    blob: new Blob(["docx-bytes"]),
    blobUrl: null,
    mime: "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    loading: false,
    error: null,
    retry: vi.fn(),
  };
});

describe("DocxViewer wrapper repair", () => {
  it("re-centers the rendered pages safely after renderAsync", async () => {
    render(
      <DocxViewer
        agentId="a1"
        workspaceId="ws1"
        relPath="docs/paper.docx"
        fileName="paper.docx"
      />,
    );
    const container = await screen.findByTestId("docx-preview-container");
    const wrapper = container.querySelector<HTMLElement>(".docx-wrapper");
    expect(wrapper).toBeTruthy();
    // Never the bare `center` again — either the `safe center` upgrade or
    // the flex-start fallback (jsdom's CSSOM rejects `safe`).
    expect(wrapper!.style.alignItems).not.toBe("center");
    expect(wrapper!.style.alignItems).not.toBe("");
  });

  it("does not zoom when the page width cannot be measured (jsdom)", async () => {
    render(
      <DocxViewer
        agentId="a1"
        workspaceId="ws1"
        relPath="docs/paper2.docx"
        fileName="paper2.docx"
      />,
    );
    const container = await screen.findByTestId("docx-preview-container");
    const wrapper = container.querySelector<HTMLElement>(".docx-wrapper")!;
    // No layout in jsdom → the fit branch must bail out, not set a bogus
    // zoom that would shrink a correctly-sized document. (jsdom's CSSOM
    // has no `zoom` property, so an untouched one reads back undefined.)
    expect(wrapper.style.zoom || "").toBe("");
  });
});
