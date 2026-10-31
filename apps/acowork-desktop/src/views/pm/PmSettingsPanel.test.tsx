/**
 * PmSettingsPanel smoke — the pm 视图右侧设置面板（齿轮轨唤出）。
 *
 * 覆盖计划钉住的三件事：
 *   1. 选中项目时渲染各分区（项目信息 / 显示设置 / 成员管理 / 项目操作 / 服务状态），
 *      内容来自项目本身（标题）与内嵌的 MemberManagerSection；
 *   2. 未选中项目 → 空态提示，不渲染操作区；
 *   3. 删除按钮 → ConfirmDialog（级联语义）→ 确认后调用 deleteProject。
 *
 * Store stub 策略镜像 `MemberManagerDialog.test.tsx`：`vi.mock` 叶子 store
 * （无网络、无 i18n — t(key) 返回 key 原文），断言渲染出的 DOM。
 */
import { describe, it, expect, vi, beforeEach } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import type { PmProject, PmProjectMember } from "../../lib/pm-types";

const MOCK_USER_ID = "21a727ee-d2ac-4d7a-9bb1-a846cb34cf3c";
const addedAt = "2026-10-08T07:22:18Z";

const mocks = vi.hoisted(() => ({
  selected: null as unknown,
  updateProjectMeta: vi.fn(async () => true),
  deleteProject: vi.fn(async () => true),
  addMember: vi.fn(async () => {}),
  removeMember: vi.fn(async () => {}),
  transferOwner: vi.fn(async () => {}),
  fetchAgents: vi.fn(async () => {}),
  healthy: true as boolean | null,
  checking: false,
  check: vi.fn(async () => {}),
}));

vi.mock("../../stores/pm/projectStore", () => ({
  usePmProjectStore: (selector: (state: Record<string, unknown>) => unknown) =>
    selector({
      selected: mocks.selected,
      updateProjectMeta: mocks.updateProjectMeta,
      deleteProject: mocks.deleteProject,
      addMember: mocks.addMember,
      removeMember: mocks.removeMember,
      transferOwner: mocks.transferOwner,
    }),
}));

vi.mock("../../stores/pm/healthStore", () => ({
  usePmHealthStore: (selector: (state: Record<string, unknown>) => unknown) =>
    selector({ healthy: mocks.healthy, checking: mocks.checking, check: mocks.check }),
}));

// MemberManagerSection dependencies — minimal faithful stubs, same shape as
// MemberManagerDialog.test.tsx (agents empty, caller = owner account).
vi.mock("../../stores/agentStore", () => ({
  useAgentStore: (selector: (state: Record<string, unknown>) => unknown) =>
    selector({ agents: {}, fetchAgents: mocks.fetchAgents }),
}));

vi.mock("../../stores/authStore", () => {
  // Mutable like `MemberManagerDialog.test.tsx`: one test flips the caller to a
  // plain member to pin the governance gate, then restores it.
  (globalThis as unknown as { __panelAccount: unknown }).__panelAccount = {
    user_id: "21a727ee-d2ac-4d7a-9bb1-a846cb34cf3c",
    username: "admin",
    display_name: "admin",
    role: "admin",
    language: "zh-CN",
    timezone: "Asia/Shanghai",
    avatar: null,
    builtin_avatar: null,
    created_at: "2026-01-01T00:00:00Z",
    updated_at: "2026-01-01T00:00:00Z",
  };
  return {
    useAuthStore: (selector: (state: Record<string, unknown>) => unknown) =>
      selector({
        account: (globalThis as unknown as { __panelAccount: unknown }).__panelAccount,
        mode: "multi_user",
      }),
  };
});

vi.mock("../../stores/userProfileStore", () => ({
  useUserProfileStore: (selector: (state: Record<string, unknown>) => unknown) =>
    selector({
      profile: { displayName: "admin", backendAvatarUrl: null, backendBuiltinAvatarId: "icon-02" },
      setProfile: vi.fn(),
    }),
}));

