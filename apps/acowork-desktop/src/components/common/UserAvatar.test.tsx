/**
 * ADR-076 §决策 4 / ADR-017: the `builtinAvatarId` tri-state contract.
 *
 * `null` = "this is somebody else and they have no builtin avatar" — the
 * viewer's own profile icon must NEVER stand in (regression t-9764c5fa, the
 * bug the initiator refactor exists to kill). `undefined` = "this is me /
 * unknown" — the profile icon may resolve.
 *
 * The contract lives in the leaf (`UserAvatar`), not in the callers: a
 * bubble coalescing `null ?? undefined` re-breaks it while `tsc` stays green
 * — which is exactly how the regression slipped through once.
 */
import { describe, expect, it, beforeEach } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { UserAvatar } from "./UserAvatar";
import { useUserProfileStore } from "../../stores/userProfileStore";
import { pickDeterministicBuiltinIconId } from "../../lib/avatar";

beforeEach(() => {
  cleanup();
  // The logged-in viewer picked "icon-07"; it must not leak onto others.
  useUserProfileStore.setState({
    profile: { ...useUserProfileStore.getState().profile, avatarIcon: "icon-07" },
  });
});

describe("UserAvatar builtinAvatarId tri-state", () => {
  it("renders the viewer's own profile icon when undefined (self/unknown)", () => {
    render(<UserAvatar displayName="Me" builtinAvatarId={undefined} />);
    expect(screen.getByRole("img").getAttribute("alt")).toBe("icon-07");
  });

  it("never falls back to the profile icon when null (another person)", () => {
    render(<UserAvatar displayName="Bob" builtinAvatarId={null} />);
    const alt = screen.getByRole("img").getAttribute("alt");
    expect(alt).not.toBe("icon-07");
    expect(alt).toBe(pickDeterministicBuiltinIconId("Bob"));
  });

  it("renders the explicit builtin avatar for another person when known", () => {
    render(<UserAvatar displayName="Bob" builtinAvatarId="icon-03" />);
    expect(screen.getByRole("img").getAttribute("alt")).toBe("icon-03");
  });
});
