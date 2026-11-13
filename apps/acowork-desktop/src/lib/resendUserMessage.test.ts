/**
 * `resendUserMessageText` — the one behaviour behind four entry points
 * (right-click menu and hover icon, on each of the two execution-surface
 * user bubbles). The bubbles' own tests mock this helper to prove they reach
 * it; this file pins what it does once they do.
 */
import { beforeEach, describe, expect, it, vi } from "vitest";

const sendMessage = vi.fn();
let selectedAgentId: string | null = "inst-sse";

vi.mock("../stores/agentStore", () => ({
  useAgentStore: { getState: () => ({ selectedAgentId }) },
}));
vi.mock("../stores/chatStore", () => ({
  useChatStore: { getState: () => ({ sendMessage }) },
}));

import { resendUserMessageText } from "./resendUserMessage";

beforeEach(() => {
  sendMessage.mockReset();
  selectedAgentId = "inst-sse";
});

describe("resendUserMessageText", () => {
  it("sends the text back to the agent that is selected now", () => {
    resendUserMessageText("再来一次");
    // The agent is read at click time, not carried from the original turn:
    // a re-send means "do this again", against whatever session is open.
    expect(sendMessage).toHaveBeenCalledWith("再来一次", "inst-sse");
  });

  // Both guards leave the button inert rather than half-firing: with no agent
  // there is no session to publish into, and with no text there is nothing
  // to send — an empty re-send would still allocate a message id.
  it("does nothing when no agent is selected", () => {
    selectedAgentId = null;
    resendUserMessageText("hello");
    expect(sendMessage).not.toHaveBeenCalled();
  });

  it("does nothing when the turn has no text", () => {
    resendUserMessageText("");
    expect(sendMessage).not.toHaveBeenCalled();
  });
});
