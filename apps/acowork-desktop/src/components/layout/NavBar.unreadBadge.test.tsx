/**
 * Nav unread dot for the inbox (ADR-076 §决策 8).
 *
 * The dot is the one piece of the ADR that a UI refactor deleted outright:
 * when the full-screen `MessagesView` became the conditional `InboxPanel`
 * pane, the poll's host moved with the JSX and the nav badge — which was
 * specified to live on `NavBar` — disappeared along with it.
 *
 * Pinned here because it is the only inbox signal that must stay live while
 * the user is NOT looking at the inbox: it is what tells them to go look.
 */
import { describe, it, expect, beforeEach, vi } from "vitest";
import { render, screen } from "@testing-library/react";

import { NavBar } from "./NavBar";
import { useUserChatStore } from "../../stores/userChatStore";

vi.mock("../account/AccountMenu", () => ({
  AccountMenu: () => <div data-testid="account-menu" />,
}));

const PEER = "aaaaaaaa-0000-4000-8000-000000000001";

function seedChats(...unread: number[]) {
  useUserChatStore.setState({
    chats: unread.map((n, i) => ({
      chat_id: `${PEER}__${i}`,
      peer_user_id: `${PEER}${i}`,
      peer_display_name: `Peer ${i}`,
      last_active_at: 100,
      last_message_preview: "hi",
      unread_count: n,
    })),
  });
}

describe("NavBar inbox unread dot", () => {
  beforeEach(() => {
    useUserChatStore.setState({ chats: [], activePeerId: null, messages: [] });
  });

  it("shows nothing while the inbox is clean", () => {
    seedChats(0, 0);
    render(<NavBar currentView="chat" onViewChange={() => {}} onAvatarClick={() => {}} />);
    expect(screen.queryByTestId("nav-badge")).toBeNull();
  });

  it("shows one dot for the total across conversations", () => {
    seedChats(1, 2);
    render(<NavBar currentView="chat" onViewChange={() => {}} onAvatarClick={() => {}} />);
    expect(screen.getAllByTestId("nav-badge")).toHaveLength(1);
  });

  it("stays on the chat tile when another view is selected", () => {
    // The dot's whole purpose is the case where the user is elsewhere — the
    // inbox pane is not mounted there, and now neither is the poll.
    seedChats(3);
    render(<NavBar currentView="settings" onViewChange={() => {}} onAvatarClick={() => {}} />);
    expect(screen.getByTestId("nav-badge")).toBeTruthy();
  });
});
