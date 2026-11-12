/**
 * `taskExecSessionIn` — "这个任务的执行 session 是哪一个" 的唯一判据。
 *
 * 这条规则住在客户端（群聊转录里找引用行），所以它得有一个能跑的检查：
 * 认错任务 / 认错行，用户就会被送到别人的执行现场。
 */
import { describe, expect, it } from "vitest";
import { taskExecSessionIn } from "./pmTaskExecSession";

const round = (taskId: string, instance: string, session: string) => ({
  kind: "execution_round",
  meta: { task_id: taskId, agent_instance_id: instance, agent_session_id: session },
});

describe("taskExecSessionIn", () => {
  it("returns the newest round for the task (rows ascend by seq)", () => {
    const rows = [
      round("t-1", "inst-a", "sess-old"),
      round("t-2", "inst-x", "sess-other"),
      round("t-1", "inst-a", "sess-new"),
    ];
    expect(taskExecSessionIn(rows, "t-1")).toEqual({
      agentInstanceId: "inst-a",
      execSessionId: "sess-new",
    });
  });

  it("ignores rows of another task, other kinds, and half-addresses", () => {
    const rows = [
      { kind: "user_message", meta: { task_id: "t-1" } },
      { kind: "execution_round", meta: { task_id: "t-1", agent_instance_id: "inst-a" } },
      { kind: "execution_round", meta: { task_id: "t-1", agent_session_id: "sess-b" } },
      round("t-2", "inst-a", "sess-other"),
    ];
    expect(taskExecSessionIn(rows, "t-1")).toBeNull();
  });

  it("never throws on an empty transcript", () => {
    expect(taskExecSessionIn([], "t-1")).toBeNull();
  });
});