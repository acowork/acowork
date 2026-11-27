// t-01412679 — the explore-stub page projection, end to end on the client:
// conversion (chatStore), folding (messageFolder), locate mapping, and the
// lazy ExploreBlock contract.
//
// The failures these guard against:
//  1. a stub row rendered as a blank assistant bubble (kind not detected
//     before role mapping — the stub's role is deliberately "assistant");
//  2. the cursor math breaking because folding changed what a "row" counts
//     as — folding is a VIEW, offset/limit/total stay raw-entry units;
//  3. §9.5: a lazy block force-expanding on a pending tool authorization —
//     it has no rows to answer with, and a half-open empty block is exactly
//     the "invisible decision" bug the file comments forbid;
//  4. a locate hit inside a stub landing on the WRONG block, because array
//     indices and raw coordinates diverge once a row covers N entries.
import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { convertConversationEntry } from "../../stores/chatStore";
import { blockIndexOfRawMessage, foldMessages } from "./messageFolder";
import { ExploreBlock } from "./ExploreBlock";
import type { ChatMessage, ConversationEntry, ExploreStubMeta } from "../../lib/types";

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({
    t: (key: string, opts?: { count?: number }) =>
      opts && typeof opts.count === "number" ? `${key}:${opts.count}` : key,
  }),
}));

function stubRow(
  start: number,
  end: number,
  tools: string[] = ["file_read", "bash"],
  steps: number = end - start,
): ChatMessage {
  const exploreStub: ExploreStubMeta = {
    start,
    end,
    rawCount: end - start,
    steps,
    tools,
    toolsTruncated: false,
  };
  return { id: `stub-${start}`, type: "explore_stub", content: "", timestamp: 1000, exploreStub };
}

describe("convertConversationEntry — explore_stub", () => {
  it("maps a stub row to an explore_stub message with camelCase metadata", () => {
    const entry: ConversationEntry = {
      id: "s2",
      ts: "2026-01-01T12:00:01.000Z",
      // The Runtime sends role "assistant" on stubs (legacy-safe); `kind`
      // is the discriminator. A converter that switches on role first
      // would paint this as a blank assistant bubble.
      role: "assistant",
      content: "",
      kind: "explore_stub",
      metadata: { start: 1, end: 4, raw_count: 3, steps: 2, tools: ["bash"], tools_truncated: false },
    };
    const msg = convertConversationEntry(entry, "com.test.agent");
    expect(msg).not.toBeNull();
    expect(msg!.type).toBe("explore_stub");
    expect(msg!.exploreStub).toEqual({
      start: 1,
      end: 4,
      rawCount: 3,
      steps: 2,
      tools: ["bash"],
      toolsTruncated: false,
    });
  });
});

describe("foldMessages — lazy explore blocks", () => {
  it("folds a stub row into a lazy explore_group with empty items", () => {
    const user: ChatMessage = { id: "u1", type: "user", content: "go", timestamp: 900 };
    const answer: ChatMessage = { id: "a1", type: "assistant", content: "done", timestamp: 1100 };
    const blocks = foldMessages([user, stubRow(1, 4), answer]);
    expect(blocks.map((b) => b.type)).toEqual(["user", "explore_group", "assistant"]);
    const group = blocks[1];
    expect(group.lazy).toBe(true);
    expect(group.stub?.tools).toEqual(["file_read", "bash"]);
    expect(group.stubId).toBe("stub-1");
    expect(group.items).toEqual([]);
    // The block's raw size is the RUN's, not the stub row's — the estimator
    // and the header both read it from here.
    expect(group.rawCount).toBe(3);
    // Content-derived id keeps the same anchor as the raw path.
    expect(group.blockId).toBe("block-stub-1");
  });

  // Bug report 2: one RUN was painted as two adjacent chips. The Runtime now
  // pages by RECORD (page edges snap to run boundaries, protocol §5.2.1), so a
  // run arrives either as one stub row or as its raw rows — never as a mix, and
  // the folder has nothing to fuse. These two shapes must stay one block each.
  it("folds two stub rows into two blocks — two runs, never one split", () => {
    const user: ChatMessage = { id: "u1", type: "user", content: "go", timestamp: 900 };
    const answer: ChatMessage = { id: "a1", type: "assistant", content: "done", timestamp: 1100 };
    // Adjacent because the runs really are adjacent in the log: the row that
    // would end the first run starts the second.
    const blocks = foldMessages([user, stubRow(11, 13, ["bash"], 2), stubRow(13, 17, ["file_read"], 3), answer]);
    expect(blocks.map((b) => b.type)).toEqual(["user", "explore_group", "explore_group", "assistant"]);
    expect(blocks[1].stub?.steps).toBe(2);
    expect(blocks[2].stub?.steps).toBe(3);
    // Each closed by the next block, exactly like two raw runs.
    expect(blocks[1].hasFollowUpReply).toBe(false);
    expect(blocks[2].hasFollowUpReply).toBe(true);
  });
});

