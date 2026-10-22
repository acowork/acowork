/**
 * DocPermissionsCard — 文档库节点（目录/文档）的权限编辑卡片（设计 §9）。
 *
 * 作用对象 = 左侧树最后选中的节点（`treeStore.selectedNode`）。数据走
 * doc 服务 `GET/PUT /api/permissions/{kind}/{id}`（Gateway 注入可信
 * `x-actor` / `x-auth-role`）。
 *
 * 权限模型（与服务端 acl.rs 对齐）：
 * - 显式 ACL 三态：null=继承父目录链（全链无=公开）；[]=私有（仅创建者
 *   与 admin）；非空=指定用户名单（`edit` 隐含 `view`）。
 * - manage 门：仅节点创建者或 admin 可修改；其他人只读（服务端 403
 *   required=manage 兜底，前端用 GET 返回的 can_manage 预判）。
 *
 * 仅在 `multi_user` 模式挂载——local 模式无账号体系，服务端恒放行。
 */

import { useCallback, useEffect, useMemo, useState } from "react";
import { useTranslation } from "../../i18n/useTranslation";
import { cn } from "../../lib/utils";
import { getGatewayUrl } from "../../lib/config";
import { fetchAccounts, fetchDirectory } from "../../lib/auth-api";
import type { DirectoryUser } from "../../lib/types";
import { useAuthStore } from "../../stores/authStore";
import { useDocTreeStore } from "../../stores/doc/treeStore";
import * as docApi from "../../lib/doc-api";
import type { PermissionKind } from "../../lib/doc-api";
import type { Ace, AclRole, PermissionsView } from "../../lib/doc-types";
import { ErrorBox } from "../../components/common/ErrorBox";
import { showToast } from "../../components/common/ToastProvider";
import { listRowHover } from "../../lib/ui-styles";

/** ACL 编辑器的三态草稿（与服务端 acl.set 的 None / [] / 名单对应） */
type AclMode = "inherit" | "private" | "custom";

interface DocPermissionsCardProps {
  kind: PermissionKind;
  id: string;
  name: string;
}

