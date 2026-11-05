/**
 * Tests for the avatar-anchor rule and the messageFolder fold algorithm.
 *
 * The bug we're guarding against: when a user message carries two
 * attachments, the fold algorithm consolidates them into a single
 * `user_with_parts` block.  The original avatar-anchor rule only
 * recognised `user` blocks, so the agent avatar header disappeared for
 * every reply that followed such a block.  These tests pin both halves
 * of the fix: the fold produces the right block type, and the anchor
 * rule accepts it as a user-anchored block.
 */

import { describe, expect, it } from "vitest";
import type { ChatMessage } from "../../lib/types";
import { blockIndexOfRawMessage, foldMessages } from "./messageFolder";
import { isAgentBlock, shouldShowAgentAvatar, shouldShowTrailingAgentHeader } from "./avatarAnchor";

// ── helpers ──────────────────────────────────────────────────────────────

let counter = 0;
function id(): string {
  counter += 1;
  return `m-${counter}`;
}

function userMsg(timestamp: number, content = "hi", idOverride?: string): ChatMessage {
  return { id: idOverride ?? id(), type: "user", content, timestamp };
}

function assistantMsg(timestamp: number, content = "ok"): ChatMessage {
  return { id: id(), type: "assistant", content, timestamp };
}

function thoughtMsg(timestamp: number, content = "thinking"): ChatMessage {
  return { id: id(), type: "thought", content, timestamp };
}

function toolCallMsg(timestamp: number): ChatMessage {
  return { id: id(), type: "tool_call", content: "{}", timestamp };
}

function attachedFileMsg(
  timestamp: number,
  fileName: string,
  absPath?: string,
): ChatMessage {
  return {
    id: id(),
    type: "system",
    content: `Attached file: ${fileName}`,
    timestamp,
    metadata: {
      type: "attached_file",
      name: fileName,
      abs_path: absPath ?? `/tmp/${fileName}`,
    },
  };
}

function attachedSelectionMsg(timestamp: number): ChatMessage {
  return {
    id: id(),
    type: "system",
    content: "Attached selection",
    timestamp,
    metadata: { type: "attached_selection" },
  };
}

function compactionMsg(timestamp: number): ChatMessage {
  return { id: id(), type: "compaction", content: "...", timestamp };
}

function systemMsg(timestamp: number, content = "sys"): ChatMessage {
  return { id: id(), type: "system", content, timestamp };
}

// ── foldMessages: user + multiple attachments ─────────────────────────────

describe("foldMessages: user + multiple attachments", () => {
  it("folds user + two attached_file system entries into a single user_with_parts block", () => {
    // Mirrors the conversation file the bug was reported from.
    const messages: ChatMessage[] = [
      userMsg(1785482337194, "两个问题：…", "msg-8758d4c0"),
      attachedFileMsg(
        1785482337197,
        "useScrollController.ts",
        "D:/projects/tranxon/ACoworkDev/apps/acowork-desktop/src/components/chat/useScrollController.ts",
      ),
      attachedFileMsg(
        1785482337197,
        "chatListAdapter.ts",
        "D:/projects/tranxon/ACoworkDev/apps/acowork-desktop/src/components/chat/chatListAdapter.ts",
      ),
      thoughtMsg(1785482345289),
      toolCallMsg(1785482345294),
    ];

    const blocks = foldMessages(messages);

    expect(blocks).toHaveLength(2);
    expect(blocks[0].type).toBe("user_with_parts");
    expect(blocks[0].rawCount).toBe(3);
    expect(blocks[0].items.map((m) => m.id)).toEqual([
      "msg-8758d4c0",
      messages[1].id,
      messages[2].id,
    ]);
    expect(blocks[1].type).toBe("explore_group");
    expect(blocks[1].rawCount).toBe(2);
  });

  it("folds user + three different attachment types", () => {
    const messages: ChatMessage[] = [
      userMsg(1000, "with mix"),
      attachedFileMsg(1001, "a.ts"),
      attachedSelectionMsg(1002),
      toolCallMsg(1500),
    ];

    const blocks = foldMessages(messages);
    expect(blocks).toHaveLength(2);
    expect(blocks[0].type).toBe("user_with_parts");
    expect(blocks[0].rawCount).toBe(3);
  });

  it("does NOT fold a system entry that is not an attachment type", () => {
    const messages: ChatMessage[] = [
      userMsg(1000),
      // A non-attachment system message must remain a separate block.
      systemMsg(1001, "session notice"),
      assistantMsg(2000),
    ];
    const blocks = foldMessages(messages);
    expect(blocks).toHaveLength(3);
    expect(blocks[0].type).toBe("user");
    expect(blocks[1].type).toBe("system");
    expect(blocks[2].type).toBe("assistant");
  });

  it("does NOT fold an attachment that arrived more than the fold window after the user message", () => {
    // ATTACHMENT_FOLD_WINDOW_MS is 100; place the attachment >100ms later.
    const messages: ChatMessage[] = [
      userMsg(1000),
      attachedFileMsg(1200, "late.ts"),
      assistantMsg(2000),
    ];
    const blocks = foldMessages(messages);
    expect(blocks).toHaveLength(3);
    expect(blocks[0].type).toBe("user");
    expect(blocks[1].type).toBe("system");
    expect(blocks[2].type).toBe("assistant");
  });

  it("preserves the content-derived blockId for the user_with_parts block", () => {
    const userId = "msg-abc";
    const messages: ChatMessage[] = [
      userMsg(1000, "x", userId),
      attachedFileMsg(1001, "a.ts"),
      attachedFileMsg(1001, "b.ts"),
    ];
    const blocks = foldMessages(messages);
    expect(blocks[0].blockId).toBe(`block-${userId}`);
  });
});

