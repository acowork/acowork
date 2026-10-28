/**
 * MemberManagerSection — 项目成员管理主体（无外壳）。
 *
 * 需求：每个项目可添加 Agent 实例为项目成员。联动指派的前提：
 * 任务 assignee 必须是项目成员。两个宿主表面复用同一份主体：
 * - `MemberManagerDialog`（群聊 GroupMembersPanel 的“+”入口）
 * - `PmSettingsPanel`（pm 视图右侧设置面板内嵌）
 *
 * 数据契约：
 * - 成员以 `instance_id` 标识，与 task.assignee 同一身份体系；`kind` 区分
 *   Agent 实例 / 人类账号（ADR-076 §决策 11）。候选列表分两块：Agent 来自
 *   agentStore，用户来自登录账号 + 账号目录（`pmUserDirectoryStore`，仅
 *   多用户模式有数据；目录端点排除调用者，所以登录账号单独补进候选）。
 * - 头像/名称不存快照，实时 join —— Agent 走 agentStore，人类走登录账号，
 *   两条路都收敛到 `pmMembers.resolveMembers`。
 * - 已是成员但解析不出名字（Agent 已卸载）→ 占位行，仅允许移除。
 * - 添加失败按 PmApiError.code 映射本地化文案（409 member_already_exists /
 *   member_has_open_tasks / 404 member_not_found）。
 */

import { useEffect, useMemo, useState } from "react";
import { useAgentStore } from "../../stores/agentStore";
import { useAuthStore } from "../../stores/authStore";
import { usePmProjectStore } from "../../stores/pm/projectStore";
import { usePmUserDirectoryStore } from "../../stores/pmUserDirectoryStore";
import { AgentAvatar } from "../../components/common/AgentAvatar";
import { UserAvatar } from "../../components/common/UserAvatar";
import { showToast } from "../../components/common/ToastProvider";
import { OverlayScrollbar } from "../../components/common/OverlayScrollbar";
import { useTranslation } from "../../i18n/useTranslation";
import { PmApiError } from "../../lib/pm-api";
import type { PmMemberKind, PmProject } from "../../lib/pm-types";
import {
  MemberAvatar,
  canGovernProject,
  projectOwnerId,
  usePmDirectoryUsers,
  useResolvedMembers,
  useSelfAccount,
  withOwner,
} from "./pmMembers";

