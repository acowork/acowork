/**
 * The history-load converter carries the Runtime's human projection through
 * untouched (design 27 §5.4.5).
 *
 * The bubble paints `display` when present, so this is the one place that could
 * lose it: a converter that dropped the field would send every agent-origin intent
 * back to being painted as the machine envelope, and no bubble test would catch it
 * (they start from a `ChatMessage`).
 *
 * The rule the converter must keep is "transport only": no key is read here, no
 * validity is judged here — the Runtime resolved the projection and said whether
 * it has one.
 */
import { describe, expect, it } from "vitest";
import { convertConversationEntry } from "./chatStore";
import type { ConversationEntry, IntentDisplay } from "../lib/types";

const DISPLAY: IntentDisplay = {
  origin: "response",
  action: "notify",
  summary: "任务已拆好",
  body: "任务已拆好，共 6 条。",
};

function entry(display?: IntentDisplay): ConversationEntry {
  return {
    id: "m1",
    ts: "2026-11-09T08:00:00.000Z",
    role: "user",
    content: '[intent:inst-a:notify] {"message":"任务已拆好，共 6 条。"}',
    display,
  };
}

describe("convertConversationEntry", () => {
  it("carries the display projection onto the message", () => {
    const message = convertConversationEntry(entry(DISPLAY), "inst-pm");
    expect(message?.display).toEqual(DISPLAY);
    // `content` keeps the machine text: Resend re-sends the recorded turn.
    expect(message?.content).toContain("[intent:inst-a:notify]");
  });

  it("leaves display undefined when the row has none", () => {
    // A human turn, or a row written before the projection existed.
    const message = convertConversationEntry(entry(), "inst-pm");
    expect(message?.display).toBeUndefined();
  });
});