// ── isAgentBlock ─────────────────────────────────────────────────────────

describe("isAgentBlock", () => {
  function block(type: ChatMessage["type"] | "explore_group" | "user_with_parts") {
    return {
      blockId: "x",
      type,
      items: [] as ChatMessage[],
      rawCount: 0,
      anchorToLatest: false,
      hasFollowUpReply: false,
      isLive: false,
    };
  }

  it("treats explore_group as an agent block", () => {
    expect(isAgentBlock(block("explore_group"))).toBe(true);
  });

  it("treats assistant / thought / tool_call as agent blocks", () => {
    expect(isAgentBlock(block("assistant"))).toBe(true);
    expect(isAgentBlock(block("thought"))).toBe(true);
    expect(isAgentBlock(block("tool_call"))).toBe(true);
    expect(isAgentBlock(block("tool_result"))).toBe(true);
    expect(isAgentBlock(block("error"))).toBe(true);
  });

  it("treats user / user_with_parts / system / compaction as non-agent", () => {
    expect(isAgentBlock(block("user"))).toBe(false);
    expect(isAgentBlock(block("user_with_parts"))).toBe(false);
    expect(isAgentBlock(block("system"))).toBe(false);
    expect(isAgentBlock(block("compaction"))).toBe(false);
  });
});

// ── shouldShowAgentAvatar: the regression ─────────────────────────────────

