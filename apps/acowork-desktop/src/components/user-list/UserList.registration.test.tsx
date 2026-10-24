/**
 * Self-check for the click-on-row → openDm wiring that ships user↔user DM
 * threading (a `user_user` chat session) straight from the sidebar group (no
 * separate "Messages" view).
 *
 * The ADR-076 §决策 6 invite affordance used to live on a banner "+" inside
 * UserList; it now lives in the agent-list bottom "+" menu (`canInviteUser`
 * in [AgentList.tsx](../agent-list/AgentList.tsx)), which mirrors this file's
 * old `canInvite` rule. The visibility tests are gone from here because the
 * DOM node they asserted against was removed; the gating logic itself is
 * unchanged.
 */
import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, act, fireEvent } from "@testing-library/react";
import { UserList } from "./UserList";
import { useAuthStore } from "../../stores/authStore";
import { useProjectGroupStore } from "../../stores/projectGroupStore";
import { useAgentStore } from "../../stores/agentStore";
import type { Role, UserAccount } from "../../lib/types";

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(async () => ({})),
}));

// `UserList` subscribes to the user-service profile-change signal
// (`acowork/user/profiles/changed` → Tauri `user-profiles-changed`).
// Capture the handler so a test can fire it; the noop default keeps the
// other cases in this file from needing to know the listener exists.
const mqttHandlers = new Map<string, (event: { payload: unknown }) => void>();
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(
    (name: string, cb: (event: { payload: unknown }) => void) =>
      new Promise<() => void>((resolve) => {
        mqttHandlers.set(name, cb);
        resolve(() => {
          mqttHandlers.delete(name);
        });
      }),
  ),
}));

function account(role: Role, user_id?: string, display_name?: string): UserAccount {
  return {
    user_id: user_id ?? `u-${role}`,
    username: role,
    display_name: display_name ?? (role === "admin" ? "Alice" : "Bob"),
    role,
    language: "en",
    timezone: "UTC",
    created_at: "2025-01-01T00:00:00Z",
    updated_at: "2025-01-01T00:00:00Z",
  };
}

async function signIn(role: Role, registrationOpen: boolean) {
  useAuthStore.setState({
    mode: "multi_user",
    status: "logged_in",
    account: account(role),
    accessToken: "token",
    viewAsUserId: null,
    registrationOpen,
  });
  // Flush the mount-time account fetch before a test seeds the DM store, so
  // the (irrelevant) bootstrap lands first and can't race the fixture.
  await new Promise((r) => setTimeout(r, 0));
}

