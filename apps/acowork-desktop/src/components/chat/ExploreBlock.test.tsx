// @vitest-environment jsdom
/**
 * ExploreBlock — the folded "work detail" block of a chat transcript.
 *
 * Why this file exists — the failures the fold must not reintroduce:
 *   1. a transcript whose body is the agent's scratchpad. The explore block
 *      is a SUMMARY line; the reader opens it when they want the tool trace.
 *      A block that opens itself on every tool call turns the conversation
 *      into a wall of thinking (this is the 2026-11-11 default-folded rule,
 *      mirroring ADR-106's 2026-11-10 revision of the chat-side execution
 *      block);
 *   2. an authorization button behind a fold. A control the reader cannot
 *      see is a control they cannot answer (ADR-106 D8) — so a pending tool
 *      authorization opens the block on its own;
 *   3. a block that never closes again. The authorization was a question;
 *      once it is answered the block returns to its resting folded shape,
 *      otherwise the transcript keeps a permanently-open tool list;
 *   4. a fold that fights the reader. A block the reader opened BY HAND
 *      stays open — the authorization's answer must not close a panel the
 *      reader is still reading.
 *
 * The environment has no layout (jsdom), so the assertions are about the
 * rendered TREE (what is in the DOM), not about pixels.
 */
import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { ExploreBlock } from "./ExploreBlock";
import type { ChatMessage, ToolApprovalNeededEvent } from "../../lib/types";

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({
    t: (key: string, opts?: { count?: number }) =>
      opts && typeof opts.count === "number" ? `${key}:${opts.count}` : key,
  }),
}));

/** A tool_call row — the item an authorization is matched against. */
function toolCall(id: string, toolCallId = "call-1"): ChatMessage {
  return {
    id,
    type: "tool_call",
    content: JSON.stringify({ command: "ls -la" }),
    timestamp: 1000,
    toolName: "bash",
    toolCallId,
  };
}

/** A thought row — the "thinking" half of the explore block. */
function thought(id: string): ChatMessage {
  return {
    id,
    type: "thought",
    content: "let me look around",
    timestamp: 1000,
  };
}

/** An approval event bound to `call-1`. */
function approval(overrides: Partial<ToolApprovalNeededEvent> = {}): ToolApprovalNeededEvent {
  return {
    type: "tool_approval_needed",
    request_id: "req-1",
    tool_name: "bash",
    risk_level: "Medium",
    tool_call_id: "call-1",
    session_id: "sess-1",
    ...overrides,
  };
}

/** The block's header toggle. */
function header(container: HTMLElement): HTMLElement {
  return container.querySelector("button[aria-expanded]") as HTMLElement;
}

/** The Allow / Deny pair, or null when no authorization is on screen. */
function approvalButtons(container: HTMLElement): HTMLElement[] {
  return Array.from(container.querySelectorAll("button")).filter((b) =>
    b.textContent === "Allow" || b.textContent === "Deny",
  );
}

describe("ExploreBlock — folded by default", () => {
  it("rests folded, so the tool trace is not the transcript's body", () => {
    const { container } = render(
      <ExploreBlock items={[thought("t1"), toolCall("c1")]} isStreaming={false} />,
    );

    expect(header(container).getAttribute("aria-expanded")).toBe("false");
    // The tool row itself is NOT in the DOM while the block is folded.
    expect(container.textContent).not.toContain("bash");
  });

  it("does not open itself while the agent is exploring", () => {
    // A tool_call with no result yet = work in flight.  The old state
    // machine opened the block on exactly this signal; the folded default
    // must not.
    const { container } = render(
      <ExploreBlock items={[toolCall("c1")]} isStreaming={true} />,
    );

    expect(header(container).getAttribute("aria-expanded")).toBe("false");
  });

  it("opens on the reader's click and closes again on the next click", () => {
    const { container } = render(
      <ExploreBlock items={[thought("t1"), toolCall("c1")]} isStreaming={false} />,
    );

    fireEvent.click(header(container));
    expect(header(container).getAttribute("aria-expanded")).toBe("true");
    expect(container.textContent).toContain("bash");

    fireEvent.click(header(container));
    expect(header(container).getAttribute("aria-expanded")).toBe("false");
  });
});

