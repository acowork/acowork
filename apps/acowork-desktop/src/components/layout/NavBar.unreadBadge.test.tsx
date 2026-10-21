/**
 * Nav unread dot for the unified chat surface.
 *
 * The dot is the one piece of ADR-076 §决策 8 that a UI refactor deleted
 * outright: when the full-screen `MessagesView` became a conditional pane the
 * poll's host moved with the JSX and the nav badge — specified to live on
 * `NavBar` — disappeared along with it.
 *
 * The badge now reads the total unread across the caller's `user_user` DM
 * sessions AND project groups — both are `chat_sessions`, so the nav dot and
 * the sidebar rows share one source of truth (`projectGroupStore`). Pinned
 * here because it is the only chat signal that must stay live while the user
 * is NOT looking at a conversation: it is what tells them to go look.
 */
import { describe, it, expect, beforeEach, vi } from "vitest";
import { render, screen } from "@testing-library/react";

import { NavBar } from "./NavBar";
import { useProjectGroupStore } from "../../stores/projectGroupStore";
import { useAuthStore } from "../../stores/authStore";
import type { UserAccount } from "../../lib/types";

vi.mock("../account/AccountMenu", () => ({
  AccountMenu: () => <div data-testid="account-menu" />,
}));

const ME = "me-0000-4000-8000-000000000000";

const self: UserAccount = {
  user_id: ME,
  username: "me",
  display_name: "Me",
  role: "user",
  language: "en",
  timezone: "UTC",
  created_at: "2025-01-01T00:00:00Z",
  updated_at: "2025-01-01T00:00:00Z",
};

/** Seed `unread[me]` on one DM session per argument. */
function seedDms(...unread: number[]) {
  useProjectGroupStore.setState({
    activeGroupId: null,
    groups: [],
    dms: unread.map((n, i) => ({
      session_id: `dm-${i}`,
      kind: "user_user",
      title: "",
      created_at: 0,
      last_active_at: 100,
      message_count: 1,
      participants: [{ id: ME }, { id: `peer-${i}` }],
      unread: { [ME]: n },
    })),
  });
}

describe("NavBar chat unread dot", () => {
  beforeEach(() => {
    useAuthStore.setState({
      mode: "multi_user",
      status: "logged_in",
      account: self,
      accessToken: "token",
      viewAsUserId: null,
      registrationOpen: false,
    });
    useProjectGroupStore.setState({ groups: [], dms: [], activeGroupId: null });
  });

  it("shows nothing while every chat is clean", () => {
    seedDms(0, 0);
    render(<NavBar currentView="chat" onViewChange={() => {}} onAvatarClick={() => {}} />);
    expect(screen.queryByTestId("nav-badge")).toBeNull();
  });

  it("shows one dot for the total across conversations", () => {
    seedDms(1, 2);
    render(<NavBar currentView="chat" onViewChange={() => {}} onAvatarClick={() => {}} />);
    expect(screen.getAllByTestId("nav-badge")).toHaveLength(1);
  });

  it("counts project-group unread, not only DMs", () => {
    // Every DM is clean but a project group has unread — the dot is the
    // unified total, so it must still light up.
    seedDms(0);
    useProjectGroupStore.setState((s) => ({
      groups: [
        ...s.groups,
        {
          session_id: "g-1",
          kind: "project_group",
          title: "Team",
          project_id: "p-1",
          created_at: 0,
          last_active_at: 100,
          message_count: 1,
          participants: [{ id: ME }],
          unread: { [ME]: 2 },
        },
      ],
    }));
    render(<NavBar currentView="chat" onViewChange={() => {}} onAvatarClick={() => {}} />);
    expect(screen.getByTestId("nav-badge")).toBeTruthy();
  });

  it("stays on the chat tile when another view is selected", () => {
    // The dot's whole purpose is the case where the user is elsewhere — the
    // chat pane is not mounted there, so the badge is their only nudge.
    seedDms(3);
    render(<NavBar currentView="settings" onViewChange={() => {}} onAvatarClick={() => {}} />);
    expect(screen.getByTestId("nav-badge")).toBeTruthy();
  });
});
