/**
 * Vitest global setup (wired via `test.setupFiles` in vitest.config.ts).
 *
 * jsdom ships no `ResizeObserver`, yet overlay-scroll surfaces (chat message
 * streams, the @ picker) now mount `OverlayScrollbar`, which observes the
 * viewport to size its thumb. A no-op stub keeps those mount tests running:
 * the observer never fires, metrics stay at their zero initial value, and the
 * custom thumb simply is not rendered — none of which those tests assert on.
 * Guarded so a future jsdom that implements it is left untouched.
 */

class ResizeObserverStub {
  observe(): void {}
  unobserve(): void {}
  disconnect(): void {}
}

if (typeof globalThis.ResizeObserver === "undefined") {
  globalThis.ResizeObserver = ResizeObserverStub as unknown as typeof ResizeObserver;
}

/**
 * jsdom's `Blob` has no `arrayBuffer()` in the version we run, yet the Create
 * Agent wizard reads a dropped skill ZIP with `await file.arrayBuffer()` —
 * exactly as the browser offers it. Without this shim every wizard test would
 * have to hand-roll a fake File, and the fake would be the thing under test.
 *
 * Implemented on top of `FileReader` (which jsdom does provide) so the bytes
 * come from the real Blob contents, not from a test fixture.
 */
if (typeof Blob !== "undefined" && typeof Blob.prototype.arrayBuffer !== "function") {
  Blob.prototype.arrayBuffer = function arrayBuffer(this: Blob): Promise<ArrayBuffer> {
    return new Promise<ArrayBuffer>((resolve, reject) => {
      const reader = new FileReader();
      reader.onload = () => resolve(reader.result as ArrayBuffer);
      reader.onerror = () => reject(reader.error);
      reader.readAsArrayBuffer(this);
    });
  };
}
