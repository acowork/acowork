/**
 * Profile tab — the avatar must not flash on entry.
 *
 * The bug: the tab kept its own copy of the avatar config, and that copy
 * started out `null`. The avatar therefore rendered a BUILTIN icon on the
 * first paint and only swapped to the custom avatar once
 * `GET /api/user/avatar-config` had resolved — on every visit to the page.
 * The avatar now renders from the persisted store value (mirrored from the
 * account at login, see `authStore.app`), so the first paint is already the
 * custom avatar.
 *
 * The bytes themselves are cached per URL by `AuthedImage`; the nav bar's
 * account avatar warms that cache at app start, so the profile tab paints
 * the image in its very first frame. The test reproduces that warm cache.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, render, waitFor } from "@testing-library/react";

import { ProfileTab } from "./ProfileTab";
import { AuthedImage, clearAuthedImageCache } from "../common/AuthedImage";
import { useUserProfileStore } from "../../stores/userProfileStore";
import { useAuthStore } from "../../stores/authStore";
import { resolveUserAvatarFileUrl } from "../../lib/avatar";
import type { UserAccount } from "../../lib/types";

const AVATAR_PATH = "assets/avatars/u-1/avatar-01.png";

const h = vi.hoisted(() => ({ configCalls: 0 }));

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: vi.fn() }));

// The avatar config request never resolves: the point of the test is the
// FIRST paint, i.e. the state the page is in while that request is still
// in flight.
vi.mock("../../lib/avatar", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../lib/avatar")>();
  return {
    ...actual,
    fetchUserAvatarConfig: () => {
      h.configCalls += 1;
      return new Promise(() => {});
    },
    fetchUserAvatarAssets: async () => ({ assets: [] }),
    updateUserAvatarConfig: async () => ({ avatar: null, builtin_avatar: null }),
    deleteUserAvatarFile: async () => {},
  };
});

vi.mock("../../lib/selfProfile", () => ({
  fetchSelfProfile: () => new Promise(() => {}),
  updateSelfProfile: async () => ({}),
}));

let originalFetch: typeof globalThis.fetch;
let originalCreate: typeof URL.createObjectURL;
let originalRevoke: typeof URL.revokeObjectURL;

beforeEach(() => {
  cleanup();
  clearAuthedImageCache();

  originalFetch = globalThis.fetch;
  originalCreate = URL.createObjectURL;
  originalRevoke = URL.revokeObjectURL;
  globalThis.fetch = vi.fn(async () => ({
    ok: true,
    status: 200,
    blob: async () => new Blob(["bytes"]),
  })) as unknown as typeof globalThis.fetch;
  URL.createObjectURL = vi.fn(() => "blob:avatar");
  URL.revokeObjectURL = vi.fn();

  // What the store holds after a login: the custom avatar path plus the
  // locally picked builtin icon that the old code fell back to.
  useUserProfileStore.setState({
    profile: {
      ...useUserProfileStore.getState().profile,
      displayName: "Tester",
      avatarIcon: "icon-07",
      backendAvatarUrl: AVATAR_PATH,
      backendBuiltinAvatarId: null,
    },
  });
  useAuthStore.setState({
    status: "logged_in",
    account: { user_id: "u-1" } as unknown as UserAccount,
  });
});

afterEach(() => {
  cleanup();
  clearAuthedImageCache();
  globalThis.fetch = originalFetch;
  URL.createObjectURL = originalCreate;
  URL.revokeObjectURL = originalRevoke;
});

describe("ProfileTab avatar", () => {
  it("paints the custom avatar on the first frame, not a builtin icon", async () => {
    // The nav bar's account avatar already fetched these bytes at app
    // start — same URL, same `AuthedImage` cache entry.
    let warm!: ReturnType<typeof render>;
    await act(async () => {
      warm = render(<AuthedImage src={resolveUserAvatarFileUrl(AVATAR_PATH)} alt="warm" />);
    });
    await waitFor(() => expect(warm.container.querySelector("img")).toBeTruthy());
    warm.unmount();

    let view!: ReturnType<typeof render>;
    await act(async () => {
      view = render(<ProfileTab />);
    });
    const { container } = view;

    // The config request is still in flight — the avatar is right anyway.
    expect(h.configCalls).toBe(1);
    expect(container.querySelector('img[alt="User avatar"]')).toBeTruthy();
    expect(container.querySelector('img[alt="icon-07"]')).toBeNull();
    // The ID line is on the first frame too, so the card does not grow
    // (a growing card is the second half of the "page flashes" report).
    expect(container.textContent).toContain("u-1");
  });
});
