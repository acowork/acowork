/**
 * Regression tests for the assignee dropdown and the date fields
 * behind the task form (`useTaskForm`, extracted from the retired TaskEditDialog).
 *
 * ADR-073 background:
 *   Before ADR-073 was fully applied to acowork-pm, the assignee dropdown
 *   used `meta.agent_id` (the package id, e.g. "com.acowork.ponytail") as
 *   its `value`. Meanwhile the agentStore keys entries by `meta.instance_id`
 *   (a UUID v4). The two were out of sync, so the dropdown round-trip was
 *   broken: the displayed selection (UUID) never matched the saved task's
 *   `assignee` (package id) when the user changed selection.
 *
 *   The dropdown `value` is now `meta.instance_id`, matching
 *   `task.assignee` storage. This file pins that mapping.
 *
 * ADR-076 §决策 11 follow-up:
 *   The dropdown no longer derives its options from agentStore alone — it
 *   builds them from `project.members` (Agent and human alike), because the
 *   project creator is a `kind: "user"` member and must be assignable. The
 *   builder and its ADR-073 invariants now live in `pmMembers.ts`; this file
 *   keeps the assignee-specific expectations so the rename does not silently
 *   drop coverage.
 */
import { describe, it, expect } from "vitest";
import { memberOptions } from "./pmMembers";

const USER_ID = "21a727ee-d2ac-4d7a-9bb1-a846cb34cf3c";
const AGENT_ID = "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d";
const addedAt = "2026-10-08T07:22:18Z";

const agents = {
  [AGENT_ID]: {
    meta: {
      instance_id: AGENT_ID,
      agent_id: "com.acowork.ponytail",
      display_name: "Ponytail Display",
      name: "Ponytail Name",
    },
  },
  "a91b07e4-c2d3-4f8b-a91b-07e4c2d34f8b": {
    meta: {
      instance_id: "a91b07e4-c2d3-4f8b-a91b-07e4c2d34f8b",
      agent_id: "com.acowork.ponytail",
      display_name: "Ponytail (workspace-b)",
    },
  },
};

const self = { user_id: USER_ID, username: "admin", display_name: "admin" };

const members = (ids: Array<{ id: string; kind?: "agent" | "user" }>) =>
  ids.map((m) => ({ instance_id: m.id, kind: m.kind, added_at: addedAt }));

