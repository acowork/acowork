/**
 * Regression lock-down for the setup-panel avatar flash.
 *
 * The panel header used to read the avatar exclusively from the async
 * `GET /agents/{id}/avatar-config` response, which is `null` on mount. That
 * made `AgentAvatar` fall through to `DeterministicBuiltinAvatar` and paint
 * a builtin icon for one round-trip before swapping to the real avatar.
 *
 * These tests pin the resolution ORDER that removes it: the store's
 * already-effective `AgentInfo` seeds the first paint, and the response
 * refines it once loaded.
 */
import { describe, it, expect } from "vitest";
import { resolveEffectiveAvatar } from "./agentSetupAvatar";

describe("resolveEffectiveAvatar", () => {
  it("seeds from the store before the config response arrives", () => {
    // The exact flash state: component mounted, fetch still in flight.
    expect(
      resolveEffectiveAvatar(
        { avatar: "assets/avatar-01.png", builtin_avatar: "icon-05" },
        null,
      ),
    ).toEqual({ avatarUrl: "assets/avatar-01.png", builtinAvatarId: "icon-05" });
  });

  it("seeds a builtin icon so no icon flash precedes it", () => {
    expect(
      resolveEffectiveAvatar({ builtin_avatar: "icon-03" }, null),
    ).toEqual({ avatarUrl: null, builtinAvatarId: "icon-03" });
  });

  it("lets the loaded response refine the seed", () => {
    expect(
      resolveEffectiveAvatar(
        { avatar: "assets/avatar-01.png" },
        { avatar: "assets/avatar-02.png", builtin_avatar: "icon-07" },
      ),
    ).toEqual({ avatarUrl: "assets/avatar-02.png", builtinAvatarId: "icon-07" });
  });

  it("honours a loaded response that CLEARS the avatar (no stale seed)", () => {
    // The server answering "no avatar" is a real answer. Falling back
    // per-field would resurrect the avatar the user just removed, which
    // is the mirror-image flash.
    expect(
      resolveEffectiveAvatar(
        { avatar: "assets/avatar-01.png", builtin_avatar: "icon-05" },
        { avatar: null, builtin_avatar: null },
      ),
    ).toEqual({ avatarUrl: null, builtinAvatarId: null });
  });

  it("yields no opinion when neither source has an avatar", () => {
    expect(resolveEffectiveAvatar({}, null)).toEqual({
      avatarUrl: null,
      builtinAvatarId: null,
    });
    expect(resolveEffectiveAvatar(null, null)).toEqual({
      avatarUrl: null,
      builtinAvatarId: null,
    });
  });
});