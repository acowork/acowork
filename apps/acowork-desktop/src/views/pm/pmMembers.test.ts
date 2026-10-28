/**
 * Self-check for `pmMembers` — 项目成员身份解析（ADR-076 §决策 11）。
 *
 * What broke (and what this pins):
 *   `create_project_as` 自动把创建者放进 `members`。REST 面创建者是登录
 *   账号，`created_by` 是 user_id、成员 `kind: "user"`（见
 *   `core/acowork-pm/src/store/tree.rs`）。但每个 PM 视图各自 join
 *   `agentStore`，而 agentStore 只以 agent 实例 UUID 为键 —— 于是那个
 *   `kind: "user"` 成员永远解析不出名字：
 *
 *   - 成员管理对话框画出问号占位 + "未找到该 Agent 实例"（本 issue 的现场）
 *   - 项目头部头像组把它过滤掉，但计数用 `members.length` → 头像数对不上
 *   - 指派下拉只列 agentStore 里的 Agent → 项目创建者没法把任务指派给自己，
 *     而服务端 `ensure_assignee_is_member` 是允许的（它只查 members）
 *   - 任务卡片 / 详情抽屉里的创建者显示成裸 UUID
 *
 *   纯函数，无需挂 React 就能钉住映射（同 `useTaskForm.test.ts` 的做法）。
 */
import { describe, it, expect } from "vitest";
import {
  canGovern,
  canGovernProject,
  memberOptions,
  resolveActorLabel,
  resolveMember,
  resolveMembers,
} from "./pmMembers";
import type { PmProjectMember } from "../../lib/pm-types";
import type { UserAccount } from "../../lib/types";

const USER_ID = "21a727ee-d2ac-4d7a-9bb1-a846cb34cf3c";
const AGENT_ID = "58ef4139-ce28-46db-9ae9-7daecb042cb5";

const self = {
  user_id: USER_ID,
  username: "admin",
  display_name: "admin",
  avatar: "assets/avatars/21a727ee/avatar-01.jpg",
  builtin_avatar: null,
};

const agents = {
  [AGENT_ID]: {
    meta: {
      instance_id: AGENT_ID,
      agent_id: "com.acowork.ponytail",
      display_name: "Ponytail",
      avatar: null,
      builtin_avatar: "icon-07",
    },
  },
};

const addedAt = "2026-10-08T07:22:18Z";

function member(id: string, kind?: PmProjectMember["kind"]): PmProjectMember {
  return { instance_id: id, kind, added_at: addedAt };
}