describe("blockIndexOfRawMessage — coordinate-aware locate", () => {
  it("lands a raw coordinate inside a stub span on the stub block", () => {
    const user: ChatMessage = { id: "u1", type: "user", content: "go", timestamp: 900 };
    const answer: ChatMessage = { id: "a1", type: "assistant", content: "done", timestamp: 1100 };
    // Window starts at raw coordinate 10: user=10, stub run=[11,15), answer=15.
    const blocks = foldMessages([user, stubRow(11, 15), answer]);
    expect(blockIndexOfRawMessage(blocks, 10, 10)).toBe(0);
    expect(blockIndexOfRawMessage(blocks, 11, 10)).toBe(1);
    expect(blockIndexOfRawMessage(blocks, 14, 10)).toBe(1);
    expect(blockIndexOfRawMessage(blocks, 15, 10)).toBe(2);
    // Array math (the pre-stub contract) still holds for non-lazy blocks:
    const raw = foldMessages([user, answer]);
    expect(blockIndexOfRawMessage(raw, 1, 0)).toBe(1);
  });
});

describe("ExploreBlock — lazy stub contract", () => {
  const stub: ExploreStubMeta = { start: 1, end: 4, rawCount: 3, steps: 3, tools: ["file_read", "bash"], toolsTruncated: false };

  it("renders the collapsed header from stub metadata alone", () => {
    const { container } = render(
      <ExploreBlock items={[]} isStreaming={false} stub={stub} stubId="s2" lazy details={undefined} />,
    );
    // "3 steps" straight off the Runtime's `steps` — no rows exist to count.
    expect(container.textContent).toContain("exploreBlock.step:3");
    expect(container.textContent).toContain("file_read");
    // Nothing from the run's body is on the page — that's the whole point.
    expect(container.textContent).not.toContain("let me look around");
  });

  // Bug report 1: the collapsed chip said 8 steps, expanding it said 4. The
  // header must count what the body renders — one entry per thought and per
  // tool invocation (`tool_result` renders inside its call) — so the number
  // cannot move when the reader opens the block.
  it("keeps the step count identical collapsed vs expanded", () => {
    const rows: ChatMessage[] = [
      { id: "d1", type: "thought", content: "look", timestamp: 1000 },
      { id: "d2", type: "tool_call", content: "{}", timestamp: 1001, toolName: "bash", toolCallId: "c1" },
      { id: "d3", type: "tool_result", content: "ok", timestamp: 1002, toolCallId: "c1" },
      { id: "d4", type: "thought", content: "and", timestamp: 1003 },
      { id: "d5", type: "tool_call", content: "{}", timestamp: 1004, toolName: "bash", toolCallId: "c2" },
      { id: "d6", type: "tool_result", content: "ok", timestamp: 1005, toolCallId: "c2" },
    ];
    const wide: ExploreStubMeta = { start: 1, end: 7, rawCount: 6, steps: 4, tools: ["bash"], toolsTruncated: false };
    const collapsed = render(<ExploreBlock items={[]} isStreaming={false} stub={wide} stubId="s2" />);
    expect(collapsed.container.textContent).toContain("exploreBlock.step:4");
    expect(collapsed.container.textContent).not.toContain("exploreBlock.step:6");
    collapsed.unmount();
    const expanded = render(<ExploreBlock items={[]} isStreaming={false} stub={wide} stubId="s2" details={rows} />);
    fireEvent.click(expanded.container.querySelector("button")!);
    expect(expanded.container.textContent).toContain("exploreBlock.step:4");
  });

  // A fused block carries raw window rows next to the stub, and the fetched
  // detail spans the whole run — including those rows. They must render once.
  it("dedupes rows that are both in the window and in the fetched span", () => {
    const head: ChatMessage[] = [
      { id: "r1", type: "thought", content: "the head reasoning", timestamp: 1000 },
    ];
    const detail: ChatMessage[] = [
      { id: "r1", type: "thought", content: "the head reasoning", timestamp: 1000 },
      { id: "d2", type: "tool_call", content: "{}", timestamp: 1001, toolName: "bash", toolCallId: "c1" },
    ];
    const { container } = render(
      <ExploreBlock items={head} isStreaming={false} stub={stub} stubId="s2" details={detail} />,
    );
    fireEvent.click(container.querySelector("button")!);
    // The thought row is itself a disclosure — open it to read its text.
    Array.from(container.querySelectorAll("button"))
      .slice(1)
      .forEach((b) => fireEvent.click(b));
    const occurrences = (container.textContent!.match(/the head reasoning/g) ?? []).length;
    expect(occurrences).toBe(1);
  });

  it("fetches detail on first expand only", () => {
    const onFetchDetail = vi.fn();
    const { container } = render(
      <ExploreBlock items={[]} isStreaming={false} stub={stub} stubId="s2" lazy onFetchDetail={onFetchDetail} />,
    );
    expect(onFetchDetail).not.toHaveBeenCalled();
    fireEvent.click(container.querySelector("button")!);
    expect(onFetchDetail).toHaveBeenCalledTimes(1);
  });

  it("renders fetched detail rows like a raw run", () => {
    const details: ChatMessage[] = [
      { id: "d1", type: "thought", content: "the hidden reasoning", timestamp: 1000 },
      { id: "d2", type: "tool_call", content: "{\"cmd\":\"ls\"}", timestamp: 1001, toolName: "bash", toolCallId: "c1" },
    ];
    const { container } = render(
      <ExploreBlock items={[]} isStreaming={false} stub={stub} stubId="s2" details={details} />,
    );
    // The stub body renders through the same PairedExploreItem pipeline as a
    // raw run: expanding the group reveals the thought row (itself a nested
    // disclosure), and the tool row renders its arguments.
    fireEvent.click(container.querySelector("button")!);
    // The group toggle is the first button; the rows it just revealed sit
    // behind it. Open each nested disclosure (the thought row is one itself)
    // without re-clicking the group toggle — that would collapse the body
    // and every later click would hit stale nodes.
    Array.from(container.querySelectorAll("button"))
      .slice(1)
      .forEach((b) => fireEvent.click(b));
    expect(container.textContent).toContain("the hidden reasoning");
    expect(container.textContent).toContain("ls");
  });

  it("§9.5: a pending authorization does NOT force-open a lazy stub", () => {
    // The invariant: a lazy block has no rows to pair against, so its
    // pending set is structurally empty and the auto-expand effect never
    // fires. If this test starts failing, someone made the stub path
    // visible to `pendingApprovalKeys` — re-read design §9.5.
    const pending = {
      "call-1": {
        type: "tool_approval_needed",
        request_id: "req-1",
        tool_name: "bash",
        risk_level: "Medium",
        tool_call_id: "call-1",
        session_id: "sess-1",
        arguments: "{}",
      } as any,
    };
    const { container } = render(
      <ExploreBlock
        items={[]}
        isStreaming={false}
        stub={stub}
        stubId="s2"
        lazy
        pendingApproval={pending}
        currentSessionId="sess-1"
      />,
    );
    expect(container.querySelector("button")!.getAttribute("aria-expanded")).toBe("false");
    // No Allow/Deny controls rendered against an empty body.
    expect(container.textContent).not.toContain("exploreBlock.cancelTool");
  });
});
