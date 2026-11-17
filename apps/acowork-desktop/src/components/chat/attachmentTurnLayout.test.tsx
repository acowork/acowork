// @vitest-environment jsdom
/**
 * Regression: a turn carrying screenshots must render its pictures as a
 * horizontal, right-aligned row ABOVE the user's bubble — on the execution
 * surface exactly as on the chat surface.
 *
 * The reported symptom had three separate causes, and each one is pinned
 * here:
 *
 *  1. **The fold lost the attachments.** The Runtime writes the user row
 *     and its attachment rows with the SAME millisecond timestamp, so their
 *     relative order is decided by whatever assembled the array. When the
 *     attachment rows landed BEFORE the user row, the folder's forward-only
 *     scan claimed nothing, the attachments stayed standalone `system`
 *     blocks, and `MessageBubble` drew them as CENTERED notices — the
 *     pictures appeared in the middle of the screen, detached from the turn.
 *     Switching sessions reloaded the pages in a different order, which is
 *     why the same turn looked right-aligned one minute and centered the
 *     next.
 *
 *  2. **The chip row was a column.** `flex flex-col items-end` put one
 *     56px tile per line, where the chat surface puts three on one line.
 *
 *  3. **The height estimator had no branch for the block.** It fell through
 *     to `SAFE_FALLBACK_HEIGHT` (60px), so the virtualizer reserved a
 *     fraction of the real height and the rows below overlapped the chips.
 */
import { describe, expect, it, vi } from "vitest";
import { render } from "@testing-library/react";
import { UserWithAttachmentsBubble } from "./UserWithAttachmentsBubble";
import { foldMessages } from "./messageFolder";
import { estimateBlockHeight } from "./blockHeightEstimator";
import { SAFE_FALLBACK_HEIGHT } from "./blockLayout";
import { mergeMessageWindow } from "../../stores/chatStore";
import type { ChatMessage } from "../../lib/types";

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));
vi.mock("../../lib/logger", () => ({
  log: { warn: vi.fn(), debug: vi.fn(), info: vi.fn(), error: vi.fn() },
  setLevel: vi.fn(),
}));

/** The Runtime stamps the user row and its attachment rows with the SAME
 *  millisecond — `write_attached_items` runs in the same batch. */
const SAME_MS = 1_794_916_226_570;

function userRow(ts = SAME_MS): ChatMessage {
  return { id: "u1", type: "user", content: "看这三个截图", timestamp: ts } as ChatMessage;
}

function imageRow(id: string, ts = SAME_MS): ChatMessage {
  return {
    id,
    type: "system",
    content: "",
    timestamp: ts,
    metadata: { type: "image_upload", document_id: `doc-${id}`, filename: `${id}.png`, format: "png", size_bytes: 1 },
  } as unknown as ChatMessage;
}

function images(ids: string[], ts = SAME_MS): ChatMessage[] {
  return ids.map((i) => imageRow(i, ts));
}

// ── 1. the fold must not depend on the array order ──────────────────────