describe("assignee dropdown options", () => {
  it("uses instance_id as value (NOT package agent_id)", () => {
    const opts = memberOptions(members([{ id: AGENT_ID, kind: "agent" }]), agents, self);
    expect(opts).toHaveLength(1);
    expect(opts[0].value).toBe(AGENT_ID);
    // Regression: must NOT leak the package id as the option value.
    expect(opts[0].value).not.toBe("com.acowork.ponytail");
    expect(opts[0].value).not.toContain("com.acowork");
  });

  it("prefers display_name over name over agent_id for label", () => {
    const opts = memberOptions(
      members([
        { id: AGENT_ID, kind: "agent" },
        { id: "u2", kind: "agent" },
        { id: "u3", kind: "agent" },
      ]),
      {
        ...agents,
        u2: { meta: { instance_id: "u2", agent_id: "p2", name: "Architect Name" } },
        u3: { meta: { instance_id: "u3", agent_id: "p3" } },
      },
      self,
    );
    expect(opts[0].label).toBe("Ponytail Display");
    expect(opts[1].label).toBe("Architect Name");
    expect(opts[2].label).toBe("p3"); // 最后 fallback to agent_id
  });

  it("keeps multiple instances of the same package as distinct options", () => {
    // ADR-073 invariant 1: same package can have multiple instances, each
    // with its own UUID. The dropdown must show them as separate options,
    // not collapse them (which the old code did when keyed by agent_id).
    const opts = memberOptions(
      members([
        { id: AGENT_ID, kind: "agent" },
        { id: "a91b07e4-c2d3-4f8b-a91b-07e4c2d34f8b", kind: "agent" },
      ]),
      agents,
      self,
    );
    expect(opts).toHaveLength(2);
    expect(opts[0].value).not.toBe(opts[1].value);
    expect(opts[0].label).toBe("Ponytail Display");
    expect(opts[1].label).toBe("Ponytail (workspace-b)");
    // Both values are valid ids (no package id leakage)
    for (const opt of opts) {
      expect(opt.value).not.toBe("com.acowork.ponytail");
    }
  });

  it("returns empty array for empty member list", () => {
    expect(memberOptions([], agents, self)).toEqual([]);
  });

  it("downstream consumer can round-trip selection to task.assignee", () => {
    // Simulate: user selects the first option, code reads `opts[0].value`
    // and sends it as task.assignee. The persisted assignee must be the
    // instance_id (UUID), not the display label.
    const opts = memberOptions(members([{ id: AGENT_ID, kind: "agent" }]), agents, self);
    const selectedValue = opts[0].value;
    // The saved assignee is the UUID, never the display name.
    expect(selectedValue).toMatch(/^[0-9a-f-]{36}$/);
    expect(selectedValue).not.toBe("Ponytail Display");
  });

  it("offers the project's human member alongside its agents (ADR-076 §决策 11)", () => {
    // 联动指派: assignee must be a project member. The creator is a
    // `kind: "user"` member — the dropdown has to offer them, or the user
    // cannot assign a task to themselves while the server accepts it.
    const opts = memberOptions(
      members([{ id: USER_ID, kind: "user" }, { id: AGENT_ID, kind: "agent" }]),
      agents,
      self,
    );
    expect(opts).toHaveLength(2);
    expect(opts[0]).toEqual({ value: USER_ID, label: "admin" });
  });

  it("does not offer agents that are not project members", () => {
    // 联动指派：assignee 必须是项目成员。非成员 Agent 不得出现在选项中。
    const nonMember = "22222222-2222-2222-2222-222222222222";
    const opts = memberOptions(
      members([{ id: AGENT_ID, kind: "agent" }]),
      {
        ...agents,
        [nonMember]: {
          meta: { instance_id: nonMember, agent_id: "com.acowork.nonmember", display_name: "Not A Member" },
        },
      },
      self,
    );
    expect(opts).toHaveLength(1);
    expect(opts[0].value).toBe(AGENT_ID);
    expect(opts[0].label).toBe("Ponytail Display");
  });
});
/**
 * Date-field round-trip (plan §8 debt: the timezone drift the old TaskEditDialog
 * had).
 * The date input carries a *local* calendar day; the stored instant is
 * local midnight. The old code (`new Date("YYYY-MM-DD")`) parsed as UTC
 * midnight, so users west of UTC saw the previous day after a re-edit.
 * These run in the test runner's timezone (Asia/Shanghai, UTC+8):
 * UTC-midnight storage would render 08:00 local, local-midnight storage
 * renders 00:00 — the assertions below pin the local-midnight semantics.
 */
describe("task form date fields", () => {
  it("stores a date field as local midnight", async () => {
    const { localDateFieldToIso } = await import("./useTaskForm");
    const iso = localDateFieldToIso("2026-10-30");
    const d = new Date(iso);
    expect(d.getFullYear()).toBe(2026);
    expect(d.getMonth()).toBe(9);
    expect(d.getDate()).toBe(30);
    expect(d.getHours()).toBe(0);
    expect(d.getMinutes()).toBe(0);
  });

  it("round-trips iso back to the same local calendar day", async () => {
    const { localDateFieldToIso, isoToLocalDate } = await import("./useTaskForm");
    expect(isoToLocalDate(localDateFieldToIso("2026-10-30"))).toBe("2026-10-30");
    expect(isoToLocalDate(localDateFieldToIso("2026-02-01"))).toBe("2026-02-01");
  });

  it("tolerates empty and malformed input", async () => {
    const { isoToLocalDate } = await import("./useTaskForm");
    expect(isoToLocalDate(null)).toBe("");
    expect(isoToLocalDate(undefined)).toBe("");
    expect(isoToLocalDate("not-a-date")).toBe("not-a-date");
  });
});
