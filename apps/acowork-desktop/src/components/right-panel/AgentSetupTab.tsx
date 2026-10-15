import { useCallback, useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { invoke } from "@tauri-apps/api/core";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { useAgentStore, type AgentProfileSettings } from "../../stores/agentStore";
import { useChatStore } from "../../stores/chatStore";
import { useFileEditorStore } from "../../stores/fileEditorStore";
import { BUILTIN_ICONS, BUILTIN_ICON_IDS } from "../common/UserAvatar";
import { AgentAvatar } from "../common/AgentAvatar";
import { AuthedImage } from "../common/AuthedImage";
import { getGatewayUrl } from "../../lib/config";
import { httpApiError } from "../../lib/api-error";
import { ConfirmDialog } from "../common/ConfirmDialog";
import { useTranslation } from "../../i18n/useTranslation";
import { StyledInput } from "../common/StyledInput";
import { Dropdown } from "../common/Dropdown";
import {
  fetchAvatarAssets,
  fetchAvatarConfig,
  updateAvatarConfig,
  deleteAvatarFile,
  resolveAgentAvatarFileUrl,
} from "../../lib/avatar";
import type { AvatarAssetEntry, AvatarConfigResponse } from "../../lib/types";
import { log } from "../../lib/logger";
import { useToast } from "../common/ToastProvider";
import { ListBox, ExpandableRow } from "../common/list";
import { flushPendingWrites } from "./agentSetupPendingWrites";
import { buildProfilePatchFromServerConfig } from "./agentSetupConfigSync";
import { OverlayScrollbar } from "../common/OverlayScrollbar";

// ── Helpers ───────────────────────────────────────────────────────────

/** Compute the next available avatar-XX filename (zero-padded to 2 digits). */
function nextAvatarName(assets: AvatarAssetEntry[], ext: string): string {
  const used = new Set<number>();
  for (const a of assets) {
    const fn = a.relative_path.split("/").pop() ?? "";
    const m = fn.match(/^avatar-(\d+)\./i);
    if (m) used.add(parseInt(m[1], 10));
  }
  let n = 1;
  while (used.has(n)) n++;
  return `avatar-${String(n).padStart(2, "0")}.${ext}`;
}

const IMAGE_EXTENSIONS = ["png", "jpg", "jpeg", "gif", "webp", "svg"];

// ── Write-through persistence config (ADR-052 follow-up) ──────────────
//
// Setup panel writes through to the runtime per-field on change. No more
// batched "Apply" step — see `saveField` below. A single 1000ms debounce
// applies to every wired field:
//
//   - Visual feedback (`setProfile`) is synchronous — users see the
//     change instantly, regardless of debounce.
//   - 1000ms is long enough that typing a multi-digit number collapses
//     into a single PUT. At 500ms a user who pauses between digits
//     ("1" → "16" → "160" → "1600" → "16000" in max output tokens)
//     persisted every intermediate value; the runtime's on-disk config
//     then churned once per digit. The write is cheap, so the only cost
//     of the wider window is that `Saving N change(s)...` lingers a
//     beat longer after the last keystroke.
//   - Single value keeps the mental model simple — adding a new wired
//     field doesn't require picking a debounce bucket.
//
// The wire field names mirror `UpdateAgentConfigRequest` in
// `acowork-runtime/src/http/server.rs`; keep these in lockstep when
// adding a new field.

type WiredField =
  | "maxTokens"
  | "maxIterations"
  | "maxSessions"
  | "temperature"
  | "contextWindow"
  | "shellApprovalThreshold"
  | "approvalTimeoutSecs"
  | "compressionRatioThreshold"
  | "sessionLanguage"
  // ADR-094 note: the `skillsAutoLoad` switch was removed from the
  // Agent Setup panel. Skill mode (Progressive vs Manual) is now
  // driven entirely by the `load_skill` tool switch in the Tools
  // panel — see ADR-094 §4 (single source of truth, no drift).
;

const WIRE_FIELD: Record<WiredField, string> = {
  maxTokens: "max_output_tokens",
  maxIterations: "max_iterations",
  maxSessions: "max_sessions",
  temperature: "temperature",
  contextWindow: "context_window",
  shellApprovalThreshold: "shell_approval_threshold",
  approvalTimeoutSecs: "approval_timeout_secs",
  compressionRatioThreshold: "compression_ratio_threshold",
  sessionLanguage: "session_language",
  // ADR-094 note: `skillsAutoLoad` was removed from the
  // `WIRE_FIELD` table. Skill mode lives on the `load_skill`
  // tool entry in `agent_tools.json` (ADR-094 §4) — Toggling it
  // goes through `PUT /agents/{id}/builtin-tools`, not this
  // endpoint.
};

const FIELD_DEBOUNCE_MS = 1000;

const DEBOUNCE_BY_FIELD: Record<WiredField, number> = {
  shellApprovalThreshold: FIELD_DEBOUNCE_MS,
  temperature: FIELD_DEBOUNCE_MS,
  contextWindow: FIELD_DEBOUNCE_MS,
  approvalTimeoutSecs: FIELD_DEBOUNCE_MS,
  maxTokens: FIELD_DEBOUNCE_MS,
  maxIterations: FIELD_DEBOUNCE_MS,
  maxSessions: FIELD_DEBOUNCE_MS,
  compressionRatioThreshold: FIELD_DEBOUNCE_MS,
  sessionLanguage: FIELD_DEBOUNCE_MS,
  // ADR-094 note: `skillsAutoLoad` was removed from the debounce
  // table. Skill mode toggling goes through the Tools panel
  // (`load_skill` Switch) and uses the built-in tools debounce,
  // not this per-field map.
};

// ── Component ───────────────────────────────────────────────────────────

export function AgentSetupTab() {
  const { t } = useTranslation();
  const { addToast } = useToast();
  const { agents, selectedAgentId, fetchAgents } = useAgentStore();
  const { getProfile, setProfile, resetProfile } = useAgentStore();

  const storage = selectedAgentId ? agents[selectedAgentId] : null;
  const selectedAgent = storage?.meta ?? null;
  const profile = selectedAgentId ? getProfile(selectedAgentId) : null;

  // Fetch agent runtime config from Gateway API on mount
  const [_configLoading, setConfigLoading] = useState(false);
  const [showResetConfirm, setShowResetConfirm] = useState(false);

  // Track edits from the instant they change through the debounce window and PUT.
  const [pendingFields, setPendingFields] = useState<ReadonlySet<string>>(
    () => new Set(),
  );
  const [savingFields, setSavingFields] = useState<ReadonlySet<string>>(
    () => new Set(),
  );
  const debounceTimersRef = useRef<Map<string, ReturnType<typeof setTimeout>>>(
    new Map(),
  );
  const pendingValuesRef = useRef<
    Map<string, { agentId: string; field: WiredField; value: unknown; version: number }>
  >(new Map());
  const fieldVersionsRef = useRef<Map<string, number>>(new Map());
  const inFlightCountsRef = useRef<Map<string, number>>(new Map());
  const writeChainsRef = useRef<Map<string, Promise<void>>>(new Map());
  const prevValuesRef = useRef<Map<string, unknown>>(new Map());

  // Avatar picker state (ADR-017)
  const [avatarTab, setAvatarTab] = useState<"custom" | "builtin">("custom");
  const [avatarPopupOpen, setAvatarPopupOpen] = useState(false);
  const [avatarAssets, setAvatarAssets] = useState<AvatarAssetEntry[]>([]);
  const [avatarConfig, setAvatarConfig] = useState<AvatarConfigResponse | null>(null);
  const [avatarBusy, setAvatarBusy] = useState(false);
  /** In-flight rename draft; `null` = show the server value. */
  const [nameDraft, setNameDraft] = useState<string | null>(null);

  // ── Avatar picker popup positioning ─────────────────────────────
  // The picker popup used to be `absolute` inside the avatar button's
  // `.relative` wrapper. Once the setup panel became a set of
  // collapsible `ListBox` cards (each `overflow-hidden`), that absolute
  // popup got clipped by the card. Fix mirrors the Tooltip /
  // ContextMenu pattern: render the popup through `createPortal` to
  // `document.body` with `position: fixed`, anchored to the avatar
  // button's measured rect.
  //   - `avatarAnchorRef`   → the `.relative` wrapper around the button
  //   - `avatarPopupPos`    → { top, left } viewport coords for the popup
  //   - Position is computed synchronously on open (no first-frame
  //     flash) and re-computed on window resize / any scroll so the
  //     popup tracks the button when the panel or page scrolls.
  const avatarAnchorRef = useRef<HTMLDivElement>(null);
  const [avatarPopupPos, setAvatarPopupPos] = useState<{ top: number; left: number } | null>(null);

  const computeAvatarPopupPos = useCallback(() => {
    const el = avatarAnchorRef.current;
    if (!el) return;
    const rect = el.getBoundingClientRect();
    // 8px gap below the avatar button — same visual as the old
    // `absolute left-0 top-full mt-2` placement.
    setAvatarPopupPos({ top: rect.bottom + 8, left: rect.left });
  }, []);

  useEffect(() => {
    if (!avatarPopupOpen) return;
    computeAvatarPopupPos();
    window.addEventListener("resize", computeAvatarPopupPos);
    // Capture-phase scroll: catches scrolling of the right-panel /
    // card containers, not just the window, so the fixed popup stays
    // glued to the avatar button.
    window.addEventListener("scroll", computeAvatarPopupPos, true);
    return () => {
      window.removeEventListener("resize", computeAvatarPopupPos);
      window.removeEventListener("scroll", computeAvatarPopupPos, true);
    };
  }, [avatarPopupOpen, computeAvatarPopupPos]);

  // ── Section collapse state ──────────────────────────────────────
  // Three level-1 collapsible cards (Agent Info / Session Parameters /
  // Model Parameters) using the same grammar as Status-tab Session
  // Status / Agent Status cards in RightPanel and Tools-tab Builtin
  // Tools / Web Search / MCP cards in ToolsTab. Default open so the
  // previous always-visible behavior is preserved on first mount.
  const [agentInfoOpen, setAgentInfoOpen] = useState(true);
  const [sessionParamsOpen, setSessionParamsOpen] = useState(true);
  const [modelParamsOpen, setModelParamsOpen] = useState(true);

  // Load avatar config + assets on mount and agent switch
  useEffect(() => {
    if (!selectedAgentId) return;
    let cancelled = false;
    setAvatarAssets([]);
    setAvatarConfig(null);

    fetchAvatarConfig(selectedAgentId)
      .then((cfg) => { if (!cancelled) setAvatarConfig(cfg); })
      .catch((err) => { if (!cancelled) log.debug("[AgentSetup] Avatar config fetch failed:", err); });

    fetchAvatarAssets(selectedAgentId)
      .then((resp) => { if (!cancelled) setAvatarAssets(resp.assets); })
      .catch((err) => { if (!cancelled) log.debug("[AgentSetup] Avatar assets fetch failed:", err); });

    return () => { cancelled = true; };
  }, [selectedAgentId]);

  // Fetch agent runtime config on mount
  useEffect(() => {
    if (!selectedAgentId) return;
    let cancelled = false;
    setConfigLoading(true);
    fetch(`${getGatewayUrl()}/api/agents/${selectedAgentId}/config`)
      .then((res) => (res.ok ? res.json() : null))
      .then((data) => {
        if (cancelled || !data) return;
        // ADR-052 follow-up: the unwrap + patch-build logic lives
        // in `buildProfilePatchFromServerConfig` (single source of
        // truth, shared with the refresh-event listener below).
        // Do NOT inline another cfg-cast here — the drift between
        // two such blocks is exactly how fields used to get missed.
        const patch = buildProfilePatchFromServerConfig(data);
        setProfile(selectedAgentId, patch);
      })
      .catch((err) => {
        if (!cancelled) log.debug("[AgentSetup] Agent not ready:", err);
      })
      .finally(() => { if (!cancelled) setConfigLoading(false); });
    return () => { cancelled = true; };
  }, [selectedAgentId]);

  // Listen for global resource refresh events
  useEffect(() => {
    if (!selectedAgentId) return;
    const handler = (e: Event) => {
      const ce = e as CustomEvent<{ agentId: string }>;
      if (ce.detail?.agentId === selectedAgentId) {
        fetch(`${getGatewayUrl()}/api/agents/${selectedAgentId}/config`)
          .then((res) => (res.ok ? res.json() : null))
          .then((data) => {
            if (!data) return;
            // Same single-source-of-truth helper as the mount effect.
            // The retained-MQTT snapshot fires this refresh *after*
            // every successful PUT, so the clobber-to-undefined path
            // is closed by the typeof-guard inside the helper.
            const patch = buildProfilePatchFromServerConfig(data);
            setProfile(selectedAgentId, patch);
          })
          .catch(() => { });
      }
    };
    window.addEventListener('acowork:refresh-agent-config', handler);
    return () => window.removeEventListener('acowork:refresh-agent-config', handler);
  }, [selectedAgentId]);

  // ── Write-through persistence (ADR-052 follow-up) ──────────────────
  //
  // Replaces the old batched `handleApply` / "Apply" button. Every field
  // mutation now flows through `saveField`:
  //
  //   1. Optimistic local update (`setProfile`) so the UI reflects the
  //      change instantly.
  //   2. Schedule a PUT to `/api/agents/{id}/config` carrying only the
  //      changed field. Debounced per field type (see
  //      `DEBOUNCE_BY_FIELD`), so "1" → "12" → "123" collapses to one PUT.
  //   3. The runtime's `UpdateAgentConfigRequest` is a partial-PUT DTO
  //      (each field `Option<serde_json::Value>`, missing = leave alone),
  //      so per-field PUTs compose cleanly and never clobber unrelated
  //      on-disk values.
  //
  // The race window that used to lose a Switch click (load effect's
  // `?? undefined` overwriting the user's intent before Apply could
  // persist it) is now closed two ways:
  //   - local updates land before the next debounced PUT fires, so the
  //     PUT body always carries the user's intent.
  //   - the load effect / refresh handler only overwrite fields the
  //     server actually returned (typeof-guard, see above).

  const putField = useCallback(
    async (
      agentId: string,
      field: WiredField,
      value: unknown,
      version: number,
    ) => {
      const key = `${agentId}:${field}`;
      inFlightCountsRef.current.set(
        key,
        (inFlightCountsRef.current.get(key) ?? 0) + 1,
      );
      setSavingFields((prev) => new Set(prev).add(key));
      let succeeded = false;
      try {
        const previousWrite = writeChainsRef.current.get(key) ?? Promise.resolve();
        const write = previousWrite.catch(() => {}).then(async () => {
          const res = await fetch(
            `${getGatewayUrl()}/api/agents/${agentId}/config`,
            {
              method: "PUT",
              headers: { "Content-Type": "application/json" },
              // `undefined` -> `null` so the runtime's partial-PUT DTO
              // sees an explicit clear (`FieldPatch::Clear`) instead of
              // an absent field (`skip`).
              body: JSON.stringify({ [WIRE_FIELD[field]]: value ?? null }),
            },
          );
          if (!res.ok) {
            log.warn("[AgentSetup] Field save failed:", field, res.status);
            const err = await httpApiError(res);
            addToast({
              type: "error",
              message:
                err.status === 403
                  ? err.message
                  : t("agentSetup.saveFailed", {
                      field: WIRE_FIELD[field],
                      status: res.status,
                    }),
            });
          } else {
            succeeded = true;
          }
        });
        writeChainsRef.current.set(key, write);
        await write;
        if (writeChainsRef.current.get(key) === write) {
          writeChainsRef.current.delete(key);
        }
      } catch (err) {
        log.warn("[AgentSetup] Field save error:", field, err);
        addToast({
          type: "error",
          message: t("agentSetup.saveFailed", {
            field: WIRE_FIELD[field],
            status: "network",
          }),
        });
      } finally {
        const inFlight = (inFlightCountsRef.current.get(key) ?? 1) - 1;
        if (inFlight > 0) inFlightCountsRef.current.set(key, inFlight);
        else inFlightCountsRef.current.delete(key);

        if (fieldVersionsRef.current.get(key) === version) {
          if (succeeded) {
            prevValuesRef.current.delete(key);
          } else if (!pendingValuesRef.current.has(key)) {
            if (prevValuesRef.current.has(key)) {
              useAgentStore.getState().setProfile(agentId, {
                [field]: prevValuesRef.current.get(key),
              } as Partial<AgentProfileSettings>);
            }
            prevValuesRef.current.delete(key);
          }
        }
        if (inFlight === 0 && !pendingValuesRef.current.has(key)) {
          setSavingFields((prev) => {
            if (!prev.has(key)) return prev;
            const next = new Set(prev);
            next.delete(key);
            return next;
          });
        }
      }
    },
    [t, addToast],
  );

  const saveField = useCallback(
    (field: WiredField, value: unknown) => {
      if (!selectedAgentId) return;
      const agentId = selectedAgentId;
      const key = `${agentId}:${field}`;
      if (!prevValuesRef.current.has(key)) {
        prevValuesRef.current.set(key, getProfile(agentId)[field]);
      }
      const version = (fieldVersionsRef.current.get(key) ?? 0) + 1;
      fieldVersionsRef.current.set(key, version);
      setProfile(agentId, { [field]: value } as Partial<AgentProfileSettings>);
      pendingValuesRef.current.set(key, { agentId, field, value, version });
      setPendingFields((prev) => new Set(prev).add(key));

      const existing = debounceTimersRef.current.get(key);
      if (existing) clearTimeout(existing);
      const timer = setTimeout(() => {
        debounceTimersRef.current.delete(key);
        const pending = pendingValuesRef.current.get(key);
        pendingValuesRef.current.delete(key);
        setPendingFields((prev) => {
          if (!prev.has(key)) return prev;
          const next = new Set(prev);
          next.delete(key);
          return next;
        });
        if (pending) {
          void putField(
            pending.agentId,
            pending.field,
            pending.value,
            pending.version,
          );
        }
      }, DEBOUNCE_BY_FIELD[field]);
      debounceTimersRef.current.set(key, timer);
    },
    [selectedAgentId, setProfile, getProfile, putField],
  );

  // Flush pending writes when the setup tab unmounts or the selected
  // agent changes; never cancel an optimistic edit without persisting it.
  useEffect(() => {
    return () => {
      if (!selectedAgentId) return;
      const flushedKeys = flushPendingWrites(
        selectedAgentId,
        debounceTimersRef.current,
        pendingValuesRef.current,
        (pending) => {
          void putField(
            pending.agentId,
            pending.field,
            pending.value,
            pending.version,
          );
        },
      );
      setPendingFields((prev) => {
        const next = new Set(prev);
        flushedKeys.forEach((key) => next.delete(key));
        return next;
      });
      setSavingFields((prev) => {
        const next = new Set(prev);
        for (const key of next) {
          if (key.startsWith(`${selectedAgentId}:`)) next.delete(key);
        }
        return next;
      });
    };
  }, [selectedAgentId, putField]);

  // ── ADR-094 note: `handleSkillsAutoLoadToggle` was removed. The
  //    skill mode (Progressive vs Manual) toggle now lives in the
  //    Tools panel as the `load_skill` Switch and goes through
  //    `PUT /agents/{id}/builtin-tools` like every other tool.

  // Sync the new temperature into the chat store so the RightPanel
  // status tab shows the updated value right away, without waiting for
  // the next WebSocket session_state event (which may be delayed if the
  // agent is mid-streaming). Lifted out of the old `handleApply` and
  // driven directly off `profile.temperature` so write-through updates
  // trigger it too.
  useEffect(() => {
    if (!selectedAgentId) return;
    const newTemp = profile?.temperature ?? null;
    const chatStore = useChatStore.getState();
    const agentState = chatStore.agentStates[selectedAgentId];
    if (!agentState?.activeSessionId) return;
    const sessionState =
      agentState.sessionStates[agentState.activeSessionId] ?? {};
    if (sessionState.temperature === newTemp) return;
    useChatStore.setState({
      agentStates: {
        ...chatStore.agentStates,
        [selectedAgentId]: {
          ...agentState,
          sessionStates: {
            ...agentState.sessionStates,
            [agentState.activeSessionId]: {
              ...sessionState,
              temperature: newTemp,
            },
          },
        },
      },
    });
  }, [selectedAgentId, profile?.temperature]);

  // ── Avatar selection handlers ──────────────────────────────────────

  // ── Display-name handler (ADR-009 §V-Q) ────────────────────────────
  //
  // Committed on blur / Enter: the Runtime persists it into the instance's
  // `.overrides.json` and the node republishes its inventory, which is what
  // refreshes the sidebar. `nameDraft === null` means "show the server
  // value".
  const commitAgentName = async () => {
    if (!selectedAgentId || !selectedAgent || nameDraft === null) return;
    const next = nameDraft.trim();
    setNameDraft(null);
    const current = selectedAgent.display_name ?? "";
    if (next === current) return;
    try {
      await updateAvatarConfig(selectedAgentId, { display_name: next });
      await fetchAgents();
    } catch (err) {
      log.warn("[AgentSetup] Rename agent failed:", err);
    }
  };

  const handleSelectCustom = async (relativePath: string) => {
    if (!selectedAgentId) return;
    setAvatarBusy(true);
    try {
      const cfg = await updateAvatarConfig(selectedAgentId, { avatar: relativePath, builtin_avatar: "" });
      setAvatarConfig(cfg);
      await fetchAgents();
    } catch (err) {
      log.warn("[AgentSetup] Select custom avatar failed:", err);
    } finally {
      setAvatarBusy(false);
      setAvatarPopupOpen(false);
    }
  };

  const handleSelectBuiltin = async (iconId: string) => {
    if (!selectedAgentId) return;
    setAvatarBusy(true);
    try {
      const cfg = await updateAvatarConfig(selectedAgentId, { avatar: "", builtin_avatar: iconId });
      setAvatarConfig(cfg);
      await fetchAgents();
    } catch (err) {
      log.warn("[AgentSetup] Select builtin avatar failed:", err);
    } finally {
      setAvatarBusy(false);
      setAvatarPopupOpen(false);
    }
  };

  // ── Avatar upload (does NOT auto-select) ──────────────────────────

  const handleUploadClick = async () => {
    if (!selectedAgentId) return;
    const selected = await openDialog({
      multiple: false,
      filters: [{ name: "Images", extensions: IMAGE_EXTENSIONS }],
    });
    if (!selected || typeof selected !== "string") return;

    const ext = selected.split(".").pop()?.toLowerCase() ?? "png";
    if (!IMAGE_EXTENSIONS.includes(ext)) return;

    const relative = `assets/${nextAvatarName(avatarAssets, ext)}`;
    setAvatarBusy(true);
    try {
      await invoke("upload_agent_file", {
        agentId: selectedAgentId,
        relativePath: relative,
        filePath: selected,
      });
      // Refresh assets list — user manually selects afterwards
      const resp = await fetchAvatarAssets(selectedAgentId);
      setAvatarAssets(resp.assets);
    } catch (err) {
      log.warn("[AgentSetup] Avatar upload failed:", err);
    } finally {
      setAvatarBusy(false);
    }
  };

  // ── Avatar delete ──────────────────────────────────────────────────

  const handleDeleteAvatar = async (relativePath: string) => {
    if (!selectedAgentId) return;
    setAvatarBusy(true);
    try {
      await deleteAvatarFile(selectedAgentId, relativePath);
      // Refresh both — backend clears avatar field if deleted file was current
      const [assetsResp, cfg] = await Promise.all([
        fetchAvatarAssets(selectedAgentId),
        fetchAvatarConfig(selectedAgentId),
      ]);
      setAvatarAssets(assetsResp.assets);
      setAvatarConfig(cfg);
      await fetchAgents();
    } catch (err) {
      log.warn("[AgentSetup] Delete avatar failed:", err);
    } finally {
      setAvatarBusy(false);
      setAvatarPopupOpen(false);
    }
  };

  if (!selectedAgentId || !selectedAgent || !profile) {
    return (
      <div className="flex flex-1 items-center justify-center bg-right-panel p-6">
        <span className="text-xs text-text-tertiary ">{t("agentSetup.noAgentSelected")}</span>
      </div>
    );
  }

  // ADR-009 §V-Q: the display name lives on the server (Runtime
  // `.overrides.json`, mirrored into the Gateway's list view), so it is
  // read from the agent meta — never from a local profile override.
  const agentName = selectedAgent.display_name ?? selectedAgent.name ?? selectedAgentId;

  // Derived once so the slider thumb and the numeric badge can never
  // disagree (bug: previously the slider used `?? 0.9` while the badge
  // used strict `!== undefined`, so a freshly loaded agent whose runtime
  // config omitted `compression_ratio_threshold` showed thumb-at-90%
  // with a "—" badge until the user dragged — see ADR-061 §16).
  const ratioPct = Math.round((profile.compressionRatioThreshold ?? 0.9) * 100);

  return (
    <OverlayScrollbar dataTabScroll className="flex-1 bg-right-panel p-3">
      {/* ADR-087: view-only caller — say so up front instead of letting
          each write fail one toast at a time. `can_manage === false` is
          server-computed; `undefined` (Local mode / pre-087 Gateway)
          means manageable. */}
      {selectedAgent?.can_manage === false && (
        <div
          role="note"
          className="mb-2 rounded-md border border-[var(--color-warning,#f5a623)]/40 bg-[var(--color-warning,#f5a623)]/10 px-3 py-2 text-11 text-text-secondary"
        >
          {t("apiError.readOnlyBanner")}
        </div>
      )}
      {/* ── Card 1: Agent Info ────────────────────────────────
          Avatar picker + display name. Level-1 collapsible card
          matching the Status-tab Session/Agent Status and
          Tools-tab Builtin/Web Search/MCP cards grammar (same
          body chrome via `bg-panel-inset` + border-t hairline).
          Default open so the previous always-visible behavior
          is preserved. */}
      <ListBox dividers={false}>
        <ExpandableRow
          open={agentInfoOpen}
          onToggle={() => setAgentInfoOpen((v) => !v)}
          title={t("agentSetup.sectionAgentInfo")}
          ariaLabel={t("agentSetup.sectionAgentInfo")}
          bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset px-3 py-3"
        >
          <div className="space-y-3">
    {/* Avatar preview — click to open picker popup */}
          <div className="flex items-center gap-3">
            <div className="relative" ref={avatarAnchorRef}>
              <button
                onClick={() => {
                  if (!avatarPopupOpen) computeAvatarPopupPos();
                  setAvatarPopupOpen((v) => !v);
                }}
                className="relative block rounded-lg ring-1 ring-zinc-300/60 transition hover:ring-zinc-400 dark:ring-zinc-600/60 dark:hover:ring-zinc-400"
              >
                <AgentAvatar
                  agentId={selectedAgentId}
                  avatarUrl={avatarConfig?.avatar ?? null}
                  builtinAvatarId={avatarConfig?.builtin_avatar ?? null}
                  version={selectedAgent.version}
                  size={64}
                />
                {/* Pencil badge */}
                <span className="absolute -bottom-0.5 -right-0.5 flex h-5 w-5 items-center justify-center rounded-full bg-zinc-800 text-white shadow-sm dark:bg-zinc-600">
                  <svg viewBox="0 0 16 16" className="h-3 w-3 fill-current" xmlns="http://www.w3.org/2000/svg">
                    <path d="M11.013 1.427a1.75 1.75 0 0 1 2.474 0l1.086 1.086a1.75 1.75 0 0 1 0 2.474l-8.61 8.61c-.21.21-.47.364-.756.445l-3.251.93a.75.75 0 0 1-.927-.928l.929-3.25c.081-.286.235-.547.445-.758l8.61-8.61Zm.176 4.823L11.5 7l-3-3-.31.31a.75.75 0 0 0-.177.764l.93 3.251a.75.75 0 0 1-.927.928l-3.251-.93Z" />
                  </svg>
                </span>
              </button>

              {/* Avatar picker popup — rendered via createPortal to
                  document.body so the ListBox card's overflow-hidden
                  does not clip it (same pattern as Tooltip/ContextMenu).
                  `position: fixed` anchors to the avatar button's
                  measured viewport coords (avatarPopupPos). */}
              {avatarPopupOpen &&
                createPortal(
                  <>
                    {/* Click-outside overlay */}
                    <div
                      className="fixed inset-0 z-40"
                      onClick={() => setAvatarPopupOpen(false)}
                    />
                    <div
                      className="fixed z-50 w-72 rounded-lg border border-border-outer bg-modal-surface p-3 shadow-lg max-h-[min(calc(100vh-120px),460px)] overflow-y-auto overscroll-contain"
                      style={{ top: avatarPopupPos?.top ?? 0, left: avatarPopupPos?.left ?? 0 }}
                    >
                    {/* Tabs */}
                    <div className="mb-3 flex gap-1 border-b border-border-divider">
                      <button
                        onClick={() => setAvatarTab("custom")}
                        className={`px-3 py-1 text-xs font-medium transition-colors ${avatarTab === "custom"
                          ? "border-b-2 border-zinc-800 text-text dark:border-zinc-200 "
                          : "text-text-tertiary hover:text-zinc-600 "
                          }`}
                      >
                        Custom
                      </button>
                      <button
                        onClick={() => setAvatarTab("builtin")}
                        className={`px-3 py-1 text-xs font-medium transition-colors ${avatarTab === "builtin"
                          ? "border-b-2 border-zinc-800 text-text dark:border-zinc-200 "
                          : "text-text-tertiary hover:text-zinc-600 "
                          }`}
                      >
                        Builtin
                      </button>
                    </div>

                    {/* Custom tab */}
                    {avatarTab === "custom" && (
                      <div className="grid grid-cols-4 gap-2">
                        <button
                          onClick={handleUploadClick}
                          disabled={avatarBusy}
                          className="flex aspect-square items-center justify-center rounded-md border border-dashed border-zinc-300 text-text-tertiary transition-colors hover:border-zinc-400 hover:text-zinc-600 disabled:opacity-50 dark:border-zinc-600  dark:hover:border-zinc-400"
                        >
                          <span className="text-lg">+</span>
                        </button>
                        {avatarAssets.map((asset) => {
                          const isSelected = avatarConfig?.avatar === asset.relative_path;
                          return (
                            <div
                              key={asset.relative_path}
                              className={`group relative aspect-square overflow-hidden rounded-md border-2 transition-colors ${isSelected
                                ? "border-zinc-800 dark:border-zinc-200"
                                : "border-transparent hover:border-zinc-300 dark:hover:border-zinc-600"
                                }`}
                            >
                              <AuthedImage
                                src={resolveAgentAvatarFileUrl(selectedAgentId, asset.relative_path)}
                                alt={asset.relative_path}
                                className="h-full w-full cursor-pointer object-cover"
                                onClick={() => handleSelectCustom(asset.relative_path)}
                              />
                              <button
                                onClick={(e) => {
                                  e.stopPropagation();
                                  handleDeleteAvatar(asset.relative_path);
                                }}
                                disabled={avatarBusy}
                                className="absolute right-0.5 top-0.5 flex h-4 w-4 items-center justify-center rounded bg-red-500/80 text-[8px] text-white opacity-0 transition-opacity group-hover:opacity-100"
                              >
                                ×
                              </button>
                            </div>
                          );
                        })}
                      </div>
                    )}

                    {/* Builtin tab */}
                    {avatarTab === "builtin" && (
                      <div className="grid grid-cols-4 gap-2">
                        {BUILTIN_ICON_IDS.map((iconId) => {
                          const isSelected = avatarConfig?.builtin_avatar === iconId;
                          return (
                            <button
                              key={iconId}
                              onClick={() => handleSelectBuiltin(iconId)}
                              disabled={avatarBusy}
                              className={`flex items-center justify-center rounded-md p-1 transition-colors ${isSelected
                                ? "bg-zinc-200 dark:bg-zinc-600"
                                : "hover:bg-zinc-100 dark:hover:bg-zinc-700"
                                }`}
                            >
                              <img
                                src={BUILTIN_ICONS[iconId] ?? ""}
                                alt={iconId}
                                draggable={false}
                                className="h-12 w-12 rounded-lg object-cover"
                              />
                            </button>
                          );
                        })}
                      </div>
                    )}
                  </div>
                  </>,
                  document.body,
                )}
            </div>
            <div className="min-w-0 flex-1">
              <p className="truncate text-sm font-medium text-text ">
                {agentName}
              </p>
              {selectedAgent.role && (
                <p className="mt-1.5 truncate text-10 text-text-tertiary ">
                  {selectedAgent.role}
                </p>
              )}
              <p className="truncate text-10 text-text-tertiary ">
                {selectedAgentId}
              </p>
            </div>
          </div>

    {/* Agent Name */}
          <div className="space-y-1">
            <label className="block text-10 font-medium text-text-tertiary ">
              {t("agentSetup.agentName")}
            </label>
            <StyledInput
              type="text"
              value={nameDraft ?? selectedAgent.display_name ?? selectedAgent.name ?? ""}
              onChange={(e) => setNameDraft(e.target.value)}
              onBlur={() => void commitAgentName()}
              onKeyDown={(e) => {
                if (e.key === "Enter") void commitAgentName();
              }}
              placeholder={selectedAgent.name ?? "Agent name"}
              className="rounded-md bg-panel-block"
            />
          </div>
          </div>
        </ExpandableRow>
      </ListBox>

      {/* Divider — full panel-width hairline separating the
          Agent Info card (above) from the Session Parameters
          card (below). Matches the workspace/memory panel
          divider style. */}
      <div className="-mx-3 my-2 border-t border-border-divider" />

      {/* ── Card 2: Session Parameters ─────────────────────────
          Iteration / session-count / approval /
          shell-approval / compression-ratio knobs. Same
          level-1 collapsible grammar as Card 1. */}
      <ListBox dividers={false}>
        <ExpandableRow
          open={sessionParamsOpen}
          onToggle={() => setSessionParamsOpen((v) => !v)}
          title={t("agentSetup.sectionSessionParams")}
          ariaLabel={t("agentSetup.sectionSessionParams")}
          bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset px-3 py-3"
        >
          <div className="space-y-3">
    {/* Session Language (per-agent LLM language override).
        Empty value = "follow global UserProfile.language" (the
        existing default behaviour). The empty option triggers a
        `null` PUT body thanks to the `value ?? null` shim in
        `putField`, which the runtime maps to `FieldPatch::Clear`. */}
          <div className="space-y-1">
            <label className="block text-10 font-medium text-text-tertiary ">
              {t("agentSetup.sessionLanguage")}
            </label>
            <Dropdown
              value={profile.sessionLanguage ?? ""}
              onChange={(v) => saveField("sessionLanguage", v === "" ? undefined : v)}
              options={[
                { value: "", label: t("agentSetup.sessionLanguageFollow") },
                { value: "zh-CN", label: t("language.zhCN") },
                { value: "zh-TW", label: t("language.zhTW") },
                { value: "en", label: t("language.en") },
                { value: "ja", label: t("language.ja") },
                { value: "ko", label: t("language.ko") },
              ]}
            />
            <p className="text-9 text-text-tertiary ">
              {t("agentSetup.sessionLanguageDesc")}
            </p>
          </div>

    {/* Max Iterations */}
          <div className="space-y-1">
            <label className="block text-10 font-medium text-text-tertiary ">
              {t("agentSetup.maxIterations")}
            </label>
            <StyledInput
              type="number"
              min={0}
              max={500}
              value={profile.maxIterations && profile.maxIterations > 0 ? profile.maxIterations : ""}
              onChange={(e) => {
                const v = e.target.value;
                saveField(
                  "maxIterations",
                  v === "" ? undefined : Math.max(0, parseInt(v, 10) || 0),
                );
              }}
              placeholder={t("agentSetup.defaultIterations")}
              className="rounded-md"
            />
          </div>

    {/* Max Sessions (ADR-024) */}
          <div className="space-y-1">
            <label className="block text-10 font-medium text-text-tertiary ">
              {t("agentSetup.maxSessions")}
            </label>
            <StyledInput
              type="number"
              min={0}
              max={10000}
              value={profile.maxSessions && profile.maxSessions > 0 ? profile.maxSessions : ""}
              onChange={(e) => {
                const v = e.target.value;
                saveField(
                  "maxSessions",
                  v === "" ? undefined : Math.max(0, parseInt(v, 10) || 0),
                );
              }}
              placeholder="2000 (default)"
              className="rounded-md"
            />
            <p className="text-9 text-text-tertiary ">
              {t("agentSetup.maxSessionsDesc")}
            </p>
          </div>

    {/* Approval Timeout */}
          <div className="space-y-1">
            <label className="block text-10 font-medium text-text-tertiary ">
              {t("agentSetup.approvalTimeout")}
            </label>
            <StyledInput
              type="number"
              min={0}
              max={3600}
              step={30}
              value={profile.approvalTimeoutSecs && profile.approvalTimeoutSecs > 0 ? profile.approvalTimeoutSecs : ""}
              onChange={(e) => {
                const v = e.target.value;
                saveField(
                  "approvalTimeoutSecs",
                  v === "" ? undefined : Math.max(0, parseInt(v, 10) || 0),
                );
              }}
              placeholder="300 (5 min)"
              className="rounded-md"
            />
            <p className="text-9 text-text-tertiary ">
              {t("agentSetup.approvalTimeoutDesc")}
            </p>
          </div>


    {/* Shell Command Approval Threshold */}
          <div className="space-y-1">
            <label className="block text-10 font-medium text-text-tertiary ">
              {t("agentSetup.shellCommandApproval")}
            </label>
            <Dropdown
              value={
                // Legacy "never" (pre-rename) is normalized to "auto_approve" so
                // profiles saved before the rename still show the right option.
                profile.shellApprovalThreshold === "never"
                  ? "auto_approve"
                  : (profile.shellApprovalThreshold ?? "high")
              }
              onChange={(v) => saveField("shellApprovalThreshold", v)}
              options={[
                { value: "medium", label: t("agentSetup.approvalMedium") },
                { value: "low", label: t("agentSetup.approvalLow") },
                { value: "high", label: t("agentSetup.approvalHigh") },
                { value: "auto_approve", label: t("agentSetup.approvalAutoApprove") },
              ]}
            />
            <p className="text-9 text-text-tertiary ">
              {t("agentSetup.approvalDesc")}
            </p>
            <button
              onClick={async () => {
                if (!selectedAgentId) return;
                const url = `${getGatewayUrl()}/api/agents/${selectedAgentId}/shell-risk-rules`;
                try {
                  const resp = await fetch(url);
                  if (!resp.ok) {
                    log.error("[AgentSetupTab] Failed to fetch shell risk rules:", resp.status);
                    return;
                  }
                  const data = (await resp.json()) as { content: string; has_user_override: boolean };
                  useFileEditorStore.getState().openFileWithContent(
                    selectedAgentId,
                    "__agent_home__",
                    "config/shell_risk_rules.toml",
                    data.content,
                    "ini", // TOML is mapped to "ini" language in the editor
                  );
                } catch (e) {
                  log.error("[AgentSetupTab] Error opening shell risk rules:", e);
                }
              }}
              className="mt-1 text-9 text-text-tertiary hover:text-zinc-600 dark:hover:text-zinc-300 underline underline-offset-2 transition-colors"
            >
              {t("agentSetup.editRiskRules")}
            </button>
          </div>

    {/* Compression Ratio Threshold (ADR-061) */}
          <div className="space-y-1">
            <label className="block text-10 font-medium text-text-tertiary ">
              {t("agentSetup.compressionRatioThreshold")}
            </label>
            <div className="flex items-center gap-3">
              <input
                type="range"
                min={50}
                max={95}
                step={5}
                value={ratioPct}
                onChange={(e) => {
                  saveField(
                    "compressionRatioThreshold",
                    parseFloat(e.target.value) / 100,
                  );
                }}
                className="flex-1 h-1.5 rounded-full appearance-none cursor-pointer
                  bg-zinc-200 dark:bg-zinc-700
                  accent-zinc-600 dark:accent-zinc-400"
              />
              <span className="w-10 text-right text-xs text-text-secondary  tabular-nums">
                {ratioPct}%
              </span>
            </div>
            <p className="text-9 text-text-tertiary ">
              {t("agentSetup.compressionRatioThresholdDesc")}
            </p>
          </div>

    {/* ADR-094 note: the `skillsAutoLoad` Switch + label block was
        removed from the Session Parameters card. Skill mode now
        toggles via the `load_skill` Switch in the Tools panel —
        see ADR-094 §4. */}
          </div>
        </ExpandableRow>
      </ListBox>

      {/* Divider — full panel-width hairline separating the
          Session Parameters card (above) from the Model
          Parameters card (below). */}
      <div className="-mx-3 my-2 border-t border-border-divider" />

      {/* ── Card 3: Model Parameters ───────────────────────────
          max_output_tokens / context_window / temperature.
          Same level-1 collapsible grammar as Cards 1 and 2. */}
      <ListBox dividers={false}>
        <ExpandableRow
          open={modelParamsOpen}
          onToggle={() => setModelParamsOpen((v) => !v)}
          title={t("agentSetup.sectionModelParams")}
          ariaLabel={t("agentSetup.sectionModelParams")}
          bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset px-3 py-3"
        >
          <div className="space-y-3">
    {/* Max Output Tokens */}
          <div className="space-y-1">
            <label className="block text-10 font-medium text-text-tertiary ">
              {t("agentSetup.maxOutputTokens")}
            </label>
            <StyledInput
              type="number"
              min={0}
              max={131072}
              step={1024}
              value={profile.maxTokens && profile.maxTokens > 0 ? profile.maxTokens : ""}
              onChange={(e) => {
                const v = e.target.value;
                // Empty input → omit the field on the wire (don't clobber
                // the on-disk value). 0 / non-numeric collapses to 0 by
                // input convention, which is also omitted by the
                // `> 0` gate in the old handleApply and matches
                // the pre-write-through behavior.
                saveField(
                  "maxTokens",
                  v === "" ? undefined : Math.max(0, parseInt(v, 10) || 0),
                );
              }}
              placeholder={`${profile.globalMaxTokens ?? 16384} ${t("agentSetup.defaultModelLimit")}`}
              className="rounded-md"
            />
            <p className="text-9 text-text-tertiary ">
              {t("agentSetup.leaveEmptyDefault")}
            </p>
          </div>

    {/* Context Window */}
          <div className="space-y-1">
            <label className="block text-10 font-medium text-text-tertiary ">
              {t("agentSetup.contextWindow")}
            </label>
            <div className="flex items-center gap-3">
              <StyledInput
                type="number"
                min={0}
                max={1000000}
                step={1000}
                value={profile.contextWindow ?? ""}
                placeholder={t("agentSetup.contextWindowPlaceholder")}
                onChange={(e) => {
                  const raw = e.target.value;
                  if (raw === "" || raw === "0") {
                    // 0 = no limit (use model's full window)
                    saveField("contextWindow", 0);
                  } else {
                    const n = parseInt(raw, 10);
                    if (!isNaN(n) && n >= 0) {
                      saveField("contextWindow", n);
                    }
                  }
                }}
                className="w-32 rounded-md"
              />
              <span className="text-10 text-text-tertiary ">
                {t("agentSetup.tokens")}
              </span>
            </div>
            <p className="text-9 text-text-tertiary ">
              {t("agentSetup.contextWindowDesc")}
            </p>
          </div>

    {/* Temperature slider */}
          <div className="space-y-1">
            <label className="block text-10 font-medium text-text-tertiary ">
              {t("agentSetup.temperature")}
            </label>
            <div className="flex items-center gap-3">
              <input
                type="range"
                min={0}
                max={2}
                step={0.05}
                value={profile.temperature ?? 0.3}
                onChange={(e) => {
                  saveField("temperature", parseFloat(e.target.value));
                }}
                className="flex-1 h-1.5 rounded-full appearance-none cursor-pointer
                  bg-zinc-200 dark:bg-zinc-700
                  accent-zinc-600 dark:accent-zinc-400"
              />
              <span className="w-10 text-right text-xs text-text-secondary  tabular-nums">
                {profile.temperature !== undefined ? profile.temperature.toFixed(2) : "—"}
              </span>
            </div>
            <p className="text-9 text-text-tertiary ">
              {t("agentSetup.temperatureDesc")}
            </p>
          </div>
          </div>
        </ExpandableRow>
      </ListBox>
<div className="-mx-3 my-2 border-t border-border-divider" />

      {/* Footer: saving indicator + reset (ADR-052 follow-up) */}
      <div className="flex items-center gap-3">
        <span
          className="flex-1 text-10 tabular-nums text-text-tertiary "
          aria-live="polite"
        >
          {pendingFields.size > 0
            ? t("agentSetup.changesNotSaved", { count: pendingFields.size })
            : savingFields.size > 0
              ? t("agentSetup.savingChanges", { count: savingFields.size })
              : t("agentSetup.allChangesSaved")}
        </span>
        <button
          onClick={() => setShowResetConfirm(true)}
          className="rounded btn-solid px-3 py-1.5 text-xs font-medium"
        >
          {t("agentSetup.resetToDefaults")}
        </button>
      </div>

      <ConfirmDialog
        open={showResetConfirm}
        title={t("agentSetup.resetAgentSetup")}
        message={t("agentSetup.resetConfirm")}
        confirmLabel={t("agentSetup.reset")}
        destructive
        onConfirm={async () => {
          setShowResetConfirm(false);
          if (!selectedAgentId) return;
          // Discard pending debounced edits and invalidate their requests
          // before sending the reset, so stale values cannot be retried.
          const prefix = `${selectedAgentId}:`;
          for (const [key, timer] of debounceTimersRef.current) {
            if (key.startsWith(prefix)) {
              clearTimeout(timer);
              debounceTimersRef.current.delete(key);
            }
          }
          for (const key of pendingValuesRef.current.keys()) {
            if (key.startsWith(prefix)) pendingValuesRef.current.delete(key);
          }
          setPendingFields((prev) => new Set(
            [...prev].filter((key) => !key.startsWith(prefix)),
          ));
          for (const field of Object.keys(WIRE_FIELD) as WiredField[]) {
            const key = `${selectedAgentId}:${field}`;
            fieldVersionsRef.current.set(key, (fieldVersionsRef.current.get(key) ?? 0) + 1);
            prevValuesRef.current.delete(key);
          }
          // Finish any PUT already in flight before the reset request, so
          // an older write cannot land after the clear.
          await Promise.all(
            [...writeChainsRef.current]
              .filter(([key]) => key.startsWith(prefix))
              .map(([, write]) => write.catch(() => {})),
          );
          // Clear local state first so the UI updates immediately.
          resetProfile(selectedAgentId);
          // Push all wired fields as JSON-null → runtime maps null to
          // `FieldPatch::Clear` for each, which drops the on-disk
          // entry (`skip_serializing_if = Option::is_none`). Next
          // boot falls through to the runtime defaults:
          //   max_output_tokens → manifest / global default
          //   max_iterations    → global default
          //   max_sessions      → 1000
          //   temperature       → 0.3
          //   context_window    → 200K
          //   shell_approval_threshold → manifest default
          //   approval_timeout_secs     → 300
          try {
            setSavingFields((prev) => {
              const next = new Set(prev);
              next.add("__reset__");
              return next;
            });
            const res = await fetch(
              `${getGatewayUrl()}/api/agents/${selectedAgentId}/config`,
              {
                method: "PUT",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify({
                  max_output_tokens: null,
                  max_iterations: null,
                  max_sessions: null,
                  temperature: null,
                  context_window: null,
                  shell_approval_threshold: null,
                  approval_timeout_secs: null,
                  session_language: null,
                }),
              },
            );
            if (!res.ok) {
              log.warn("[AgentSetup] Reset PUT failed:", res.status);
            }
          } catch (err) {
            log.warn("[AgentSetup] Reset PUT error:", err);
          } finally {
            setSavingFields((prev) => {
              const next = new Set(prev);
              next.delete("__reset__");
              return next;
            });
          }
        }}
        onCancel={() => setShowResetConfirm(false)}
      />
    </OverlayScrollbar>
  );
}

