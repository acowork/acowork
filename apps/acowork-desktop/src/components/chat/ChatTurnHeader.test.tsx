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

  it("paints the agent's role under the name, on the block header's own tier", () => {
    // The takeover is a swap of the state line, not a re-layout: the block
    // header shows name + role, so the claim window has to show the role
    // already or the handover inserts a line under the name and shifts the
    // column. Tier and order are the assertion; the block's own suite pins
    // what the role's tier IS.
    const withRole: TurnIdentity = { ...A, role: "资深工程师" };
    render(<ChatTurnHeader identities={[withRole]} text="working" />);
    const header = document.querySelector("[data-turn-header]")!;
    const text = header.textContent ?? "";
    const nameAt = text.indexOf("Agent A");
    const roleAt = text.indexOf("资深工程师");
    const wordAt = text.indexOf("working");
    expect(nameAt).toBeGreaterThanOrEqual(0);
    expect(roleAt).toBeGreaterThan(nameAt);
    expect(wordAt).toBeGreaterThan(roleAt);
    expect(screen.getByText("资深工程师").className).toContain("text-10");
  });

  it("a claim carrying no role paints no line, not an empty one", () => {
    render(<ChatTurnHeader identities={[A]} text="working" />);
    const header = document.querySelector("[data-turn-header]")!;
    expect(header.querySelectorAll(".text-10")).toHaveLength(0);
  });

  it("a fan-out leaves the role out — a joined role list names nobody", () => {
    // One send is one fact, and a fan-out's identity line is already a list of
    // names; joining the roles as well ("A工程师、B工程师") would state a fact
    // about no single responder. The faces say "several" on their own.
    render(
      <ChatTurnHeader
        identities={[{ ...A, role: "资深工程师" }, { ...B, role: "测试工程师" }]}
        text="working"
      />,
    );
    const header = document.querySelector("[data-turn-header]")!;
    expect(header.textContent).not.toContain("资深工程师");
    expect(header.textContent).not.toContain("测试工程师");
    expect(header.querySelectorAll(".text-10")).toHaveLength(0);
  });

  it("no identities, no header — a faceless avatar is the centered line's job", () => {
    const { container } = render(<ChatTurnHeader identities={[]} text="working" />);
    expect(container.innerHTML).toBe("");
  });

  it("puts the claim's word in the content column, not under the name and role", () => {
    // The word used to be a THIRD line inside the identity column. Two things
    // went wrong there: the column grew and the `items-center` row pulled the
    // name and role up off the avatar's axis, and when the block landed the
    // word had to jump sideways into the block's content column — the
    // re-layout ADR-106 决策四 forbids ("接管只消失状态词，位置不动").
    render(
      <ChatTurnHeader identities={[{ ...A, role: "资深工程师" }]} text="working" />,
    );
    const header = document.querySelector("[data-turn-header]")!;
    const word = screen.getByText("working");

    // 1. The identity column — the one holding name and role — does not
    //    contain the word. `closest` walks up from the role, so this fails if
    //    the word is ever nested there again.
    const identityColumn = screen.getByText("资深工程师").parentElement!;
    expect(identityColumn.textContent).toContain("Agent A");
    expect(identityColumn.textContent).not.toContain("working");

    // 2. The word starts on the block's own content column: a row whose first
    //    child is the same 40px spacer `ChatExecutionBlock` pads its body
    //    with, and the word row is a SIBLING of the identity row rather than
    //    a child of it.
    const wordRow = word.parentElement!.parentElement!;
    const spacer = wordRow.firstElementChild as HTMLElement;
    expect(spacer.className).toContain("w-10");
    expect(spacer.nextElementSibling?.contains(word)).toBe(true);
    expect(identityColumn.contains(wordRow)).toBe(false);
    expect(header.contains(wordRow)).toBe(true);
  });
});
