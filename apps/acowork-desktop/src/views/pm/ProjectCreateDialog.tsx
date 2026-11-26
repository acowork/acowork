/**
 * ProjectCreateDialog — 新建项目对话框（一步到位）。
 *
 * 前身是 `ProjectSidebar` 里的一个内联标题输入框：建完项目还要去设置面板
 * 补图标、去成员面板邀人、再转让群主 —— 三次跳转、四份状态。这里一次收齐
 * 图标 / 描述 / 成员 / 群主，提交后由 `usePmProjectStore.createProject`
 * 编排那 1 + N 次调用（时序约束见该 action 的注释：转让必须最后做）。
 *
 * 布局取向：**紧凑、且全部用应用内已有的控件语法**。图标是
 * `ProjectIconPicker`（mode="draft"）—— 与项目设置面板里那个"预览即触发器
 * + 铅笔角标 + 弹层网格"是同一个组件，不另发明一套上传按钮；成员行与
 * `MemberManagerSection` 的候选行同形（`MemberAvatar` + 名字）；输入框用
 * `StyledInput` / `StyledTextarea`，群主用 `Dropdown`。间距一律走
 * `space-y-3` / 行内 `py-1`，整页不滚动就能看完一屏。
 *
 * 图标为什么是"创建后上传"：`POST /projects/:pid/icon` 按项目寻址，草稿期
 * 没有 pid。所以选中的文件先以 object URL 本地预览，项目落地后才真正上传并
 * 回写 `metadata.icon`；上传失败不影响项目本体，只降级成一条 warning toast。
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Check, X } from "lucide-react";
import { usePmProjectStore } from "../../stores/pm/projectStore";
import type { PmMemberKind } from "../../lib/pm-types";
import { Dropdown } from "../../components/common/Dropdown";
import { OverlayScrollbar } from "../../components/common/OverlayScrollbar";
import { StyledInput, StyledTextarea } from "../../components/common/StyledInput";
import { ProjectIconPicker } from "../../components/common/ProjectIconPicker";
import { showToast } from "../../components/common/ToastProvider";
import { useTranslation } from "../../i18n/useTranslation";
import { cn } from "../../lib/utils";
import {
  MemberAvatar,
  useInviteCandidates,
  useSelfAccount,
  type ResolvedMember,
} from "./pmMembers";

/** 警告键 → 文案 key（store 不做 i18n，调用方映射）。 */
const WARNING_KEYS = {
  icon: "pm.createWarnIcon",
  member: "pm.createWarnMember",
  owner: "pm.createWarnOwner",
} as const;