describe("shouldShowAgentAvatar — user_with_parts regression", () => {
  it("shows the avatar when an agent block follows a user_with_parts block (the reported bug)", () => {
    // The exact sequence from the reported conversation:
    //   [user_with_parts (user + 2 attached_file), explore_group, assistant]
    const messages: ChatMessage[] = [
      userMsg(1785482337194, "q"),
      attachedFileMsg(1785482337197, "useScrollController.ts"),
      attachedFileMsg(1785482337197, "chatListAdapter.ts"),
      thoughtMsg(1785482345289),
      toolCallMsg(1785482345294),
      assistantMsg(1785482400000, "answer"),
    ];
    const blocks = foldMessages(messages);
    expect(blocks.map((b) => b.type)).toEqual([
      "user_with_parts",
      "explore_group",
      "assistant",
    ]);

    // The avatar appears exactly once per agent turn: above the FIRST
    // agent block after the user — in this sequence the explore_group
    // (index 1).  The assistant (index 2) is a continuation of the same
    // turn, so it must NOT show a second avatar (regression guard for
    // the duplicate-avatar bug).
    expect(shouldShowAgentAvatar(blocks, 1)).toBe(true);
    expect(shouldShowAgentAvatar(blocks, 2)).toBe(false);
  });

  it("shows the avatar even when a late attachment system entry sits between the user block and the agent reply", () => {
    // User message arrives at t=100, attachment >100ms later (outside the
    // fold window) becomes its own `system` block, then the agent replies.
    const messages: ChatMessage[] = [
      userMsg(1000),
      attachedFileMsg(1200, "late.ts"),
      thoughtMsg(2000),
      assistantMsg(3000),
    ];
    const blocks = foldMessages(messages);
    expect(blocks.map((b) => b.type)).toEqual([
      "user",
      "system",
      "explore_group",
      "assistant",
    ]);

    // Same rule: the late `system` block IS a skip type (transparent),
    // so the avatar still anchors above the explore_group.  But the
    // assistant following the explore_group must NOT show a second avatar.
    expect(shouldShowAgentAvatar(blocks, 2)).toBe(true);
    expect(shouldShowAgentAvatar(blocks, 3)).toBe(false);
  });

  it("shows the avatar after a plain user block (legacy behaviour preserved)", () => {
    const messages: ChatMessage[] = [
      userMsg(1000),
      assistantMsg(2000),
      assistantMsg(3000),
    ];
    const blocks = foldMessages(messages);
    expect(blocks.map((b) => b.type)).toEqual(["user", "assistant", "assistant"]);

    expect(shouldShowAgentAvatar(blocks, 1)).toBe(true);
    // The second assistant is NOT the first reply after the user — its
    // immediate predecessor is another agent block, so the avatar
    // belongs only to the first one.
    expect(shouldShowAgentAvatar(blocks, 2)).toBe(false);
  });

  it("does not show the avatar above the very first block", () => {
    const messages: ChatMessage[] = [userMsg(1000), assistantMsg(2000)];
    const blocks = foldMessages(messages);
    expect(shouldShowAgentAvatar(blocks, 0)).toBe(false);
  });

  it("does not show the avatar when the conversation starts with an agent message", () => {
    const messages: ChatMessage[] = [assistantMsg(1000)];
    const blocks = foldMessages(messages);
    expect(shouldShowAgentAvatar(blocks, 0)).toBe(false);
  });

  it("skips compaction markers between the user and the agent reply", () => {
    const messages: ChatMessage[] = [
      userMsg(1000),
      compactionMsg(1500),
      assistantMsg(2000),
    ];
    const blocks = foldMessages(messages);
    expect(blocks.map((b) => b.type)).toEqual([
      "user",
      "compaction",
      "assistant",
    ]);
    expect(shouldShowAgentAvatar(blocks, 2)).toBe(true);
  });

  it("does not show the avatar when two consecutive agent blocks precede a new user message", () => {
    const messages: ChatMessage[] = [
      userMsg(1000),
      assistantMsg(2000),
      assistantMsg(3000),
      userMsg(4000),
      assistantMsg(5000),
    ];
    const blocks = foldMessages(messages);
    expect(blocks.map((b) => b.type)).toEqual([
      "user",
      "assistant",
      "assistant",
      "user",
      "assistant",
    ]);
    // First user→agent transition anchors the avatar on the first agent only.
    expect(shouldShowAgentAvatar(blocks, 1)).toBe(true);
    expect(shouldShowAgentAvatar(blocks, 2)).toBe(false);
    // Second user→agent transition anchors the avatar on the new agent only.
    expect(shouldShowAgentAvatar(blocks, 4)).toBe(true);
  });

  it("treats back-to-back user_with_parts blocks correctly: only the most recent anchors", () => {
    const messages: ChatMessage[] = [
      userMsg(1000, "first", "u1"),
      attachedFileMsg(1001, "a.ts"),
      userMsg(3000, "second", "u2"),
      attachedFileMsg(3001, "b.ts"),
      attachedFileMsg(3001, "c.ts"),
      assistantMsg(5000),
    ];
    const blocks = foldMessages(messages);
    expect(blocks.map((b) => b.type)).toEqual([
      "user_with_parts",
      "user_with_parts",
      "assistant",
    ]);
    // The assistant at index 2 should anchor to the second user_with_parts.
    expect(shouldShowAgentAvatar(blocks, 2)).toBe(true);
  });
});

