/**
 * CopyButton.test.tsx — the shared copy affordance.
 *
 * The five pre-existing copy buttons each reimplemented this logic and
 * drifted apart: `ErrorBox` reset after 1500 ms while the rest used 2000 ms,
 * two of them hard-coded the English "Copied" instead of reading i18n, and
 * none cleared the reset timer on unmount. These tests pin the three
 * behaviours that were previously unspecified, so consolidating onto one
 * component cannot silently reintroduce any of them.
 */

import { describe, expect, it, vi, beforeEach, afterEach } from "vitest";

// Locale-independent: t() echoes the key so assertions pin exact hooks and
// the "translated" label is verifiable.
vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));
import { act, fireEvent, render, screen } from "@testing-library/react";
import { CopyButton } from "./CopyButton";

const writeText = vi.fn();

beforeEach(() => {
  writeText.mockReset();
  writeText.mockResolvedValue(undefined);
  Object.assign(navigator, { clipboard: { writeText } });
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
});

describe("CopyButton — writes to the clipboard", () => {
  it("copies the exact text it was given", async () => {
    render(<CopyButton text="段落 [[r1]]" testId="copy" />);
    await act(async () => {
      fireEvent.click(screen.getByTestId("copy"));
    });
    expect(writeText).toHaveBeenCalledWith("段落 [[r1]]");
  });

  // An empty clipboard would produce a "Copied" lie on the surface.
  it("is inert and never claims success when there is no text", () => {
    render(<CopyButton text="" testId="copy" />);
    const btn = screen.getByTestId("copy");
    expect(btn.hasAttribute("disabled")).toBe(true);
    fireEvent.click(btn);
    expect(writeText).not.toHaveBeenCalled();
  });

  // Sandboxed WebViews reject the async Clipboard API; the user must still
  // get the text out rather than losing the click.
  it("falls back to execCommand when the Clipboard API rejects", async () => {
    writeText.mockRejectedValueOnce(new Error("not allowed"));
    const exec = vi.fn();
    Object.assign(document, { execCommand: exec });
    render(<CopyButton text="fallback body" testId="copy" />);
    await act(async () => {
      fireEvent.click(screen.getByTestId("copy"));
    });
    expect(exec).toHaveBeenCalledWith("copy");
    // And it still reports the copy as done.
    expect(screen.getByTestId("copy").dataset.copied).toBe("true");
  });
});

describe("CopyButton — copied confirmation", () => {
  it("shows the translated copied state, then reverts on the shared timer", async () => {
    render(<CopyButton text="body" testId="copy" />);
    const btn = screen.getByTestId("copy");

    expect(btn.dataset.copied).toBeUndefined();
    expect(btn.textContent).toContain("common.copy");

    await act(async () => {
      fireEvent.click(btn);
    });
    expect(btn.dataset.copied).toBe("true");
    expect(btn.textContent).toContain("common.copied");

    // One app-wide reset constant — the old 1500/2000 split had no basis.
    act(() => {
      vi.advanceTimersByTime(2000);
    });
    expect(btn.dataset.copied).toBeUndefined();
    expect(btn.textContent).toContain("common.copy");
  });

  // Clicking twice must restart the window, not let the first timer wipe
  // the second confirmation early.
  it("restarts the timer on a repeat click", async () => {
    render(<CopyButton text="body" testId="copy" />);
    const btn = screen.getByTestId("copy");

    await act(async () => {
      fireEvent.click(btn);
    });
    act(() => {
      vi.advanceTimersByTime(1500);
    });
    await act(async () => {
      fireEvent.click(btn);
    });
    // The first click's timer would have expired at t=2000; the second
    // click reset it, so the state must still be showing.
    act(() => {
      vi.advanceTimersByTime(600);
    });
    expect(btn.dataset.copied).toBe("true");
  });

  // A pending reset must not write into a component that has gone away —
  // copy then close the dialog used to leave a dangling timer.
  it("clears the pending timer on unmount", async () => {
    const clearSpy = vi.spyOn(globalThis, "clearTimeout");
    const { unmount } = render(<CopyButton text="body" testId="copy" />);
    await act(async () => {
      fireEvent.click(screen.getByTestId("copy"));
    });
    unmount();
    expect(clearSpy).toHaveBeenCalled();
    clearSpy.mockRestore();
  });
});

describe("CopyButton — labelling and shape", () => {
  it("prefers an explicit aria-label over the generic one", () => {
    render(<CopyButton text="body" ariaLabel="Copy summary" testId="copy" />);
    expect(screen.getByTestId("copy").getAttribute("aria-label")).toBe("Copy summary");
  });

  // Icon-only drops the visible text, so the accessible name is the only
  // thing left — it must still be set.
  it("keeps the accessible name when the label is hidden", () => {
    render(<CopyButton text="body" ariaLabel="Copy summary" iconOnly testId="copy" />);
    const btn = screen.getByTestId("copy");
    expect(btn.textContent).toBe("");
    expect(btn.getAttribute("aria-label")).toBe("Copy summary");
  });

  it("drops the title once copied so the tooltip does not contradict the label", async () => {
    render(<CopyButton text="body" ariaLabel="Copy summary" testId="copy" />);
    const btn = screen.getByTestId("copy");
    expect(btn.getAttribute("title")).toBe("Copy summary");
    await act(async () => {
      fireEvent.click(btn);
    });
    expect(btn.getAttribute("title")).toBeNull();
  });
});
