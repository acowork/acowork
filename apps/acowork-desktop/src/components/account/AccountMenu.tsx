/**
 * Top-bar account menu (ADR-076 §决策 7).
 *
 * Reuses the shared `ContextMenu` popover (the same one the right-click
 * menus use) instead of hand-rolling a dropdown. Left click on the avatar
 * opens user preferences (the historical behaviour, same as the `!loggedIn`
 * path below); right click opens the account menu — WeChat-style.
 *
 * 切换账号 / 退出登录 / 注销 all funnel into a store action that clears
 * the token pair and reloads the webview — the §9 open-question-6
 * "logged out but not logged in" mid-state, resolved by landing on
 * LoginView after a clean reboot of every store + MQTT listener.
 */

import { useState } from "react";
import { KeyRound, LogOut, RefreshCw, SlidersHorizontal, UserX } from "lucide-react";
import { useAuthStore } from "../../stores/authStore";
import { useUserProfileStore } from "../../stores/userProfileStore";
import { useTranslation } from "../../i18n/useTranslation";
import { ContextMenu } from "../common/ContextMenu/ContextMenu";
import { useContextMenu } from "../common/ContextMenu/useContextMenu";
import { ConfirmDialog } from "../common/ConfirmDialog";
import { Tooltip } from "../common/Tooltip";
import { UserAvatar } from "../common/UserAvatar";
import { ChangePasswordModal } from "./ChangePasswordModal";

interface AccountMenuProps {
  /** Kept for the non-multi_user path: open user preferences (old behaviour). */
  onOpenProfile: () => void;
}

export function AccountMenu({ onOpenProfile }: AccountMenuProps) {
  const { t } = useTranslation();
  const profile = useUserProfileStore((s) => s.profile);
  const status = useAuthStore((s) => s.status);
  const account = useAuthStore((s) => s.account);

  const menu = useContextMenu<undefined>();
  const [changePasswordOpen, setChangePasswordOpen] = useState(false);
  const [deleteConfirmOpen, setDeleteConfirmOpen] = useState(false);

  const loggedIn = status === "logged_in";

  const avatar = (
    <UserAvatar
      displayName={profile.displayName}
      avatarUrl={profile.backendAvatarUrl ?? null}
      builtinAvatarId={profile.backendBuiltinAvatarId ?? undefined}
      size={40}
      className="shrink-0"
    />
  );

  // Local mode / not logged in: preserve the historical "click avatar →
  // profile settings" entry unchanged.
  if (!loggedIn) {
    return (
      <Tooltip content={t("navBar.editProfile")} variant="plain" position="right">
        <button
          onClick={onOpenProfile}
          className="mb-3 flex items-center justify-center rounded-md transition-colors duration-150 hover:ring-2 hover:ring-zinc-400 dark:hover:ring-zinc-500"
          aria-label={t("navBar.editProfile")}
        >
          {avatar}
        </button>
      </Tooltip>
    );
  }

  return (
    <>
      <Tooltip content={account?.display_name ?? profile.displayName} variant="plain" position="right">
        <button
          // 左键 = 打开用户资料（原有点击行为），右键 = 账号菜单，
          // 与微信一致。`openAt` 内部已 preventDefault 掉浏览器原生菜单。
          onClick={onOpenProfile}
          onContextMenu={(e) => menu.openAt(e)}
          className="mb-3 flex items-center justify-center rounded-md transition-colors duration-150 hover:ring-2 hover:ring-zinc-400 dark:hover:ring-zinc-500"
          aria-label={t("account.menuAriaLabel")}
        >
          {avatar}
        </button>
      </Tooltip>

      <ContextMenu
        isOpen={menu.isOpen}
        menuProps={menu.menuProps}
        payload={menu.payload}
        selectionAtOpen={menu.selectionAtOpen}
        onClose={menu.close}
        items={[
          {
            key: "switch",
            icon: <RefreshCw size={14} />,
            label: t("account.switchAccount"),
            onClick: () => void useAuthStore.getState().switchAccount(),
          },
          {
            key: "change-password",
            icon: <KeyRound size={14} />,
            label: t("account.changePassword"),
            onClick: () => setChangePasswordOpen(true),
          },
          {
            key: "delete-self",
            icon: <UserX size={14} />,
            label: t("account.deleteSelf"),
            variant: "danger",
            onClick: () => setDeleteConfirmOpen(true),
          },
          {
            key: "profile",
            icon: <SlidersHorizontal size={14} />,
            label: t("account.preferences"),
            dividerBefore: true,
            onClick: () => onOpenProfile(),
          },
          {
            key: "logout",
            icon: <LogOut size={14} />,
            label: t("account.logout"),
            onClick: () => void useAuthStore.getState().logout(),
          },
        ]}
      />

      <ChangePasswordModal
        open={changePasswordOpen}
        onClose={() => setChangePasswordOpen(false)}
      />

      <ConfirmDialog
        open={deleteConfirmOpen}
        title={t("account.deleteSelfTitle")}
        message={t("account.deleteSelfConfirm")}
        confirmLabel={t("account.deleteSelf")}
        destructive
        onConfirm={() => {
          setDeleteConfirmOpen(false);
          void useAuthStore.getState().deleteSelf();
        }}
        onCancel={() => setDeleteConfirmOpen(false)}
      />
    </>
  );
}
