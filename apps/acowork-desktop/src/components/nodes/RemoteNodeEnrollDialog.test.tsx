/**
 * RemoteNodeEnrollDialog — the dialog must follow the app-wide three-zone
 * chrome: header, body, footer. It previously had no footer at all, so the
 * only way out was the ✕ in the header.
 */
import { describe, it, expect, vi } from "vitest";
import { render, screen } from "@testing-library/react";

// The panel mints a token asynchronously on mount; stub it so the test
// asserts the dialog chrome only.
vi.mock("./RemoteNodeEnrollPanel", () => ({
  RemoteNodeEnrollPanel: () => <div data-testid="panel" />,
}));

import { RemoteNodeEnrollDialog } from "./RemoteNodeEnrollDialog";

describe("RemoteNodeEnrollDialog", () => {
  it("renders a footer with a Close action", () => {
    render(<RemoteNodeEnrollDialog open onClose={() => {}} />);
    const footer = document.querySelector("footer");
    expect(footer).not.toBeNull();
    expect(footer?.className).toContain("min-h-[var(--ui-dialog-zone-h)]");
    expect(screen.getAllByRole("button", { name: /close/i }).length).toBeGreaterThan(0);
  });
});
