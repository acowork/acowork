/**
 * Nav artwork hover state.
 *
 * The rail's affordance used to be the hover BACKGROUND only: the icon stayed
 * grey while the tile tinted, and on a rail that is mostly grey artwork the
 * pointer gave no sign that the tile was live. The fix drops the grayscale
 * filter on hover (`group-hover:grayscale-0`), so the icon comes back to
 * colour with the pointer.
 *
 * What this pins:
 *   1. an INACTIVE tile's artwork is grey by default;
 *   2. that same artwork carries the `group-hover:grayscale-0` rule, so the
 *      hover un-greys it;
 *   3. an ACTIVE tile's artwork is NOT grey (and does not carry the hover
 *      rule — it is already in colour, so there is nothing to restore).
 *
 * The rule is asserted on the class list because the effect is a CSS
 * filter: jsdom does not compute `grayscale()`, so a computed-style
 * assertion would pass on a broken rule.
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

/** Every artwork <img> in the rail, in DOM order. */
function artwork(): HTMLImageElement[] {
  return Array.from(document.querySelectorAll("nav img"));
}

describe("NavBar artwork hover state", () => {
  beforeEach(() => {
    useAuthStore.setState({
      accountState: "ready",
      status: "logged_in",
      account: self,
      accessToken: "token",
      viewAsUserId: null,
      registrationOpen: false,
    });
    useProjectGroupStore.setState({ groups: [], dms: [], activeGroupId: null });
  });

  it("greys an inactive artwork and un-greys it on hover", () => {
    render(<NavBar currentView="chat" onViewChange={() => {}} onAvatarClick={() => {}} />);
    // The chat tile is active here, so the INACTIVE artwork is one of the
    // others (projects / docs / gallery / harness / settings).
    const inactive = artwork().find((img) => img.className.includes("grayscale"));
    expect(inactive, "an inactive artwork tile must be greyed").toBeTruthy();
    expect(inactive!.className).toContain("group-hover:grayscale-0");
  });

  it("keeps the active artwork in colour with no hover rule", () => {
    render(<NavBar currentView="chat" onViewChange={() => {}} onAvatarClick={() => {}} />);
    const active = artwork().find((img) => !img.className.includes("grayscale"));
    expect(active, "the active artwork must not be greyed").toBeTruthy();
    expect(active!.className).not.toContain("group-hover:grayscale-0");
  });

  it("puts the group hook on the tile so the artwork can react to hover", () => {
    // `group-hover:` only fires if an ancestor carries `group`. The tile is
    // that ancestor — without it the hover rule is inert.
    render(<NavBar currentView="chat" onViewChange={() => {}} onAvatarClick={() => {}} />);
    // The artwork is `alt=""`, so the tile has no accessible name of its own
    // (the tooltip carries the label) — match the tile structurally: the
    // button that WRAPS an artwork image.
    const tile = document.querySelector("nav button:has(img)");
    expect(tile, "a nav tile must wrap an artwork image").toBeTruthy();
    expect((tile as HTMLElement).className).toContain("group");
  });
});
