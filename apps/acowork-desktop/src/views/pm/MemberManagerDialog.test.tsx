/**
 * Self-check for MemberManagerDialog: the `kind: "user"` creator row must
 * render with a real name + UserAvatar, not the "?" placeholder.
 *
 * Why this exists:
 *   The original report was "新建项目后成员管理对话框里出现一个问号未识别
 *   agent，id 21a727 开头". The fix put a single `resolveMembers` call in
 *   place of the old `agents[instance_id]?.meta` join in five views. The
 *   pure-function tests in `pmMembers.test.ts` cover the resolver, but a
 *   earlier probe showed they do NOT cover the wiring: replacing the call
 *   with the old `agents[...].meta` join still passed every assertion.
 *
 *   This test renders the actual dialog and asserts the user-visible outcome
 *   ("?" absent, display_name present, remove button enabled). If the wiring
 *   ever regresses to a per-callsite agentStore-only join, this goes red.
 *
 * Stub strategy mirrors `TaskDetailDrawer.test.tsx`: `vi.mock` the leaf
 * stores (no network, no i18n), assert on the rendered DOM.
 */
import { describe, it, expect, vi } from "vitest";
import { fireEvent, render, screen, within } from "@testing-library/react";
import type { PmProject, PmProjectMember } from "../../lib/pm-types";

const MOCK_AGENT_ID = "58ef4139-ce28-46db-9ae9-7daecb042cb5";
const MOCK_USER_ID = "21a727ee-d2ac-4d7a-9bb1-a846cb34cf3c";
const MOCK_PEER_ID = "9b3d44cc-1f2e-4a5b-8c9d-0e7f6a5b4c3d";

const mocks = vi.hoisted(() => ({
  addMember: vi.fn(async () => {}),
  removeMember: vi.fn(async () => {}),
  fetchAgents: vi.fn(async () => {}),
  agents: {} as Record<string, { meta: { instance_id: string; display_name?: string; name?: string; avatar?: string | null; builtin_avatar?: string | null } }>,
  directoryUsers: [] as Array<{ user_id: string; username: string; display_name: string; avatar?: string | null; builtin_avatar?: string | null }>,
  localProfile: {
    displayName: "admin",
    backendAvatarUrl: null,
    backendBuiltinAvatarId: "icon-02",
  },
}));

vi.mock("../../stores/agentStore", () => ({
  useAgentStore: (selector: (state: Record<string, unknown>) => unknown) =>
    selector({ agents: mocks.agents, fetchAgents: mocks.fetchAgents }),
}));

vi.mock("../../stores/pm/projectStore", () => ({
  usePmProjectStore: (selector: (state: Record<string, unknown>) => unknown) =>
    selector({ addMember: mocks.addMember, removeMember: mocks.removeMember }),
}));

vi.mock("../../stores/authStore", () => {
  // Read the current account from a mutable module-scope holder. Tests
  // reassign it to flip between multi_user (admin) and local (null) mode
  // without rebuilding the mock. The holder lives on globalThis so the
  // mutable binding survives factory hoisting.
  (globalThis as unknown as { __authAccount: unknown }).__authAccount = {
    user_id: "21a727ee-d2ac-4d7a-9bb1-a846cb34cf3c",
    username: "admin",
    display_name: "admin",
    role: "admin",
    language: "zh-CN",
    timezone: "Asia/Shanghai",
    avatar: "assets/avatars/21a727ee/avatar-01.jpg",
    builtin_avatar: null,
    created_at: "2026-01-01T00:00:00Z",
    updated_at: "2026-01-01T00:00:00Z",
  };
  return {
    useAuthStore: (selector: (state: Record<string, unknown>) => unknown) =>
      selector({ account: (globalThis as unknown as { __authAccount: unknown }).__authAccount }),
  };
});

vi.mock("../../stores/userProfileStore", () => ({
  useUserProfileStore: (selector: (state: Record<string, unknown>) => unknown) =>
    selector({ profile: mocks.localProfile, setProfile: vi.fn() }),
}));

// The dialog reads the account directory straight from the store fields for
// its user candidates (loading stays idempotent inside the real store — not
// exercised here, hence the no-op). `ownerUserId` must match the mocked
// account or `usePmDirectoryUsers` withholds the cached list as another
// account's data.
vi.mock("../../stores/pmUserDirectoryStore", () => ({
  usePmUserDirectoryStore: (selector: (state: Record<string, unknown>) => unknown) =>
    selector({
      users: mocks.directoryUsers,
      ownerUserId: "21a727ee-d2ac-4d7a-9bb1-a846cb34cf3c",
      loadDirectory: vi.fn(async () => {}),
      reset: vi.fn(),
    }),
}));

vi.mock("../../components/common/ToastProvider", () => ({
  showToast: vi.fn(),
}));

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));

import { MemberManagerDialog } from "./MemberManagerDialog";

function member(id: string, kind: "user" | "agent"): PmProjectMember {
  return { instance_id: id, kind, added_at: "2026-10-08T07:22:18Z" };
}

