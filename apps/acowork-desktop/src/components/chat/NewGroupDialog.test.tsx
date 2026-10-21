/**
 * NewGroupDialog — the create-`user_group` flow (ADR-098 P1).
 *
 * Pins the wiring rather than pixels:
 *   - the candidate set is directory humans (self excluded) ∪ registry agents;
 *   - submitting sends the trimmed title + the toggled ids to
 *     `projectGroupStore.createGroup` and closes on success;
 *   - a blank title never fires the create.
 *
 * `createGroup` and `fetchDirectory` are the seams; agents are seeded straight
 * onto the store so the mount-time `fetchAgents` is a no-op.
 */
import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, act, fireEvent } from "@testing-library/react";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(async () => ({})) }));
vi.mock("../../lib/config", () => ({ getGatewayUrl: () => "http://gw.test" }));

const fetchDirectory = vi.hoisted(() =>
  vi.fn(async () => [
    { user_id: "u-me", username: "me", display_name: "Me" },
    { user_id: "u-bob", username: "bob", display_name: "Bob" },
  ]),
);
vi.mock("../../lib/auth-api", () => ({ fetchDirectory }));

import { NewGroupDialog } from "./NewGroupDialog";
import { useAuthStore } from "../../stores/authStore";
import { useAgentStore } from "../../stores/agentStore";
import { useProjectGroupStore } from "../../stores/projectGroupStore";
import type { AgentStorage } from "../../stores/agentStore";

const ME = "u-me";

function agent(id: string, display_name: string): AgentStorage {
  return { meta: { instance_id: id, agent_id: id, name: id, display_name } } as unknown as AgentStorage;
}

beforeEach(() => {
  fetchDirectory.mockClear();
  useAuthStore.setState({
    mode: "multi_user", status: "logged_in", accessToken: "token",
    account: { user_id: ME, username: "me", display_name: "Me", role: "user", language: "en", timezone: "UTC", created_at: "", updated_at: "" },
  });
  useAgentStore.setState({ agents: { "ag-1": agent("ag-1", "Bot") }, fetchAgents: vi.fn(async () => {}) } as never);
});

describe("NewGroupDialog", () => {
  it("offers directory humans (minus self) and agents, then creates with the toggled ids", async () => {
    const createGroup = vi.fn(async () => "ug-new");
    const onClose = vi.fn();
    useProjectGroupStore.setState({ createGroup } as never);

    await act(async () => {
      render(<NewGroupDialog onClose={onClose} />);
    });

    // Self is never a candidate; Bob and Bot are.
    expect(screen.queryByText("Me")).toBeNull();
    const bobRow = screen.getByRole("button", { name: /Bob/ });
    const botRow = screen.getByRole("button", { name: /Bot/ });

    await act(async () => {
      fireEvent.click(bobRow);
      fireEvent.click(botRow);
    });
    await act(async () => {
      fireEvent.change(screen.getByLabelText("Group name"), { target: { value: "  Design sync  " } });
    });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Create" }));
    });

    expect(createGroup).toHaveBeenCalledWith("Design sync", ["u-bob", "ag-1"]);
    expect(onClose).toHaveBeenCalled();
  });

  it("does not create when the title is blank", async () => {
    const createGroup = vi.fn(async () => "ug-new");
    useProjectGroupStore.setState({ createGroup } as never);
    await act(async () => {
      render(<NewGroupDialog onClose={vi.fn()} />);
    });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: /Bob/ }));
    });
    // The create button stays disabled with an empty trimmed title.
    const createBtn = screen.getByRole("button", { name: "Create" }) as HTMLButtonElement;
    expect(createBtn.disabled).toBe(true);
    expect(createGroup).not.toHaveBeenCalled();
  });

  it("refuses to create a group with no other member (≥2 floor)", async () => {
    const createGroup = vi.fn(async () => "ug-new");
    useProjectGroupStore.setState({ createGroup } as never);
    await act(async () => {
      render(<NewGroupDialog onClose={vi.fn()} />);
    });
    await act(async () => {
      fireEvent.change(screen.getByLabelText("Group name"), { target: { value: "Solo" } });
    });
    // Title present but nobody toggled: the create is refused, self alone is
    // not a group.
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Create" }));
    });
    expect(createGroup).not.toHaveBeenCalled();
  });
});
