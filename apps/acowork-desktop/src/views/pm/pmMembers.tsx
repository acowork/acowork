/**
 * pmMembers — 项目成员身份解析（ADR-076 §决策 11）。
 *
 * `ProjectMember.instance_id` 承载两种身份：`kind: "agent"` → agent 实例
 * UUID（ADR-073），`kind: "user"` → user_id（ADR-076）。两者只差一个字段值，
 * 显示信息来自完全不同的源：Agent 走 `agentStore`，人类走当前登录账号。
 *
 * 之前每个视图各自 join `agentStore`，于是 `kind: "user"` 的成员（恒为项目
 * 创建者，见 `create_project_as`）永远查不到 → 画问号占位 / 显示裸 UUID /
 * 指派下拉里选不到自己。本模块是该 join 的**唯一**实现。
 *
 * 不存快照：成员身份是身份，显示名/头像是当前状态，join 实时源。
 *
 * 为什么不查 `/api/users/directory`：User 成员只可能来自"创建者"（加成员的
 * REST/MCP 路径都要求 `agent_exists`），所以当前登录账号就能命名列表里
 * 唯一可能的 User 成员。新增 HTTP 调用会让 PM 离线只读模式多一个失败点，
 * 收益为零。
 *
 * ponytail: 跨用户显示（给别人的项目看成员）需要 `/api/users/directory`。
 * 支持"添加人类成员"时再接这个源——那时列表里会有非本机账号，当前实现
 * 对它们退化为裸 UUID，与加成员之前的行为一致。
 */

import { useMemo } from "react";
import { useAuthStore } from "../../stores/authStore";
import { useAgentStore } from "../../stores/agentStore";
import { useUserProfileStore } from "../../stores/userProfileStore";
import { AgentAvatar } from "../../components/common/AgentAvatar";
import { UserAvatar } from "../../components/common/UserAvatar";
import type { PmProjectMember } from "../../lib/pm-types";
import type { UserAccount } from "../../lib/types";

/**
 * `kind: "user"` 成员的解析源。
 *
 * 两条候选身份，**取第一个命中的**：
 *
 * 1. `authStore.account`（multi_user 模式）—— 用 user_id 精确匹配，命中即
 *    返回该账号（带自定义头像）。账号可能比 user_id 字段晚几个渲染帧到位
 *    （`authStore.init()` 走 `fetchMe`），hook 订阅 store，会自动重渲染。
 * 2. `userProfileStore.profile`（local 模式或单用户回退）—— local 模式下
 *    `authStore` 在 `status="disabled"` 处早退、`account` 恒 null，但
 *    Gateway 仍然注入 `X-Actor: "human"` 哨兵常量，PM 把
 *    `{instance_id: "human", kind: "user"}` 写进 members。userProfileStore
 *    是 local 模式下唯一稳定的人类身份源（持久化在 localStorage）。
 *
 * 不引入 `/api/users/directory` 调用 —— PM 离线只读模式不该多一个失败点。
 * 见文件头 ponytail 注释。
 */


/**
 * 成员身份最小投影：Agent 与 User 两条路解析完都落成这一个形状，
 * 调用方不再分支。
 */
export interface ResolvedMember {
  instance_id: string;
  /** `PmProjectMember.kind`，缺省 = `agent`（兼容旧 project.json）。 */
  kind: "agent" | "user";
  /** 可展示的名字；解析不出来（Agent 已卸载 / 账号不在）时为 `null`。 */
  displayName: string | null;
  /** 自定义头像文件路径（相对路径，需经 avatar-file 端点取字节）。 */
  avatarUrl: string | null;
  /** 内置头像图标 ID（`BUILTIN_ICONS` 的 key）。 */
  builtinAvatarId: string | null;
  /** `true` = 解析不出任何名字，只有裸 UUID（Agent 已卸载等）。 */
  unresolved: boolean;
}

/** 供纯函数测试注入的 agent 形状（agentStore 的 `agents[id].meta` 子集）。 */
export interface AgentMetaLike {
  instance_id: string;
  agent_id?: string;
  display_name?: string;
  name?: string;
  avatar?: string;
  builtin_avatar?: string;
}