export function ProjectCreateDialog({ onClose }: { onClose: () => void }) {
  const { t } = useTranslation();
  const createProject = usePmProjectStore((s) => s.createProject);

  const self = useSelfAccount();
  const { agents: agentCandidates, humans: humanCandidates } = useInviteCandidates();

  const [title, setTitle] = useState("");
  const [description, setDescription] = useState("");
  const [iconFile, setIconFile] = useState<File | null>(null);
  const [iconPreviewUrl, setIconPreviewUrl] = useState<string | null>(null);
  const [selected, setSelected] = useState<Record<string, PmMemberKind>>({});
  // null = 还没选 → 落到创建者本人（后端 `Project::owner()` 的默认值）。
  const [ownerChoice, setOwnerChoice] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  const titleRef = useRef<HTMLInputElement>(null);

  // 打开即聚焦标题（原来是 sidebar 的 `creating` effect）
  useEffect(() => {
    titleRef.current?.focus();
  }, []);

  // 本地预览：object URL 生命周期跟着 File，换图 / 卸载都释放
  useEffect(() => {
    if (!iconFile) {
      setIconPreviewUrl(null);
      return;
    }
    const url = URL.createObjectURL(iconFile);
    setIconPreviewUrl(url);
    return () => URL.revokeObjectURL(url);
  }, [iconFile]);

  // ESC 关闭（保存中不打断，弹层里的 Dropdown 自己会 stopPropagation）
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !saving) onClose();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [onClose, saving]);

  // 创建者不进阶成员列表 —— 后端建群时已把 `created_by` 写成成员，再邀请
  // 必然 409；群主下拉则不过滤（默认值就是创建者）。
  const memberCandidates = useMemo(
    () => humanCandidates.filter((c) => c.instance_id !== self?.user_id),
    [humanCandidates, self?.user_id],
  );

  const ownerId = ownerChoice ?? self?.user_id ?? "";
  const ownerOptions = useMemo(
    () =>
      humanCandidates.map((c) => ({
        value: c.instance_id,
        label: c.displayName ?? c.instance_id,
      })),
    [humanCandidates],
  );

  const selectedIds = useMemo(() => new Set(Object.keys(selected)), [selected]);
  const selectedRows = useMemo(
    () => [...agentCandidates, ...humanCandidates].filter((c) => selectedIds.has(c.instance_id)),
    [agentCandidates, humanCandidates, selectedIds],
  );
  const toggleMember = useCallback((id: string, kind: PmMemberKind) => {
    setSelected((prev) => {
      const next = { ...prev };
      if (next[id]) delete next[id];
      else next[id] = kind;
      return next;
    });
  }, []);

  const handleCreate = useCallback(async () => {
    const trimmed = title.trim();
    if (!trimmed) {
      showToast({ type: "warning", message: t("pm.newProjectTitleRequired") });
      titleRef.current?.focus();
      return;
    }
    setSaving(true);
    const outcome = await createProject({
      title: trimmed,
      description: description.trim(),
      iconFile,
      members: Object.entries(selected).map(([instance_id, kind]) => ({
        instance_id,
        kind,
      })),
      // 没选 / 选的就是自己 = 不转让（后端 owner 回退 created_by）
      ownerId: ownerId && ownerId !== self?.user_id ? ownerId : null,
    });
    setSaving(false);
    if (!outcome) {
      showToast({ type: "error", message: t("pm.projectCreateFailed") });
      return;
    }
    onClose();
    showToast({ type: "success", message: t("pm.projectCreated") });
    for (const w of outcome.warnings) {
      showToast({ type: "warning", message: t(WARNING_KEYS[w]) });
    }
  }, [
    title,
    description,
    iconFile,
    selected,
    ownerId,
    self?.user_id,
    createProject,
    onClose,
    t,
  ]);

  // 候选行：与 MemberManagerSection 的候选行同形，只是点击是"勾选"而不是
  // "立即邀请"（项目还不存在）。
  const renderCandidateRow = (c: ResolvedMember) => {
    const on = selectedIds.has(c.instance_id);
    return (
      <li key={c.instance_id}>
        <button
          type="button"
          onClick={() => toggleMember(c.instance_id, c.kind)}
          aria-pressed={on}
          className={cn(
            "flex w-full items-center gap-2 rounded px-1.5 py-1 text-left transition-colors",
            "hover:bg-[var(--color-accent)]/10 dark:hover:bg-[var(--color-accent)]/15",
            on && "bg-accent-strong/10",
          )}
        >
          <MemberAvatar member={c} size={20} />
          <span className="min-w-0 flex-1 truncate text-11 text-text">
            {c.displayName ?? c.instance_id}
          </span>
          {on && <Check size={12} className="shrink-0 text-[var(--color-accent)]" />}
        </button>
      </li>
    );
  };

  const labelCls = "mb-1 block text-11 font-medium text-text-secondary";

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      <div
        className="absolute inset-0 bg-modal-overlay"
        onClick={() => !saving && onClose()}
      />
      <div
        className="relative z-10 flex max-h-[85vh] w-full max-w-md flex-col overflow-hidden rounded-md border border-border-outer bg-modal-surface shadow-xl"
        role="dialog"
        aria-modal="true"
        aria-labelledby="pm-new-project-title"
      >
        <header className="flex min-h-[var(--ui-dialog-zone-h)] shrink-0 items-center justify-between border-b border-border-divider px-4 py-2">
          <h3
            id="pm-new-project-title"
            className="text-sm font-semibold text-text"
          >
            {t("pm.newProject")}
          </h3>
          <button
            type="button"
            onClick={onClose}
            disabled={saving}
            aria-label={t("common.close")}
            className="rounded p-1 text-text-tertiary hover:bg-[var(--color-accent)]/10 disabled:opacity-40 dark:hover:bg-[var(--color-accent)]/15"
          >
            <X size={14} />
          </button>
        </header>

        <OverlayScrollbar fill="flex" className="px-4 py-3">
          <div className="space-y-3">
            {/* ── 图标 + 名称（同一行：图标即头像，与项目设置面板同形）── */}
            <div className="flex items-center gap-3">
              <ProjectIconPicker
                mode="draft"
                draft={{ previewUrl: iconPreviewUrl, onPickFile: setIconFile }}
                size={48}
                disabled={saving}
              />
              <div className="min-w-0 flex-1">
                <label htmlFor="pm-new-project-name" className={labelCls}>
                  {t("pm.projectName")}
                </label>
                <StyledInput
                  ref={titleRef}
                  id="pm-new-project-name"
                  value={title}
                  onChange={(e) => setTitle(e.target.value)}
                  onKeyDown={(e) => {
                    // Enter 只在标题框里触发创建；描述框要能换行
                    if (e.key === "Enter") void handleCreate();
                  }}
                  placeholder={t("pm.newProjectPlaceholder")}
                  disabled={saving}
                />
              </div>
            </div>

            {/* ── 描述 ───────────────────────────────────────────── */}
            <div>
              <label htmlFor="pm-new-project-desc" className={labelCls}>
                {t("pm.projectRemark")}
              </label>
              <StyledTextarea
                id="pm-new-project-desc"
                rows={2}
                value={description}
                onChange={(e) => setDescription(e.target.value)}
                placeholder={t("pm.newProjectDescPlaceholder")}
                disabled={saving}
              />
            </div>

            {/* ── 成员 ───────────────────────────────────────────── */}
            <div>
              <div className={labelCls}>
                {t("pm.members")}
                {selectedRows.length > 0 ? ` · ${selectedRows.length}` : ""}
              </div>
              {selectedRows.length > 0 && (
                <div className="mb-1.5 flex flex-wrap gap-1">
                  {selectedRows.map((c) => (
                    <button
                      key={c.instance_id}
                      type="button"
                      onClick={() => toggleMember(c.instance_id, c.kind)}
                      className="flex items-center gap-1 rounded-full border border-border-outer py-0.5 pl-0.5 pr-1.5 text-10 text-text-secondary hover:border-border-strong"
                      title={t("pm.removeMember")}
                    >
                      <MemberAvatar member={c} size={16} />
                      {c.displayName ?? c.instance_id}
                      <X size={10} className="text-text-tertiary" />
                    </button>
                  ))}
                </div>
              )}
              {agentCandidates.length === 0 && memberCandidates.length === 0 ? (
                <p className="text-11 italic text-text-tertiary">{t("pm.noMembers")}</p>
              ) : (
                // 候选可能十几条，一屏放不下就把这一块自己收进有界滚动区
                // （`OverlayScrollbar` 是应用内唯一允许的滚动容器写法）。
                <OverlayScrollbar
                  maxHeight="10rem"
                  className="space-y-1 p-1"
                  containerClassName="rounded-md border border-border-outer"
                >
                  {agentCandidates.length > 0 && (
                    <>
                      <div className="text-10 text-text-tertiary">{t("pm.agentMembers")}</div>
                      <ul className="space-y-0.5">{agentCandidates.map(renderCandidateRow)}</ul>
                    </>
                  )}
                  {memberCandidates.length > 0 && (
                    <>
                      <div className="text-10 text-text-tertiary">{t("pm.userMembers")}</div>
                      <ul className="space-y-0.5">{memberCandidates.map(renderCandidateRow)}</ul>
                    </>
                  )}
                </OverlayScrollbar>
              )}
            </div>

            {/* ── 群主 ───────────────────────────────────────────── */}
            {/* 只有解析出人类账号才谈得上转让；local 模式下 owner 恒为创建者。 */}
            {ownerOptions.length > 0 && (
              <div>
                <label htmlFor="pm-new-project-owner" className={labelCls}>
                  {t("pm.owner")}
                </label>
                <Dropdown
                  id="pm-new-project-owner"
                  value={ownerId}
                  onChange={setOwnerChoice}
                  disabled={saving}
                  options={ownerOptions}
                />
                <p className="mt-1 text-10 text-text-tertiary">
                  {t("pm.newProjectOwnerHint")}
                </p>
              </div>
            )}
          </div>
        </OverlayScrollbar>

        <footer className="flex shrink-0 items-center justify-end gap-2 border-t border-border-divider px-4 py-2.5">
          <button
            type="button"
            onClick={onClose}
            disabled={saving}
            className="rounded-md px-3 py-1.5 text-xs font-medium text-text-secondary hover:bg-[var(--color-accent)]/10 disabled:opacity-50 dark:hover:bg-[var(--color-accent)]/15"
          >
            {t("common.cancel")}
          </button>
          <button
            type="button"
            onClick={() => void handleCreate()}
            disabled={saving}
            className="rounded-md bg-btn-solid px-3 py-1.5 text-xs font-medium text-white hover:bg-btn-solid-hover hover:ring-2 hover:ring-inset hover:ring-[var(--color-accent)]/70 disabled:opacity-50"
          >
            {saving ? t("common.saving") : t("common.create")}
          </button>
        </footer>
      </div>
    </div>
  );
}