vi.mock("../../stores/pmUserDirectoryStore", () => ({
  usePmUserDirectoryStore: (selector: (state: Record<string, unknown>) => unknown) =>
    selector({
      users: [],
      ownerUserId: MOCK_USER_ID,
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

import { PmSettingsPanel } from "./PmSettingsPanel";

function projectFixture(overrides?: Partial<PmProject>): PmProject {
  const members: PmProjectMember[] = [
    { instance_id: MOCK_USER_ID, kind: "user", added_at: addedAt },
  ];
  return {
    id: "p-panel",
    title: "ACowork",
    description: "",
    status: "active",
    created_by: MOCK_USER_ID,
    created_at: addedAt,
    updated_at: addedAt,
    metadata: {},
    members,
    ...overrides,
  };
}

beforeEach(() => {
  vi.clearAllMocks();
  mocks.selected = projectFixture();
  mocks.healthy = true;
});

function renderPanel() {
  return render(
    <PmSettingsPanel width={340} onResizeStart={vi.fn()} onClose={vi.fn()} />,
  );
}

describe("PmSettingsPanel", () => {
  it("renders the settings cards with the selected project's content", () => {
    renderPanel();

    expect(screen.getByText("pm.settingsPanel.info")).toBeTruthy();
    expect(screen.getByText("pm.settingsPanel.display")).toBeTruthy();
    expect(screen.getByText("pm.settingsPanel.members")).toBeTruthy();
    expect(screen.getByText("pm.settingsPanel.actions")).toBeTruthy();
    expect(screen.getByText("pm.settingsPanel.service")).toBeTruthy();

    // 项目信息区：图标触发器 + 图标旁名字/ID 展示 + 可编辑名字输入框；
    // 成员区 hint 来自内嵌的 MemberManagerSection；服务状态 healthy=true → online。
    expect(screen.getByLabelText("pm.iconPicker.title")).toBeTruthy();
    expect(screen.getByText("ACowork")).toBeTruthy(); // 图标旁的展示名
    expect(screen.getByDisplayValue("ACowork")).toBeTruthy(); // 可编辑名字输入框
    expect(screen.getByText("ID: p-panel")).toBeTruthy();
    expect(screen.getByText("pm.memberManagerHint")).toBeTruthy();
    expect(screen.getByText("pm.settingsPanel.serviceOnline")).toBeTruthy();
  });

  it("saves the project name on blur via updateProjectMeta (title only, not metadata)", () => {
    renderPanel();
    const input = screen.getByDisplayValue("ACowork");
    fireEvent.change(input, { target: { value: "Renamed" } });
    fireEvent.blur(input);
    expect(mocks.updateProjectMeta).toHaveBeenCalledWith("p-panel", { title: "Renamed" });
  });

  it("defaults the board page size to 20 and stores it without dropping metadata siblings", async () => {
    const icon = { kind: "builtin", id: "icon-07" };
    mocks.selected = projectFixture({ metadata: { icon } });
    renderPanel();

    const select = screen.getByLabelText("pm.settingsPanel.tasksPerPage") as HTMLSelectElement;
    expect(select.value).toBe("20");

    fireEvent.change(select, { target: { value: "50" } });
    await waitFor(() => {
      // metadata is replaced wholesale server-side → the icon must survive.
      expect(mocks.updateProjectMeta).toHaveBeenCalledWith("p-panel", {
        metadata: { icon, boardPageSize: 50 },
      });
    });
  });

  it("shows the empty state and hides actions when no project is selected", () => {
    mocks.selected = null;
    renderPanel();

    expect(screen.getByText("pm.settingsPanel.noProject")).toBeTruthy();
    expect(screen.queryByText("pm.settingsPanel.actions")).toBeNull();
    expect(screen.queryByText("common.deleteProject")).toBeNull();
  });

  it("delete button opens the cascade confirm and confirming calls deleteProject", async () => {
    renderPanel();

    expect(screen.queryByRole("alertdialog")).toBeNull();
    fireEvent.click(screen.getByText("common.deleteProject"));

    const dialog = screen.getByRole("alertdialog");
    expect(dialog.textContent).toContain("pm.deleteProjectTitle");

    fireEvent.click(screen.getByText("common.delete"));
    await waitFor(() => {
      expect(mocks.deleteProject).toHaveBeenCalledWith("p-panel");
    });
  });

  it("gates the project mutations on owner-or-admin", () => {
    // PM's `require_governance` refuses rename / delete / roster for anyone but
    // the project owner or an admin. The panel mirrors it: the info stays
    // readable but read-only, and the "项目操作" card is not rendered at all.
    const BOB = "cccc0000-0000-0000-0000-000000000000";
    (globalThis as unknown as { __panelAccount: unknown }).__panelAccount = {
      user_id: BOB,
      username: "bob",
      display_name: "Bob",
      role: "user",
      language: "zh-CN",
      timezone: "Asia/Shanghai",
      avatar: null,
      builtin_avatar: null,
      created_at: "2026-01-01T00:00:00Z",
      updated_at: "2026-01-01T00:00:00Z",
    };
    try {
      renderPanel();
      // Readable…
      expect(screen.getByText("ACowork")).toBeTruthy();
      // …but not writable.
      expect((screen.getByLabelText("pm.projectName") as HTMLInputElement).disabled).toBe(true);
      expect((screen.getByLabelText("pm.projectRemark") as HTMLTextAreaElement).disabled).toBe(true);
      expect(
        (screen.getByLabelText("pm.settingsPanel.tasksPerPage") as HTMLSelectElement).disabled,
      ).toBe(true);
      expect(screen.queryByText("pm.settingsPanel.actions")).toBeNull();
      expect(screen.queryByText("common.deleteProject")).toBeNull();
    } finally {
      (globalThis as unknown as { __panelAccount: unknown }).__panelAccount = {
        user_id: MOCK_USER_ID,
        username: "admin",
        display_name: "admin",
        role: "admin",
        language: "zh-CN",
        timezone: "Asia/Shanghai",
        avatar: null,
        builtin_avatar: null,
        created_at: "2026-01-01T00:00:00Z",
        updated_at: "2026-01-01T00:00:00Z",
      };
    }
  });

  it("the owner (not an admin) keeps every affordance", () => {
    // The gate is owner OR admin, not admin only — the project creator is the
    // one who must be able to rename and delete their own project.
    (globalThis as unknown as { __panelAccount: unknown }).__panelAccount = {
      user_id: MOCK_USER_ID,
      username: "carol",
      display_name: "Carol",
      role: "user",
      language: "zh-CN",
      timezone: "Asia/Shanghai",
      avatar: null,
      builtin_avatar: null,
      created_at: "2026-01-01T00:00:00Z",
      updated_at: "2026-01-01T00:00:00Z",
    };
    try {
      renderPanel();
      expect((screen.getByLabelText("pm.projectName") as HTMLInputElement).disabled).toBe(false);
      expect(screen.getByText("common.deleteProject")).toBeTruthy();
    } finally {
      (globalThis as unknown as { __panelAccount: unknown }).__panelAccount = {
        user_id: MOCK_USER_ID,
        username: "admin",
        display_name: "admin",
        role: "admin",
        language: "zh-CN",
        timezone: "Asia/Shanghai",
        avatar: null,
        builtin_avatar: null,
        created_at: "2026-01-01T00:00:00Z",
        updated_at: "2026-01-01T00:00:00Z",
      };
    }
  });

  it("close button forwards to onClose", () => {
    const onClose = vi.fn();
    render(<PmSettingsPanel width={340} onClose={onClose} />);
    fireEvent.click(screen.getByLabelText("common.close"));
    expect(onClose).toHaveBeenCalledTimes(1);
  });
});