/** 供纯函数测试注入的账号形状。 */
export interface AccountLike {
  user_id: string;
  username: string;
  display_name: string;
  avatar?: string | null;
  builtin_avatar?: string | null;
}

type AgentMap = Record<string, { meta?: AgentMetaLike } | undefined>;

/** 解析 User 成员的纯函数 —— 从调用方已订阅的源里取，避免内部 hook。 */
function resolveUserMemberPure(
  member: PmProjectMember,
  self: AccountLike | null,
  localProfile: { displayName: string; backendAvatarUrl?: string | null; backendBuiltinAvatarId?: string | null } | null,
): { displayName: string | null; avatarUrl: string | null; builtinAvatarId: string | null; unresolved: boolean } {
  if (self && self.user_id === member.instance_id) {
    return {
      displayName: self.display_name || self.username,
      avatarUrl: self.avatar ?? null,
      builtinAvatarId: self.builtin_avatar ?? null,
      unresolved: false,
    };
  }
  // local 模式哨兵（"human"/"unknown"，见 MemberKind::from_actor）→ 落到
  // 当前用户的 userProfileStore。displayName 也用它，因为 local 模式没
  // 账号可读；multi_user 模式账号若不匹配则保留 unresolved。
  if (member.instance_id === "human" || member.instance_id === "unknown") {
    if (!localProfile) return { displayName: null, avatarUrl: null, builtinAvatarId: null, unresolved: true };
    return {
      displayName: localProfile.displayName || null,
      avatarUrl: localProfile.backendAvatarUrl ?? null,
      builtinAvatarId: localProfile.backendBuiltinAvatarId ?? null,
      unresolved: false,
    };
  }
  return { displayName: null, avatarUrl: null, builtinAvatarId: null, unresolved: true };
}

/**
 * 解析单个成员。
 *
 * - `kind: "user"` → 多用户走账号精确匹配；本地走 userProfileStore；
 *   都对不上（跨用户 ID）→ unresolved（见文件头 ponytail 注释）。
 * - `agent` → `agents[instance_id].meta`，与 ADR-073 的 agent 路径完全一致。
 */
export function resolveMember(
  member: PmProjectMember,
  agents: AgentMap,
  self: AccountLike | null,
  localProfile?: { displayName: string; backendAvatarUrl?: string | null; backendBuiltinAvatarId?: string | null } | null,
): ResolvedMember {
  const base = {
    instance_id: member.instance_id,
    kind: member.kind ?? "agent",
    avatarUrl: null,
    builtinAvatarId: null,
  };
  if (member.kind === "user") {
    const u = resolveUserMemberPure(member, self, localProfile ?? null);
    return { ...base, displayName: u.displayName, avatarUrl: u.avatarUrl, builtinAvatarId: u.builtinAvatarId, unresolved: u.unresolved };
  }
  const meta = agents[member.instance_id]?.meta;
  if (!meta) return { ...base, displayName: null, unresolved: true };
  return {
    ...base,
    displayName: meta.display_name || meta.name || meta.agent_id || null,
    avatarUrl: meta.avatar ?? null,
    builtinAvatarId: meta.builtin_avatar ?? null,
    unresolved: false,
  };
}

/** 解析整份成员列表。 */
export function resolveMembers(
  members: ReadonlyArray<PmProjectMember>,
  agents: AgentMap,
  self: AccountLike | null,
  localProfile?: { displayName: string; backendAvatarUrl?: string | null; backendBuiltinAvatarId?: string | null } | null,
): ResolvedMember[] {
  return members.map((m) => resolveMember(m, agents, self, localProfile));
}

/**
 * 成员身份 → 指派下拉选项。值 = `instance_id`，与 `task.assignee` 同一
 * 身份体系；label = 解析出的名字。无法解析的成员**仍然列出**（label = 裸
 * UUID）——否则一个已卸载的 Agent 成员会静默消失，看板上打开它名下未完成
 * 的任务时反而发现无法保持原指派。
 */
export function memberOptions(
  members: ReadonlyArray<PmProjectMember>,
  agents: AgentMap,
  self: AccountLike | null,
  localProfile?: { displayName: string; backendAvatarUrl?: string | null; backendBuiltinAvatarId?: string | null } | null,
): Array<{ value: string; label: string }> {
  return resolveMembers(members, agents, self, localProfile).map((m) => ({
    value: m.instance_id,
    label: m.displayName ?? m.instance_id,
  }));
}

