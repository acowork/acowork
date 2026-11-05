/**
 * `DeliveryAlertLine` — the live "this didn't go out" receipt (ADR-105 D5,
 * revision 2026-11-06).
 *
 * The contract has two halves, and both matter:
 *   1. it paints the failure while it is TRUE — a bounced send must not be
 *      silent (that was the original bug);
 *   2. it stops painting on its own clock — the receipt is news, not history, so
 *      nothing has to retract it and no reader sees it a week later.
 */
import { act, render } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { DeliveryAlertLine } from "./DeliveryAlertLine";

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({
    t: (key: string, params?: Record<string, string>) =>
      params && params.name !== undefined ? `${key}:${params.name}` : key,
  }),
}));

const alert = (expiresIn = 600) => ({
  target: "agent-a",
  error_code: "target_not_installed",
  expires_in: expiresIn,
});

afterEach(() => {
  vi.useRealTimers();
});

describe("DeliveryAlertLine", () => {
  it("paints the surface-owned wording in the sender's own column", () => {
    const { container } = render(<DeliveryAlertLine alert={alert()} className="px-4" />);
    const line = container.querySelector('[data-event-text="turn"]') as HTMLElement;
    expect(line.textContent).toBe("chatView.deliveryFailedNoTarget");
    // A fact about ONE message sits at that message's edge, never centered.
    expect(line.className).toContain("justify-end");
    expect(line.className).toContain("px-4"); // the surface gutter survives
    // Warn, not danger: retry-able, and the run broke nothing.
    expect(line.getAttribute("data-tone")).toBe("warn");
    // The arrow says "this refers back to what was just sent".
    expect(line.querySelector("svg")).toBeTruthy();
  });

  it("names the target when the surface can resolve one", () => {
    const { container } = render(<DeliveryAlertLine alert={alert()} name="Architect" />);
    expect(container.querySelector('[data-event-text="turn"]')?.textContent).toBe(
      "chatView.deliveryFailed:Architect",
    );
  });

  it("removes itself when its life runs out, with no re-read to trigger it", () => {
    // The whole point of not storing it: a window left open must not keep a dead
    // receipt on screen, and nothing arrives to take it away.
    vi.useFakeTimers();
    const { container } = render(<DeliveryAlertLine alert={alert(5)} />);
    expect(container.querySelector('[data-event-text="turn"]')).toBeTruthy();
    act(() => {
      vi.advanceTimersByTime(4999);
    });
    expect(container.querySelector('[data-event-text="turn"]')).toBeTruthy();
    act(() => {
      vi.advanceTimersByTime(1);
    });
    expect(container.querySelector('[data-event-text="turn"]')).toBeNull();
  });

  it("paints nothing at all when it arrives already expired", () => {
    const { container } = render(<DeliveryAlertLine alert={alert(0)} />);
    expect(container.innerHTML).toBe("");
  });

  it("a re-read reporting a shorter life re-arms the clock instead of restarting it", () => {
    vi.useFakeTimers();
    const { container, rerender } = render(<DeliveryAlertLine alert={alert(10)} />);
    act(() => {
      vi.advanceTimersByTime(4000);
    });
    // The same failure, read again 4s later: the service now says 6s are left.
    rerender(<DeliveryAlertLine alert={alert(6)} />);
    act(() => {
      vi.advanceTimersByTime(5999);
    });
    expect(container.querySelector('[data-event-text="turn"]')).toBeTruthy();
    act(() => {
      vi.advanceTimersByTime(1);
    });
    expect(container.querySelector('[data-event-text="turn"]')).toBeNull();
  });
});
