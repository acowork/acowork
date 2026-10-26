// "Add to Chat" target routing — the whole bug this resolver exists for:
// a reference has to land in the composer the user is actually looking at.
import { describe, it, expect } from "vitest";
import { pickAddToChatTarget } from "./addToChat";

const A = "com.test.Agent";
const CHAT = "chat-1";
const EXEC = "sess-1";

const base = {
  agentId: A,
  chatSessionId: CHAT,
  execSessionId: EXEC,
  execReadOnly: false,
};

describe("pickAddToChatTarget", () => {
  it("Chat surface takes the reference when it has a session", () => {
    expect(pickAddToChatTarget({ ...base, mode: "chat" })).toEqual({
      surface: "chat",
      chatSessionId: CHAT,
    });
  });

  it("execution surface takes the reference when the session is writable", () => {
    expect(pickAddToChatTarget({ ...base, mode: "exec" })).toEqual({
      surface: "exec",
      agentId: A,
      sessionId: EXEC,
    });
  });

  it("falls back to the execution session when the Chat surface has none yet", () => {
    expect(
      pickAddToChatTarget({ ...base, mode: "chat", chatSessionId: null }),
    ).toEqual({ surface: "exec", agentId: A, sessionId: EXEC });
  });

  it("a read-only execution session is not a target — the Chat session is", () => {
    expect(
      pickAddToChatTarget({ ...base, mode: "exec", execReadOnly: true }),
    ).toEqual({ surface: "chat", chatSessionId: CHAT });
  });

  it("no composer can take it → null (callers disable the affordance)", () => {
    expect(
      pickAddToChatTarget({ ...base, mode: "exec", chatSessionId: null, execReadOnly: true }),
    ).toBeNull();
    expect(
      pickAddToChatTarget({ ...base, mode: "chat", chatSessionId: null, execSessionId: null }),
    ).toBeNull();
    expect(
      pickAddToChatTarget({ ...base, mode: undefined, agentId: null }),
    ).toBeNull();
  });
});
