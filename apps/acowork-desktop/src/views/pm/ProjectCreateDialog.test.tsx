/**
 * Self-check for ProjectCreateDialog: the form has to hand the store ONE
 * complete draft, not just a title.
 *
 * Why this exists: the dialog used to be a single title input inside
 * `ProjectSidebar`, so icon / description / members / owner each needed a
 * second trip to another surface after the project already existed. The
 * contract locked down here is the wiring, not the visuals — the four fields
 * must reach `createProject` together, the creator must not be offered as a
 * member (the backend seeds them there already), and a step that failed on
 * the way must still be reported after the success toast.
 *
 * Stub strategy mirrors `MemberManagerDialog.test.tsx`: `vi.mock` the leaf
 * stores (no network, no i18n), assert on the rendered DOM.
 */
import { describe, it, expect, vi, beforeAll, beforeEach } from "vitest";
import { act, fireEvent, render, screen } from "@testing-library/react";

const SELF = "21a727ee-d2ac-4d7a-9bb1-a846cb34cf3c";
const PEER = "9b3d44cc-1f2e-4a5b-8c9d-0e7f6a5b4c3d";
const AGENT = "8f7be9a6-ddae-46be-8138-264032a70150";

const mocks = vi.hoisted(() => ({
  createProject: vi.fn(),
  fetchAgents: vi.fn(async () => {}),
  showToast: vi.fn(),
}));

vi.mock("../../stores/pm/projectStore", () => ({
  usePmProjectStore: (selector: (s: Record<string, unknown>) => unknown) =>
    selector({ createProject: mocks.createProject }),
}));

vi.mock("../../stores/agentStore", () => ({
  useAgentStore: (selector: (s: Record<string, unknown>) => unknown) =>
    selector({
      agents: {
        [AGENT]: {
          meta: {
            instance_id: AGENT,
            agent_id: "com.acowork.project-manager",
            name: "PM",
            avatar: null,
            builtin_avatar: null,
          },
        },
      },
      fetchAgents: mocks.fetchAgents,
    }),
}));

vi.mock("../../stores/authStore", () => ({
  useAuthStore: (selector: (s: Record<string, unknown>) => unknown) =>
    selector({
      account: {
        user_id: SELF,
        username: "admin",
        display_name: "admin",
        role: "admin",
        language: "zh-CN",
        timezone: "Asia/Shanghai",
        avatar: null,
        builtin_avatar: null,
        created_at: "2026-01-01T00:00:00Z",
        updated_at: "2026-01-01T00:00:00Z",
      },
      accessToken: "t",
      status: "logged_in",
    }),
}));

vi.mock("../../stores/pmUserDirectoryStore", () => ({
  usePmUserDirectoryStore: (selector: (s: Record<string, unknown>) => unknown) =>
    selector({
      users: [
        {
          user_id: PEER,
          username: "bob",
          display_name: "Bob",
          avatar: null,
          builtin_avatar: null,
        },
      ],
      ownerUserId: SELF,
      loadDirectory: vi.fn(async () => {}),
      reset: vi.fn(),
    }),
}));

vi.mock("../../components/common/ToastProvider", () => ({
  showToast: mocks.showToast,
}));

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));

import { ProjectCreateDialog } from "./ProjectCreateDialog";

beforeAll(() => {
  // jsdom implements neither half of the object-URL pair; the preview is not
  // what this test is about, the File handed to the store is.
  Object.defineProperty(URL, "createObjectURL", { value: vi.fn(() => "blob:preview") });
  Object.defineProperty(URL, "revokeObjectURL", { value: vi.fn() });
});

function renderDialog() {
  const onClose = vi.fn();
  render(<ProjectCreateDialog onClose={onClose} />);
  return onClose;
}

// The leaf-store mocks are module-level singletons; without this the spies
// carry calls (and toast history) from the previous test in the file.
beforeEach(() => {
  vi.clearAllMocks();
});

describe("ProjectCreateDialog — one-shot draft", () => {
  it("sends icon, description, members and owner in a single draft", async () => {
    mocks.createProject.mockResolvedValue({
      project: { id: "p-new", created_by: SELF },
      warnings: [],
    });
    const onClose = renderDialog();

    fireEvent.change(screen.getByLabelText("pm.projectName"), {
      target: { value: "  acowork  " },
    });
    fireEvent.change(screen.getByLabelText("pm.projectRemark"), {
      target: { value: "collaborate with your AI colleagues" },
    });
    const file = new File(["x"], "icon.png", { type: "image/png" });
    fireEvent.change(document.querySelector('input[type="file"]')!, {
      target: { files: [file] },
    });
    fireEvent.click(screen.getByRole("button", { name: /PM/ }));
    fireEvent.change(screen.getByLabelText("pm.owner"), {
      target: { value: PEER },
    });

    // 提交是异步的（store 编排 1 + N 次调用），act 里等它落地
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "common.create" }));
    });

    expect(mocks.createProject).toHaveBeenCalledWith({
      title: "acowork",
      description: "collaborate with your AI colleagues",
      iconFile: file,
      members: [{ instance_id: AGENT, kind: "agent" }],
      ownerId: PEER,
    });
    expect(onClose).toHaveBeenCalled();
  });

  it("does not offer the creator as a member candidate", () => {
    // `create_project_as` already seeds `members` with the caller, so an
    // "admin" row here would be an invite the server refuses (409). The
    // member rows are the only buttons carrying `aria-pressed`.
    renderDialog();
    const rows = screen.getAllByRole("button", { pressed: false });
    expect(rows.map((r) => r.textContent?.trim())).toEqual(["PM", "Bob"]);
  });

  it("reports the steps that did not land, after the success toast", async () => {
    mocks.createProject.mockResolvedValue({
      project: { id: "p-new", created_by: SELF },
      warnings: ["member"],
    });
    renderDialog();
    fireEvent.change(screen.getByLabelText("pm.projectName"), {
      target: { value: "acowork" },
    });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "common.create" }));
    });

    expect(mocks.showToast).toHaveBeenCalledWith(
      expect.objectContaining({ message: "pm.createWarnMember" }),
    );
    const toasts = mocks.showToast.mock.calls.map((c) => c[0].type);
    expect(toasts).toEqual(["success", "warning"]);
  });

  it("refuses an empty title without calling the store", async () => {
    renderDialog();
    fireEvent.click(screen.getByRole("button", { name: "common.create" }));
    expect(mocks.showToast).toHaveBeenCalledWith(
      expect.objectContaining({ message: "pm.newProjectTitleRequired" }),
    );
    expect(mocks.createProject).not.toHaveBeenCalled();
  });

  it("uses the shared icon picker, not a bespoke upload control", () => {
    // 图标选择的视觉在应用里只有一份（ProjectSettings 的预览即触发器 +
    // 铅笔角标 + 弹层网格）。这里必须是同一个组件的 draft 模式：触发器
    // 带 pm.iconPicker.title，收起状态下不存在第二个"上传"按钮。
    renderDialog();
    expect(screen.getByRole("button", { name: "pm.iconPicker.title" })).toBeTruthy();
    expect(screen.queryByRole("button", { name: "pm.iconPicker.upload" })).toBeNull();
  });
});