describe("UserList invite affordance (ADR-076 §决策 6)", () => {
  beforeEach(() => {
    // `reload` (admin path) and the account store hit the Gateway; jsdom has
    // no server, so answer with an empty account list.
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => ({
        ok: true,
        status: 200,
        json: async () => ({ accounts: [] }),
        text: async () => "{}",
      })),
    );
    useAuthStore.setState({ mode: "unknown", status: "unknown", account: null });
    mqttHandlers.clear();
  });

  it("clicking another user's row opens the DM session (self row is inert)", async () => {
    // Admin sees both themselves and a peer — the fetchAccounts mock returns
    // an empty list, so install the second account on the store directly.
    const peer = account("user", "u-peer", "Charlie");
    await signIn("admin", false);
    // The fetch mock is the global default from beforeEach (empty list).
    // Use a per-test override that lists both the admin and the peer.
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => ({
        ok: true,
        status: 200,
        json: async () => ({ accounts: [useAuthStore.getState().account!, peer] }),
        text: async () => "{}",
      })),
    );
    const openDm = vi.fn(async () => {});
    useProjectGroupStore.setState({
      groups: [],
      dms: [],
      activeGroupId: null,
      openDm,
    });
    await act(async () => {
      render(<UserList />);
    });
    const selfRow = screen.getByTestId(`user-row-${useAuthStore.getState().account!.user_id}`);
    const peerRow = screen.getByTestId(`user-row-${peer.user_id}`);
    // Clicking yourself is a no-op (you can't DM yourself).
    await act(async () => {
      fireEvent.click(selfRow);
    });
    expect(openDm).not.toHaveBeenCalled();
    // Clicking another user opens the 1:1 DM session with that peer.
    await act(async () => {
      fireEvent.click(peerRow);
    });
    expect(openDm).toHaveBeenCalledWith(peer.user_id);
  });

  it("opening a DM clears the active agent so the DM panel can render", async () => {
    // Regression: AppLayout renders <AgentPage /> whenever `selectedAgentId`
    // is set and only falls through to <GroupChatPanel /> when it is null. If
    // `openThread` only set the active session and left `selectedAgentId`
    // intact, opening a DM while an agent is selected silently
    // does nothing visible — the user reports "点 Nicholas 用户右键菜单,
    // 发消息给用户, 没任何反应" while the store did the work underneath.
    const peer = account("user", "u-peer", "Charlie");
    await signIn("admin", false);
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => ({
        ok: true,
        status: 200,
        json: async () => ({ accounts: [useAuthStore.getState().account!, peer] }),
        text: async () => "{}",
      })),
    );
    // Pretend an agent session is already open — this is the precondition
    // that makes the regression visible.
    useAgentStore.setState({ selectedAgentId: "u-existing-agent" });
    const openDm = vi.fn(async () => {});
    useProjectGroupStore.setState({
      groups: [],
      dms: [],
      activeGroupId: null,
      openDm,
    });
    await act(async () => {
      render(<UserList />);
    });
    const peerRow = screen.getByTestId(`user-row-${peer.user_id}`);
    await act(async () => {
      fireEvent.click(peerRow);
    });
    // Both stores must end up consistent: no agent selected, the DM opened.
    // The agent clear is what lets AppLayout fall through to GroupChatPanel.
    expect(useAgentStore.getState().selectedAgentId).toBeNull();
    expect(openDm).toHaveBeenCalledWith(peer.user_id);
  });

  it("a non-admin sees other users from /api/users/directory and can open a DM", async () => {
    // Regression for the contact-picker fix: a non-admin used to see only
    // themselves in the sidebar, with no way to start a 1:1 thread.
    // /api/users/directory (ADR-076 §§决策 8) is the non-admin endpoint.
    const peer = account("user", "u-peer", "Charlie");
    await signIn("user", false);
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string) => {
        if (String(url).endsWith("/api/users/directory")) {
          return {
            ok: true,
            status: 200,
            json: async () => ({ users: [peer] }),
            text: async (): Promise<string> => "{}",
          };
        }
        // Defensive: the non-admin path should never hit /api/users.
        return {
          ok: true,
          status: 200,
          json: async () => ({ accounts: [] }),
          text: async (): Promise<string> => "{}",
        };
      }),
    );
    const openDm = vi.fn(async () => {});
    useProjectGroupStore.setState({
      groups: [],
      dms: [],
      activeGroupId: null,
      openDm,
    });
    await act(async () => {
      render(<UserList />);
    });
    // Directory excludes the caller themselves, so the peer row is the
    // only one rendered — and is clickable to open the DM session.
    const peerRow = screen.getByTestId(`user-row-${peer.user_id}`);
    await act(async () => {
      fireEvent.click(peerRow);
    });
    expect(openDm).toHaveBeenCalledWith(peer.user_id);
  });

  it("falls back to role label only when no chat preview exists", async () => {
    // Regression for the operator-precedence trap:
    //   preview = last_message_preview ?? role === "admin" ? "admin" : "user"
    // parsed as (preview ?? roleAdminFlag) ? "admin" : "user", which made
    // any non-empty preview string flip every row to "管理员". The fix
    // parents the role fallback so it only fires when `??` falls through.
    const peer = account("user", "u-peer", "Charlie");
    await signIn("admin", false);
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => ({
        ok: true,
        status: 200,
        json: async () => ({ accounts: [useAuthStore.getState().account!, peer] }),
        text: async () => "{}",
      })),
    );

    // Case A: no DM exists with the peer → fallback fires → "user" label.
    useProjectGroupStore.setState({ groups: [], dms: [], activeGroupId: null });
    await act(async () => {
      render(<UserList />);
    });
    const peerRowA = screen.getByTestId(`user-row-${peer.user_id}`);
    // The default-locale i18n resolves `userList.roleUser` to "user" and
    // `userList.roleAdmin` to "admin" — the lowercase labels match the role
    // intent and are unlikely to collide with normal preview content.
    expect(peerRowA.textContent).toContain("user");
    expect(peerRowA.textContent).not.toContain("admin");
  });

  it("uses the chat preview instead of the role fallback when one exists", async () => {
    // Companion case: a chat row's preview must win over the role label.
    // Pre-fix, the ternary sat outside the `??`, so any truthy preview
    // selected "admin" regardless of the actual role.
    const peer = account("user", "u-peer-2", "Dana");
    await signIn("admin", false);
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => ({
        ok: true,
        status: 200,
        json: async () => ({ accounts: [useAuthStore.getState().account!, peer] }),
        text: async () => "{}",
      })),
    );
    useProjectGroupStore.setState({
      groups: [],
      activeGroupId: null,
      dms: [
        {
          session_id: "dm-1",
          kind: "user_user",
          title: "",
          created_at: 0,
          last_active_at: 0,
          message_count: 1,
          participants: [
            { id: useAuthStore.getState().account!.user_id },
            { id: peer.user_id },
          ],
          last_message_preview: "Hello from peer",
        },
      ],
    });
    await act(async () => {
      render(<UserList />);
    });
    const peerRow = screen.getByTestId(`user-row-${peer.user_id}`);
    expect(peerRow.textContent).toContain("Hello from peer");
    expect(peerRow.textContent).not.toContain("admin");
  });

  it("refetches accounts when the user-profile change signal fires", async () => {
    // Regression: the sidebar fetched the account list exactly once, on
    // mount. Changing your own avatar writes through the user service
    // (which publishes `acowork/user/profiles/changed`), but nothing
    // refetched, so a row kept rendering the avatar captured at login —
    // while the inbox thread, reading `ChatSummary.peer_avatar`, already
    // showed the new one. Same user, two faces.
    //
    // This is the behavioural half of the fix (the other half is that the
    // directory endpoint carries the avatar fields at all — pinned on the
    // Rust side by `user_directory_is_readable_by_any_account_but_bounded`).
    const peer = account("user", "u-peer", "Charlie");
    await signIn("admin", false);
    // First answer: no avatar. After the signal: the user set one.
    let call = 0;
    const fetchMock = vi.fn(async () => {
      call += 1;
      return {
        ok: true,
        status: 200,
        json: async () => ({
          accounts: [
            useAuthStore.getState().account!,
            call === 1
              ? peer
              : { ...peer, builtin_avatar: "icon-18" },
          ],
        }),
        text: async () => "{}",
      };
    });
    vi.stubGlobal("fetch", fetchMock);
    useProjectGroupStore.setState({ groups: [], dms: [], activeGroupId: null });

    await act(async () => {
      render(<UserList />);
    });
    expect(fetchMock).toHaveBeenCalledTimes(1);

    // The signal handler is registered asynchronously by `listen`.
    const onSignal = mqttHandlers.get("user-profiles-changed");
    expect(onSignal).toBeDefined();

    await act(async () => {
      onSignal!({ payload: { version: "7" } });
    });
    expect(fetchMock).toHaveBeenCalledTimes(2);
  });
});