// ── shouldShowAgentAvatar: explore_group / no-duplicate-avatar regression ──
//
// Reported symptom: "用户消息后面如果跟着一个 agent 的 think 消息，agent
// 头像会显示两次。先显示用户头像和消息，然后显示 agent 头像和深度思考块，
// 然后又显示 agent 头像，然后再显示 agent 工具调用或者其他消息".
//
// Root cause: avatarAnchor previously listed `explore_group` in SKIP_TYPES,
// so the backward scan passed THROUGH it on its way to the user block,
// re-triggering the avatar on every subsequent agent block in the turn.
//
// These tests pin the corrected rule: avatar anchors to the FIRST agent
// block after the user — once shown, never again until the next user
// message.  `explore_group` is itself an agent block and acts as a HARD
// stop, the same way a second `assistant` block would.

describe("shouldShowAgentAvatar — explore_group must not duplicate the avatar", () => {
  it("user → thought → assistant: avatar only on the explore_group, not on the assistant", () => {
    // The minimal reproduction of the reported bug.  A user message,
    // followed by a single thought (folding into an explore_group of
    // length 1), followed by the assistant reply.  The avatar must show
    // exactly once — above the explore_group — and NOT above the
    // assistant that continues the same turn.
    const messages: ChatMessage[] = [
      userMsg(1000, "hi"),
      thoughtMsg(2000, "let me think..."),
      assistantMsg(3000, "here you go"),
    ];
    const blocks = foldMessages(messages);
    expect(blocks.map((b) => b.type)).toEqual([
      "user",
      "explore_group",
      "assistant",
    ]);

    expect(shouldShowAgentAvatar(blocks, 1)).toBe(true);
    expect(shouldShowAgentAvatar(blocks, 2)).toBe(false);
  });

  it("user → thought → tool_call → tool_call → assistant: avatar only on the explore_group", () => {
    // A more realistic agent turn: thinking then multiple tool calls
    // then the final reply.  All of thought/tool_call/tool_result fold
    // into a single explore_group.  The assistant must NOT duplicate the
    // avatar.
    const messages: ChatMessage[] = [
      userMsg(1000),
      thoughtMsg(1100, "I should check the file"),
      toolCallMsg(1200),
      toolCallMsg(1250),
      assistantMsg(2000, "the file says ..."),
    ];
    const blocks = foldMessages(messages);
    expect(blocks.map((b) => b.type)).toEqual([
      "user",
      "explore_group",
      "assistant",
    ]);
    expect(blocks[1].rawCount).toBe(3);

    expect(shouldShowAgentAvatar(blocks, 1)).toBe(true);
    expect(shouldShowAgentAvatar(blocks, 2)).toBe(false);
  });

  it("user → explore_group → system → explore_group → assistant: avatar only on the FIRST explore_group", () => {
    // Streaming can emit multiple distinct explore phases (thought → tool →
    // new thought → tool → …) that fold into multiple explore_group blocks
    // because each phase is separated by something non-explore.  Here we
    // simulate that by inserting a system marker between two explore
    // phases.
    //
    // The system marker IS a skip type (transparent), so the second
    // explore_group's backward scan passes through it — but it then hits
    // the FIRST explore_group, which is a HARD stop.  Once the avatar has
    // been shown above the first explore_group of a turn, no later agent
    // block in that turn re-triggers it, regardless of what non-agent
    // markers sit between them.
    const messages: ChatMessage[] = [
      userMsg(1000),
      thoughtMsg(1100),
      toolCallMsg(1200),
      // A non-attachment system entry splits the explore group.
      systemMsg(1300, "phase boundary"),
      thoughtMsg(1400, "next phase"),
      toolCallMsg(1500),
      assistantMsg(2000),
    ];
    const blocks = foldMessages(messages);
    expect(blocks.map((b) => b.type)).toEqual([
      "user",
      "explore_group",
      "system",
      "explore_group",
      "assistant",
    ]);

    // First explore_group: avatar shown (anchored to user).
    expect(shouldShowAgentAvatar(blocks, 1)).toBe(true);
    // Second explore_group: prev=system (skip), then prev=explore_group
    // (hard stop) → NO avatar.  The avatar already belongs to this turn.
    expect(shouldShowAgentAvatar(blocks, 3)).toBe(false);
    // Assistant: prev=explore_group (hard stop) → NO avatar.
    expect(shouldShowAgentAvatar(blocks, 4)).toBe(false);
  });

  it("compaction between user and explore_group: avatar still shown on explore_group", () => {
    // Guard the legitimate skip behaviour: compaction IS a skip type and
    // must NOT prevent the avatar from anchoring to the next agent block.
    const messages: ChatMessage[] = [
      userMsg(1000),
      compactionMsg(1500),
      thoughtMsg(2000),
      assistantMsg(3000),
    ];
    const blocks = foldMessages(messages);
    expect(blocks.map((b) => b.type)).toEqual([
      "user",
      "compaction",
      "explore_group",
      "assistant",
    ]);

    expect(shouldShowAgentAvatar(blocks, 2)).toBe(true);
    expect(shouldShowAgentAvatar(blocks, 3)).toBe(false);
  });
});