describe("ExploreBlock — the authorization opens the block", () => {
  it("opens when a tool authorization is pending, so the button is answerable", () => {
    const { container } = render(
      <ExploreBlock
        items={[toolCall("c1")]}
        isStreaming={false}
        currentSessionId="sess-1"
        pendingApproval={{ "call-1": approval() }}
        onApprove={vi.fn()}
      />,
    );

    expect(header(container).getAttribute("aria-expanded")).toBe("true");
    expect(approvalButtons(container)).toHaveLength(2);
  });

  it("folds itself again once the decision lands", () => {
    const onApprove = vi.fn();
    const { container, rerender } = render(
      <ExploreBlock
        items={[toolCall("c1")]}
        isStreaming={false}
        currentSessionId="sess-1"
        pendingApproval={{ "call-1": approval() }}
        onApprove={onApprove}
      />,
    );
    expect(header(container).getAttribute("aria-expanded")).toBe("true");

    // The store clears `pendingApproval` once the decision is delivered.
    rerender(
      <ExploreBlock
        items={[toolCall("c1")]}
        isStreaming={false}
        currentSessionId="sess-1"
        pendingApproval={{}}
        onApprove={onApprove}
      />,
    );

    expect(header(container).getAttribute("aria-expanded")).toBe("false");
  });

  it("folds on the click itself, before the store round-trip answers", () => {
    const onApprove = vi.fn();
    const { container } = render(
      <ExploreBlock
        items={[toolCall("c1")]}
        isStreaming={false}
        currentSessionId="sess-1"
        pendingApproval={{ "call-1": approval() }}
        onApprove={onApprove}
      />,
    );

    const allow = approvalButtons(container).find((b) => b.textContent === "Allow")!;
    fireEvent.click(allow);

    expect(onApprove).toHaveBeenCalledWith("allow", expect.objectContaining({ request_id: "req-1" }));
    expect(header(container).getAttribute("aria-expanded")).toBe("false");
  });

  it("leaves a block the reader opened by hand open when the decision lands", () => {
    const onApprove = vi.fn();
    const { container } = render(
      <ExploreBlock
        items={[toolCall("c1")]}
        isStreaming={false}
        currentSessionId="sess-1"
        pendingApproval={{ "call-1": approval() }}
        onApprove={onApprove}
      />,
    );

    // The reader closes it by hand: the pending decision must not re-open
    // the block behind their back.
    fireEvent.click(header(container));
    expect(header(container).getAttribute("aria-expanded")).toBe("false");

    // The reader opens it by hand, then the decision lands.
    fireEvent.click(header(container));
    expect(header(container).getAttribute("aria-expanded")).toBe("true");
  });

  it("stays open while another authorization is still pending", () => {
    const onApprove = vi.fn();
    const { container } = render(
      <ExploreBlock
        items={[toolCall("c1"), toolCall("c2", "call-2")]}
        isStreaming={false}
        currentSessionId="sess-1"
        pendingApproval={{
          "call-1": approval(),
          "call-2": approval({ request_id: "req-2", tool_call_id: "call-2" }),
        }}
        onApprove={onApprove}
      />,
    );
    expect(header(container).getAttribute("aria-expanded")).toBe("true");

    // Answering the FIRST of the two must not hide the second one's button.
    const allow = approvalButtons(container).find((b) => b.textContent === "Allow")!;
    fireEvent.click(allow);

    expect(onApprove).toHaveBeenCalledTimes(1);
    expect(header(container).getAttribute("aria-expanded")).toBe("true");
  });
});

describe("ExploreBlock — the pending set, not a boolean", () => {
  it("opens once for the second authorization that arrives later", () => {
    const onApprove = vi.fn();
    const { container, rerender } = render(
      <ExploreBlock
        items={[toolCall("c1"), toolCall("c2", "call-2")]}
        isStreaming={false}
        currentSessionId="sess-1"
        pendingApproval={{ "call-1": approval() }}
        onApprove={onApprove}
      />,
    );
    expect(header(container).getAttribute("aria-expanded")).toBe("true");

    // A second authorization arrives: the block is already open, and it
    // must STAY open for the second one.
    rerender(
      <ExploreBlock
        items={[toolCall("c1"), toolCall("c2", "call-2")]}
        isStreaming={false}
        currentSessionId="sess-1"
        pendingApproval={{
          "call-1": approval(),
          "call-2": approval({ request_id: "req-2", tool_call_id: "call-2" }),
        }}
        onApprove={onApprove}
      />,
    );
    expect(header(container).getAttribute("aria-expanded")).toBe("true");
    expect(approvalButtons(container)).toHaveLength(4);
  });

  it("keeps the block open when the first of two decisions lands", () => {
    const onApprove = vi.fn();
    const { container, rerender } = render(
      <ExploreBlock
        items={[toolCall("c1"), toolCall("c2", "call-2")]}
        isStreaming={false}
        currentSessionId="sess-1"
        pendingApproval={{
          "call-1": approval(),
          "call-2": approval({ request_id: "req-2", tool_call_id: "call-2" }),
        }}
        onApprove={onApprove}
      />,
    );

    // The first decision lands; the second one is still waiting.
    rerender(
      <ExploreBlock
        items={[toolCall("c1"), toolCall("c2", "call-2")]}
        isStreaming={false}
        currentSessionId="sess-1"
        pendingApproval={{
          "call-2": approval({ request_id: "req-2", tool_call_id: "call-2" }),
        }}
        onApprove={onApprove}
      />,
    );

    expect(header(container).getAttribute("aria-expanded")).toBe("true");
    expect(approvalButtons(container)).toHaveLength(2);
  });
});
