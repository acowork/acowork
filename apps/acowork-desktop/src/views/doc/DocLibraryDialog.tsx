/**
 * DocLibraryDialog — 新建 / 编辑文档库。
 *
 * 视觉与交互对齐 `ProjectCreateDialog`（同一套 dialog 外壳、同一套
 * `StyledInput` / `StyledTextarea` / `Dropdown` / `MemberAvatar`），因为
 * 用户对「文档库」的心智就是「像项目一样的容器」：图标、名称、描述、
 * 成员、所有者一次收齐。
 *
 * 两处刻意与项目对话框不同，都是数据模型的事实，不是遗漏：
 * - **成员只在新建时收集**。文档库的成员就是节点 ACL，编辑期已有
 *   `DocPermissionsCard`（继承 / 私有 / 自定义三态）；在对话框里再做一份
 *   简化版会变成第二套语义、两个写法。编辑态给一行指引即可。
 * - **图标是内置图标 key**，不是上传图片：文档库图标没有字节端点，
 *   `LibraryIconPicker` 的网格就是完整值域。
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Check, X } from "lucide-react";
import { Dropdown } from "../../components/common/Dropdown";
import { OverlayScrollbar } from "../../components/common/OverlayScrollbar";
import { StyledInput, StyledTextarea } from "../../components/common/StyledInput";
import { showToast } from "../../components/common/ToastProvider";
import { useTranslation } from "../../i18n/useTranslation";
import { useDocTreeStore } from "../../stores/doc/treeStore";
import type { LibraryMeta } from "../../lib/doc-types";
import { cn } from "../../lib/utils";
import {
  MemberAvatar,
  useInviteCandidates,
  useSelfAccount,
} from "../pm/pmMembers";
import { LibraryIconPicker } from "./LibraryIconPicker";

type Props =
  | { mode: "create"; onClose: () => void }
  | { mode: "edit"; library: LibraryMeta; onClose: () => void };

export function DocLibraryDialog(props: Props) {
  const { t } = useTranslation();
  const { onClose } = props;
  const editing = props.mode === "edit" ? props.library : null;
  const createLibrary = useDocTreeStore((s) => s.createLibrary);
  const updateLibrary = useDocTreeStore((s) => s.updateLibrary);

  const self = useSelfAccount();
  const { humans } = useInviteCandidates();

  const [name, setName] = useState(editing?.name ?? "");
  const [description, setDescription] = useState(editing?.description ?? "");
  const [icon, setIcon] = useState<string | null>(editing?.icon ?? null);
  const [members, setMembers] = useState<Record<string, true>>({});
  const [ownerChoice, setOwnerChoice] = useState<string | null>(
    editing?.creator ?? null,
  );
  const [saving, setSaving] = useState(false);
  const nameRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    nameRef.current?.focus();
  }, []);

  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !saving) onClose();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [onClose, saving]);

  // 新建时创建者本人不进成员候选（建库已把 creator 写成成员语义上的
  // 所有者）；编辑态不收集成员。
  const memberCandidates = useMemo(
    () => humans.filter((c) => c.instance_id !== self?.user_id),
    [humans, self?.user_id],
  );

  const selectedMembers = useMemo(
    () => memberCandidates.filter((c) => members[c.instance_id]),
    [memberCandidates, members],
  );

  const ownerId = ownerChoice ?? self?.user_id ?? "";
  const ownerOptions = useMemo(
    () =>
      humans.map((c) => ({
        value: c.instance_id,
        label: c.displayName ?? c.instance_id,
      })),
    [humans],
  );

  const handleSave = useCallback(async () => {
    const trimmed = name.trim();
    if (!trimmed) {
      showToast({ type: "warning", message: t("doc.library.nameRequired") });
      nameRef.current?.focus();
      return;
    }
    setSaving(true);
    if (editing) {
      const ok = await updateLibrary(editing.dir_id, {
        name: trimmed === editing.name ? undefined : trimmed,
        // 只有真的变了才发，避免无谓的写入；清空发显式 null。
        icon: icon === (editing.icon ?? null) ? undefined : icon,
        description:
          description.trim() === (editing.description ?? "")
            ? undefined
            : description.trim(),
        owner:
          ownerId && ownerId !== (editing.creator ?? undefined)
            ? ownerId
            : undefined,
      });
      setSaving(false);
      if (!ok) {
        showToast({ type: "error", message: t("doc.library.saveFailed") });
        return;
      }
      onClose();
      showToast({ type: "success", message: t("doc.library.saved") });
      return;
    }

    const created = await createLibrary({
      name: trimmed,
      icon,
      description: description.trim(),
      members: selectedMembers.map((c) => c.instance_id),
      // 没选 / 选的就是自己 = 不转让（后端回落创建者）。
      owner: ownerId && ownerId !== self?.user_id ? ownerId : null,
    });
    setSaving(false);
    if (!created) {
      showToast({ type: "error", message: t("doc.library.saveFailed") });
      return;
    }
    onClose();
    showToast({ type: "success", message: t("doc.library.created") });
  }, [
    name,
    description,
    icon,
    editing,
    updateLibrary,
    createLibrary,
    selectedMembers,
    ownerId,
    self?.user_id,
    onClose,
    t,
  ]);

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
        aria-labelledby="doc-library-dialog-title"
      >
        <header className="flex min-h-[var(--ui-dialog-zone-h)] shrink-0 items-center justify-between border-b border-border-divider px-4 py-2">
          <h3 id="doc-library-dialog-title" className="text-sm font-semibold text-text">
            {editing ? t("doc.library.editTitle") : t("doc.library.createTitle")}
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
            {/* 图标 + 名称同一行 */}
            <div className="flex items-center gap-3">
              <LibraryIconPicker
                value={icon}
                onChange={setIcon}
                size={48}
                disabled={saving}
              />
              <div className="min-w-0 flex-1">
                <label htmlFor="doc-library-name" className={labelCls}>
                  {t("doc.library.name")}
                </label>
                <StyledInput
                  ref={nameRef}
                  id="doc-library-name"
                  value={name}
                  onChange={(e) => setName(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") void handleSave();
                  }}
                  placeholder={t("doc.library.namePlaceholder")}
                  disabled={saving}
                />
              </div>
            </div>

            <div>
              <label htmlFor="doc-library-desc" className={labelCls}>
                {t("doc.library.description")}
              </label>
              <StyledTextarea
                id="doc-library-desc"
                rows={2}
                value={description}
                onChange={(e) => setDescription(e.target.value)}
                placeholder={t("doc.library.descriptionPlaceholder")}
                disabled={saving}
              />
            </div>

            {editing ? (
              <p className="text-11 text-text-tertiary">
                {t("doc.library.membersEditHint")}
              </p>
            ) : (
              <div>
                <div className={labelCls}>
                  {t("doc.library.members")}
                  {selectedMembers.length > 0 ? ` · ${selectedMembers.length}` : ""}
                </div>
                {selectedMembers.length > 0 && (
                  <div className="mb-1.5 flex flex-wrap gap-1">
                    {selectedMembers.map((c) => (
                      <button
                        key={c.instance_id}
                        type="button"
                        onClick={() =>
                          setMembers((prev) => {
                            const next = { ...prev };
                            delete next[c.instance_id];
                            return next;
                          })
                        }
                        className="flex items-center gap-1 rounded-full border border-border-outer py-0.5 pl-0.5 pr-1.5 text-10 text-text-secondary"
                      >
                        <MemberAvatar member={c} size={16} />
                        <span className="max-w-24 truncate">
                          {c.displayName ?? c.instance_id}
                        </span>
                        <X size={10} className="text-text-tertiary" />
                      </button>
                    ))}
                  </div>
                )}
                {memberCandidates.length === 0 ? (
                  <p className="text-11 italic text-text-tertiary">
                    {t("doc.library.noMembers")}
                  </p>
                ) : (
                  <OverlayScrollbar
                    maxHeight="10rem"
                    className="space-y-1 p-1"
                    containerClassName="rounded-md border border-border-outer"
                  >
                    <ul className="space-y-0.5">
                      {memberCandidates.map((c) => {
                        const on = !!members[c.instance_id];
                        return (
                          <li key={c.instance_id}>
                            <button
                              type="button"
                              onClick={() =>
                                setMembers((prev) => {
                                  const next = { ...prev };
                                  if (next[c.instance_id]) delete next[c.instance_id];
                                  else next[c.instance_id] = true;
                                  return next;
                                })
                              }
                              className={cn(
                                "flex w-full items-center gap-2 rounded px-2 py-1 text-left text-xs",
                                on
                                  ? "bg-accent/10 text-text"
                                  : "text-text-secondary hover:bg-surface-hover",
                              )}
                              aria-pressed={on}
                            >
                              <MemberAvatar member={c} size={20} />
                              <span className="min-w-0 flex-1 truncate">
                                {c.displayName ?? c.instance_id}
                              </span>
                              {on && (
                                <Check
                                  size={12}
                                  className="shrink-0 text-[var(--color-accent)]"
                                />
                              )}
                            </button>
                          </li>
                        );
                      })}
                    </ul>
                  </OverlayScrollbar>
                )}
              </div>
            )}

            {ownerOptions.length > 0 && (
              <div>
                <label htmlFor="doc-library-owner" className={labelCls}>
                  {t("doc.library.owner")}
                </label>
                <Dropdown
                  id="doc-library-owner"
                  value={ownerId}
                  onChange={setOwnerChoice}
                  disabled={saving}
                  options={ownerOptions}
                />
                <p className="mt-1 text-10 text-text-tertiary">
                  {t("doc.library.ownerHint")}
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
            onClick={() => void handleSave()}
            disabled={saving}
            className="rounded-md bg-btn-solid px-3 py-1.5 text-xs font-medium text-white hover:bg-btn-solid-hover hover:ring-2 hover:ring-inset hover:ring-[var(--color-accent)]/70 disabled:opacity-50"
          >
            {saving
              ? t("common.saving")
              : editing
                ? t("doc.save")
                : t("common.create")}
          </button>
        </footer>
      </div>
    </div>
  );
}