/**
 * Sidebar "Users" folding group (ADR-076 §决策 5 / 6 / 7).
 *
 * Rendered below the agent list in [AgentList.tsx](../agent-list/AgentList.tsx).
 * Only appears under `AUTH_MODE=multi_user` with a live session — under
 * `local` the whole account system is a no-op (§决策 12), so the group is
 * not shown.
 *
 * Selection semantics:
 * - Single click (any role): opens a 1:1 inbox thread with that peer via
 *   `useUserChatStore.openChat`. The middle panel renders
 *   [InboxPanel](../../views/InboxPanel.tsx) once `activePeerId` is set.
 * - Admin right-click: opens the management menu (reset password / role /
 *   disable / view-as-user). "View as user" is the new home for the
 *   `viewAsUserId` toggle that previously lived on single-click; moving it
 *   keeps the inbox row click a uniform "open conversation" gesture.
 *
 * Row visuals mirror `AgentList`'s agent row (h-14, avatar 40, two-line
 * `display_name / last_message_preview`, accent active state) so the two
 * groups feel like one list.
 */

import { useCallback, useEffect, useState, type MouseEvent, forwardRef, useImperativeHandle } from "react";
import { listen } from "@tauri-apps/api/event";
import { ChevronRight, Eye, KeyRound, MessageSquare, ShieldCheck, ShieldOff, UserX, X } from "lucide-react";
import { useAuthStore } from "../../stores/authStore";
import { useUserChatStore } from "../../stores/userChatStore";
import { useAgentStore } from "../../stores/agentStore";
import { useChatStore } from "../../stores/chatStore";
import { useTranslation } from "../../i18n/useTranslation";
import { getGatewayUrl } from "../../lib/config";
import { log } from "../../lib/logger";
import {
  AuthApiError,
  disableAccount,
  fetchAccounts,
  fetchDirectory,
  resetPassword,
  setRole,
  type CreateAccountResult,
} from "../../lib/auth-api";
import type { DirectoryUser, UserAccount } from "../../lib/types";
import { cn } from "../../lib/utils";
import { listRowHover } from "../../lib/ui-styles";
import { UserAvatar } from "../common/UserAvatar";
import { ConfirmDialog } from "../common/ConfirmDialog";
import {
  ContextMenu,
  useContextMenu,
  type ContextMenuItem,
} from "../common/ContextMenu";
import { CreateAccountModal } from "../account/CreateAccountModal";
import { InviteTokenModal } from "../account/InviteTokenModal";
import { partitionAccounts } from "./partitionAccounts";

export type UserListHandle = {
  /** Open the Create-Account modal (invoked from the agent-list sidebar "+"). */
  openCreate: () => void;
};

