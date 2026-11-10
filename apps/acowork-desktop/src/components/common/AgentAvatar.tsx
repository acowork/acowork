import { useMemo, useState } from "react";
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

// ── In-hand avatar (no fetch) ───────────────────────────────────────────

/**
 * ADR-017's resolution chain for a caller that **already holds the bytes**:
 * the image itself, then the manifest's builtin id, then the deterministic icon
 * keyed by `agentId`.
 *
 * `AgentAvatar` covers the installed-agent case, where the avatar is a path
 * served over HTTP and the bytes need the bearer token (`AuthedImage`); it takes
 * a URL, so it cannot be used for an inlined `data:` URL. What is shared here is
 * the *policy* — one place that knows a broken image degrades to the builtin
 * icon and then to the deterministic one, rather than each surface inventing its
 * own fallback (the Local tab used to fall straight to a letter block, which is
 * how a package with `builtin_avatar` ended up looking avatar-less).
 *
 * `agentId` is required: the deterministic icon is keyed by it, and a caller
 * without one (an unreadable file) should render its own placeholder instead of
 * pretending to know whose icon this is.
 */
export function InlinedAgentAvatar({
  agentId,
  dataUrl,
  builtinAvatarId,
  size = 32,
  className,
}: {
  agentId: string;
  /** `data:` URL of an image already in memory, or `null` when none is declared. */
  dataUrl?: string | null;
  /** Builtin icon id from the manifest, e.g. `icon-15`. */
  builtinAvatarId?: string | null;
  size?: number;
  className?: string;
}) {
  // A corrupt/undecodable image must fall through to the next step of the
  // chain, not render a broken-image box.
  const [failed, setFailed] = useState(false);

  if (dataUrl && !failed) {
    return (
      <img
        src={dataUrl}
        alt={agentId}
        draggable={false}
        onError={() => setFailed(true)}
        className={`rounded-lg object-cover ring-1 ring-zinc-300/60 dark:ring-zinc-600/60 ${className ?? ""}`}
        style={{ width: size, height: size }}
      />
    );
  }

  if (builtinAvatarId && BUILTIN_ICONS[builtinAvatarId]) {
    return <BuiltinIconAvatar iconId={builtinAvatarId} size={size} className={className} />;
  }

  return <DeterministicBuiltinAvatar seed={agentId} size={size} className={className} />;
}
