// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { act, renderHook } from "@testing-library/react";
import { useUploadingFiles } from "./uploadProgress";

/** The tray is the only evidence a large upload is moving at all. What it
 *  must never do is keep a chip after the upload ended (a permanent 40% is
 *  worse than no chip) or move the wrong chip's bytes. */
describe("useUploadingFiles", () => {
  const fileOf = (name: string, size: number) =>
    new File([new Uint8Array(size)], name, { type: "video/mp4" });

  it("tracks one file from picked to landed", () => {
    const { result } = renderHook(() => useUploadingFiles());
    const file = fileOf("movie.mp4", 3000);

    let tracker!: ReturnType<typeof result.current.begin>;
    act(() => {
      tracker = result.current.begin(file);
    });
    expect(result.current.uploadingFiles).toEqual([
      { key: expect.any(String), filename: "movie.mp4", sizeBytes: 3000 },
    ]);

    act(() => tracker.onProgress(1000));
    expect(result.current.uploadingFiles[0].sentBytes).toBe(1000);

    act(() => tracker.done());
    expect(result.current.uploadingFiles).toEqual([]);
  });

  it("aborts exactly the file the X was pressed on", () => {
    const { result } = renderHook(() => useUploadingFiles());

    let a!: ReturnType<typeof result.current.begin>;
    let b!: ReturnType<typeof result.current.begin>;
    act(() => {
      a = result.current.begin(fileOf("a.mp4", 10));
      b = result.current.begin(fileOf("a.mp4", 20));
    });
    const [firstKey, secondKey] = [result.current.uploadingFiles[0].key, result.current.uploadingFiles[1].key];

    act(() => result.current.cancel(firstKey));

    expect(a.signal.aborted).toBe(true);
    expect(b.signal.aborted).toBe(false);

    // The surface drops the chip in its own `finally`; nothing here hides it,
    // so a transfer that is somehow unkillable stays visible.
    act(() => a.done());
    expect(result.current.uploadingFiles.map((u) => u.key)).toEqual([secondKey]);
  });

  it("reports only the file that moved, not its namesake", () => {
    const { result } = renderHook(() => useUploadingFiles());

    let a!: ReturnType<typeof result.current.begin>;
    act(() => {
      a = result.current.begin(fileOf("a.mp4", 10));
      result.current.begin(fileOf("a.mp4", 20));
    });
    expect(result.current.uploadingFiles).toHaveLength(2);

    act(() => a.onProgress(5));
    const [first, second] = result.current.uploadingFiles;
    expect(first.sentBytes).toBe(5);
    expect(second.sentBytes).toBeUndefined();

    act(() => a.done());
    expect(result.current.uploadingFiles.map((u) => u.sizeBytes)).toEqual([20]);
  });
});