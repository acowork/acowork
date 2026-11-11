import { describe, expect, it } from "vitest";
import {
  attentionByAgent,
  attentionReason,
  attentionSessions,
  isStaleParkedStatus,
  needsAttention,
} from "./sessionAttention";
import type { SessionStatus } from "./types";

describe("isStaleParkedStatus (the pull half of the repair)", () => {
  it("calls a parked status on a non-resident session stale", () => {
    // The state topic is retained, so the last publish outlives the Runtime
    // process that made it. A turn parked on a card is a loop blocked
    // mid-iteration and therefore necessarily resident — so `is_active: false`
    // is proof the card is gone, and the dot would otherwise never clear.
    expect(isStaleParkedStatus({ is_active: false }, waiting)).toBe(true);
    expect(isStaleParkedStatus({ is_active: false }, paused({ reason: "iteration_limit" }))).toBe(true);
  });

  it("leaves a resident session alone — the push is the fresher fact", () => {
    expect(isStaleParkedStatus({ is_active: true }, waiting)).toBe(false);
  });

  it("an idle status is never stale: there is nothing to retire", () => {
    expect(isStaleParkedStatus({ is_active: false }, { status: "idle" })).toBe(false);
    expect(isStaleParkedStatus({ is_active: false }, null)).toBe(false);
    expect(isStaleParkedStatus({ is_active: false }, undefined)).toBe(false);
  });

  it("a Runtime that does not report residency proves nothing", () => {
    // Clearing live state on an absent field would hide a real card.
    expect(isStaleParkedStatus({}, waiting)).toBe(false);
  });
});

const working: SessionStatus[] = [
  { status: "idle" },
  { status: "llm_awaiting_first_chunk" },
  { status: "thinking" },
  { status: "llm_streaming", detail: { message_id: "m-1" } },
  { status: "tool_executing" },
];

const waiting: SessionStatus = {
  status: "waiting_approval",
  detail: { request_id: "req-1" },
};

const paused = (detail: Record<string, unknown>): SessionStatus =>
  ({ status: "paused", detail }) as unknown as SessionStatus;

describe("attentionReason", () => {
  it("names a waiting_approval session as an obligation to answer", () => {
    // Both interactive cards (tool approval, ask_user_question) land on this
    // one status: the status carries a request id, never a card kind.
    expect(attentionReason(waiting)).toBe("approval");
    expect(needsAttention(waiting)).toBe(true);
  });

  it("keeps every self-driving state out of the badge", () => {
    // Streaming / thinking / tool-running is "busy", which the sidebar already
    // says with the emerald dot. Showing red for it would make red mean
    // "anything is happening" and therefore nothing.
    for (const status of working) {
      expect(attentionReason(status)).toBeNull();
      expect(needsAttention(status)).toBe(false);
    }
  });

  it("maps each human-decidable pause reason", () => {
    expect(attentionReason(paused({ reason: "iteration_limit" }))).toBe("iteration_limit");
    expect(attentionReason(paused({ reason: "loop_detected" }))).toBe("loop_detected");
    expect(attentionReason(paused({ reason: "debug" }))).toBe("debug");
  });

  it("does not badge a 429 retry wait", () => {
    // The Runtime resumes this one by itself; a dot here teaches the user to
    // ignore dots.
    expect(
      attentionReason(
        paused({
          iteration: null,
          max_iterations: null,
          retry_info: { wait_ms: 8000, attempt: 1, max_attempts: 3, provider: "openai" },
        }),
      ),
    ).toBeNull();
  });

  it("treats an unattributed non-retry pause as a wait, because it is one", () => {
    // A Runtime from before `PauseReason` sends `{status:"paused"}` with no
    // reason. Every non-retry pause waits for a decision, so the badge is
    // correct rather than invented — and `paused` stays a distinct reason so
    // the label can stay generic.
    expect(attentionReason(paused({ iteration: 12, max_iterations: 12 }))).toBe("paused");
    expect(attentionReason({ status: "paused" })).toBe("paused");
  });

  it("treats an unknown status as unknown, never as attention", () => {
    // Absence of a pushed status (never opened, older Runtime) must not summon
    // a badge.
    expect(attentionReason(null)).toBeNull();
    expect(attentionReason(undefined)).toBeNull();
  });
});

describe("attentionSessions", () => {
  it("lists the parked sessions of one agent", () => {
    const sessions = attentionSessions({
      "s-idle": { sessionStatus: { status: "idle" } },
      "s-wait": { sessionStatus: waiting },
      "s-pause": { sessionStatus: paused({ reason: "iteration_limit" }) },
      "s-none": {},
    });
    expect([...sessions].sort()).toEqual(["s-pause", "s-wait"]);
  });
});

describe("attentionByAgent", () => {
  it("reports one reason per agent, from any of its sessions", () => {
    const got = attentionByAgent({
      "a-1": { sessionStates: { "s-1": { sessionStatus: waiting } } },
      "a-2": { sessionStates: { "s-2": { sessionStatus: { status: "thinking" } } } },
    });
    expect(got.get("a-1")).toBe("approval");
    expect(got.has("a-2")).toBe(false);
  });

  it("suppresses only the session that is on screen", () => {
    // The card of the visible session is in front of the reader, so its dot has
    // nothing left to say; a SIBLING parked session still gets one, because that
    // card is somewhere the reader is not.
    const states = {
      "a-1": {
        sessionStates: {
          "s-visible": { sessionStatus: waiting },
          "s-other": { sessionStatus: paused({ reason: "iteration_limit" }) },
        },
      },
    };
    expect(attentionByAgent(states, { agentId: "a-1", sessionId: "s-visible" }).has("a-1")).toBe(
      true,
    );
    // Nothing else parked: the suppression empties the badge.
    const onlyVisible = {
      "a-1": { sessionStates: { "s-visible": { sessionStatus: waiting } } },
    };
    expect(
      attentionByAgent(onlyVisible, { agentId: "a-1", sessionId: "s-visible" }).has("a-1"),
    ).toBe(false);
    // A different agent being visible changes nothing.
    expect(
      attentionByAgent(onlyVisible, { agentId: "a-2", sessionId: "s-visible" }).has("a-1"),
    ).toBe(true);
  });

  it("tolerates agents and sessions it has never cached", () => {
    expect(attentionByAgent({}).size).toBe(0);
    expect(attentionByAgent({ "a-1": undefined }).size).toBe(0);
    expect(attentionByAgent({ "a-1": { sessionStates: {} } }).size).toBe(0);
  });
});
