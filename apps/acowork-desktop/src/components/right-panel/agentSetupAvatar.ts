/**
 * Effective-avatar derivation for the Agent Setup panel header.
 *
 * ## The bug this exists to prevent
 *
 * The panel header used to read the avatar **only** from the locally-held
 * `avatarConfig` response, which starts as `null` on mount and is filled by
 * an async `GET /agents/{id}/avatar-config`. Until that round-trip landed,
 * `avatarUrl` and `builtinAvatarId` were both `null`, so `AgentAvatar`
 * fell through to `DeterministicBuiltinAvatar` and painted a builtin icon.
 * One network hop later the real avatar replaced it — the "flash".
 *
 * Two facts make that unnecessary:
 *
 * 1. The agent store ALREADY holds the effective avatar synchronously.
 *    `AgentInfo.avatar` / `AgentInfo.builtin_avatar` come from
 *    `GET /api/agents`, and the Gateway resolves them with the *same*
 *    override-first rule the Runtime's avatar-config endpoint uses
 *    (`core/acowork-gateway/src/http/agents.rs::list_agents` →
 *    `overrides_of()` → else `resolve_avatar_from_manifest()`). The
 *    sidebar (`AgentList.tsx`) renders from exactly these fields and has
 *    never flashed, because it never waits on a second request.
 *
 * 2. The async `avatarConfig` is still the authoritative source — it also
 *    carries `display_name` / `source`, and it is what the picker's
 *    "currently selected" markers read. Dropping it would trade a flash
 *    for a staleness bug whenever an override lands out-of-band.
 *
 * So the two are not in conflict: the store is the **synchronous seed**,
 * the response is the **refinement**. `null` for the response means
 * "not loaded yet", which is exactly when the seed must win.
 */

/** Minimal shape this helper needs from `AgentInfo`. */
export interface AvatarSeed {
  /** Effective custom avatar path, or undefined when the agent has none. */
  avatar?: string;
  /** Effective builtin avatar icon ID, or undefined. */
  builtin_avatar?: string;
}

/**
 * Resolve the avatar the panel header should paint.
 *
 * `config` is the `GET /agents/{id}/avatar-config` response, or `null`
 * while it is still in flight (or if it failed — in which case the seed
 * is all we have and is still correct).
 *
 * Once `config` is present it wins outright, including when its fields are
 * `null`: the server answering "no custom avatar, no builtin icon" is a
 * real answer that must not be papered over with a stale seed. Falling
 * back per-field would resurrect a just-cleared avatar.
 */
export function resolveEffectiveAvatar(
  seed: AvatarSeed | null | undefined,
  config: { avatar: string | null; builtin_avatar: string | null } | null,
): { avatarUrl: string | null; builtinAvatarId: string | null } {
  if (config) {
    return { avatarUrl: config.avatar, builtinAvatarId: config.builtin_avatar };
  }
  return {
    avatarUrl: seed?.avatar ?? null,
    builtinAvatarId: seed?.builtin_avatar ?? null,
  };
}