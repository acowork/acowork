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
import { usePmProjectStore } from "../../stores/pm/projectStore";
import { showToast } from "../../components/common/ToastProvider";
import { OverlayScrollbar } from "../../components/common/OverlayScrollbar";
import { useTranslation } from "../../i18n/useTranslation";
import { PmApiError } from "../../lib/pm-api";
import type { PmMemberKind, PmProject } from "../../lib/pm-types";
import {
  MemberAvatar,
  canGovernProject,
  projectOwnerId,
  useInviteCandidates,
  useResolvedMembers,
  useSelfAccount,
  withOwner,
  type ResolvedMember,
} from "./pmMembers";

export function MemberManagerSection({ project }: { project: PmProject }) {
  const { t } = useTranslation();
  const addMember = usePmProjectStore((s) => s.addMember);
  const removeMember = usePmProjectStore((s) => s.removeMember);
  const transferOwner = usePmProjectStore((s) => s.transferOwner);
  const [busyId, setBusyId] = useState<string | null>(null);

  const self = useSelfAccount();
  // 候选全集与「新建项目」对话框同源（Agent 来自 agentStore，人类来自登录
  // 账号 + 账号目录；两个数据源的加载由 hook 自己拉起，不依赖当前成员非空）。
  const invite = useInviteCandidates();

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
  const canGovern = canGovernProject(project, self);
  const displayMembers = useMemo(() => withOwner(project), [project]);
  const memberIds = useMemo(
    () => new Set(displayMembers.map((m) => m.instance_id)),
    [displayMembers],
  );

  // 候选数据源（agentStore + 账号目录）的加载由 `useInviteCandidates` 自己
  // 负责，这里不再重复挂 effect。

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

  // 可添加候选 = 全集 − 已在册（owner 已在 displayMembers 里，所以也排除）。
  const agentCandidates = useMemo(
    () => invite.agents.filter((c) => !memberIds.has(c.instance_id)),
    [invite.agents, memberIds],
  );
  const userCandidates = useMemo(
    () => invite.humans.filter((c) => !memberIds.has(c.instance_id)),
    [invite.humans, memberIds],
  );

  // 已是成员：解析得出名字 → 显示；解析不出（Agent 已卸载）→ 占位
  const currentMembers = useResolvedMembers(displayMembers);

  // Agent / 人类候选行同形，只差 kind 决定头像画法与 add 的 kind —— 一份渲染
  const renderCandidate = (c: ResolvedMember) => (
    <li
      key={c.instance_id}
      className="flex items-center gap-2 rounded-md px-2 py-1.5 hover:bg-[var(--color-accent)]/10 dark:hover:bg-[var(--color-accent)]/15"
    >
      <MemberAvatar member={c} size={24} />
      <div className="min-w-0 flex-1">
        <div className="truncate text-xs text-text ">{c.displayName ?? c.instance_id}</div>
        <div className="truncate text-10 text-text-tertiary">{c.instance_id}</div>
      </div>
      <button
        type="button"
        disabled={busyId === c.instance_id}
        onClick={() => handleAdd(c.instance_id, c.kind)}
        className="shrink-0 rounded-md px-2 py-1 text-11 font-medium text-text-secondary hover:bg-[var(--color-accent)]/10 disabled:opacity-40  dark:hover:bg-[var(--color-accent)]/15"
      >
        {t("pm.addMember")}
      </button>
    </li>
  );

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
                    className="flex items-center gap-2 rounded-md px-2 py-1.5 hover:bg-[var(--color-accent)]/10 dark:hover:bg-[var(--color-accent)]/15"
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
                            className="rounded-md px-2 py-1 text-11 text-danger-text hover:bg-danger-surface disabled:opacity-40"
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
            {agentCandidates.length === 0 ? (
              <p className="mb-4 text-xs italic text-text-tertiary">{t("pm.noMembers")}</p>
            ) : (
              <ul className="mb-4 space-y-1">{agentCandidates.map(renderCandidate)}</ul>
            )}
  
            {/* 可添加的用户候选（仅多用户模式有目录数据） */}
            <div className="mb-1 text-11 font-medium text-text-tertiary ">
              {t("pm.userMembers")}
            </div>
            {userCandidates.length === 0 ? (
              <p className="text-xs italic text-text-tertiary">{t("pm.noMembers")}</p>
            ) : (
              <ul className="space-y-1">{userCandidates.map(renderCandidate)}</ul>
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
            className="rounded px-1.5 text-sm text-text-tertiary hover:bg-[var(--color-accent)]/10  dark:hover:bg-[var(--color-accent)]/15"
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
