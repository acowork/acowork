/**
 * GroupMembersPanel (project path) — the group chat prompt card gate.
 *
 * A `project_group` has no user-service owner: its owner is the backing PM
 * project's (`owner_id ?? created_by`, the same id `MemberManagerDialog`
 * transfers against). The prompt card is owner-or-admin only (ADR-098
 * governance) — a plain member of the channel gets no card at all, not even
 * a read of the prompt.
 *
 * The stores are the real ones, seeded through `setState` (no network): the
 * group row (a `project_group`), the PM project behind it, and the logged-in
 * account.
 */
import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, act } from "@testing-library/react";

vi.mock("../../lib/config", () => ({ getGatewayUrl: () => "http://gw.test" }));
vi.mock("../../lib/auth-api", () => ({ fetchDirectory: vi.fn(async () => []) }));

import { GroupMembersPanel } from "./GroupMembersPanel";
import { useAuthStore } from "../../stores/authStore";
import { useProjectGroupStore } from "../../stores/projectGroupStore";
import { usePmProjectStore } from "../../stores/pm/projectStore";
import type { PmProject } from "../../lib/pm-types";
import type { ChatSessionInfo } from "../../stores/chatSessionStore";

const ME = "u-me";
const OWNER = "u-owner";

const project = {
  id: "p-1",
  title: "Proj",
  description: "",
  status: "active",
  created_by: OWNER,
  owner_id: null,
  created_at: "2026-01-01T00:00:00Z",
  updated_at: "2026-01-01T00:00:00Z",
  metadata: {},
  members: [{ instance_id: ME, kind: "user", added_at: "2026-01-01T00:00:00Z" }],
} as unknown as PmProject;

const group = {
  session_id: "pg-1",
  kind: "project_group",
  title: "Proj channel",
  project_id: "p-1",
  created_at: 0,
  last_active_at: 0,
  message_count: 0,
  participants: [{ id: ME }],
} as unknown as ChatSessionInfo;

function loginAs(userId: string, role: "user" | "admin" = "user") {
  useAuthStore.setState({
    mode: "multi_user",
    status: "logged_in",
    accessToken: "token",
    account: {
      user_id: userId,
      username: userId,
      display_name: userId,
      role,
      language: "en",
      timezone: "UTC",
      created_at: "",
      updated_at: "",
    },
  } as never);
}

beforeEach(() => {
  // The prompt card reads `chat-md` on mount — stub the transport.
  vi.stubGlobal(
    "fetch",
    vi.fn(async () => ({
      ok: true,
      status: 200,
      json: async () => ({ content: "Prompt.", is_default: true, editable: true }),
    })),
  );
  useProjectGroupStore.setState({ groups: [group], activeGroupId: "pg-1" } as never);
  usePmProjectStore.setState({ projects: [project] } as never);
});

describe("GroupMembersPanel — project group prompt card", () => {
  it("hides the card from a plain member of the channel", async () => {
    loginAs(ME);
    await act(async () => {
      render(<GroupMembersPanel width={260} />);
    });
    expect(screen.queryByTestId("group-chat-prompt")).toBeNull();
  });

  it("shows the card to the project owner", async () => {
    loginAs(OWNER);
    await act(async () => {
      render(<GroupMembersPanel width={260} />);
    });
    expect(screen.getByTestId("group-chat-prompt")).toBeTruthy();
  });

  it("shows the card to an admin who is not the project owner", async () => {
    loginAs(ME, "admin");
    await act(async () => {
      render(<GroupMembersPanel width={260} />);
    });
    expect(screen.getByTestId("group-chat-prompt")).toBeTruthy();
  });
});

describe("GroupMembersPanel — project group default responder (ADR-105)", () => {
  const withAgent = {
    ...project,
    members: [
      { instance_id: ME, kind: "user", added_at: "2026-01-01T00:00:00Z" },
      { instance_id: "ag-1", kind: "agent", added_at: "2026-01-01T00:00:00Z" },
    ],
  } as unknown as PmProject;

  /** Route the responder GET separately from the prompt GET. */
  function stubFetch(responder: string | null, editable: boolean) {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string) => ({
        ok: true,
        status: 200,
        json: async () =>
          String(url).endsWith("/responder")
            ? { responder, editable }
            : { content: "Prompt.", is_default: true, editable: true },
      })),
    );
  }

  it("shows the card to a PLAIN member (the read is open) with the agent members as options", async () => {
    usePmProjectStore.setState({ projects: [withAgent] } as never);
    stubFetch(null, false);
    loginAs(ME);
    await act(async () => {
      render(<GroupMembersPanel width={260} />);
    });
    const card = await screen.findByTestId("group-responder");
    const select = screen.getByTestId("group-responder-select") as HTMLSelectElement;
    // Read-only for a non-governor: the panel mirrors the server's answer
    // instead of re-deriving the governance rule.
    expect(select.disabled).toBe(true);
    // Candidates are agent members only — "who answers" cannot be a human.
    const values = Array.from(select.options).map((o) => o.value);
    expect(values).toEqual(["", "ag-1"]);
    expect(card).toBeTruthy();
  });

  it("renders the configured responder by name for the owner", async () => {
    usePmProjectStore.setState({ projects: [withAgent] } as never);
    stubFetch("ag-1", true);
    loginAs(OWNER);
    await act(async () => {
      render(<GroupMembersPanel width={260} />);
    });
    const select = (await screen.findByTestId("group-responder-select")) as HTMLSelectElement;
    expect(select.disabled).toBe(false);
    expect(select.value).toBe("ag-1");
  });
});
