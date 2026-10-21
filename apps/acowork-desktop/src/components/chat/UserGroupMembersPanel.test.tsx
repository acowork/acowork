/**
 * UserGroupMembersPanel — the `user_group` member surface (ADR-098 P2).
 *
 * A `user_group`'s roster IS the chat-session participants, managed through
 * the user-side routes. This pins the wiring:
 *   - the panel names each participant and, for any member (self included),
 *     exposes a remove tile → `removeMember`;
 *   - a non-member human from the directory is offered by the invite dialog →
 *     `addMembers`, while an existing member (Bob) is not.
 *
 * `addMembers` / `removeMember` and `fetchDirectory` are the seams; the group
 * is passed straight in as a prop (no store subscription needed).
 */
import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, act, fireEvent, within } from "@testing-library/react";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(async () => ({})) }));
vi.mock("../../lib/config", () => ({ getGatewayUrl: () => "http://gw.test" }));

const fetchDirectory = vi.hoisted(() =>
  vi.fn(async () => [
    { user_id: "u-bob", username: "bob", display_name: "Bob" },
    { user_id: "u-carol", username: "carol", display_name: "Carol" },
  ]),
);
vi.mock("../../lib/auth-api", () => ({ fetchDirectory }));

import { UserGroupMembersPanel } from "./UserGroupMembersPanel";
import { useAuthStore } from "../../stores/authStore";
import { useProjectGroupStore } from "../../stores/projectGroupStore";
import type { ChatSessionInfo } from "../../stores/chatSessionStore";

const ME = "u-me";

const group = {
  session_id: "ug-1",
  kind: "user_group",
  title: "Design sync",
  created_at: 0,
  last_active_at: 0,
  message_count: 0,
  participants: [{ id: ME }, { id: "u-bob" }],
} as unknown as ChatSessionInfo;

beforeEach(() => {
  fetchDirectory.mockClear();
  useAuthStore.setState({
    mode: "multi_user", status: "logged_in", accessToken: "token",
    account: { user_id: ME, username: "me", display_name: "Me", role: "user", language: "en", timezone: "UTC", created_at: "", updated_at: "" },
  });
});

describe("UserGroupMembersPanel", () => {
  it("removes another member through the user-side route", async () => {
    const removeMember = vi.fn(async () => {});
    useProjectGroupStore.setState({ removeMember } as never);
    await act(async () => {
      render(<UserGroupMembersPanel group={group} width={260} />);
    });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Remove Bob" }));
    });
    expect(removeMember).toHaveBeenCalledWith("ug-1", "u-bob");
  });

  it("invites a directory human who is not already a member", async () => {
    const addMembers = vi.fn(async () => {});
    useProjectGroupStore.setState({ addMembers } as never);
    await act(async () => {
      render(<UserGroupMembersPanel group={group} width={260} />);
    });
    // Open the invite dialog via the trailing "+" tile (aria-label "Add").
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Add" }));
    });
    const dialog = screen.getByRole("dialog");
    // Bob is already a member and must not be offered; Carol is.
    expect(within(dialog).queryByText("Bob")).toBeNull();
    await act(async () => {
      fireEvent.click(within(dialog).getByRole("button", { name: /Carol/ }));
    });
    await act(async () => {
      fireEvent.click(within(dialog).getByRole("button", { name: "Add" }));
    });
    expect(addMembers).toHaveBeenCalledWith("ug-1", ["u-carol"]);
  });
});
