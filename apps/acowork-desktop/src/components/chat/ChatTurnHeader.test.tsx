// @vitest-environment jsdom
/**
 * ChatTurnHeader — the claim-window identity line (ADR-106 revision
 * 2026-11-08). What is pinned here is the component's own contract: faces
 * only with names to wear, one stacked header for a fan-out (one send is one
 * fact), and the takeover hook (`data-turn-header`) the surfaces and the
 * handover rule key on. The degradation decision (unresolved identity → the
 * centered line) belongs to the surfaces and is pinned in their suites.
 */
import { render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { ChatTurnHeader, type TurnIdentity } from "./ChatTurnHeader";

// The component reads the locale for its name list, nothing else from i18n.
vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key, i18n: { language: "zh-CN" } }),
}));

vi.mock("../common/AgentAvatar", () => ({
  AgentAvatar: ({ agentId }: { agentId: string }) => (
    <span data-testid={`avatar-${agentId}`} />
  ),
}));

const A: TurnIdentity = { id: "agent-a", displayName: "Agent A" };
const B: TurnIdentity = { id: "agent-b", displayName: "Agent B" };

describe("ChatTurnHeader", () => {
  it("renders the responder's face, name and the claim's word", () => {
    render(<ChatTurnHeader identities={[A]} text="chatView.notice.pendingTurnGeneric" />);
    const header = document.querySelector("[data-turn-header]");
    expect(header).toBeTruthy();
    expect(screen.getByTestId("avatar-agent-a")).toBeTruthy();
    expect(header!.textContent).toContain("Agent A");
    expect(header!.textContent).toContain("chatView.notice.pendingTurnGeneric");
    // It speaks for a turn still in flight — the same live role the centered
    // line carries, so assistive tech hears one announcement either way.
    expect(header!.getAttribute("role")).toBe("status");
  });

  it("a fan-out wears ONE stacked header naming every responder", () => {
    render(<ChatTurnHeader identities={[A, B]} text="working" />);
    expect(document.querySelectorAll("[data-turn-header]")).toHaveLength(1);
    expect(screen.getByTestId("avatar-agent-a")).toBeTruthy();
    expect(screen.getByTestId("avatar-agent-b")).toBeTruthy();
    // The names are a locale's list, not a hand-picked separator: zh reads
    // "A、B" (design 33 §5 rule 4).
    expect(document.querySelector("[data-turn-header]")!.textContent).toContain("Agent A、Agent B");
    // And it is a STACK, not a row: every face after the first is pulled
    // under the one before it, and the earlier face paints on top. The
    // comment that claims the overlap is what this pins — an overlap-less
    // row widens the header's left column and breaks the anchor the block
    // header takes over.
    const first = screen.getByTestId("avatar-agent-a").parentElement as HTMLElement;
    const second = screen.getByTestId("avatar-agent-b").parentElement as HTMLElement;
    expect(second.style.marginLeft).toBe("-24px");
    expect(Number(first.style.zIndex)).toBeGreaterThan(Number(second.style.zIndex));
  });

  it("no identities, no header — a faceless avatar is the centered line's job", () => {
    const { container } = render(<ChatTurnHeader identities={[]} text="working" />);
    expect(container.innerHTML).toBe("");
  });
});
