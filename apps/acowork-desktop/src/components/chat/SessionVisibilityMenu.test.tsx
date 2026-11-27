/**
 * The composer strip's read-visibility control is a MENU over the same two
 * states the icon toggle uses. The mapping is the whole risk: `Lock` must
 * mean private and `Unlock` public, the checked row must match the session's
 * current visibility, and a read-only session must not be openable.
 */
import { beforeEach, describe, expect, it, vi } from "vitest";

const h = vi.hoisted(() => ({
  setSessionVisibility: vi.fn(async () => {}),
  session: { session_id: "s1", visibility: "private" as "public" | "private", can_write: true },
}));

vi.mock("react-i18next", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));
vi.mock("../../stores/agentStore", () => {
  const state = {
    agents: { a1: { sessions: [h.session] } },
    fetchSessions: vi.fn(async () => {}),
    setSessionVisibility: h.setSessionVisibility,
  };
  const store = (selector: (s: typeof state) => unknown) => selector(state);
  (store as unknown as { getState: () => typeof state }).getState = () => state;
  return { useAgentStore: store };
});
vi.mock("../common/ToastProvider", () => ({
  useToast: () => ({ addToast: vi.fn() }),
  showToast: vi.fn(),
}));

import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { SessionVisibilityMenu } from "./SessionVisibilityToggle";

beforeEach(() => {
  cleanup();
  h.setSessionVisibility.mockClear();
  h.session.visibility = "private";
  h.session.can_write = true;
});

describe("SessionVisibilityMenu", () => {
  it("labels a private session 'lock' and writes 'public' from the unlock row", () => {
    render(<SessionVisibilityMenu agentId="a1" sessionId="s1" />);

    // Trigger reads the current state, not the action.
    const trigger = screen.getByRole("button", { name: /sessionVisibility.lock/ });
    fireEvent.click(trigger);

    const lock = screen.getByRole("menuitemradio", { name: /sessionVisibility.lock/ });
    const unlock = screen.getByRole("menuitemradio", { name: /sessionVisibility.unlock/ });
    expect(lock.getAttribute("aria-checked")).toBe("true");
    expect(unlock.getAttribute("aria-checked")).toBe("false");

    fireEvent.click(unlock);
    expect(h.setSessionVisibility).toHaveBeenCalledWith("a1", "s1", "public");
  });

  it("leaves the session alone when the active row is clicked", () => {
    render(<SessionVisibilityMenu agentId="a1" sessionId="s1" />);
    fireEvent.click(screen.getByRole("button", { name: /sessionVisibility.lock/ }));
    fireEvent.click(screen.getByRole("menuitemradio", { name: /sessionVisibility.lock/ }));
    expect(h.setSessionVisibility).not.toHaveBeenCalled();
  });

  it("shows a public session as 'unlock' and blocks a read-only session", () => {
    h.session.visibility = "public";
    const { unmount } = render(<SessionVisibilityMenu agentId="a1" sessionId="s1" />);
    const trigger = screen.getByRole("button", { name: /sessionVisibility.unlock/ });
    expect((trigger as HTMLButtonElement).disabled).toBe(false);
    unmount();

    h.session.can_write = false;
    render(<SessionVisibilityMenu agentId="a1" sessionId="s1" />);
    expect(
      (screen.getByRole("button", { name: /sessionVisibility.unlock/ }) as HTMLButtonElement)
        .disabled,
    ).toBe(true);
  });
});