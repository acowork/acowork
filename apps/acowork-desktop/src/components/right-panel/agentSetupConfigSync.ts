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
    /** ADR-099 Chat Router block. The same skip-serializing rule
     *  applies per sub-field — absent (or the whole block absent)
     *  means "no opinion", not "cleared". */
    chat_router?: {
      enabled?: boolean | null;
      model?: string | null;
      provider?: string | null;
      timeout_ms?: number | null;
    } | null;
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
  // session_language is string-or-absent — preserve explicit `null`
  // (clear) AND concrete value, leave absent alone (preserves an
  // optimistic local edit while in-flight).
  if (cfg.session_language !== undefined) {
    patch.sessionLanguage = cfg.session_language;
  }
  // ADR-099 Chat Router sub-fields. Race-safe like every other field:
  // only overwrite when the server returned a concrete value. An
  // explicit clear (`""` on the PUT wire → `None` on disk) drops the
  // key from the GET response, and the local `""`/undefined optimistic
  // value already renders the "auto" option — so no clobbering is
  // needed to keep the two in agreement.
  const router = cfg.chat_router;
  if (router) {
    if (typeof router.enabled === "boolean") {
      patch.chatRouterEnabled = router.enabled;
    }
    if (typeof router.model === "string" && router.model) {
      patch.chatRouterModel = router.model;
    }
    if (typeof router.provider === "string" && router.provider) {
      patch.chatRouterProvider = router.provider;
    }
    if (typeof router.timeout_ms === "number") {
      patch.chatRouterTimeoutMs = router.timeout_ms;
    }
  }
  return patch;
}