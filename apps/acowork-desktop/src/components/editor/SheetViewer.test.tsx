/**
 * SheetViewer — minimal render test (SheetJS JSON → grid rows).
 *
 * The sheet parsing itself is SheetJS' job; this pins the component's own
 * contract from the preview plan:
 *   - blob bytes are handed to `XLSX.read` (array mode for xlsx/xls,
 *     string mode for csv) and `sheet_to_json(header:1)` output becomes
 *     one grid row per entry via `VirtualRowList`;
 *   - multiple sheets surface as switchable tabs;
 *   - a parse failure degrades to the in-body error line instead of
 *     crashing the tab.
 * `xlsx` is mocked (the real one is a ~900 KB CJS parser; dynamic imports
 * are intercepted by `vi.mock` too), and the blob feed is the stubbed hook.
 */
import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";

const h = vi.hoisted(() => ({
  hookState: {
    blob: null as Blob | null,
    blobUrl: null as string | null,
    mime: null as string | null,
    loading: false,
    error: null as string | null,
    retry: vi.fn(),
  },
  read: vi.fn(),
  sheetToJson: vi.fn(),
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
vi.mock("xlsx", () => ({
  read: h.read,
  utils: { sheet_to_json: h.sheetToJson },
}));

import SheetViewer from "./SheetViewer";

const file = {
  agentId: "a1",
  workspaceId: "ws1",
  relPath: "docs/budget.xlsx",
  fileName: "budget.xlsx",
};

/** Workbook-shaped stand-in: `read` returns names + opaque sheet marks. */
function workbook(...names: string[]) {
  return {
    SheetNames: names,
    Sheets: Object.fromEntries(names.map((n) => [n, { __sheet: n }])),
  };
}

function feedBlob(content = "xlsx-bytes") {
  const blob = new Blob([content], { type: "application/octet-stream" });
  // jsdom 25's Blob has no `text()` (setup.ts only shims `arrayBuffer`);
  // the csv branch calls it exactly as browsers offer it.
  if (typeof (blob as Blob & { text?: unknown }).text !== "function") {
    (blob as Blob & { text: () => Promise<string> }).text = () => Promise.resolve(content);
  }
  h.hookState = {
    blob,
    blobUrl: null,
    mime: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    loading: false,
    error: null,
    retry: vi.fn(),
  };
}

beforeEach(() => {
  feedBlob();
  h.read.mockReset();
  h.sheetToJson.mockReset();
});

describe("SheetViewer", () => {
  it("sanity: dynamic xlsx import resolves to the mocked fns", async () => {
    const XLSX = await import("xlsx");
    expect(XLSX.read).toBe(h.read);
    expect((XLSX.utils as { sheet_to_json: unknown }).sheet_to_json).toBe(h.sheetToJson);
  });

  it("renders sheet_to_json rows as a grid (header row included)", async () => {
    // SheetJS `read` is SYNCHRONOUS in the real library — the mocks must
    // return values, not promises.
    h.read.mockReturnValue(workbook("Sheet1"));
    h.sheetToJson.mockReturnValue([
      ["Name", "Amount"],
      ["Rent", "1200"],
      ["Food", "300"],
    ]);
    render(<SheetViewer {...file} />);
    expect(await screen.findByText("Rent")).toBeTruthy();
    expect(screen.getByText("Amount")).toBeTruthy();
    expect(screen.getByText("300")).toBeTruthy();
    // xlsx path parses array bytes, not text.
    const [data, opts] = h.read.mock.calls[0];
    expect(data).toBeInstanceOf(ArrayBuffer);
    expect(opts).toEqual({ type: "array" });
  });

  it("reads csv blobs as string", async () => {
    feedBlob("a,b\n1,2");
    h.read.mockReturnValue(workbook("Sheet1"));
    h.sheetToJson.mockReturnValue([["a", "b"], ["1", "2"]]);
    render(<SheetViewer {...file} fileName="data.csv" relPath="docs/data.csv" />);
    expect(await screen.findByText("a")).toBeTruthy();
    const [data, opts] = h.read.mock.calls[0];
    expect(data).toBe("a,b\n1,2");
    expect(opts).toEqual({ type: "string" });
  });

  it("surfaces multiple sheets as switchable tabs", async () => {
    h.read.mockReturnValue(workbook("Q1", "Q2"));
    h.sheetToJson.mockImplementation((ws: { __sheet: string }) =>
      ws.__sheet === "Q1" ? [["q1-cell"]] : [["q2-cell"]],
    );
    render(<SheetViewer {...file} />);
    const q2 = await screen.findByRole("button", { name: "Q2" });
    // findBy, not get: the tab strip and the grid body can land in
    // different commits — a bare getByText raced the grid mount.
    expect(await screen.findByText("q1-cell")).toBeTruthy();
    fireEvent.click(q2);
    expect(await screen.findByText("q2-cell")).toBeTruthy();
  });

  it("degrades a parse failure to the in-body error line", async () => {
    h.read.mockImplementation(() => {
      throw new Error("bad zip");
    });
    render(<SheetViewer {...file} />);
    expect(await screen.findByText("fileEditor.sheetViewer.parseFailed")).toBeTruthy();
    expect(screen.getByText("Error: bad zip")).toBeTruthy();
  });
});
