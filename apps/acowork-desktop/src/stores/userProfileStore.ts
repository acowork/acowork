import { create } from "zustand";
import type { AvatarType, BoringAvatarVariant, ColorPalette, UserProfile } from "../lib/types";
import { COLOR_PALETTES } from "../lib/types";
import { pickRandomBuiltinIconId } from "../lib/avatar";
import { readPref, writePref, onScopeChange } from "../lib/prefsScope";
import i18n from "../i18n";

/** Logical key, not the final localStorage name: the avatar presentation
 *  choices (variant / palette / icon) are user-level, so `prefsScope`
 *  namespaces them per account. The identity fields are overwritten from the
 *  backend account on every resolve (`authStore.app`), but these are
 *  client-only and used to be shared by every account on the machine. */
const STORAGE_KEY = "acowork-user-profile";

// ── Defaults ───────────────────────────────────────────────────────────

const DEFAULT_PROFILE: UserProfile = {
  displayName: i18n.t("common.me"),
  avatarType: "icon",
  avatarVariant: "beam",
  avatarSeed: "user",
  avatarIcon: null,
  colorPalette: "rainbow",
  avatarColors: [],
  backendAvatarUrl: null,
  backendBuiltinAvatarId: null,
};

// ── Persistence helpers ────────────────────────────────────────────────

function loadProfile(): UserProfile {
  try {
    const raw = readPref(STORAGE_KEY);
    if (raw) {
      const parsed = JSON.parse(raw) as Partial<UserProfile>;
      return {
        displayName: parsed.displayName ?? DEFAULT_PROFILE.displayName,
        avatarType: validateAvatarType(parsed.avatarType),
        avatarVariant: validateVariant(parsed.avatarVariant),
        avatarSeed: parsed.avatarSeed ?? DEFAULT_PROFILE.avatarSeed,
        avatarIcon: parsed.avatarIcon ?? DEFAULT_PROFILE.avatarIcon,
        colorPalette: validatePalette(parsed.colorPalette),
        avatarColors: Array.isArray(parsed.avatarColors) ? parsed.avatarColors : [],
        backendAvatarUrl: parsed.backendAvatarUrl ?? null,
        backendBuiltinAvatarId: parsed.backendBuiltinAvatarId ?? null,
      };
    }
  } catch {
    // localStorage unavailable or corrupted; use defaults
  }
  // No saved profile — pick a random builtin icon so the user shows a
  // real avatar from the very first session, even if onboarding is skipped.
  return {
    ...DEFAULT_PROFILE,
    avatarIcon: pickRandomBuiltinIconId(),
    backendAvatarUrl: null,
    backendBuiltinAvatarId: null,
  };
}

function saveProfile(profile: UserProfile) {
  writePref(STORAGE_KEY, JSON.stringify(profile));
}

function validateAvatarType(v?: unknown): AvatarType {
  if (v === "boring" || v === "icon" || v === "letter") return v;
  return "icon";
}

function validateVariant(v?: unknown): BoringAvatarVariant {
  const valid = ["beam", "marble", "pixel", "sunset", "ring", "bauhaus"];
  if (typeof v === "string" && valid.includes(v)) return v as BoringAvatarVariant;
  return "beam";
}

function validatePalette(v?: unknown): ColorPalette {
  const valid: ColorPalette[] = ["rainbow", "ocean", "forest", "sunset", "neon"];
  if (typeof v === "string" && valid.includes(v as ColorPalette)) return v as ColorPalette;
  return "rainbow";
}

// ── Store ──────────────────────────────────────────────────────────────

interface UserProfileState {
  profile: UserProfile;
  /** Update profile partially and persist */
  setProfile: (partial: Partial<UserProfile>) => void;
  /** Get effective colors (custom or palette default) */
  getColors: () => string[];
  /** Reset to defaults */
  resetProfile: () => void;
  /**
   * Idempotently assign a random builtin avatar to the user profile if it
   * doesn't already have one. Called from the onboarding completion hook so
   * the freshly-onboarded user gets a builtin icon instead of the legacy
   * letter/gradient fallback. Skips if `avatarIcon` is already set.
   */
  assignRandomAvatarIfMissing: () => void;
}

export const useUserProfileStore = create<UserProfileState>((set, get) => ({
  profile: loadProfile(),

  setProfile: (partial) => {
    const next = { ...get().profile, ...partial };
    saveProfile(next);
    set({ profile: next });
  },

  getColors: () => {
    const p = get().profile;
    if (p.avatarColors.length > 0) return p.avatarColors;
    return COLOR_PALETTES[p.colorPalette] ?? COLOR_PALETTES.rainbow;
  },

  resetProfile: () => {
    const next = { ...DEFAULT_PROFILE };
    saveProfile(next);
    set({ profile: next });
  },

  assignRandomAvatarIfMissing: () => {
    const current = get().profile;
    if (current.avatarIcon) return;
    const iconId = pickRandomBuiltinIconId();
    if (!iconId) return;
    const next: UserProfile = { ...current, avatarIcon: iconId };
    saveProfile(next);
    set({ profile: next });
  },
}));

// A login that does not reload the page leaves this store holding the
// previous scope's avatar presentation. `authStore.app` then overwrites the
// identity fields from the account, so only the client-only choices need this.
onScopeChange(() => useUserProfileStore.setState({ profile: loadProfile() }));