describe("resolveMember (ADR-076 §决策 11 成员身份)", () => {
  it("resolves a kind:'user' member to the signed-in account", () => {
    const m = resolveMember(member(USER_ID, "user"), agents, self);
    expect(m.kind).toBe("user");
    expect(m.unresolved).toBe(false);
    expect(m.displayName).toBe("admin");
    expect(m.avatarUrl).toBe(self.avatar);
  });

  it("resolves an agent member through agentStore, not the account", () => {
    const m = resolveMember(member(AGENT_ID, "agent"), agents, self);
    expect(m.kind).toBe("agent");
    expect(m.displayName).toBe("Ponytail");
    expect(m.builtinAvatarId).toBe("icon-07");
    expect(m.unresolved).toBe(false);
  });

  it("treats a missing kind as 'agent' (old project.json, zero migration)", () => {
    const m = resolveMember(member(AGENT_ID), agents, self);
    expect(m.kind).toBe("agent");
    expect(m.displayName).toBe("Ponytail");
    // A user_id with no `kind` must NOT be silently named by the account —
    // old data predates the field, so the Agent path is the only honest read.
    expect(resolveMember(member(USER_ID), agents, self).displayName).toBeNull();
  });

  it("marks an unloaded agent member unresolved instead of throwing", () => {
    const m = resolveMember(member("ffffffff-0000-0000-0000-000000000000", "agent"), agents, self);
    expect(m.unresolved).toBe(true);
    expect(m.displayName).toBeNull();
  });

  it("resolves another user's name from the directory while signed in as admin", () => {
    const directory = [{
      user_id: "a5a57ab4-9a6c-4b53-909a-c7d6ca91ac05",
      username: "nicholas",
      display_name: "Nicholas",
      avatar: null,
      builtin_avatar: null,
    }];
    const m = resolveMember(
      member(directory[0].user_id, "user"), agents, self, null, directory,
    );
    expect(m.displayName).toBe("Nicholas");
    expect(m.unresolved).toBe(false);
  });

  it("resolves the other direction when nicholas views admin's project", () => {
    const nicholas = {
      user_id: "a5a57ab4-9a6c-4b53-909a-c7d6ca91ac05",
      username: "nicholas",
      display_name: "Nicholas",
    };
    const directory = [{
      user_id: USER_ID,
      username: "admin",
      display_name: "Administrator",
      avatar: null,
      builtin_avatar: null,
    }];
    const m = resolveMember(member(USER_ID, "user"), agents, nicholas, null, directory);
    expect(m.displayName).toBe("Administrator");
    expect(m.unresolved).toBe(false);
  });

  it("uses username when a directory user's display name is empty", () => {
    const directory = [{
      user_id: "a5a57ab4-9a6c-4b53-909a-c7d6ca91ac05",
      username: "nicholas",
      display_name: "",
    }];
    const m = resolveMember(
      member(directory[0].user_id, "user"), agents, self, null, directory,
    );
    expect(m.displayName).toBe("nicholas");
    expect(m.unresolved).toBe(false);
  });

  it("marks another account's user member unresolved when absent from the directory", () => {
    const m = resolveMember(
      member("a5a57ab4-9a6c-4b53-909a-c7d6ca91ac05", "user"), agents, self, null, [],
    );
    expect(m.unresolved).toBe(true);
    expect(m.displayName).toBeNull();
  });
});

 describe("memberOptions (assignee dropdown, linked assignment)", () => {
  it("lists the creator's user_id so a task can be assigned to oneself", () => {
    const opts = memberOptions(
      [member(USER_ID, "user"), member(AGENT_ID, "agent")],
      agents,
      self,
    );
    expect(opts).toEqual([
      { value: USER_ID, label: "admin" },
      { value: AGENT_ID, label: "Ponytail" },
    ]);
  });

  it("keeps an unresolved member selectable, labelled by its raw id", () => {
    // Dropping it would silently clear the assignee of any task the
    // (unloaded) agent still owns — the edit dialog would offer no way to
    // keep the current value.
    const opts = memberOptions([member("ffffffff-0000-0000-0000-000000000000", "agent")], agents, self);
    expect(opts).toEqual([{ value: "ffffffff-0000-0000-0000-000000000000", label: "ffffffff-0000-0000-0000-000000000000" }]);
  });

  it("returns an empty list when the project has no members", () => {
    expect(memberOptions([], agents, self)).toEqual([]);
  });
});

describe("resolveActorLabel (task.assignee / created_by — no kind on the wire)", () => {
  it("prefers the agent name, then the account, then the raw id", () => {
    expect(resolveActorLabel(agents, self, AGENT_ID)).toBe("Ponytail");
    expect(resolveActorLabel(agents, self, USER_ID)).toBe("admin");
    expect(resolveActorLabel(agents, self, "nope")).toBe("nope");
  });

  it("returns null for an absent actor", () => {
    expect(resolveActorLabel(agents, self, null)).toBeNull();
  });
});

describe("resolveMembers (header avatar stack)", () => {
  it("keeps unresolved members in the list so the count matches the badge", () => {
    // ProjectHeader renders `members.length` next to the avatars; filtering
    // unresolved entries out made the two disagree.
    const views = resolveMembers(
      [member(USER_ID, "user"), member(AGENT_ID, "agent"), member("dead-beef", "agent")],
      agents,
      self,
    );
    expect(views).toHaveLength(3);
    expect(views.map((v) => v.unresolved)).toEqual([false, false, true]);
  });
});