export const UserList = forwardRef<UserListHandle>(function UserList(_props, ref) {
  const { t } = useTranslation();
  const mode = useAuthStore((s) => s.mode);
  const status = useAuthStore((s) => s.status);
  const self = useAuthStore((s) => s.account);
  const accessToken = useAuthStore((s) => s.accessToken);
  const viewAsUserId = useAuthStore((s) => s.viewAsUserId);
  const registrationOpen = useAuthStore((s) => s.registrationOpen);

  // The agent-list sidebar "+" owns the single create-menu entry point.
  // It calls into here via the imperative ref opened at the bottom of the
  // file (`UserListHandle.openCreate`) so the modal + invite flow stays
  // colocated with the rest of the user state.
  // ADR-076 §决策 7 banner. Default = expanded, same as the agent
  // node groups in AgentList (empty set = nothing collapsed).
  const [collapsed, setCollapsed] = useState(false);
  const [accounts, setAccounts] = useState<UserAccount[]>([]);
  const [loadFailed, setLoadFailed] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);
  const [createOpen, setCreateOpen] = useState(false);
  const [invite, setInvite] = useState<{ token: string; username?: string } | null>(null);
  const [disableTarget, setDisableTarget] = useState<UserAccount | null>(null);

  const menu = useContextMenu<UserAccount>();

  const loggedIn = mode === "multi_user" && status === "logged_in";
  const isAdmin = self?.role === "admin";
  // §决策 6: a non-admin may create accounts only while self-registration is
  // open (the Gateway hardcodes the created role to `user`). Admins always may.
  const canInvite = isAdmin || registrationOpen;

  // Sidebar "+" → "Create account" → forward into this modal. Same
  // `canInvite` gate as the (removed) banner button so non-admins with
  // closed registration can't bypass it via the agent-list menu.
  useImperativeHandle(
    ref,
    () => ({
      openCreate: () => {
        if (canInvite) setCreateOpen(true);
      },
    }),
    [canInvite],
  );

  const refetch = useCallback(async () => {
    if (!isAdmin) {
      // Non-admin: the full `/api/users` list is admin-only, but the
      // `/api/users/directory` endpoint exposes just enough (id / username /
      // display_name / avatar pair) for the contact picker so they can
      // start a 1:1 chat (ADR-076 §决策 8). Without this a non-admin's
      // sidebar shows only themselves and the inbox is unreachable.
      //
      // ponytail: directory entries are projected into the `UserAccount`
      // shape with `role: 'user'` and empty profile fields so the row +
      // `partitionAccounts` rendering path is shared with admin. The lie is
      // harmless — non-admin viewers never show the admin badge (gated by
      // `isAdmin` in the row), and admin viewers keep using `fetchAccounts`
      // for the real role. The avatar fields ARE carried through, so a
      // non-admin's sidebar draws the same faces the inbox does.
      const token = useAuthStore.getState().accessToken ?? "";
      try {
        const dir = await fetchDirectory(getGatewayUrl(), token);
        setAccounts(dir.map(directoryRow));
        setLoadFailed(false);
      } catch {
        setLoadFailed(true);
      }
      return;
    }
    const token = useAuthStore.getState().accessToken;
    try {
      setAccounts(await fetchAccounts(getGatewayUrl(), token ?? ""));
      setLoadFailed(false);
    } catch {
      setLoadFailed(true);
    }
  }, [isAdmin]);

  useEffect(() => {
    if (!loggedIn) return;
    void refetch();
  }, [loggedIn, self, accessToken, refetch]);

  // ── Realtime account refresh (ADR-084 §决策 4b) ──
  //
  // The user service publishes `acowork/user/profiles/changed` after
  // *every* account/profile mutation, avatar writes included; the Tauri
  // side re-emits it as `user-profiles-changed` (see
  // `src-tauri/src/commands/chat_mqtt.rs`). Without this, a user who
  // changed their avatar kept seeing the old one in the sidebar until a
  // remount — the row data is a one-shot fetch with no other trigger.
  //
  // The signal is non-retained, so anything that changed while the
  // Desktop was disconnected never reaches us; the MQTT connect edge
  // bumps `inventoryVersion` for exactly this catch-up on the agent
  // sidebar, and we read the same counter here. The initial `0` is
  // skipped because the mount fetch above already ran.
  const inventoryVersion = useChatStore((s) => s.inventoryVersion);
  useEffect(() => {
    if (!loggedIn || inventoryVersion === 0) return;
    void refetch();
  }, [loggedIn, inventoryVersion, refetch]);

  useEffect(() => {
    if (!loggedIn) return;
    let disposed = false;
    let unlisten: (() => void) | null = null;
    void listen("user-profiles-changed", () => {
      if (disposed) return;
      void refetch();
    })
      .then((fn) => {
        // The component can unmount before `listen` resolves; releasing
        // the subscription then is the whole reason for this dance.
        if (disposed) fn();
        else unlisten = fn;
      })
      .catch((err) => {
        log.warn("[UserList] failed to listen for user-profiles-changed:", err);
      });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [loggedIn, refetch]);

  if (!loggedIn) return null;

  const rows = partitionAccounts(accounts)[0]?.accounts ?? [];
  const viewingName =
    viewAsUserId != null
      ? rows.find((a) => a.user_id === viewAsUserId)?.display_name ?? viewAsUserId
      : null;

  // Single-click gesture: open / focus the inbox thread with this peer.
  const openThread = (account: UserAccount) => {
    // Clear the active agent selection: AppLayout renders <ChatPanel /> when
    // `selectedAgentId` is set and falls through to <InboxPanel /> only when
    // it is null. Without this clear, opening a 1:1 thread while an agent
    // is selected would set `activePeerId` underneath the unchanged ChatPanel
    // — the user sees "nothing happens" even though the store did the work.
    useAgentStore.getState().selectAgent(null);
    void useUserChatStore.getState().openChat(account.user_id, account.display_name);
  };

  // Admin-only: toggle the "view sessions as this user" lens.
  const toggleViewAs = (userId: string) => {
    const next = userId === viewAsUserId || userId === self?.user_id ? null : userId;
    useAuthStore.getState().setViewAsUserId(next);
  };

  const run = async (fn: () => Promise<unknown>) => {
    try {
      await fn();
    } catch (err) {
      setActionError(err instanceof AuthApiError ? err.message : String(err));
    }
  };

  const menuItems = (account: UserAccount): ContextMenuItem<UserAccount>[] => {
    const isSelf = account.user_id === self?.user_id;
    const items: ContextMenuItem<UserAccount>[] = [];
    if (isAdmin && !isSelf) {
      items.push({
        key: "view-as",
        icon: <Eye size={14} />,
        label: t("userList.viewAsUser"),
        onClick: () => toggleViewAs(account.user_id),
      });
    }
    if (!isSelf) {
      items.push({
        key: "message",
        icon: <MessageSquare size={14} />,
        label: t("messages.openChat"),
        onClick: () => openThread(account),
      });
    }
    // Non-admin viewers only get the "message" action above. The four
    // account-management items below are admin-only endpoints and would 403
    // for a non-admin caller (backend `require_admin` guards).
    if (!isAdmin) return items;
    items.push(
      {
        key: "reset-password",
        icon: <KeyRound size={14} />,
        label: t("account.resetPassword"),
        onClick: () =>
          run(async () => {
            const token = useAuthStore.getState().accessToken ?? "";
            const inviteToken = await resetPassword(getGatewayUrl(), token, account.user_id);
            setInvite({ token: inviteToken, username: account.display_name });
          }),
      },
      account.role === "user"
        ? {
            key: "promote",
            icon: <ShieldCheck size={14} />,
            label: t("account.makeAdmin"),
            onClick: () =>
              run(async () => {
                const token = useAuthStore.getState().accessToken ?? "";
                await setRole(getGatewayUrl(), token, account.user_id, "admin");
              }),
          }
        : {
            key: "demote",
            icon: <ShieldOff size={14} />,
            label: t("account.makeUser"),
            // Demoting yourself can strand the deployment without an admin.
            disabled: isSelf,
            onClick: () =>
              run(async () => {
                const token = useAuthStore.getState().accessToken ?? "";
                await setRole(getGatewayUrl(), token, account.user_id, "user");
              }),
          },
    );
    if (!isSelf) {
      items.push({
        key: "disable",
        icon: <UserX size={14} />,
        label: t("account.disableAccount"),
        variant: "danger",
        dividerBefore: true,
        onClick: () => setDisableTarget(account),
      });
    }
    return items;
  };

  return (
    <div data-testid="user-list">
      <div className="flex h-6 items-center border-y border-nav-divider/40 bg-panel-block dark:border-zinc-600/40">
        <button
          type="button"
          onClick={() => setCollapsed((c) => !c)}
          aria-expanded={!collapsed}
          aria-label={t("userList.title")}
          data-testid="user-group-header"
          className={cn(
            "flex h-6 min-w-0 flex-1 items-center gap-1.5 px-3 text-left",
            // `text-xs` + normal case, matching the user row names below and
            // the node header in the agent list — same sidebar, same role.
            "text-xs font-medium text-text-tertiary",
            "transition-colors duration-150 hover:text-zinc-600 dark:hover:text-zinc-300",
          )}
        >
          <ChevronRight
            className={cn(
              "h-3 w-3 shrink-0 transition-transform duration-150",
              !collapsed && "rotate-90",
            )}
          />
          <span className="truncate">{t("userList.title")}</span>
          <span className="ml-auto text-xs font-normal opacity-60">{rows.length}</span>
        </button>
      </div>

      {!collapsed && (
        <>
          {viewingName && (
            <div className="flex items-center gap-1 px-3 py-1 text-10 text-text-tertiary">
              <span className="truncate">{t("userList.viewingAs", { name: viewingName })}</span>
              <button
                type="button"
                onClick={() => toggleViewAs(viewAsUserId!)}
                aria-label={t("userList.clearView")}
                title={t("userList.clearView")}
                className="ml-auto rounded p-0.5 hover:bg-nav-item-hover"
              >
                <X className="h-3 w-3" />
              </button>
            </div>
          )}

          {actionError && (
            <div role="alert" className="px-3 py-1 text-10 text-red-500">
              {actionError}
            </div>
          )}

          {rows.map((account, index) => (
            <UserRow
              key={account.user_id}
              account={account}
              isAdmin={isAdmin}
              isSelf={account.user_id === self?.user_id}
              isLast={index === rows.length - 1}
              onOpen={() => openThread(account)}
              onContextMenu={(e) => menu.openAt(e, account)}
            />
          ))}

          {rows.length === 0 && (
            <div className="px-3 py-2 text-11 text-text-tertiary">
              {loadFailed ? t("userList.loadFailed") : t("userList.empty")}
            </div>
          )}
        </>
      )}

      {isAdmin && (
        <ContextMenu
          isOpen={menu.isOpen}
          menuProps={menu.menuProps}
          payload={menu.payload}
          selectionAtOpen={menu.selectionAtOpen}
          onClose={menu.close}
          items={menu.payload ? menuItems(menu.payload) : []}
        />
      )}

      <CreateAccountModal
        open={createOpen}
        onClose={() => setCreateOpen(false)}
        onCreated={(result: CreateAccountResult) => {
          if (result.invite_token) {
            setInvite({ token: result.invite_token, username: result.account.display_name });
          }
          // The new account isn't in the local snapshot yet — refetch so the
          // sidebar row appears immediately instead of waiting for a tab
          // switch to remount <UserList /> and re-fire the load effect.
          void refetch();
        }}
      />

      <InviteTokenModal
        open={invite !== null}
        token={invite?.token ?? ""}
        username={invite?.username}
        onClose={() => setInvite(null)}
      />

      <ConfirmDialog
        open={disableTarget !== null}
        title={t("account.disableTitle")}
        message={t("account.disableConfirm", { name: disableTarget?.display_name ?? "" })}
        confirmLabel={t("account.disableAccount")}
        destructive
        onConfirm={() => {
          const target = disableTarget;
          setDisableTarget(null);
          if (target) {
            void run(async () => {
              const token = useAuthStore.getState().accessToken ?? "";
              await disableAccount(getGatewayUrl(), token, target.user_id);
            });
          }
        }}
        onCancel={() => setDisableTarget(null)}
      />
    </div>
  );
});

