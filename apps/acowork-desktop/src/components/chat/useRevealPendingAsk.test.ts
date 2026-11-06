// @vitest-environment jsdom
/**
 * `useRevealPendingAsk` — the one thing that makes an anchored block navigable.
 *
 * The block never moves to the reader (that is the folding contract), so when the
 * router's question lands on it the viewport has to move instead. These tests
 * pin the boundaries of that move: only for a question that is NEWS (not one
 * already open at mount), only once, and only when the block is off screen.
 */
import { renderHook } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { useRevealPendingAsk } from "./useRevealPendingAsk";
import type { VirtualRowListHandle } from "../common/VirtualRowList";
import type { RunBlock, TranscriptItem } from "./chatTranscriptGroups";
import type { ChatMessageRow } from "../../stores/chatSessionStore";

function row(seq: number, id: string): ChatMessageRow {
  return { seq, ts: seq, from: "ag", kind: "execution_round", body: "", to: "u", meta: { message_id: id } };
}

function block(anchorSeq: number, askSeq?: number): RunBlock {
  const anchor = row(anchorSeq, `round:s1|r${anchorSeq}`);
  return {
    kind: "run",
    instanceId: "ag",
    execSessionId: "s1",
    rounds: [{ ref: `ag|s1|r${anchorSeq}`, roundRow: anchor, showUserLine: true }],
    anchorRow: anchor,
    ...(askSeq === undefined
      ? {}
      : {
          pendingAsk: {
            seq: askSeq,
            ts: askSeq,
            from: "ag",
            kind: "system_notice",
            body: "",
            to: "u",
            meta: { message_id: `confirm_continue:${askSeq}`, notice_type: "confirm_continue" },
          },
        }),
  };
}

function list(inView: boolean) {
  return {
    scrollToKey: vi.fn(() => true),
    isKeyInViewport: vi.fn(() => inView),
  } as unknown as VirtualRowListHandle & {
    scrollToKey: ReturnType<typeof vi.fn>;
    isKeyInViewport: ReturnType<typeof vi.fn>;
  };
}

const keys = (anchorSeq: number, askSeq: number) =>
  new Map<number, string>([
    [anchorSeq, `k:${anchorSeq}`],
    [askSeq, `k:${anchorSeq}`],
  ]);

describe("useRevealPendingAsk", () => {
  it("scrolls to the block when the question arrives and the block is off screen", () => {
    const l = list(false);
    const ref = { current: l };
    const before: TranscriptItem[] = [block(2)];
    const after: TranscriptItem[] = [block(2, 9)];
    const { rerender } = renderHook(
      ({ items, seqs }: { items: TranscriptItem[]; seqs: Map<number, string> }) =>
        useRevealPendingAsk(items, seqs, ref),
      { initialProps: { items: before, seqs: keys(2, 9) } },
    );
    rerender({ items: after, seqs: keys(2, 9) });
    expect(l.scrollToKey).toHaveBeenCalledWith("k:2", "center");
  });

  it("leaves a question that is already open at mount alone", () => {
    // A reload with an unanswered question is history: the surface opens at the
    // bottom, and dragging the reader up would fight it.
    const l = list(false);
    const ref = { current: l };
    const { rerender } = renderHook(
      ({ items }: { items: TranscriptItem[] }) =>
        useRevealPendingAsk(items, keys(2, 9), ref),
      { initialProps: { items: [block(2, 9)] as TranscriptItem[] } },
    );
    rerender({ items: [block(2, 9), block(4)] });
    expect(l.scrollToKey).not.toHaveBeenCalled();
  });

  it("does not move the viewport when the block is already in view", () => {
    const l = list(true);
    const ref = { current: l };
    const { rerender } = renderHook(
      ({ items }: { items: TranscriptItem[] }) =>
        useRevealPendingAsk(items, keys(2, 9), ref),
      { initialProps: { items: [block(2)] as TranscriptItem[] } },
    );
    rerender({ items: [block(2, 9)] });
    expect(l.scrollToKey).not.toHaveBeenCalled();
  });

  it("reveals a given question once", () => {
    const l = list(false);
    const ref = { current: l };
    const { rerender } = renderHook(
      ({ items }: { items: TranscriptItem[] }) =>
        useRevealPendingAsk(items, keys(2, 9), ref),
      { initialProps: { items: [block(2)] as TranscriptItem[] } },
    );
    rerender({ items: [block(2, 9)] });
    // An unrelated row arriving re-runs the effect with the same layout.
    rerender({ items: [block(2, 9), block(6)] });
    expect(l.scrollToKey).toHaveBeenCalledTimes(1);
  });

  /** The transcript window arrives asynchronously: the first commit has no rows
   *  at all, and the card that lands with them is a reload's history, not news.
   *  A baseline taken from the empty first frame would scroll on open. */
  it("takes its baseline from the first non-empty layout", () => {
    const l = list(false);
    const ref = { current: l };
    const { rerender } = renderHook(
      ({ items }: { items: TranscriptItem[] }) =>
        useRevealPendingAsk(items, keys(2, 9), ref),
      { initialProps: { items: [] as TranscriptItem[] } },
    );
    rerender({ items: [block(2, 9)] });
    expect(l.scrollToKey).not.toHaveBeenCalled();
  });

  it("does nothing when the layout cannot resolve the block's key", () => {
    const l = list(false);
    const ref = { current: l };
    const { rerender } = renderHook(
      ({ items, seqs }: { items: TranscriptItem[]; seqs: Map<number, string> }) =>
        useRevealPendingAsk(items, seqs, ref),
      { initialProps: { items: [block(2)] as TranscriptItem[], seqs: new Map<number, string>() } },
    );
    rerender({ items: [block(2, 9)], seqs: new Map<number, string>() });
    expect(l.scrollToKey).not.toHaveBeenCalled();
  });
});