/**
 * 成员头像：Agent 与人类走不同组件（字节分别来自 agent avatar-file 与
 * user avatar-file 端点），解析不出名字时退回问号占位。
 *
 * 头像栈（ProjectHeader）刻意**不**过滤未解析成员 —— 过滤会让画出来的
 * 头像数和旁边的成员计数对不上；这里渲染问号，计数就对齐了。
 */
export function MemberAvatar({
  member,
  size,
  className,
}: {
  member: ResolvedMember;
  size: number;
  className?: string;
}) {
  if (member.unresolved) {
    return (
      <div
        className={`flex shrink-0 items-center justify-center rounded-lg bg-zinc-200 text-10 text-text-tertiary dark:bg-zinc-700 ${className ?? ""}`}
        style={{ width: size, height: size }}
      >
        ?
      </div>
    );
  }
  return member.kind === "user" ? (
    <UserAvatar
      displayName={member.displayName ?? undefined}
      avatarUrl={member.avatarUrl}
      builtinAvatarId={member.builtinAvatarId}
      size={size}
      className={className}
    />
  ) : (
    <AgentAvatar
      agentId={member.instance_id}
      displayName={member.displayName ?? undefined}
      avatarUrl={member.avatarUrl}
      builtinAvatarId={member.builtinAvatarId}
      size={size}
      className={className}
    />
  );
}

/**
 * 任务上的 actor id → 显示名。
 *
 * `task.assignee` / `task.created_by` 是裸字符串，没有 `kind` 可判（REST
 * 契约里 assignee 只有值域没有身份类型），所以按域试查：先 agentStore，再
 * 登录账号，都没有就退回裸 id —— 也就是修复前的行为。
 *
 * ponytail: agent instance_id 与 user_id 都是 UUID v4，理论可碰撞，撞上时
 * agentStore 优先（agent 显示优先于人类）。要让 actor 侧也无歧义，得让
 * REST 在 task 上也透出 `kind`（跟成员一样）——那是契约变更，等真有需求
 * （跨用户项目 / 给他人指派）再说。
 */
export function resolveActorLabel(
  agents: AgentMap,
  self: AccountLike | null,
  id: string | null,
): string | null {
  if (!id) return null;
  const meta = agents[id]?.meta;
  if (meta) return meta.display_name || meta.name || meta.agent_id || id;
  if (self && self.user_id === id) return self.display_name || self.username;
  return id;
}

/**
 * `resolveActorLabel` 的 hook 版（订阅 agentStore + 登录账号）。
 * 调用方直接传 agent 渲染需要的话不要用这个。
 */
export function useActorLabel(id: string | null): string | null {
  const agents = useAgentStore((s) => s.agents);
  const self = useSelfAccount();
  return useMemo(() => resolveActorLabel(agents, self, id), [agents, self, id]);
}

/**
 * 当前登录账号 —— React 订阅（`UserAccount | null`；local 模式恒 null）。
 *
 * 账号是异步解析的（`authStore.init()` 里的 `fetchMe`），首次渲染可能还是
 * null；订阅 store 让账号到位后自动重渲染，不用每个调用方自己轮询。
 */
export function useSelfAccount(): UserAccount | null {
  return useAuthStore((s) => s.account);
}

/**
 * 成员解析 hook：成员列表 + agentStore + 登录账号 + 本地 profile → 解析结果。
 *
 * memo 依赖是四个源本身；`projects` store 的 `members` 数组在每次 reload 时
 * 都是新引用，所以按引用比内容是对的（也是 store 契约要求的读法）。
 */
export function useResolvedMembers(
  members: ReadonlyArray<PmProjectMember> | undefined,
): ResolvedMember[] {
  const agents = useAgentStore((s) => s.agents);
  const self = useSelfAccount();
  const localProfile = useUserProfileStore((s) => s.profile);
  return useMemo(
    () => resolveMembers(members ?? [], agents, self, localProfile),
    [members, agents, self, localProfile],
  );
}