describe("foldMessages: attachment rows that precede the user row", () => {
  it("claims them when they landed BEFORE the user row", () => {
    // The exact shape `mergeMessageWindow` produces when a server page
    // carries the attachments and the cache carries the user row: both
    // share a millisecond, so the stable sort keeps the insertion order.
    const cache = [userRow()];
    const server = images(["i1", "i2", "i3"]);
    const merged = mergeMessageWindow(cache, server).messages;

    // Precondition: the attachments really did land first.
    expect(merged.map((m) => m.id)).toEqual(["i1", "i2", "i3", "u1"]);

    const blocks = foldMessages(merged);
    expect(blocks).toHaveLength(1);
    expect(blocks[0].type).toBe("user_with_parts");
    // The words lead; the parts follow.
    expect(blocks[0].items.map((m) => m.id)).toEqual(["u1", "i1", "i2", "i3"]);
  });

  it("claims them when they landed AFTER the user row (the usual order)", () => {
    const messages = [userRow(), ...images(["i1", "i2", "i3"])];
    const blocks = foldMessages(messages);
    expect(blocks).toHaveLength(1);
    expect(blocks[0].type).toBe("user_with_parts");
    expect(blocks[0].items.map((m) => m.id)).toEqual(["u1", "i1", "i2", "i3"]);
  });

  it("never emits an attachment row as a block of its own", () => {
    // Whatever the assembly order, the attachment rows must not survive as
    // standalone `system` blocks — that is what put them in the middle of
    // the screen.
    for (const messages of [
      [userRow(), ...images(["i1", "i2"])],
      [...images(["i1", "i2"]), userRow()],
    ]) {
      const blocks = foldMessages(messages);
      expect(blocks.some((b) => b.type === "system")).toBe(false);
      expect(blocks).toHaveLength(1);
    }
  });

  it("does not steal a part row that belongs to an EARLIER user turn", () => {
    // Two turns, both carrying a part. The second user row must not claim
    // the first turn's attachment.
    const first = userRow(SAME_MS);
    const firstPart = imageRow("p1", SAME_MS);
    const second = { ...userRow(SAME_MS + 500), id: "u2" } as ChatMessage;
    const secondPart = imageRow("p2", SAME_MS + 500);

    const blocks = foldMessages([first, firstPart, second, secondPart]);
    expect(blocks).toHaveLength(2);
    expect(blocks[0].items.map((m) => m.id)).toEqual(["u1", "p1"]);
    expect(blocks[1].items.map((m) => m.id)).toEqual(["u2", "p2"]);
  });

  it("still refuses a part that arrived long after the user row", () => {
    // The fold window is 100ms; a part beyond it is not part of the turn.
    const blocks = foldMessages([userRow(1000), imageRow("late", 1200)]);
    expect(blocks).toHaveLength(2);
    expect(blocks[0].type).toBe("user");
    expect(blocks[1].type).toBe("system");
  });
});

// ── 2. the chip row is horizontal and right-aligned ─────────────────────

describe("UserWithAttachmentsBubble: the attachment row", () => {
  function renderTurn(parts: ChatMessage[]) {
    const blocks = foldMessages([userRow(), ...parts]);
    return render(
      <UserWithAttachmentsBubble
        userMessage={blocks[0].items[0] as ChatMessage}
        parts={blocks[0].items.slice(1) as ChatMessage[]}
        liveUserName="Nicholas"
        liveUserAvatarUrl={null}
        liveUserBuiltinAvatarId={null}
        currentSessionId="s1"
        agentId="a1"
      />,
    );
  }

  it("lays the chips out in a wrapped, right-aligned row", () => {
    const { container } = renderTurn(images(["i1", "i2", "i3"]));
    const row = container.querySelector(".mt-2.flex") as HTMLElement;
    expect(row).not.toBeNull();
    // Horizontal, wrapping, hugging the turn's own (right) edge — the same
    // grammar the chat surface uses. `flex-col` was the column layout.
    expect(row.className).toContain("flex-wrap");
    expect(row.className).toContain("justify-end");
    expect(row.className).not.toContain("flex-col");
    // All three chips live in the SAME row container.
    expect(row.children).toHaveLength(3);
  });
});

// ── 3. the height estimator knows the block ─────────────────────────────

describe("blockHeightEstimator: user_with_parts", () => {
  it("reserves room for the chip row instead of the 60px fallback", () => {
    const messages = [userRow(), ...images(["i1", "i2", "i3"])];
    const blocks = foldMessages(messages);
    expect(blocks[0].type).toBe("user_with_parts");

    // The fallback would be exactly SAFE_FALLBACK_HEIGHT (60px) — far too
    // small for a turn with a chip row and a bubble, and under-reporting is
    // what made the rows below overlap the pictures.
    const height = estimateBlockHeight(0, blocks, 900);
    expect(height).toBeGreaterThan(SAFE_FALLBACK_HEIGHT);
  });

  it("grows when the chips wrap onto more than one line", () => {
    const messages = [userRow(), ...images(["i1", "i2", "i3"])];
    const blocks = foldMessages(messages);

    // Wide enough for one line of three 56px chips.
    const wide = estimateBlockHeight(0, blocks, 900);
    // Narrow enough that they must wrap onto three lines.
    const narrow = estimateBlockHeight(0, blocks, 70);
    expect(narrow).toBeGreaterThan(wide);
  });
});