export function DocPermissionsCard({ kind, id, name }: DocPermissionsCardProps) {
  const { t } = useTranslation();
  const self = useAuthStore((s) => s.account);
  const isAdmin = self?.role === "admin";

  const [perms, setPerms] = useState<PermissionsView | null>(null);
  const [users, setUsers] = useState<DirectoryUser[]>([]);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  // 草稿：模式 + principal→role 映射（Save 一次性提交，与 ADR-087
  // PermissionDialog 同一交互契约）。
  const [mode, setMode] = useState<AclMode>("inherit");
  const [roles, setRoles] = useState<Record<string, AclRole>>({});

  const load = useCallback(async () => {
    setLoading(true);
    setLoadError(null);
    try {
      const p = await docApi.getPermissions(kind, id);
      setPerms(p);
      setMode(p.explicit === null ? "inherit" : p.explicit.length === 0 ? "private" : "custom");
      setRoles(
        Object.fromEntries((p.explicit ?? []).map((ace) => [ace.principal, ace.role])),
      );
    } catch (e) {
      setPerms(null);
      setLoadError(e instanceof Error ? e.message : String(e));
      return;
    } finally {
      setLoading(false);
    }
    // 授权对象候选名单（best-effort，模式与 ADR-087 PermissionDialog 一致）：
    // admin 走全量 `/api/users`（含停用账号，名单里可能出现），普通用户走
    // `/api/users/directory` 联系人目录；目录按设计排除调用者自己，把
    // `me` 幂等补上，名单里的自己才不会显示成裸 UUID。
    const token = useAuthStore.getState().accessToken;
    if (!token) {
      setUsers([]);
      return;
    }
    try {
      const rows = isAdmin
        ? (await fetchAccounts(getGatewayUrl(), token)).map((a) => ({
            user_id: a.user_id,
            username: a.username,
            display_name: a.display_name,
            avatar: a.avatar ?? null,
            builtin_avatar: a.builtin_avatar ?? null,
          }))
        : await fetchDirectory(getGatewayUrl(), token);
      const me = useAuthStore.getState().account;
      if (me && !rows.some((u) => u.user_id === me.user_id)) {
        rows.push({
          user_id: me.user_id,
          username: me.username,
          display_name: me.display_name,
          avatar: me.avatar ?? null,
          builtin_avatar: me.builtin_avatar ?? null,
        });
      }
      setUsers(rows);
    } catch {
      setUsers([]);
    }
  }, [kind, id, isAdmin]);

  useEffect(() => {
    void load();
  }, [load]);

  const nameOf = useCallback(
    (userId: string): string => {
      if (self && self.user_id === userId) return self.display_name || self.username;
      const u = users.find((c) => c.user_id === userId);
      return u ? u.display_name || u.username : userId;
    },
    [self, users],
  );

  // 名单 = 目录用户 ∪ ACL 里已存在的 principal（孤儿保留显示 id，编辑
  // 中被删的用户不能静默消失，同 PermissionDialog 的 orphanGuests）。
  const listed = useMemo(() => {
    const ids = new Set<string>(users.map((u) => u.user_id));
    for (const p of Object.keys(roles)) ids.add(p);
    if (perms?.creator) ids.add(perms.creator);
    return Array.from(ids);
  }, [users, roles, perms?.creator]);

  const draftAces: Ace[] = useMemo(
    () =>
      mode === "custom"
        ? Object.entries(roles)
            .filter(([principal]) => principal.trim() !== "")
            .map(([principal, role]) => ({ principal, role }))
        : [],
    [mode, roles],
  );

  const dirty = useMemo(() => {
    if (!perms) return false;
    const wantMode: AclMode =
      perms.explicit === null ? "inherit" : perms.explicit.length === 0 ? "private" : "custom";
    if (wantMode !== mode) return true;
    if (mode !== "custom") return false;
    const base = perms.explicit ?? [];
    const draft = draftAces;
    if (base.length !== draft.length) return true;
    return base.some(
      (ace) => draft.find((d) => d.principal === ace.principal)?.role !== ace.role,
    );
  }, [perms, mode, draftAces]);

  const canManage = perms?.can_manage === true;

  const handleSave = useCallback(async () => {
    if (!perms || !dirty || saving) return;
    setSaving(true);
    setSaveError(null);
    try {
      const acl: Ace[] | null =
        mode === "inherit" ? null : mode === "private" ? [] : draftAces;
      await docApi.setPermissions(kind, id, acl);
      showToast({ type: "success", message: t("doc.permissions.saved") });
      // 树里的 creator/acl 徽章缓存要跟着新状态走
      void useDocTreeStore.getState().refreshVisible();
      await load();
    } catch (e) {
      // DocApiError.message 已是本地化拒绝文案（ADR-087 D5.3）
      setSaveError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  }, [perms, dirty, saving, mode, draftAces, kind, id, load, t]);

  const togglePrincipal = (userId: string) => {
    setRoles((prev) => {
      const next = { ...prev };
      if (next[userId]) delete next[userId];
      else next[userId] = "view";
      return next;
    });
  };

  const setRole = (userId: string, role: AclRole) => {
    setRoles((prev) => ({ ...prev, [userId]: role }));
  };

  // 生效态一句话摘要（继承模式下说明实际来源）
  let effectiveSummary: string;
  if (!perms) {
    effectiveSummary = "";
  } else if (perms.effective === null) {
    effectiveSummary = t("doc.permissions.effectivePublic");
  } else if (perms.explicit === null) {
    effectiveSummary = t("doc.permissions.effectiveFromParent", {
      count: perms.effective.length,
    });
  } else if (perms.effective.length === 0) {
    effectiveSummary = t("doc.permissions.privateEffective");
  } else {
    effectiveSummary = t("doc.permissions.effectiveNamed", {
      names: perms.effective.map((ace) => `${nameOf(ace.principal)}（${t(`doc.permissions.role_${ace.role}`)}）`).join("、"),
    });
  }

  return (
    <div className="text-xs">
      {loading && !perms && (
        <div className="flex items-center justify-center py-6">
          <div className="h-4 w-4 animate-spin rounded-full border-2 border-zinc-300 border-t-zinc-600 dark:border-zinc-600 dark:border-t-zinc-300" />
        </div>
      )}

      {loadError && (
        <ErrorBox message={`${t("doc.permissions.loadFailed")}: ${loadError}`} onClose={() => setLoadError(null)} />
      )}

      {perms && !loading && (
        <div className="space-y-3">
          {/* 节点 + 创建者 + 当前生效态 */}
          <div className="space-y-1">
            <div className="flex items-center justify-between gap-2">
              <span className="truncate font-medium text-text" title={name}>
                {name}
              </span>
              <span className="shrink-0 text-10 text-text-tertiary">
                {t(`doc.permissions.kind_${kind}`)}
              </span>
            </div>
            <div className="text-10 text-text-tertiary">
              {t("doc.permissions.creator")}: {nameOf(perms.creator ?? "") || "—"}
            </div>
            <div className="text-10 text-text-tertiary">{effectiveSummary}</div>
          </div>

          {/* 三态选择；不可管理时只呈现当前值 */}
          <div className="flex gap-1" role="radiogroup" aria-label={t("doc.permissions.title")}>
            {(["inherit", "private", "custom"] as const).map((m) => (
              <button
                key={m}
                type="button"
                role="radio"
                aria-checked={mode === m}
                disabled={!canManage}
                onClick={() => setMode(m)}
                className={cn(
                  "flex-1 rounded-md px-2 py-1 text-11 font-medium transition-colors",
                  mode === m
                    ? "bg-accent-strong text-white"
                    : "bg-zinc-100 text-text-secondary hover:bg-zinc-200 dark:bg-zinc-800 dark:hover:bg-zinc-700",
                  !canManage && "cursor-not-allowed opacity-60 hover:bg-zinc-100 dark:hover:bg-zinc-800",
                )}
              >
                {t(`doc.permissions.mode_${m}`)}
              </button>
            ))}
          </div>

          {mode === "custom" && (
            <div className="rounded-md border border-border-divider">
              {listed.length === 0 ? (
                <div className="px-3 py-2 text-text-tertiary">{t("doc.permissions.noUsers")}</div>
              ) : (
                <div className="max-h-44 overflow-y-auto px-2 py-1.5">
                  {listed.map((userId) => {
                    const granted = !!roles[userId];
                    const isSelf = self?.user_id === userId;
                    const isCreator = perms.creator === userId;
                    return (
                      <div
                        key={userId}
                        className={cn(
                          "flex items-center gap-2 rounded px-1.5 py-1",
                          canManage ? listRowHover : "opacity-60",
                        )}
                      >
                        <input
                          type="checkbox"
                          className="accent-[var(--color-accent)]"
                          checked={granted}
                          disabled={!canManage || isCreator}
                          onChange={() => togglePrincipal(userId)}
                          aria-label={nameOf(userId)}
                        />
                        <span className="min-w-0 truncate" title={nameOf(userId)}>
                          {nameOf(userId)}
                        </span>
                        {isSelf && (
                          <span className="shrink-0 text-10 text-text-tertiary">{t("common.me")}</span>
                        )}
                        {isCreator && (
                          <span className="shrink-0 text-10 text-text-tertiary">
                            {t("doc.permissions.creatorTag")}
                          </span>
                        )}
                        {granted && mode === "custom" && canManage && (
                          <span className="ml-auto flex shrink-0 gap-0.5">
                            {(["view", "edit"] as const).map((r) => (
                              <button
                                key={r}
                                type="button"
                                onClick={() => setRole(userId, r)}
                                className={cn(
                                  "rounded px-1.5 py-0.5 text-10 font-medium",
                                  roles[userId] === r
                                    ? "bg-accent-strong text-white"
                                    : "bg-zinc-100 text-text-secondary hover:bg-zinc-200 dark:bg-zinc-800 dark:hover:bg-zinc-700",
                                )}
                              >
                                {t(`doc.permissions.role_${r}`)}
                              </button>
                            ))}
                          </span>
                        )}
                      </div>
                    );
                  })}
                </div>
              )}
            </div>
          )}

          {!canManage ? (
            <div className="rounded-md border border-border-divider bg-nav-item-hover px-3 py-2 text-10 text-text-tertiary">
              {t("doc.permissions.readOnlyHint")}
            </div>
          ) : (
            <button
              type="button"
              disabled={!dirty || saving}
              onClick={() => void handleSave()}
              className="w-full rounded-md bg-accent-strong px-3 py-1.5 text-xs font-medium text-white transition-opacity hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-50"
            >
              {saving ? t("doc.permissions.saving") : t("doc.permissions.save")}
            </button>
          )}

          {saveError && (
            <ErrorBox message={`${t("doc.permissions.saveFailed")}: ${saveError}`} onClose={() => setSaveError(null)} />
          )}
        </div>
      )}
    </div>
  );
}
