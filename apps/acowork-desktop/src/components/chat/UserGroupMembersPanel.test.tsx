/**
 * UserGroupMembersPanel — the `user_group` member surface (ADR-098 P2 +
 * governance).
 *
 * A `user_group`'s roster IS the chat-session participants, managed through
 * the user-side routes. This pins the wiring:
 *   - the panel names each participant; removing ANOTHER member exposes a ×
 *     tile → `removeMember`, but only for an owner/admin actor (a plain member
 *     has no × — they leave via the bottom button, which always works);
 *   - a non-member human from the directory is offered by the invite dialog →
 *     `addMembers`, while an existing member (Bob) is not;
 *   - governance (ADR-098): the owner is a permanent member (badge, no remove,
 *     leave disabled until transfer), rename/transfer only render for
 *     owner/admin, and the actions ride `renameGroup` / `transferOwner`.
 *
 * `addMembers` / `removeMember` / `renameGroup` / `transferOwner` and
 * `fetchDirectory` are the seams; the group is passed straight in as a prop
 * (no store subscription needed).
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
  it("removes another member when the actor is the owner", async () => {
    const removeMember = vi.fn(async () => {});
    useProjectGroupStore.setState({ removeMember } as never);
    await act(async () => {
      render(<UserGroupMembersPanel group={{ ...group, owner: ME }} width={260} />);
    });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Remove Bob" }));
    });
    expect(removeMember).toHaveBeenCalledWith("ug-1", "u-bob");
  });

  it("hides the remove tile from a plain member (removal is owner/admin only)", async () => {
    // Carol owns the group; ME is a plain member. A non-owner member (Bob)
    // still gets no × for ME — only owner/admin may remove others.
    const trio = {
      ...group,
      owner: "u-carol",
      participants: [{ id: ME }, { id: "u-bob" }, { id: "u-carol" }],
    } as unknown as ChatSessionInfo;
    await act(async () => {
      render(<UserGroupMembersPanel group={trio} width={260} />);
    });
    expect(screen.queryByRole("button", { name: "Remove Bob" })).toBeNull();
    // Yet ME can still leave (self-removal always allowed).
    expect(screen.getByRole("button", { name: "Leave group" })).toBeTruthy();
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

  it("renames through the store when the actor is the owner", async () => {
    const renameGroup = vi.fn(async () => {});
    useProjectGroupStore.setState({ renameGroup } as never);
    await act(async () => {
      render(<UserGroupMembersPanel group={{ ...group, owner: ME }} width={260} />);
    });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Rename" }));
    });
    const dialog = screen.getByRole("dialog", { name: "Rename group" });
    fireEvent.change(within(dialog).getByPlaceholderText("Group name"), {
      target: { value: "Design sync 2" },
    });
    await act(async () => {
      fireEvent.click(within(dialog).getByRole("button", { name: "Rename" }));
    });
    expect(renameGroup).toHaveBeenCalledWith("ug-1", "Design sync 2");
  });

  it("hides rename and transfer from a plain (non-owner, non-admin) member", async () => {
    await act(async () => {
      render(<UserGroupMembersPanel group={{ ...group, owner: "u-bob" }} width={260} />);
    });
    expect(screen.queryByTestId("group-rename-button")).toBeNull();
    expect(screen.queryByRole("button", { name: "Make Bob the owner" })).toBeNull();
    // The owner tile is permanent: even the plain member's remove grammar
    // skips it — only self-removal (leave) and other non-owner tiles stand.
    expect(screen.queryByRole("button", { name: "Remove Bob" })).toBeNull();
  });

  it("marks the owner tile permanently and disables the owner's leave", async () => {
    await act(async () => {
      render(<UserGroupMembersPanel group={{ ...group, owner: ME }} width={260} />);
    });
    // Owner badge on self; no remove affordance for the owner tile. (Self is
    // not in the mocked directory, so the panel resolves the name from the
    // auth store — "Me", never the id fragment.)
    expect(screen.getByText("Owner")).toBeTruthy();
    expect(screen.queryByText(/u-me/)).toBeNull();
    expect(screen.queryByRole("button", { name: "Remove Me" })).toBeNull();
    // Leave exists but is locked until ownership moves (backend refuses).
    const leave = screen.getByRole("button", { name: "Leave group" });
    expect(leave.hasAttribute("disabled")).toBe(true);
    expect(leave.getAttribute("title")).toBe("Transfer the owner role before leaving");
    // The other human tile gets the transfer target (owner may move it).
    expect(screen.getByRole("button", { name: "Make Bob the owner" })).toBeTruthy();
  });

  it("transfers ownership through the store", async () => {
    const transferOwner = vi.fn(async () => {});
    useProjectGroupStore.setState({ transferOwner } as never);
    await act(async () => {
      render(<UserGroupMembersPanel group={{ ...group, owner: ME }} width={260} />);
    });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Make Bob the owner" }));
    });
    expect(transferOwner).toHaveBeenCalledWith("ug-1", "u-bob");
  });

  it("an admin member governs even without owning the group", async () => {
    useAuthStore.setState((s) => ({
      account: { ...s.account!, role: "admin" },
    }));
    await act(async () => {
      render(<UserGroupMembersPanel group={{ ...group, owner: "u-bob" }} width={260} />);
    });
    // Rename + transfer affordances show; the owner tile stays unremovable.
    expect(screen.getByTestId("group-rename-button")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Make Me the owner" })).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Remove Bob" })).toBeNull();
    // Admin who is not the owner CAN leave.
    expect(screen.getByRole("button", { name: "Leave group" }).hasAttribute("disabled")).toBe(
      false,
    );
  });
});
