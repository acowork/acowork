import { useMemo } from "react";
import { BUILTIN_ICONS } from "./UserAvatar";
import { AuthedImage } from "./AuthedImage";
import {
  pickDeterministicBuiltinIconId,
  resolveAgentAvatarFileUrl,
} from "../../lib/avatar";

// ── Built-in icon wrapper ────────────────────────────────────────────────

function BuiltinIconAvatar({ iconId, size, className }: { iconId: string; size: number; className?: string }) {
  const src = BUILTIN_ICONS[iconId] ?? BUILTIN_ICONS["icon-01"];
  return (
    <img
      src={src}
      alt={iconId}
      draggable={false}
      className={`rounded-lg object-cover ring-1 ring-zinc-300/60 dark:ring-zinc-600/60 ${className ?? ""}`}
      style={{ width: size, height: size }}
    />
  );
}

// ── Public component ────────────────────────────────────────────────────

export interface AgentAvatarProps {
  /** Agent identifier — used as seed for deterministic avatar generation */
  agentId: string;
  /** Display name (fallback for letter avatar) */
  displayName?: string;
  /**
   * Effective custom avatar path (from agent_config.json or manifest).
   * When set, the avatar is fetched via the avatar-file endpoint.
   * Null means no custom avatar is configured.
   */
  avatarUrl?: string | null;
  /**
   * Manifest version (semver). Appended as `?v=<version>` to bust the HTTP
   * cache when the package is re-installed with a new version.
   */
  version?: string | null;
  /**
   * Effective builtin avatar icon ID from the API (e.g. "icon-02").
   * Used when avatarUrl is null. Falls back to deterministic random if absent.
   */
  builtinAvatarId?: string | null;
  /** Size in pixels */
  size?: number;
  /** Additional CSS classes */
  className?: string;
}

export function AgentAvatar({
  agentId,
  avatarUrl,
  version,
  builtinAvatarId,
  size = 32,
  className,
}: AgentAvatarProps) {
  // ADR-017: Avatar resolution priority:
  //
  // 1. Effective custom avatar path (from config or manifest) — rendered
  //    via the avatar-file endpoint.
  // 2. Effective builtin avatar ID (from config or manifest).
  // 3. Deterministic random builtin icon — the final fallback.
  if (avatarUrl) {
    return (
      <CustomAgentAvatar
        agentId={agentId}
        avatarPath={avatarUrl}
        version={version}
        fallbackSeed={agentId}
        size={size}
        className={className}
      />
    );
  }

  if (builtinAvatarId && BUILTIN_ICONS[builtinAvatarId]) {
    return <BuiltinIconAvatar iconId={builtinAvatarId} size={size} className={className} />;
  }

  return <DeterministicBuiltinAvatar seed={agentId} size={size} className={className} />;
}

// ── Internal: custom avatar (from config or manifest path) ───────────────

function CustomAgentAvatar({
  agentId,
  avatarPath,
  version,
  fallbackSeed,
  size,
  className,
}: {
  agentId: string;
  avatarPath: string;
  version?: string | null;
  fallbackSeed: string;
  size: number;
  className?: string;
}) {
  // Build the avatar-file URL. The `version` parameter is appended for
  // cache busting when the package is re-installed.
  const url = useMemo(() => {
    const base = resolveAgentAvatarFileUrl(agentId, avatarPath);
    return version ? `${base}&v=${encodeURIComponent(version)}` : base;
  }, [agentId, avatarPath, version]);

  // The bytes need the bearer token, so they are fetched rather than
  // handed to `<img src>` directly (see `AuthedImage`). A failed fetch
  // falls back to a deterministic random builtin icon.
  return (
    <AuthedImage
      src={url}
      alt={agentId}
      className={`rounded-lg object-cover ring-1 ring-zinc-300/60 dark:ring-zinc-600/60 ${className ?? ""}`}
      style={{ width: size, height: size }}
      fallback={
        <DeterministicBuiltinAvatar seed={fallbackSeed} size={size} className={className} />
      }
    />
  );
}

// ── Internal: deterministic builtin icon (final fallback) ────────────────

function DeterministicBuiltinAvatar({
  seed,
  size,
  className,
}: {
  seed: string;
  size: number;
  className?: string;
}) {
  const fallbackIconId = useMemo(() => pickDeterministicBuiltinIconId(seed), [seed]);

  if (!fallbackIconId) {
    return <BuiltinIconAvatar iconId="icon-01" size={size} className={className} />;
  }
  return <BuiltinIconAvatar iconId={fallbackIconId} size={size} className={className} />;
}