describe("local mode (X-Actor: 'human' sentinel, account=null)", () => {
  // local 模式下 Gateway 注入 `X-Actor: "human"`，PM 落盘
  // `{instance_id: "human", kind: "user"}`；`authStore` 在 status="disabled"
  // 处早退，account 恒 null。这种成员**不能**判 unresolved —— 它就是当前
  // 桌面操作者本人，应该用 userProfileStore 命名。
  const localProfile = {
    displayName: "admin",
    backendAvatarUrl: "assets/avatars/local.jpg",
    backendBuiltinAvatarId: "icon-03",
  };

  it("resolves {instance_id: 'human'} via userProfileStore when account is null", () => {
    const m = resolveMember(member("human", "user"), agents, null, localProfile);
    expect(m.unresolved).toBe(false);
    expect(m.displayName).toBe("admin");
    expect(m.avatarUrl).toBe("assets/avatars/local.jpg");
    expect(m.builtinAvatarId).toBe("icon-03");
  });

  it("resolves {instance_id: 'unknown'} (X-Actor 缺失时的兜底) the same way", () => {
    const m = resolveMember(member("unknown", "user"), agents, null, localProfile);
    expect(m.unresolved).toBe(false);
    expect(m.displayName).toBe("admin");
  });

  it("profile with empty displayName does not produce a blank dropdown option", () => {
    // ProfileStore 的 default 是 `i18n.t("common.me")` = "我"，通常非空；
    // 但如果有人手动清掉，label = "" 会让下拉显示空白选项（更糟）。
    // `memberOptions` 用 `displayName ?? instance_id` 兜底，所以空名退化
    // 成 raw id "human" 而不是空白字符串。
    const opts = memberOptions(
      [member("human", "user")],
      agents,
      null,
      { ...localProfile, displayName: "" },
    );
    expect(opts).toEqual([{ value: "human", label: "human" }]);
  });

  it("memberOptions in local mode offers the creator under their profile name", () => {
    const opts = memberOptions([member("human", "user")], agents, null, localProfile);
    expect(opts).toEqual([{ value: "human", label: "admin" }]);
  });
});
// ── Governance: who may mutate a project / a group ─────────────────────
// The card, the roster buttons and the project settings all read this one
// predicate, which mirrors the backend gates (`chat_sessions::can_govern_group`
// and PM's `require_governance`). A plain member must get neither the group
// prompt nor a project mutation the server would refuse with a 403.
describe("canGovern / canGovernProject (ADR-098 owner-or-admin)", () => {
  const account = (user_id: string, role: "user" | "admin" = "user") =>
    ({ user_id, role }) as unknown as UserAccount;
  const project = (created_by: string, owner_id?: string) =>
    ({ created_by, owner_id }) as unknown as Parameters<typeof canGovernProject>[0];

  it("admits the owner", () => {
    expect(canGovern("u-owner", account("u-owner"), false)).toBe(true);
  });

  it("admits an admin who is not the owner", () => {
    expect(canGovern("u-owner", account("u-admin", "admin"), false)).toBe(true);
  });

  it("refuses a plain member, with or without a known owner", () => {
    expect(canGovern("u-owner", account("u-plain"), false)).toBe(false);
    // A project channel whose project has not resolved yet — still nothing.
    expect(canGovern("", account("u-plain"), false)).toBe(false);
  });

  it("refuses while no account is resolved (still loading)", () => {
    expect(canGovern("u-owner", null, false)).toBe(false);
  });

  it("admits the single local actor with no account at all", () => {
    // local: `authStore.account` is permanently null and the Gateway injects
    // the constant actor "human", so the machine's one human governs
    // everything — including a project an agent created.
    expect(canGovern("com.acowork.some-agent", null, true)).toBe(true);
    expect(canGovernProject(project("com.acowork.some-agent"), null, true)).toBe(true);
  });

  it("follows owner_id over created_by (a transfer moved the governance)", () => {
    expect(canGovernProject(project("u-old", "u-new"), account("u-new"), false)).toBe(true);
    expect(canGovernProject(project("u-old", "u-new"), account("u-old"), false)).toBe(false);
  });
});
