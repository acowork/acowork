/**
 * The composer strip's read-visibility control is a TOGGLE over the same two
 * states the icon toggle uses. The mapping is the whole risk: `Lock` must mean
 * private and `Unlock` public, one click must write the opposite of what is
 * shown, and a read-only session must not write at all.
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
import { SessionVisibilityButton } from "./SessionVisibilityToggle";

beforeEach(() => {
  cleanup();
  h.setSessionVisibility.mockClear();
  h.session.visibility = "private";
  h.session.can_write = true;
});

describe("SessionVisibilityButton", () => {
  it("labels a private session 'lock' and one click writes 'public'", () => {
    render(<SessionVisibilityButton agentId="a1" sessionId="s1" />);

    // The label reads the current state, not the action.
    const button = screen.getByRole("button", { name: /sessionVisibility.lock/ });
    expect(button.getAttribute("aria-pressed")).toBe("true");

    fireEvent.click(button);
    expect(h.setSessionVisibility).toHaveBeenCalledWith("a1", "s1", "public");
  });

  it("labels a public session 'unlock' and one click writes 'private'", () => {
    h.session.visibility = "public";
    render(<SessionVisibilityButton agentId="a1" sessionId="s1" />);

    const button = screen.getByRole("button", { name: /sessionVisibility.unlock/ });
    expect(button.getAttribute("aria-pressed")).toBe("false");

    fireEvent.click(button);
    expect(h.setSessionVisibility).toHaveBeenCalledWith("a1", "s1", "private");
  });

  // A viewer must still SEE the current state, but a click is not a write.
  it("renders disabled and writes nothing on a read-only session", () => {
    h.session.can_write = false;
    render(<SessionVisibilityButton agentId="a1" sessionId="s1" />);

    const button = screen.getByRole("button", {
      name: /sessionVisibility.lock/,
    }) as HTMLButtonElement;
    expect(button.disabled).toBe(true);

    fireEvent.click(button);
    expect(h.setSessionVisibility).not.toHaveBeenCalled();
  });
});