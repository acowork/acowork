/**
 * Self-check for `AuthedImage` (ADR-076 follow-up).
 *
 * The bug this locks down: under `multi_user` a bare `<img src="…/avatar-file">`
 * cannot send the bearer token, so every custom avatar came back 401 and the
 * UI silently showed the builtin icon instead. `AuthedImage` must therefore
 * (a) resolve through `fetch` and render the resulting blob URL, and
 * (b) hand back `fallback` for a falsy src *and* for a failed fetch.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { cleanup, render, waitFor } from "@testing-library/react";
import { AuthedImage, clearAuthedImageCache } from "./AuthedImage";
import { log } from "../../lib/logger";

let originalFetch: typeof globalThis.fetch;
let originalCreate: unknown;
let originalRevoke: unknown;

const AVATAR_URL =
  "http://gw.test/api/agents/a/avatar-file?path=assets/avatar-01.png";

beforeEach(() => {
  originalFetch = globalThis.fetch;
  originalCreate = URL.createObjectURL;
  originalRevoke = URL.revokeObjectURL;
  // jsdom ships no blob-URL implementation; the component only needs a
  // stable string back and a revoke it can call on unmount.
  URL.createObjectURL = vi.fn(() => "blob:avatar");
  URL.revokeObjectURL = vi.fn();
  // The cache is module-level (that is what lets a remount paint on its
  // first frame), so it has to be dropped between cases — otherwise the
  // second render of the same url is a cache hit and never fetches, which
  // would silently disarm the failure-path assertions below.
  clearAuthedImageCache();
});

afterEach(() => {
  // Unmount while the blob stubs are still installed — the component may
  // release its cache entry in the effect cleanup.
  cleanup();
  clearAuthedImageCache();
  globalThis.fetch = originalFetch;
  URL.createObjectURL = originalCreate as typeof URL.createObjectURL;
  URL.revokeObjectURL = originalRevoke as typeof URL.revokeObjectURL;
});

function okResponse(): Response {
  return {
    ok: true,
    status: 200,
    blob: async () => new Blob(["bytes"]),
  } as unknown as Response;
}

describe("AuthedImage", () => {
  it("fetches the url and renders the blob it resolves to", async () => {
    const fetchSpy = vi.fn(async () => okResponse());
    globalThis.fetch = fetchSpy as unknown as typeof globalThis.fetch;

    const { container } = render(
      <AuthedImage src="http://gw.test/api/agents/a/avatar-file?path=assets/avatar-01.png" alt="a" />,
    );

    await waitFor(() => {
      expect(container.querySelector("img")?.getAttribute("src")).toBe("blob:avatar");
    });
    expect(fetchSpy).toHaveBeenCalledWith(
      "http://gw.test/api/agents/a/avatar-file?path=assets/avatar-01.png",
    );
  });

  it("renders the fallback AND warns on a non-2xx fetch (the 401 path)", async () => {
    const warnSpy = vi.spyOn(log, "warn").mockImplementation(() => {});
    globalThis.fetch = vi.fn(async () => ({
      ok: false,
      status: 401,
      blob: async () => new Blob([]),
    })) as unknown as typeof globalThis.fetch;

    const { container, getByText } = render(
      <AuthedImage src="http://gw.test/api/agents/a/avatar-file?path=assets/avatar-01.png" fallback={<span>builtin</span>} />,
    );

    expect(getByText("builtin")).toBeTruthy();
    expect(container.querySelector("img")).toBeNull();
    // A silent fallback is indistinguishable from "no avatar configured",
    // which is how the 401 regression hid in the first place. The fetch
    // is async, so wait for it — the old assertions above pass even when
    // the failure path is completely silent.
    await waitFor(() => {
      expect(warnSpy).toHaveBeenCalledWith(
        expect.stringContaining("status=401"),
      );
    });
    warnSpy.mockRestore();
  });

  it("renders the fallback for a falsy src without touching the network", () => {
    const fetchSpy = vi.fn();
    globalThis.fetch = fetchSpy as unknown as typeof globalThis.fetch;

    const { getByText } = render(<AuthedImage src={null} fallback={<span>builtin</span>} />);

    expect(getByText("builtin")).toBeTruthy();
    expect(fetchSpy).not.toHaveBeenCalled();
  });
});

/**
 * The remount flash (the reported bug).
 *
 * The setup panel unmounts and remounts on every tab switch. Without a
 * cache the second mount re-fetched the bytes and rendered `fallback` for a
 * frame or two, so the builtin icon visibly flashed before the real avatar
 * — read as "the avatar I configured is being ignored".
 *
 * The assertion is deliberately SYNCHRONOUS and takes place before any
 * await: a fallback that merely resolves one microtask later would still
 * show up in a `waitFor`-based test, which is exactly why the original
 * shipped the bug unnoticed.
 */