// ── shouldShowTrailingAgentHeader ───────────────────────────────────────

describe("shouldShowTrailingAgentHeader", () => {
  it("returns true when the last block is a user block", () => {
    const messages: ChatMessage[] = [
      userMsg(1000),
    ];
    const blocks = foldMessages(messages);
    expect(shouldShowTrailingAgentHeader(blocks, 0)).toBe(true);
  });

  it("returns true when the last block is a user_with_parts block", () => {
    const messages: ChatMessage[] = [
      userMsg(1000),
      attachedFileMsg(1001, "a.ts"),
    ];
    const blocks = foldMessages(messages);
    expect(blocks[0].type).toBe("user_with_parts");
    expect(shouldShowTrailingAgentHeader(blocks, 0)).toBe(true);
  });

  it("returns false when the last block is an agent block", () => {
    const messages: ChatMessage[] = [
      userMsg(1000),
      assistantMsg(2000),
    ];
    const blocks = foldMessages(messages);
    expect(shouldShowTrailingAgentHeader(blocks, 1)).toBe(false);
  });

  it("returns false when the user block is not the last block", () => {
    const messages: ChatMessage[] = [
      userMsg(1000),
      assistantMsg(2000),
      userMsg(3000),
      assistantMsg(4000),
    ];
    const blocks = foldMessages(messages);
    // User at index 0 is NOT the last block
    expect(shouldShowTrailingAgentHeader(blocks, 0)).toBe(false);
    // User at index 2 is NOT the last block
    expect(shouldShowTrailingAgentHeader(blocks, 2)).toBe(false);
  });

  it("returns false for an empty blocks array", () => {
    const blocks = foldMessages([]);
    expect(shouldShowTrailingAgentHeader(blocks, 0)).toBe(false);
  });

  it("returns false when the last block is a compaction marker", () => {
    const messages: ChatMessage[] = [
      userMsg(1000),
      compactionMsg(1500),
    ];
    const blocks = foldMessages(messages);
    expect(shouldShowTrailingAgentHeader(blocks, 1)).toBe(false);
  });
});

describe("blockIndexOfRawMessage — ADR-081 locate", () => {
  const raw = (id: string, type: ChatMessage["type"]): ChatMessage =>
    ({ id, type, content: "", timestamp: 0 } as ChatMessage);

  it("maps a raw index into the folded block that contains it", () => {
    // user, thought+tool_call+tool_result (folded), assistant
    const blocks = foldMessages([
      raw("u1", "user"),
      raw("t1", "thought"),
      raw("tc1", "tool_call"),
      raw("tr1", "tool_result"),
      raw("a1", "assistant"),
    ]);
    expect(blocks).toHaveLength(3); // user / explore_group / assistant
    expect(blockIndexOfRawMessage(blocks, 0)).toBe(0); // user
    expect(blockIndexOfRawMessage(blocks, 1)).toBe(1); // thought
    expect(blockIndexOfRawMessage(blocks, 3)).toBe(1); // tool_result
    expect(blockIndexOfRawMessage(blocks, 4)).toBe(2); // assistant
  });

  it("returns -1 for an index outside every block", () => {
    const blocks = foldMessages([raw("a1", "assistant")]);
    expect(blockIndexOfRawMessage(blocks, 5)).toBe(-1);
    expect(blockIndexOfRawMessage([], 0)).toBe(-1);
  });
});