/** Project a non-admin directory entry into the `UserAccount` shape so the
 *  row + `partitionAccounts` rendering path stays shared with admin. See the
 *  `ponytail:` comment in the loading effect for why the empty fields /
 *  `role: 'user'` are acceptable here. */
function directoryRow(d: DirectoryUser): UserAccount {
  return {
    user_id: d.user_id,
    username: d.username,
    display_name: d.display_name,
    role: "user",
    language: "",
    timezone: "",
    // The whole point of the avatar fix: these used to be dropped here,
    // so a non-admin's list fell through `UserAvatar` to a derived
    // placeholder icon for every peer while the inbox (reading
    // `ChatSummary.peer_avatar`) showed the real one. A cleared field
    // arrives as `null` and becomes `undefined` here to match
    // `UserAccount`; the row normalises back with `?? null`.
    avatar: d.avatar ?? undefined,
    builtin_avatar: d.builtin_avatar ?? undefined,
    created_at: "",
    updated_at: "",
  };
}

/** Single sidebar row, mirroring [AgentList.tsx](../agent-list/AgentList.tsx)
 *  agent row visuals: h-14, 40px avatar, two-line layout, accent active state.
 *  Selects through `useUserChatStore.activePeerId` rather than `viewAsUserId`
 *  so the inbox thread and the view-as-user lens are independent.
 */