function projectWith(members: PmProjectMember[]): PmProject {
  return {
    id: "p-test",
    title: "acowork",
    description: "",
    status: "active",
    created_by: members[0]?.instance_id ?? MOCK_USER_ID,
    created_at: "2026-10-08T07:22:18Z",
    updated_at: "2026-10-08T07:22:18Z",
    metadata: {},
    members,
  };
}

function rowFor(instanceId: string): HTMLElement {
  // The list item is keyed by instance_id; the id appears in a text node as
  // a 10px secondary line, so we locate by the row containing that exact id.
  return screen.getByText(instanceId).closest("li")!;
}

describe("MemberManagerDialog — user-member wiring", () => {
  it("renders a kind:user member by its account name, with no '?' placeholder", () => {
    // Only the creator (user member) is present; agentStore is empty so the
    // old `agents[id]?.meta` join would resolve to nothing and draw "?".
    render(
      <MemberManagerDialog
        project={projectWith([member(MOCK_USER_ID, "user")])}
        onClose={vi.fn()}
      />,
    );

    const row = rowFor(MOCK_USER_ID);
    expect(within(row).queryByText("?")).toBeNull();
    // Plain DOM check; no @testing-library/jest-dom in this project on
    // purpose — adding a dep just for `toBeInTheDocument` is not worth it.
    expect(within(row).getByText("admin")).toBeTruthy();
    // The remove button is enabled — the row is fully actionable, not a stub.
    expect(within(row).getByText("pm.removeMember").hasAttribute("disabled")).toBe(false);
  });

  it("renders an agent member via agentStore, and the '?' fallback for unloaded ones", () => {
    mocks.agents = {
      [MOCK_AGENT_ID]: {
        meta: { instance_id: MOCK_AGENT_ID, display_name: "Ponytail", name: "Ponytail" },
      },
    };
    render(
      <MemberManagerDialog
        project={projectWith([
          member(MOCK_USER_ID, "user"),
          member(MOCK_AGENT_ID, "agent"),
          member("ffffffff-0000-0000-0000-000000000000", "agent"),
        ])}
        onClose={vi.fn()}
      />,
    );

    // user row: named
    expect(within(rowFor(MOCK_USER_ID)).queryByText("?")).toBeNull();
    expect(within(rowFor(MOCK_USER_ID)).getByText("admin")).toBeTruthy();

    // agent row: named
    expect(within(rowFor(MOCK_AGENT_ID)).queryByText("?")).toBeNull();
    expect(within(rowFor(MOCK_AGENT_ID)).getByText("Ponytail")).toBeTruthy();

    // unloaded agent row: keeps the row (so the count stays correct) but
    // falls back to the "?" placeholder and the localized memberNotFound label.
    const unloadedRow = rowFor("ffffffff-0000-0000-0000-000000000000");
    expect(within(unloadedRow).getByText("?")).toBeTruthy();
    expect(within(unloadedRow).getByText("pm.memberNotFound")).toBeTruthy();
  });

  it("local mode: account=null but userProfileStore available → still resolves", () => {
    // Mirror what `authStore` does under `mode === "local"`: account is
    // null, but Gateway still injects `X-Actor: "human"`, so the member is
    // { instance_id: "human", kind: "user" }. The previous wiring showed
    // this as "?"; the fix routes the "human" sentinel through
    // userProfileStore.
    (globalThis as unknown as { __authAccount: unknown }).__authAccount = null;
    mocks.localProfile = {
      displayName: "Local Admin",
      backendAvatarUrl: null,
      backendBuiltinAvatarId: "icon-02",
    };
    render(
      <MemberManagerDialog
        project={projectWith([member("human", "user")])}
        onClose={vi.fn()}
      />,
    );

    const row = rowFor("human");
    expect(within(row).queryByText("?")).toBeNull();
    expect(within(row).getByText("Local Admin")).toBeTruthy();
  });

  it("lists user candidates below the agents and adds them with kind:user", () => {
    // Regression for "add-member only offered agents": the dialog must also
    // surface the signed-in account and directory accounts as addable user
    // members, and POST them with `kind: "user"` so the PM backend skips the
    // agent-directory existence check.
    (globalThis as unknown as { __authAccount: unknown }).__authAccount = {
      user_id: MOCK_USER_ID,
      username: "admin",
      display_name: "admin",
      avatar: null,
      builtin_avatar: null,
    };
    mocks.directoryUsers = [
      { user_id: MOCK_PEER_ID, username: "alice", display_name: "Alice", avatar: null, builtin_avatar: null },
    ];
    // The signed-in account is already a member (creator) → only Alice and
    // the seeded peer remain candidates; agents list stays empty.
    render(
      <MemberManagerDialog
        project={projectWith([member(MOCK_USER_ID, "user")])}
        onClose={vi.fn()}
      />,
    );

    const row = rowFor(MOCK_PEER_ID);
    expect(within(row).getByText("Alice")).toBeTruthy();
    fireEvent.click(within(row).getByText("pm.addMember"));
    expect(mocks.addMember).toHaveBeenCalledWith("p-test", MOCK_PEER_ID, "user");

    // The signed-in account is NOT offered again (already a member).
    const candidateRows = screen.getAllByText(MOCK_USER_ID);
    expect(candidateRows).toHaveLength(1); // current-members row only
  });
});