export function MemberManagerSection({ project }: { project: PmProject }) {
  const { t } = useTranslation();
  const agents = useAgentStore((s) => s.agents);
  const fetchAgents = useAgentStore((s) => s.fetchAgents);
  const addMember = usePmProjectStore((s) => s.addMember);
  const removeMember = usePmProjectStore((s) => s.removeMember);
  const transferOwner = usePmProjectStore((s) => s.transferOwner);
  const [busyId, setBusyId] = useState<string | null>(null);

  // 用户候选数据源：登录账号（自己）+ 账号目录里的其他账号（仅多用户
  // 模式）。usePmDirectoryUsers 订阅目录 store，但加载由 useResolvedMembers
  // 的 effect 驱动 —— 这里再挂一个独立 effect，避免候选列表依赖“当前成员
  // 非空”才触发加载。
  const self = useSelfAccount();
  const directoryUsers = usePmDirectoryUsers();
  const accessToken = useAuthStore((s) => s.accessToken);
  const mode = useAuthStore((s) => s.mode);
  const status = useAuthStore((s) => s.status);
  const loadDirectory = usePmUserDirectoryStore((s) => s.loadDirectory);

  // The full display set — the owner folded in (see `withOwner`) so this
  // dialog agrees with the members panel / chat header on who is in the
  // group. The owner (`owner_id ?? created_by`) is a permanent member: it is
  // listed but not removable, and never offered as a candidate.
  const ownerId = projectOwnerId(project);
  // The backend gates every project mutation — rename / description / status,
  // delete, the member roster, and the ownership transfer — on
  // `x-actor == Project::owner() || admin` (PM's `require_governance`). Mirror
  // it here: a plain member reads the roster but gets no invite list and no
  // remove / make-owner button for an action the server would refuse (403).
  const canGovern = canGovernProject(project, self, mode === "local");
  const displayMembers = useMemo(() => withOwner(project), [project]);
  const memberIds = useMemo(
    () => new Set(displayMembers.map((m) => m.instance_id)),
    [displayMembers],
  );

  // 打开时确保 agent 列表已加载（成员候选来自 agentStore）
  useEffect(() => {
    if (Object.keys(agents).length === 0) void fetchAgents();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // 打开时确保账号目录已加载（用户成员候选）。loadDirectory 自带幂等与
  // 缓存，重复调用不会重复发请求。
  useEffect(() => {
    if (mode === "multi_user" && status === "logged_in" && self && accessToken) {
      void loadDirectory(self.user_id, accessToken);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // ESC 不再由主体处理：主体可内嵌在面板里，关闭语义归宿主
  //（Dialog 自己挂 ESC，PmSettingsPanel 无此需求）。

  const memberError = (e: unknown): string => {
    if (e instanceof PmApiError) {
      switch (e.code) {
        case "member_already_exists":
          return t("pm.memberAlreadyExists");
        case "member_has_open_tasks":
          return t("pm.memberHasOpenTasks");
        case "member_not_found":
          return t("pm.memberNotFound");
      }
    }
    return e instanceof Error ? e.message : String(e);
  };

  const handleAdd = async (instanceId: string, kind: PmMemberKind = "agent") => {
    setBusyId(instanceId);
    try {
      await addMember(project.id, instanceId, kind);
      showToast({ type: "success", message: t("pm.memberAdded") });
    } catch (e) {
      showToast({ type: "error", message: memberError(e) });
    } finally {
      setBusyId(null);
    }
  };

  const handleRemove = async (instanceId: string) => {
    setBusyId(instanceId);
    try {
      await removeMember(project.id, instanceId);
      showToast({ type: "success", message: t("pm.memberRemoved") });
    } catch (e) {
      showToast({ type: "error", message: memberError(e) });
    } finally {
      setBusyId(null);
    }
  };

  const handleTransfer = async (instanceId: string) => {
    setBusyId(instanceId);
    try {
      await transferOwner(project.id, instanceId);
      showToast({ type: "success", message: t("pm.ownerTransferred") });
    } catch (e) {
      const code = e instanceof PmApiError ? e.code : "";
      const message =
        code === "forbidden"
          ? t("pm.transferForbidden")
          : code === "bad_request"
            ? t("pm.transferTargetInvalid")
            : t("pm.transferFailed");
      showToast({ type: "error", message });
    } finally {
      setBusyId(null);
    }
  };

  // 可添加候选：agentStore 中存在、且还不是成员（按显示名排序）
  const candidates = useMemo(
    () =>
      Object.values(agents)
        .filter((a) => !memberIds.has(a.meta.instance_id))
        .sort((a, b) =>
          (a.meta.display_name || a.meta.name || "").localeCompare(
            b.meta.display_name || b.meta.name || "",
          ),
        ),
    [agents, memberIds],
  );

  // 可添加的用户候选：登录账号（目录接口排除调用者，自己不在目录里）+
  // 目录中其他账号，排除已是成员的（按显示名排序）。local 模式没有目录，
  // 列表为空 —— 成员解析走的是 "human" 哨兵，不是真实 user_id。
  const userCandidates = useMemo(() => {
    const rows: Array<{
      user_id: string;
      displayName: string;
      avatarUrl: string | null;
      builtinAvatarId: string | null;
    }> = [];
    if (self && !memberIds.has(self.user_id)) {
      rows.push({
        user_id: self.user_id,
        displayName: self.display_name || self.username,
        avatarUrl: self.avatar ?? null,
        builtinAvatarId: self.builtin_avatar ?? null,
      });
    }
    for (const u of directoryUsers) {
      if (memberIds.has(u.user_id) || rows.some((r) => r.user_id === u.user_id)) continue;
      rows.push({
        user_id: u.user_id,
        displayName: u.display_name || u.username,
        avatarUrl: u.avatar ?? null,
        builtinAvatarId: u.builtin_avatar ?? null,
      });
    }
    return rows.sort((a, b) => a.displayName.localeCompare(b.displayName));
    // 按 user_id 取值：账号异步解析到位后自动重算，且不依赖对象引用稳定性
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [memberIds, directoryUsers, self?.user_id]);

  // 已是成员：解析得出名字 → 显示；解析不出（Agent 已卸载）→ 占位
  const currentMembers = useResolvedMembers(displayMembers);

  return (
    <>
      <p className="mb-2 text-11 text-text-tertiary">
        {t("pm.memberManagerHint")}
      </p>

      {/* 当前成员 */}
          <div className="mb-1 text-11 font-medium text-text-tertiary ">
            {t("pm.members")}（{currentMembers.length}）
          </div>
          {currentMembers.length === 0 ? (
            <p className="mb-4 text-xs italic text-text-tertiary">{t("pm.noMembers")}</p>
          ) : (
            <ul className="mb-4 space-y-1">
              {currentMembers.map((m) => {
                const isOwner = m.instance_id === ownerId;
                // Only a human member can own the group (the backend rejects
                // agents and non-members), and only the owner/admin can start
                // a transfer.
                const canSetOwner = canGovern && !isOwner && m.kind === "user";
                return (
                  <li
                    key={m.instance_id}
                    className="flex items-center gap-2 rounded-md px-2 py-1.5 hover:bg-zinc-50 dark:hover:bg-zinc-800/60"
                  >
                    <MemberAvatar member={m} size={24} />
                    <div className="min-w-0 flex-1">
                      <div className="truncate text-xs text-text ">
                        {m.displayName ?? t("pm.memberNotFound")}
                      </div>
                      <div className="truncate text-10 text-text-tertiary">{m.instance_id}</div>
                    </div>
                    {isOwner ? (
                      // The group owner is a permanent member (the backend
                      // folds `owner()` into the participant set), so it is
                      // shown as the owner with no remove affordance. To
                      // remove the old owner, transfer first — they demote to
                      // a normal member and become removable.
                      <span className="shrink-0 rounded-md bg-accent-strong/10 px-2 py-1 text-11 font-medium text-[var(--color-accent)]">
                        {t("pm.owner")}
                      </span>
                    ) : (
                      <div className="flex shrink-0 items-center gap-1">
                        {canSetOwner && (
                          <button
                            type="button"
                            disabled={busyId === m.instance_id}
                            onClick={() => handleTransfer(m.instance_id)}
                            className="rounded-md px-2 py-1 text-11 font-medium text-[var(--color-accent)] hover:bg-accent-strong/10 disabled:opacity-40"
                          >
                            {t("pm.makeOwner")}
                          </button>
                        )}
                        {canGovern && (
                          <button
                            type="button"
                            disabled={busyId === m.instance_id}
                            onClick={() => handleRemove(m.instance_id)}
                            className="rounded-md px-2 py-1 text-11 text-red-600 hover:bg-red-50 disabled:opacity-40 dark:text-red-400 dark:hover:bg-red-950/40"
                          >
                            {t("pm.removeMember")}
                          </button>
                        )}
                      </div>
                    )}
                  </li>
                );
              })}
            </ul>
          )}

          {canGovern && (
            <>
            {/* 可添加的 Agent 候选 */}
            <div className="mb-1 text-11 font-medium text-text-tertiary ">
              {t("pm.agentMembers")}
            </div>
            {candidates.length === 0 ? (
              <p className="mb-4 text-xs italic text-text-tertiary">{t("pm.noMembers")}</p>
            ) : (
              <ul className="mb-4 space-y-1">
                {candidates.map((a) => (
                  <li
                    key={a.meta.instance_id}
                    className="flex items-center gap-2 rounded-md px-2 py-1.5 hover:bg-zinc-50 dark:hover:bg-zinc-800/60"
                  >
                    <AgentAvatar
                      agentId={a.meta.instance_id}
                      displayName={a.meta.display_name ?? a.meta.name}
                      avatarUrl={a.meta.avatar ?? null}
                      builtinAvatarId={a.meta.builtin_avatar ?? null}
                      size={24}
                    />
                    <div className="min-w-0 flex-1">
                      <div className="truncate text-xs text-text ">
                        {a.meta.display_name ?? a.meta.name ?? a.meta.agent_id}
                      </div>
                      <div className="truncate text-10 text-text-tertiary">{a.meta.instance_id}</div>
                    </div>
                    <button
                      type="button"
                      disabled={busyId === a.meta.instance_id}
                      onClick={() => handleAdd(a.meta.instance_id, "agent")}
                      className="shrink-0 rounded-md px-2 py-1 text-11 font-medium text-text-secondary hover:bg-zinc-100 disabled:opacity-40  dark:hover:bg-zinc-700"
                    >
                      {t("pm.addMember")}
                    </button>
                  </li>
                ))}
              </ul>
            )}
  
            {/* 可添加的用户候选（仅多用户模式有目录数据） */}
            <div className="mb-1 text-11 font-medium text-text-tertiary ">
              {t("pm.userMembers")}
            </div>
            {userCandidates.length === 0 ? (
              <p className="text-xs italic text-text-tertiary">{t("pm.noMembers")}</p>
            ) : (
              <ul className="space-y-1">
                {userCandidates.map((u) => (
                  <li
                    key={u.user_id}
                    className="flex items-center gap-2 rounded-md px-2 py-1.5 hover:bg-zinc-50 dark:hover:bg-zinc-800/60"
                  >
                    <UserAvatar
                      displayName={u.displayName}
                      avatarUrl={u.avatarUrl}
                      builtinAvatarId={u.builtinAvatarId}
                      size={24}
                    />
                    <div className="min-w-0 flex-1">
                      <div className="truncate text-xs text-text ">{u.displayName}</div>
                      <div className="truncate text-10 text-text-tertiary">{u.user_id}</div>
                    </div>
                    <button
                      type="button"
                      disabled={busyId === u.user_id}
                      onClick={() => handleAdd(u.user_id, "user")}
                      className="shrink-0 rounded-md px-2 py-1 text-11 font-medium text-text-secondary hover:bg-zinc-100 disabled:opacity-40  dark:hover:bg-zinc-700"
                    >
                      {t("pm.addMember")}
                    </button>
                  </li>
                ))}
              </ul>
            )}
            </>
          )}
    </>
  );
}

interface MemberManagerDialogProps {
  project: PmProject;
  onClose: () => void;
}

/** 居中对话框外壳 + `MemberManagerSection`（群聊面板入口）。 */
export function MemberManagerDialog({ project, onClose }: MemberManagerDialogProps) {
  const { t } = useTranslation();

  // ESC 关闭
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [onClose]);

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      <div className="absolute inset-0 bg-modal-overlay" onClick={onClose} />
      <div
        className="relative z-10 flex max-h-[80vh] w-full max-w-md flex-col overflow-hidden rounded-md border border-border-outer bg-modal-surface shadow-xl"
        role="dialog"
        aria-modal="true"
        aria-label={t("pm.memberManagerTitle")}
      >
        <header className="flex shrink-0 items-center justify-between border-b border-border-divider px-5 py-3 min-h-[var(--ui-dialog-zone-h)]">
          <h3 className="text-sm font-semibold text-text ">
            {t("pm.memberManagerTitle")}
          </h3>
          <button
            type="button"
            onClick={onClose}
            className="rounded px-1.5 text-sm text-text-tertiary hover:bg-zinc-100  dark:hover:bg-zinc-700"
            aria-label={t("common.close")}
          >
            ×
          </button>
        </header>

        <OverlayScrollbar fill="flex" className="px-5 py-3">
          <MemberManagerSection project={project} />
        </OverlayScrollbar>
      </div>
    </div>
  );
}
