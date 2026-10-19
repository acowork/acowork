import { beforeEach, describe, expect, it, vi } from "vitest";

// `detectPaste` asks the Rust clipboard for real file paths — the mock
// controls what `get_clipboard_file_paths` returns per case.
const invokeMock = vi.fn(async (..._args: unknown[]): Promise<unknown> => []);
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args) as Promise<string[]>,
}));

import { detectPaste, type PasteOutcome } from "./pasteDetect";

/** Minimal DataTransfer stand-in: only the surfaces detectPaste reads. */
function fakeDataTransfer(opts: {
  text?: string;
  items?: Array<{ kind: string; type: string; file?: Blob }>;
  files?: File[];
}): DataTransfer {
  return {
    getData: (fmt: string) => (fmt === "text/plain" ? (opts.text ?? "") : ""),
    items: (opts.items ?? []).map((it) => ({
      kind: it.kind,
      type: it.type,
      getAsFile: () => it.file ?? null,
    })),
    files: opts.files ?? [],
  } as unknown as DataTransfer;
}

describe("detectPaste", () => {
  beforeEach(() => {
    invokeMock.mockResolvedValue([]);
    vi.restoreAllMocks();
  });

  it("prefers OS-confirmed file paths over the text payload", async () => {
    // Explorer puts the N paths into text/plain too — inserting it would
    // duplicate the upload UX. Files win, text is dropped.
    invokeMock.mockResolvedValue(["C:/tmp/a.pdf", "C:/tmp/b.png"]);
    const outcome = await detectPaste(
      fakeDataTransfer({ text: "C:/tmp/a.pdf\nC:/tmp/b.png" }),
    );
    expect(outcome).toEqual({
      kind: "files",
      paths: ["C:/tmp/a.pdf", "C:/tmp/b.png"],
    });
  });

  it("filters empty / non-string path entries from the Rust command", async () => {
    invokeMock.mockResolvedValue(["", 42, "C:/real.txt"]);
    const outcome = await detectPaste(fakeDataTransfer({ text: "C:/real.txt" }));
    expect(outcome).toEqual({ kind: "files", paths: ["C:/real.txt"] });
  });

  it("falls through to text when the OS clipboard holds no files", async () => {
    // The mis-classification guard: markdown that LOOKS like paths must
    // not trigger uploads — only the Rust clipboard says "real files".
    invokeMock.mockResolvedValue([]);
    const outcome = await detectPaste(
      fakeDataTransfer({ text: "/api/v1/users\nD:/data/file.txt" }),
    );
    expect(outcome).toEqual({
      kind: "text",
      text: "/api/v1/users\nD:/data/file.txt",
    });
  });

  it("takes an image from clipboardData.items before the async clipboard", async () => {
    const png = new Blob([new Uint8Array([1, 2, 3])], { type: "image/png" });
    const outcome = await detectPaste(
      fakeDataTransfer({ items: [{ kind: "file", type: "image/png", file: png }] }),
    );
    expect(outcome.kind).toBe("image");
    if (outcome.kind === "image") {
      expect(outcome.blob).toBe(png);
      expect(outcome.extension).toBe("png");
    }
  });

  it("surfaces name-only files as text", async () => {
    const f = new File(["x"], "photo.jpg", { type: "image/jpeg" });
    const outcome = await detectPaste(fakeDataTransfer({ files: [f] }));
    expect(outcome).toEqual({ kind: "fileNames", names: ["photo.jpg"] });
  });

  it("returns nothing for an empty clipboard", async () => {
    const outcome: PasteOutcome = await detectPaste(fakeDataTransfer({}));
    expect(outcome).toEqual({ kind: "nothing" });
  });

  it("uses navigator.clipboard.read when items carry no image", async () => {
    const png = new Blob([new Uint8Array([1])], { type: "image/png" });
    const readMock = vi.fn(async () => [
      {
        types: ["image/png"],
        getType: async () => png,
      },
    ]);
    Object.defineProperty(globalThis.navigator, "clipboard", {
      configurable: true,
      value: { read: readMock },
    });
    const outcome = await detectPaste(fakeDataTransfer({}));
    expect(outcome.kind).toBe("image");
    if (outcome.kind === "image") {
      expect(outcome.blob).toBe(png);
      expect(outcome.extension).toBe("png");
    }
  });
});
