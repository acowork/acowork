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
