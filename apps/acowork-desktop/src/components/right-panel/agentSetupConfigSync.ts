import type { AgentProfileSettings } from "../../stores/agentStore";

/**
 * Wire shape of `GET /api/agents/{id}/config` (proxy through Gateway →
 * `GET /agents/{id}/config` on Runtime, see
 * `core/acowork-runtime/src/http/server.rs::get_agent_config`).
 *
 * The runtime returns a nested envelope
 * `{ agent_id, matches, config: <AgentConfig>, manifest_path, work_dir,
 *   model, provider }` — the Setup panel cares about the flattened
 * `config` fields plus the active model/provider hint.
 *
 * Kept structural (no full type) because the wire is partial:
 * `AgentConfig` uses `skip_serializing_if = "Option::is_none"` on every
 * field, so absent ≠ null, and the panel treats both as "no opinion".
 */
export interface ServerConfigEnvelope {
  agent_id?: string;
  matches?: boolean;
  config?: {
    max_output_tokens?: number | null;
    max_iterations?: number | null;
    max_sessions?: number | null;
    temperature?: number | null;
    context_window?: number | null;
    shell_approval_threshold?: string | null;
    approval_timeout_secs?: number | null;
    compression_ratio_threshold?: number | null;
    /**
     * ADR-094 note: the per-agent `skills_auto_load` field was
     * removed from the wire. Skill mode is now driven entirely by
     * the `load_skill` tool entry in `agent_tools.json` — the
     * Tools panel keeps the two surfaces in lockstep so a separate
     * "auto-load skills" switch would only re-introduce the drift
     * that this helper was extracted to prevent.
     */
    session_language?: string | null;
    /** Design 27 §2.1: T1 idle-rotation threshold in hours. `0` is a
     *  meaningful value (disables T1 rotation), so it must be passed
     *  through, not treated as "absent". */
    intent_idle_rotate_hours?: number | null;
    /** Design 27 §2.1: T2 handoff target token cap. */
    intent_handoff_max_tokens?: number | null;
  } | null;
  model?: string | null;
  provider?: string | null;
}

/**
 * Build the `Partial<AgentProfileSettings>` patch that the Agent Setup
 * panel should apply when the runtime returns a fresh
 * `GET /agents/{id}/config` response (mount-time fetch + the
 * `acowork:refresh-agent-config` event after a successful PUT).
 *
 * Race-safe merge (ADR-052 follow-up): only overwrite each local
 * profile field when the server returned a concrete value. Every
 * `AgentConfig` field uses `skip_serializing_if = "Option::is_none"`
 * on the runtime side, so the response represents "no opinion" as
 * either a missing field or a JSON-null — both are passed through
 * unchanged. The old `?? undefined` / `?? <default>` shape silently
 * clobbered a value the user typed into a number input or clicked on
 * the Switch before this GET returned, dropping their intent before
 * the next Apply could persist it (the wider pattern behind the
 * original switch-clobber bug — see `saveField` in AgentSetupTab).
 *
 * Returning an empty patch is fine: callers `setProfile` with it and
 * nothing changes (no-op).
 *
 * Single source of truth for the panel-init path. Both the mount
 * `useEffect` and the refresh-event listener in `AgentSetupTab` call
 * this — do NOT inline another cfg-cast + patch-build block, the
 * drift between two such blocks already bit us once
 * (compressionRatioThreshold missing in the refresh path).
 */
export function buildProfilePatchFromServerConfig(
  data: ServerConfigEnvelope | null | undefined,
): Partial<AgentProfileSettings> {
  const patch: Partial<AgentProfileSettings> = {};
  if (!data) return patch;

  // `activeModel` / `activeProvider` are always taken when present —
  // they are display-only hints, not user-editable, and a missing
  // value here just means "the runtime hasn't bound one yet".
  // Type is `string | undefined` (no `null` — see AgentProfileSettings
  // + the normalize step in agentStore), so coerce JSON-null →
  // undefined here.
  if (data.model !== undefined) patch.activeModel = data.model ?? undefined;
  if (data.provider !== undefined)
    patch.activeProvider = data.provider ?? undefined;

  const cfg = data.config ?? {};
  if (typeof cfg.max_output_tokens === "number") {
    patch.maxTokens = cfg.max_output_tokens;
  }
  if (typeof cfg.max_iterations === "number") {
    patch.maxIterations = cfg.max_iterations;
  }
  if (typeof cfg.max_sessions === "number") {
    patch.maxSessions = cfg.max_sessions;
  }
  if (typeof cfg.temperature === "number") {
    patch.temperature = cfg.temperature;
  }
  if (typeof cfg.context_window === "number") {
    patch.contextWindow = cfg.context_window;
  }
  if (typeof cfg.shell_approval_threshold === "string") {
    patch.shellApprovalThreshold = cfg.shell_approval_threshold;
  }
  if (typeof cfg.approval_timeout_secs === "number") {
    patch.approvalTimeoutSecs = cfg.approval_timeout_secs;
  }
  if (typeof cfg.compression_ratio_threshold === "number") {
    patch.compressionRatioThreshold = cfg.compression_ratio_threshold;
  }
  // ADR-094 note: `skills_auto_load` was removed from the wire. The
  // `load_skill` tool entry in `agent_tools.json` is the single
  // source of truth for skill-mode (Progressive / Manual) now.
  // Intent rotation knobs (design 27 §2.1). `0` is a meaningful
  // idle-hours value (it disables T1 rotation), so guard on
  // `typeof === "number"` only — never on `> 0`.
  if (typeof cfg.intent_idle_rotate_hours === "number") {
    patch.intentIdleRotateHours = cfg.intent_idle_rotate_hours;
  }
  if (typeof cfg.intent_handoff_max_tokens === "number") {
    patch.intentHandoffMaxTokens = cfg.intent_handoff_max_tokens;
  }
  // session_language is string-or-absent — preserve explicit `null`
  // (clear) AND concrete value, leave absent alone (preserves an
  // optimistic local edit while in-flight).
  if (cfg.session_language !== undefined) {
    patch.sessionLanguage = cfg.session_language;
  }
  return patch;
}