describe("AuthedImage remount does not flash its fallback", () => {
  it("paints the cached image on the first frame of a remount", async () => {
    const fetchSpy = vi.fn(async () => okResponse());
    globalThis.fetch = fetchSpy as unknown as typeof globalThis.fetch;

    const first = render(
      <AuthedImage src={AVATAR_URL} fallback={<span>builtin</span>} />,
    );
    // Cold mount: bytes are in flight, fallback is the honest answer.
    expect(first.getByText("builtin")).toBeTruthy();

    await waitFor(() => {
      expect(first.container.querySelector("img")?.getAttribute("src")).toBe("blob:avatar");
    });
    first.unmount();

    // Remount (panel reopened). Synchronous read: the image must already be
    // in the DOM, with no fallback frame in between.
    const second = render(
      <AuthedImage src={AVATAR_URL} fallback={<span>builtin</span>} />,
    );
    expect(second.queryByText("builtin")).toBeNull();
    expect(second.container.querySelector("img")?.getAttribute("src")).toBe("blob:avatar");
    // And it cost no second network round-trip.
    expect(fetchSpy).toHaveBeenCalledTimes(1);
  });

  it("keeps entries separate per url", async () => {
    const fetchSpy = vi.fn(async () => okResponse());
    globalThis.fetch = fetchSpy as unknown as typeof globalThis.fetch;

    const other = AVATAR_URL.replace("avatar-01", "avatar-02");
    const first = render(<AuthedImage src={AVATAR_URL} fallback={<span>a</span>} />);
    await waitFor(() => expect(first.container.querySelector("img")).toBeTruthy());
    first.unmount();

    const second = render(<AuthedImage src={other} fallback={<span>b</span>} />);
    // A different file is a different key — it must still fetch and must
    // not inherit the other avatar's bytes.
    expect(second.getByText("b")).toBeTruthy();
    await waitFor(() => {
      expect(second.container.querySelector("img")?.getAttribute("src")).toBe("blob:avatar");
    });
    expect(fetchSpy).toHaveBeenCalledTimes(2);
  });

  it("does not cache a failure, so a later mount retries", async () => {
    const warnSpy = vi.spyOn(log, "warn").mockImplementation(() => {});
    globalThis.fetch = vi.fn(async () => ({
      ok: false,
      status: 401,
      blob: async () => new Blob([]),
    })) as unknown as typeof globalThis.fetch;

    const first = render(
      <AuthedImage src={AVATAR_URL} fallback={<span>builtin</span>} />,
    );
    expect(first.getByText("builtin")).toBeTruthy();
    await waitFor(() => expect(warnSpy).toHaveBeenCalled());
    first.unmount();

    // Re-login restores the token that 401'd; the retry has to happen.
    globalThis.fetch = vi.fn(async () => okResponse()) as unknown as typeof globalThis.fetch;
    const second = render(
      <AuthedImage src={AVATAR_URL} fallback={<span>builtin</span>} />,
    );
    await waitFor(() => {
      expect(second.container.querySelector("img")?.getAttribute("src")).toBe("blob:avatar");
    });
    warnSpy.mockRestore();
  });

  it("clearAuthedImageCache drops entries and revokes their object URLs", async () => {
    globalThis.fetch = vi.fn(async () => okResponse()) as unknown as typeof globalThis.fetch;
    const view = render(<AuthedImage src={AVATAR_URL} />);
    await waitFor(() => expect(view.container.querySelector("img")).toBeTruthy());

    const revokeSpy = vi.spyOn(URL, "revokeObjectURL").mockImplementation(() => {});
    clearAuthedImageCache();
    expect(revokeSpy).toHaveBeenCalledWith("blob:avatar");
    revokeSpy.mockRestore();
  });
});