function UserRow({
  account,
  isAdmin,
  isSelf,
  isLast,
  onOpen,
  onContextMenu,
}: {
  account: UserAccount;
  isAdmin: boolean;
  isSelf: boolean;
  isLast: boolean;
  onOpen: () => void;
  onContextMenu: (e: MouseEvent) => void;
}) {
  const { t } = useTranslation();
  const activePeerId = useUserChatStore((s) => s.activePeerId);
  const chatPreview = useUserChatStore((s) =>
    s.chats.find((c) => c.peer_user_id === account.user_id),
  );
  const active = account.user_id === activePeerId;
  const unread = chatPreview?.unread_count ?? 0;
  // Operator-precedence trap: written as one expression below, this reads as
  //   (preview ?? roleAdmin-flag) ? "admin" : "user"
  // — i.e. ANY non-empty preview string (truthy) flips every row to
  // "管理员", and only the no-chat fallback uses the role. The fix is to
  // parenthesise the role-based default so it only applies when `??` falls
  // through (null / undefined preview), the way the surrounding code reads.
  const preview =
    chatPreview?.last_message_preview ??
    (account.role === "admin" ? t("userList.roleAdmin") : t("userList.roleUser"));

  return (
    <div
      role={isSelf ? undefined : "button"}
      onClick={isSelf ? undefined : onOpen}
      onContextMenu={isSelf ? undefined : onContextMenu}
      data-testid={`user-row-${account.user_id}`}
      title={account.display_name}
      className={cn(
        "relative flex items-center rounded-md px-3 py-2 transition-colors duration-150",
        "gap-3",
        !isSelf && `cursor-pointer ${listRowHover}`,
        active && "bg-[var(--color-accent)]/90 text-white hover:bg-[var(--color-accent)]",
        // Same hairline the agent rows carry, so the two groups read as one
        // list. `isLast` is passed in rather than derived here: the rows are
        // flat (accounts are Gateway-wide, never partitioned by node), so
        // the caller alone knows which row closes the group.
        !isLast &&
          "row-divider-b",
      )}
    >
      <UserAvatar
        displayName={account.display_name}
        avatarUrl={account.avatar ?? null}
        builtinAvatarId={account.builtin_avatar ?? null}
        size={40}
      />

      <div className="min-w-0 flex-1 overflow-hidden">
        <div className="flex items-center justify-between gap-2">
          {/* `text-xs`, not an inline `var(--ui-font-size)`: matches the
              agent list row name and every other sidebar list, all of which
              inherit `text-xs` from the pane container. */}
          <span
            className={cn(
              "truncate font-medium text-xs",
              active ? "text-white" : "text-text-secondary",
            )}
          >
            {account.display_name}
          </span>
          {unread > 0 && (
            <span
              className={cn(
                "shrink-0 rounded-full px-1.5 text-10 font-medium",
                active ? "bg-white/20 text-white" : "bg-[var(--color-accent)] text-white",
              )}
            >
              {unread > 99 ? "99+" : unread}
            </span>
          )}
        </div>
        <div className="flex items-center gap-1.5">
          <span
            className={cn(
              "truncate text-11",
              active ? "text-white/80" : "text-text-tertiary",
            )}
          >
            {preview}
          </span>
          {isAdmin && account.role === "admin" && (
            <span
              className={cn(
                "shrink-0 rounded px-1 text-9 font-medium uppercase tracking-wide",
                active ? "bg-white/20 text-white" : "bg-nav-item-hover text-text-tertiary",
              )}
            >
              {t("userList.roleAdmin")}
            </span>
          )}
        </div>
      </div>
    </div>
  );
}