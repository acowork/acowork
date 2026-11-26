/**
 * `projectStore.createProject` — the one-shot create orchestration.
 *
 * The dialog collects icon / description / members / owner in one form, but
 * the PM API has no single endpoint for that: the icon upload and every
 * invite are addressed by `pid`, so they can only run after the project
 * exists. The two invariants that actually bite:
 *
 *   1. **The ownership transfer runs last.** Inviting and transferring are
 *      both governance acts (`require_governance`), and the caller stops
 *      being the owner the moment the transfer lands. Transfer first and
 *      every remaining invite is a 403.
 *   2. **The new owner must already be a human member** (PM store
 *      `transfer_owner`). A project owner who was never ticked in the member
 *      list has to be invited on the way, or the transfer is refused.
 *
 * And the failure policy: the project itself is never rolled back — a step
 * that did not land degrades into a `warnings` entry.
 */

import { describe, it, expect, vi, beforeEach } from "vitest";

const SELF = "21a727ee-d2ac-4d7a-9bb1-a846cb34cf3c";
const PEER = "9b3d44cc-1f2e-4a5b-8c9d-0e7f6a5b4c3d";
const AGENT = "8f7be9a6-ddae-46be-8138-264032a70150";

const calls: string[] = [];
const mocks = vi.hoisted(() => ({
  createProject: vi.fn(),
  uploadProjectIcon: vi.fn(),
  updateProject: vi.fn(),
  addProjectMember: vi.fn(),
  transferProjectOwner: vi.fn(),
  listProjectCounts: vi.fn(),
}));

vi.mock("../../lib/pm-api", () => ({
  PmApiError: class PmApiError extends Error {},
  createProject: (input: unknown) => mocks.createProject(input),
  uploadProjectIcon: (pid: string, file: File) =>
    mocks.uploadProjectIcon(pid, file),
  updateProject: (pid: string, input: unknown) => {
    calls.push("icon-patch");
    return mocks.updateProject(pid, input);
  },
  addProjectMember: (pid: string, input: unknown) =>
    mocks.addProjectMember(pid, input),
  transferProjectOwner: (pid: string, owner: string) =>
    mocks.transferProjectOwner(pid, owner),
  listProjectCounts: () => mocks.listProjectCounts(),
}));
vi.mock("../../lib/logger", () => ({
  log: { debug: () => {}, info: () => {}, warn: () => {}, error: () => {} },
}));

const { usePmProjectStore } = await import("./projectStore");

function project(overrides: Record<string, unknown> = {}) {
  return {
    id: "p-new",
    title: "acowork",
    description: "",
    status: "active",
    created_by: SELF,
    owner_id: null,
    created_at: "2026-11-26T00:00:00Z",
    updated_at: "2026-11-26T00:00:00Z",
    metadata: {},
    members: [{ instance_id: SELF, kind: "user", added_at: "2026-11-26T00:00:00Z" }],
    ...overrides,
  };
}

/** Each mutation returns the project as the server would, so the store's
 *  roster/metadata reads in the orchestration see what it really sees. */
function track(name: string, next: (prev: unknown) => unknown) {
  mocks[name].mockImplementation(async () => {
    calls.push(name);
    return next(usePmProjectStore.getState().projects[0]);
  });
}

beforeEach(() => {
  calls.length = 0;
  vi.clearAllMocks();
  usePmProjectStore.setState({ projects: [], selected: null, counts: {} });
  mocks.createProject.mockImplementation(async () => {
    calls.push("createProject");
    return project();
  });
  mocks.listProjectCounts.mockResolvedValue([]);
  mocks.uploadProjectIcon.mockResolvedValue({ relative_path: "projects/p-new/icon/icon-01.png" });
  track("updateProject", (p) => ({ ...p, metadata: { icon: { kind: "custom", path: "x" } } }));
  track("addProjectMember", (p) => ({
    ...p,
    members: [...p.members, { instance_id: "pending", kind: "agent", added_at: "" }],
  }));
  track("transferProjectOwner", (p) => ({ ...p, owner_id: PEER }));
});

describe("projectStore.createProject — one-shot orchestration", () => {
  it("runs the ownership transfer after every invite", async () => {
    const outcome = await usePmProjectStore.getState().createProject({
      title: "acowork",
      members: [{ instance_id: AGENT, kind: "agent" }],
      ownerId: PEER,
    });

    expect(outcome?.warnings).toEqual([]);
    // The peer was not ticked as a member, so the store invites them on the
    // way — otherwise PM refuses the transfer ("must be an existing human
    // member") — and the transfer is the LAST call of the whole flow.
    expect(calls).toEqual([
      "createProject",
      "addProjectMember",
      "addProjectMember",
      "transferProjectOwner",
    ]);
    const invites = mocks.addProjectMember.mock.calls;
    expect(invites.map((c) => c[1].instance_id)).toEqual([AGENT, PEER]);
    expect(invites[1][1].kind).toBe("user");
  });

  it("never transfers when the owner is the creator", async () => {
    await usePmProjectStore.getState().createProject({
      title: "acowork",
      ownerId: SELF,
    });
    expect(mocks.transferProjectOwner).not.toHaveBeenCalled();
  });

  it("uploads the icon against the new pid and merges it into metadata", async () => {
    const file = new File(["x"], "icon.png", { type: "image/png" });
    await usePmProjectStore.getState().createProject({ title: "acowork", iconFile: file });

    expect(mocks.uploadProjectIcon.mock.calls[0][0]).toBe("p-new");
    // PATCH replaces metadata wholesale — the payload must carry the ref,
    // not clobber the rest of the document.
    expect(mocks.updateProject.mock.calls[0][1]).toEqual({
      metadata: { icon: { kind: "custom", path: "projects/p-new/icon/icon-01.png" } },
    });
  });

  it("keeps the project and reports the step that failed", async () => {
    mocks.uploadProjectIcon.mockRejectedValue(new Error("offline"));
    mocks.addProjectMember.mockRejectedValue(new Error("agent_not_found"));
    const outcome = await usePmProjectStore.getState().createProject({
      title: "acowork",
      iconFile: new File(["x"], "icon.png"),
      members: [
        { instance_id: AGENT, kind: "agent" },
        { instance_id: PEER, kind: "user" },
      ],
    });

    // Two invites failed but the user gets one warning, not two toasts.
    expect(outcome?.project.id).toBe("p-new");
    expect(outcome?.warnings).toEqual(["icon", "member"]);
  });
});
