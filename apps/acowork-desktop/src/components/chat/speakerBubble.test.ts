/**
 * Tests for the speaker → (skin, renderer) rule.
 *
 * The reported defect: an agent-to-agent execution session painted its
 * initiator's markdown brief as literal characters, because the execution
 * surface keyed the renderer off the conversation ROLE (`user` = every inbound
 * turn, human or Agent) instead of off the SPEAKER. The DOM half of the guard
 * is `execBubbleSpeaker.test.tsx`; this file pins the rule itself, including
 * the pairing that must never be broken apart: markdown is only ever rendered
 * on the agent skin, because the prose shell's own near-black text color beats
 * the accent bubble's white (`speakerBubble.ts`).
 */
import { describe, expect, it } from "vitest";
import {
  speakerBubbleClass,
  speakerRendersMarkdown,
} from "./speakerBubble";

describe("speakerBubble", () => {
  it("parses markdown only for an Agent speaker", () => {
    expect(speakerRendersMarkdown("agent")).toBe(true);
    expect(speakerRendersMarkdown("user")).toBe(false);
    // The conservative half of the rule: an id that resolved to neither
    // registry is not evidence anybody is an Agent.
    expect(speakerRendersMarkdown("unknown")).toBe(false);
  });

  it("paints an Agent speaker with the agent skin", () => {
    expect(speakerBubbleClass("agent")).toBe("bg-chat-bubble text-text-primary");
  });

  it("keeps a person (and an unresolved id) on the accent skin", () => {
    expect(speakerBubbleClass("user")).toBe("bg-chat-user text-chat-user-text");
    expect(speakerBubbleClass("unknown")).toBe("bg-chat-user text-chat-user-text");
  });

  it("never pairs markdown with the accent skin", () => {
    // The invariant, stated as one: wherever the parser turns on, the skin
    // switched with it. This is the assertion that stops the "user bubble text
    // turned black" defect from coming back through the execution view.
    for (const kind of ["agent", "user", "unknown"] as const) {
      const markdown = speakerRendersMarkdown(kind);
      const accentSkin = speakerBubbleClass(kind).includes("bg-chat-user");
      expect(markdown).toBe(!accentSkin);
    }
  });
});
