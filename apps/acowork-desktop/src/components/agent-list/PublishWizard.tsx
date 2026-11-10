import { useState, useEffect, useRef, useCallback, useMemo } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { cn } from "../../lib/utils";
import { useTranslation } from "../../i18n/useTranslation";
import { ErrorBox } from "../common/ErrorBox";
import { savePublishToLibrary } from "../../lib/agentHub";
import { useAgentStore } from "../../stores/agentStore";
import type {
  PreparePublishResponse,
  BuildPublishResponse,
  ExportPackageResponse,
  AgentDetail,
} from "../../lib/types";
import { BUILTIN_ICONS, BUILTIN_ICON_IDS } from "../common/UserAvatar";
import { resolveAgentAvatarUrl } from "../../lib/avatar";
import { useEscapeClose } from "../../hooks/useEscapeClose";
import { AuthedImage } from "../common/AuthedImage";
import { log } from "../../lib/logger";
import {
  CheckCircle,
  XCircle,
  AlertTriangle,
  Package,
  Brush,
  FileDown,
  Key,
  Check,
  Loader2,
  ExternalLink,
  ImagePlus,
  RotateCcw,
  X as XIcon,
} from "lucide-react";

interface PublishWizardProps {
  open: boolean;
  agentId: string;
  agentName: string;
  onClose: () => void;
}

type WizardStep = "check" | "clean" | "build" | "sign" | "distribute";

/**
 * State of the REQ-029 write-back ("copy the built package into the Local
 * library"). It is kept separate from `busy` on purpose: a slow or failed copy
 * must never block the wizard from closing, because the package itself is
 * already finished and lives on the node either way.
 *
 * `remote_skipped` is a success, not a failure — D12 scopes the copy to a
 * single-machine deployment, so in remote/relay mode the wizard's job is to
 * say which node holds the artifact (B5).
 */
type LibrarySave =
  | { status: "idle" }
  | { status: "saving" }
  | { status: "saved"; path: string; fileName: string }
  | { status: "remote_skipped"; fileName: string }
  | { status: "failed"; message: string };

/**
 * Source of the avatar that will be baked into the published package.
 * Persisted to manifest.toml at build time.
 *
 *  - `builtin`     — use one of the bundled icon-XX.jpg icons. Stored as
 *                    `builtin_avatar = "icon-XX"`.
 *  - `packaged`    — ship a local image file at `manifest.avatar`. Stored as
 *                    `avatar = "<relative path>"`.
 *  - `none`        — neither. Clients that install the package will assign
 *                    a random builtin icon at first install.
 */
type AvatarSelection =
  | { kind: "builtin"; iconId: string }
  | { kind: "packaged"; relativePath: string }
  | { kind: "none" };

export function PublishWizard({
  open,
  agentId,
  agentName,
  onClose,
}: PublishWizardProps) {
  const { t } = useTranslation();
  const STEPS: { key: WizardStep; label: string; icon: React.ElementType }[] = [
    { key: "check", label: t("publishWizard.stepCheck"), icon: CheckCircle },
    { key: "clean", label: t("publishWizard.stepClean"), icon: Brush },
    { key: "build", label: t("publishWizard.stepPackage"), icon: Package },
    { key: "sign", label: t("publishWizard.stepSign"), icon: Key },
    { key: "distribute", label: t("publishWizard.stepDistribute"), icon: FileDown },
  ];
  const [step, setStep] = useState<WizardStep>("check");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Prepare results
  const [checkResult, setCheckResult] = useState<PreparePublishResponse | null>(null);
  const [cleanResult, setCleanResult] = useState<PreparePublishResponse | null>(null);

  // Build results
  const [buildResult, setBuildResult] = useState<BuildPublishResponse | null>(null);
  const [signResult, setSignResult] = useState<BuildPublishResponse | null>(null);

  // Export result
  const [exportResult, setExportResult] = useState<ExportPackageResponse | null>(null);

  // REQ-029 write-back state (see `LibrarySave`)
  const [librarySave, setLibrarySave] = useState<LibrarySave>({ status: "idle" });

  // Agent detail (loaded on open to seed the avatar selection, and to know
  // which node owns the artifact when the write-back cannot reach it)
  const [agentDetail, setAgentDetail] = useState<AgentDetail | null>(null);
  const nodes = useAgentStore((s) => s.nodes);

  // Avatar selection — initialised from agentDetail once it loads, then
  // mutated by the user.
  const [avatar, setAvatar] = useState<AvatarSelection>({ kind: "none" });
  /**
   * Whether `avatar` came from `get_agent_detail` (as opposed to still being
   * the `none` placeholder). The selection is only trustworthy — and therefore
   * only writable — once the real one has been seeded; see `persistAvatar`.
   */
  const [avatarSeeded, setAvatarSeeded] = useState(false);
  /** Selection last written to the manifest, so a build does not rewrite it. */
  const persistedAvatarRef = useRef<string | null>(null);

  // Reset on open
  useEffect(() => {
    if (open) {
      setStep("check");
      setError(null);
      setCheckResult(null);
      setCleanResult(null);
      setBuildResult(null);
      setSignResult(null);
      setExportResult(null);
      setLibrarySave({ status: "idle" });
      setAgentDetail(null);
      setAvatar({ kind: "none" });
      setAvatarSeeded(false);
      persistedAvatarRef.current = null;
    }
  }, [open]);

  // Load agent detail on open to seed the avatar picker
  useEffect(() => {
    if (!open) return;
    let cancelled = false;
    invoke<AgentDetail>("get_agent_detail", { agentId })
      .then((detail) => {
        if (cancelled) return;
        setAgentDetail(detail);
        const seed = deriveAvatarFromManifest(detail.avatar, detail.builtin_avatar);
        setAvatar(seed);
        setAvatarSeeded(true);
      })
      .catch((err) => {
        if (cancelled) return;
        // Non-fatal: the wizard can still proceed without an initial selection.
        log.warn("Failed to load agent detail for publish wizard:", err);
      });
    return () => {
      cancelled = true;
    };
  }, [open, agentId]);

  /**
   * Write the picker's selection into the package manifest — the file the build
   * actually reads — before a build. Returns true when the manifest now agrees
   * with what the picker shows.
   *
   * Called on **every** build, not only after the user moved the picker. The
   * seed comes from `get_agent_detail`, which reports the instance's *effective*
   * avatar: the Gateway's `resolve_identity` prefers the instance's
   * `agent_config.json` (the pick made in the agent's own settings) over the
   * package manifest's default. So an untouched picker can legitimately display
   * one avatar while the manifest still holds a different, packaged default —
   * building on that bakes a picture the user never chose in this wizard. That
   * is the publish-time avatar mismatch reported from a real run (`icon-16` in
   * the config, `icon-15` in the artifact).
   *
   * Two guards keep this from doing harm:
   *   - nothing is written until the seed has loaded, because the placeholder
   *     selection would otherwise **clear** an avatar the user never touched;
   *   - nothing is written twice for the same selection, so the build→sign
   *     pair costs one write.
   */
  const persistAvatar = useCallback(async (): Promise<boolean> => {
    if (!avatarSeeded) return true;
    const key = JSON.stringify(avatar);
    if (persistedAvatarRef.current === key) return true;
    try {
      const avatarField =
        avatar.kind === "packaged" ? avatar.relativePath : avatar.kind === "none" ? "" : null;
      const builtinField = avatar.kind === "builtin" ? avatar.iconId : avatar.kind === "none" ? "" : null;
      // The backend distinguishes "omit" (don't touch) from "set to empty" (clear).
      // We always send both fields, with empty string = clear. null = don't touch.
      await invoke("update_agent_manifest_avatar", {
        agentId,
        avatar: avatarField,
        builtinAvatar: builtinField,
      });
      persistedAvatarRef.current = key;
      return true;
    } catch (err) {
      setError(`Failed to save avatar selection: ${err instanceof Error ? err.message : String(err)}`);
      return false;
    }
  }, [agentId, avatar, avatarSeeded]);

  // Close on Escape — see CreateWizard; `busy` is read at keydown time so a
  // spinner flip no longer re-subscribes the listener.
  useEscapeClose(open, onClose, !busy);

  /**
   * REQ-029 / D12 / plan §3: copy the artifact the node just built into the
   * Local library.
   *
   * Deliberately fire-and-forget from `runSign`: it never sets `busy`, never
   * throws and never blocks the wizard from closing. The command (not this
   * component) decides whether a copy is even legal — in remote/relay mode it
   * answers `remote_skipped` instead of trying to read another machine's disk,
   * and a genuine failure only costs the library row, which is what the retry
   * button is for.
   */
  const copyIntoLibrary = useCallback(async (packagePath: string) => {
    setLibrarySave({ status: "saving" });
    try {
      const result = await savePublishToLibrary(packagePath);
      setLibrarySave(
        result.status === "saved"
          ? { status: "saved", path: result.path ?? "", fileName: result.file_name }
          : { status: "remote_skipped", fileName: result.file_name },
      );
    } catch (e) {
      setLibrarySave({ status: "failed", message: String(e) });
    }
  }, []);

  /**
   * Display name of the node that owns this agent, for the `remote_skipped`
   * message (B5). Falls back to the node id, and to a generic label when the
   * node list has not loaded yet — the sentence must still read correctly.
   */
  const owningNodeLabel = useMemo(() => {
    const nodeId = agentDetail?.node_id;
    if (!nodeId) return null;
    const node = nodes.find((n) => n.node_id === nodeId);
    return node?.node_name || nodeId;
  }, [agentDetail, nodes]);

  const runCheck = async () => {
    setBusy(true);
    setError(null);
    try {
      const result = await invoke<PreparePublishResponse>("prepare_publish", {
        agentId,
        clean: false,
      });
      setCheckResult(result);
      setStep("clean");
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const runClean = async () => {
    setBusy(true);
    setError(null);
    try {
      const result = await invoke<PreparePublishResponse>("prepare_publish", {
        agentId,
        clean: true,
      });
      setCleanResult(result);
      setStep("build");
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const runBuild = async () => {
    setBusy(true);
    setError(null);
    try {
      // Persist the avatar first so the package reflects the selection the
      // picker shows (see `persistAvatar` for why this is not gated on the user
      // having moved it in this session).
      if (!(await persistAvatar())) {
        setBusy(false);
        return;
      }
      const result = await invoke<BuildPublishResponse>("build_publish", {
        agentId,
        sign: false,
      });
      setBuildResult(result);
      setStep("sign");
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const runSign = async () => {
    setBusy(true);
    setError(null);
    try {
      // This build produces the artifact the user distributes (and the one the
      // library write-back copies), so it must not depend on `runBuild` having
      // persisted first; the second call is a no-op for the same selection.
      if (!(await persistAvatar())) {
        setBusy(false);
        return;
      }
      const result = await invoke<BuildPublishResponse>("build_publish", {
        agentId,
        sign: true,
      });
      setSignResult(result);
      setStep("distribute");
      // REQ-029: the signed build is the artifact the user should be able to
      // install from the Local tab. Started after the step flips so the panel
      // renders its "saving" state instead of appearing frozen.
      void copyIntoLibrary(result.output_path);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const runExport = async () => {
    setBusy(true);
    setError(null);
    try {
      const result = await invoke<ExportPackageResponse>("export_package", {
        agentId,
      });
      setExportResult(result);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const stepActions: Record<WizardStep, { label: string; action: () => void } | null> = {
    check: { label: "Run Check", action: runCheck },
    clean: { label: "Run Clean", action: runClean },
    build: { label: "Build Package", action: runBuild },
    sign: { label: "Sign Package", action: runSign },
    distribute: null, // manual actions
  };

  const stepIndex = STEPS.findIndex((s) => s.key === step);
  const currentAction = stepActions[step];

  if (!open) return null;

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      {/* Backdrop */}
      <div
        className="absolute inset-0 bg-modal-overlay"
        onClick={busy ? undefined : onClose}
      />

      {/* Dialog */}
      <div className="relative z-10 flex max-h-[90vh] w-full max-w-2xl flex-col rounded-md border border-border-outer bg-modal-surface shadow-xl">
        {/* Header */}
        <div className="flex items-center gap-2 border-b border-border-divider px-5 py-3 min-h-[var(--ui-dialog-zone-h)]">
          <Package className="h-5 w-5 text-text-tertiary " />
          <h2 className="text-sm font-semibold text-text ">
            Publish: {agentName}
          </h2>
        </div>

        {/* Step indicators */}
        <div className="flex items-center gap-0 border-b border-border-divider px-5 min-h-[var(--ui-dialog-zone-h)]">
          {STEPS.map((s, i) => {
            const Icon = s.icon;
            const active = s.key === step;
            const passed = i < stepIndex;
            return (
              <div key={s.key} className="flex items-center">
                <div
                  className={cn(
                    "flex items-center gap-1.5 rounded-full px-2.5 py-1 text-xs font-medium transition-colors",
                    active && "bg-zinc-200 text-text dark:bg-zinc-700 ",
                    passed && "bg-green-100 text-green-700 dark:bg-green-900/30 dark:text-green-400",
                    !active && !passed && "text-text-tertiary ",
                  )}
                >
                  {passed ? (
                    <Check className="h-3 w-3" />
                  ) : (
                    <Icon className="h-3 w-3" />
                  )}
                  {s.label}
                </div>
                {i < STEPS.length - 1 && (
                  <div
                    className={cn(
                      "mx-1 h-px w-4",
                      i < stepIndex
                        ? "bg-green-300 dark:bg-green-600"
                        : "bg-zinc-200 dark:bg-zinc-600",
                    )}
                  />
                )}
              </div>
            );
          })}
        </div>

        {/* Step content — `min-h-0` lets the flex child shrink below
             its content size so `overflow-y-auto` actually triggers
             (well-known flexbox quirk; see CreateWizard for the
             pattern). */}
        <div className="flex-1 min-h-0 space-y-4 overflow-y-auto px-5 py-4">
          {/* Check results — `max-h-[360px]` + internal scroll matches
              CreateWizard's Preview step: a deeply-broken agent can
              produce dozens of check rows, and the dialog itself
              should not grow beyond the viewport. The outer step
              scroll stays as a safety net. */}
          {checkResult && (
            <div className="max-h-[360px] space-y-2 overflow-y-auto">
              <h3 className="text-xs font-medium text-text-secondary ">
                Check Results
              </h3>
              {checkResult.checks.map((item, i) => (
                <div
                  key={i}
                  className={cn(
                    "flex items-start gap-2 rounded-md px-3 py-2 text-xs",
                    item.status === "ok" && "bg-green-50 text-green-700 dark:bg-green-900/20 dark:text-green-400",
                    item.status === "warn" && "bg-yellow-50 text-yellow-700 dark:bg-yellow-900/20 dark:text-yellow-400",
                    item.status === "error" && "bg-red-50 text-red-700 dark:bg-red-900/20 dark:text-red-400",
                  )}
                >
                  {item.status === "ok" ? (
                    <CheckCircle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
                  ) : item.status === "warn" ? (
                    <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
                  ) : (
                    <XCircle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
                  )}
                  <div>
                    <span className="font-medium">{item.name}</span>
                    {item.detail && (
                      <span className="ml-1 text-text-tertiary ">
                        — {item.detail}
                      </span>
                    )}
                  </div>
                </div>
              ))}
              {checkResult.errors.length > 0 && (
                <ErrorBox
                  message={`${checkResult.errors.length} validation error(s)`}
                  details={checkResult.errors.join("\n")}
                  onClose={() => setCheckResult(null)}
                />
              )}
              {checkResult.warnings.length > 0 && (
                <div className="rounded-md bg-yellow-50 px-3 py-2 dark:bg-yellow-900/20">
                  {checkResult.warnings.map((w, i) => (
                    <p key={i} className="text-xs text-yellow-700 dark:text-yellow-400">
                      {w}
                    </p>
                  ))}
                </div>
              )}
            </div>
          )}

          {/* Clean results */}
          {cleanResult && (
            <div className="space-y-2">
              <h3 className="text-xs font-medium text-text-secondary ">
                Clean Results
              </h3>
              <p className="text-xs text-text-tertiary ">
                {cleanResult.cleaned
                  ? "Cleaned: removed dev flag, cleared recordings, reset config."
                  : "Clean completed (no changes needed)."}
              </p>
            </div>
          )}

          {/* Build step: avatar sub-form + (later) build result */}
          {step === "build" && (
            <div className="space-y-4">
              <AvatarPickerSubForm
                agentId={agentId}
                value={avatar}
                onChange={setAvatar}
                disabled={busy}
              />
              {buildResult && (
                <div className="space-y-2">
                  <h3 className="text-xs font-medium text-text-secondary ">
                    Build Result
                  </h3>
                  <div className="rounded-md bg-green-50 px-3 py-2 text-xs text-green-700 dark:bg-green-900/20 dark:text-green-400">
                    <p>
                      Package built:{" "}
                      <span className="font-mono">{buildResult.output_path}</span>
                    </p>
                    <p>
                      Size:{" "}
                      {(buildResult.file_size / 1024).toFixed(1)} KB
                    </p>
                  </div>
                </div>
              )}
            </div>
          )}

          {/* Sign result */}
          {signResult && (
            <div className="space-y-2">
              <h3 className="text-xs font-medium text-text-secondary ">
                Sign Result
              </h3>
              <div
                className={cn(
                  "rounded-md px-3 py-2 text-xs",
                  signResult.signed
                    ? "bg-green-50 text-green-700 dark:bg-green-900/20 dark:text-green-400"
                    : "bg-yellow-50 text-yellow-700 dark:bg-yellow-900/20 dark:text-yellow-400",
                )}
              >
                <p>
                  Status: {signResult.signed ? t("publishWizard.statusSigned") : t("publishWizard.statusUnsigned")}
                </p>
                <p>
                  <span className="font-mono">{signResult.output_path}</span>
                </p>
              </div>
            </div>
          )}

          {/* Export result */}
          {exportResult && (
            <div className="space-y-2">
              <h3 className="text-xs font-medium text-text-secondary ">
                Export Result
              </h3>
              <div className="rounded-md bg-green-50 px-3 py-2 text-xs text-green-700 dark:bg-green-900/20 dark:text-green-400">
                <p>
                  Status: {exportResult.status}
                </p>
                <p className="font-mono">{exportResult.output_path}</p>
              </div>
            </div>
          )}

          {/* Distribute step - the library write-back + manual export */}
          {step === "distribute" && (
            <div className="space-y-3">
              <h3 className="text-xs font-medium text-text-secondary ">
                Distribute
              </h3>
              <p className="text-xs text-text-tertiary ">
                The package is ready. You can export it or install it locally.
              </p>

              {/*
                REQ-029 / D12: the outcome of copying the artifact into the
                Local library. `remote_skipped` is a success (the package is on
                another machine — nothing to copy, nothing to retry); only a
                real failure offers a retry, and none of these states disable
                Close.
              */}
              {librarySave.status === "saving" && (
                <div className="flex items-center gap-2 rounded-md bg-panel-block px-3 py-2 text-xs text-text-tertiary">
                  <Loader2 className="h-3.5 w-3.5 animate-spin" />
                  <span>{t("common.saving")}</span>
                </div>
              )}

              {librarySave.status === "saved" && (
                <div className="rounded-md bg-green-50 px-3 py-2 text-xs text-green-700 dark:bg-green-900/20 dark:text-green-400">
                  <p className="flex items-center gap-1.5">
                    <Check className="h-3.5 w-3.5" />
                    <span>
                      {t("publishWizard.librarySaved", { file: librarySave.fileName })}
                    </span>
                  </p>
                  <p className="mt-0.5">
                    <span className="font-mono">{librarySave.path}</span>
                  </p>
                </div>
              )}

              {librarySave.status === "remote_skipped" && (
                <div className="rounded-md bg-yellow-50 px-3 py-2 text-xs text-yellow-700 dark:bg-yellow-900/20 dark:text-yellow-400">
                  <p>
                    {t("publishWizard.libraryRemoteSkipped", {
                      node: owningNodeLabel ?? t("publishWizard.libraryRemoteNodeUnknown"),
                      file: librarySave.fileName,
                    })}
                  </p>
                </div>
              )}

              {librarySave.status === "failed" && (
                <div className="rounded-md bg-yellow-50 px-3 py-2 text-xs text-yellow-700 dark:bg-yellow-900/20 dark:text-yellow-400">
                  <p>{t("publishWizard.librarySaveFailed", { error: librarySave.message })}</p>
                  <button
                    onClick={() => void copyIntoLibrary(signResult?.output_path ?? "")}
                    disabled={!signResult || busy}
                    className="mt-1.5 flex items-center gap-1.5 rounded-md border border-border-outer px-2 py-1 font-medium transition-colors hover:bg-zinc-50 disabled:opacity-50 dark:hover:bg-zinc-700"
                  >
                    <RotateCcw className="h-3 w-3" />
                    {t("common.retry")}
                  </button>
                </div>
              )}

              <button
                onClick={runExport}
                disabled={busy}
                className="flex items-center gap-2 rounded-md border border-border-outer px-3 py-1.5 text-xs font-medium text-text-secondary transition-colors hover:bg-zinc-50 disabled:opacity-50  dark:hover:bg-zinc-700"
              >
                {busy ? (
                  <Loader2 className="h-4 w-4 animate-spin" />
                ) : (
                  <ExternalLink className="h-4 w-4" />
                )}
                Export Package
              </button>
            </div>
          )}

          {/* Error */}
          {error && (
            <ErrorBox message={error} onClose={() => setError(null)} />
          )}
        </div>

        {/* Footer */}
        <div className="flex items-center justify-between border-t border-border-divider px-5 min-h-[var(--ui-dialog-zone-h)]">
          <button
            onClick={onClose}
            disabled={busy}
            className="rounded-md px-3 py-1.5 text-xs font-medium text-text-secondary hover:bg-zinc-100 disabled:opacity-50  dark:hover:bg-zinc-700"
          >
            {step === "distribute" ? t("publishWizard.buttonClose") : t("common.cancel")}
          </button>

          {currentAction && (
            <button
              onClick={currentAction.action}
              disabled={busy}
              className="flex items-center gap-2 rounded btn-accent px-3 py-1.5 text-xs font-medium disabled:cursor-not-allowed disabled:opacity-50"
            >
              {busy ? (
                <>
                  <Loader2 className="h-3.5 w-3.5 animate-spin" />
                  Working...
                </>
              ) : (
                currentAction.label
              )}
            </button>
          )}
        </div>
      </div>
    </div>
  );
}

/**
 * Translate a manifest's `avatar` / `builtin_avatar` fields into the
 * wizard's internal `AvatarSelection` representation. Prefers the
 * packaged image (since `manifest.avatar` wins at install time), falling
 * back to a builtin hint, falling back to "none".
 */
function deriveAvatarFromManifest(
  avatar: string | undefined | null,
  builtinAvatar: string | undefined | null,
): AvatarSelection {
  if (avatar) return { kind: "packaged", relativePath: avatar };
  if (builtinAvatar) return { kind: "builtin", iconId: builtinAvatar };
  return { kind: "none" };
}

// ── Avatar sub-form ────────────────────────────────────────────────────

function AvatarPickerSubForm({
  agentId,
  value,
  onChange,
  disabled,
}: {
  agentId: string;
  value: AvatarSelection;
  onChange: (next: AvatarSelection) => void;
  disabled: boolean;
}) {
  const { t } = useTranslation();
  const fileInputRef = useRef<HTMLInputElement | null>(null);
  const [pickerError, setPickerError] = useState<string | null>(null);

  const handlePickBuiltin = (iconId: string) => {
    setPickerError(null);
    onChange({ kind: "builtin", iconId });
  };

  const handlePickNone = () => {
    setPickerError(null);
    onChange({ kind: "none" });
  };

  const handlePickFile = async () => {
    setPickerError(null);
    try {
      const selected = await openDialog({
        multiple: false,
        directory: false,
        filters: [
          {
            name: "Image",
            extensions: ["png", "jpg", "jpeg", "gif", "webp", "svg"],
          },
        ],
      });
      if (!selected || typeof selected !== "string") return;
      await uploadImageFile(agentId, selected);
      // Derive a relative path inside the install dir. We adopt a
      // deterministic location — `assets/avatar.<ext>` — so the manifest
      // reference is stable across rebuilds.
      const ext = selected.split(".").pop()?.toLowerCase() ?? "png";
      const relative = `assets/avatar.${ext}`;
      onChange({ kind: "packaged", relativePath: relative });
    } catch (err) {
      setPickerError(err instanceof Error ? err.message : String(err));
    }
  };

  const handleClearPackaged = () => {
    setPickerError(null);
    onChange({ kind: "none" });
  };

  return (
    <div className="space-y-3">
      <div className="flex items-center gap-2">
        <h3 className="text-xs font-medium text-text-secondary ">
          Default Avatar
        </h3>
        <span className="text-10 text-text-tertiary">
          baked into manifest.toml at build time
        </span>
      </div>

      {/* Current preview */}
      <AvatarPreview selection={value} agentId={agentId} />

      {/* Mode tabs */}
      <div className="grid grid-cols-3 gap-1 rounded-md bg-zinc-100 p-1 text-xs dark:bg-zinc-900/50">
        <ModeTab
          active={value.kind === "builtin"}
          onClick={() => {
            if (value.kind === "builtin") return;
            onChange({ kind: "builtin", iconId: BUILTIN_ICON_IDS[0] ?? "icon-01" });
          }}
          disabled={disabled}
        >
          Builtin icon
        </ModeTab>
        <ModeTab
          active={value.kind === "packaged"}
          onClick={handlePickFile}
          disabled={disabled}
        >
          Local image
        </ModeTab>
        <ModeTab
          active={value.kind === "none"}
          onClick={handlePickNone}
          disabled={disabled}
        >
          No avatar
        </ModeTab>
      </div>

      {/* Builtin grid */}
      {value.kind === "builtin" && (
        <div className="rounded-md border border-border-outer p-2">
          <div className="grid grid-cols-7 gap-1.5">
            {BUILTIN_ICON_IDS.map((iconId) => {
              const active = value.iconId === iconId;
              return (
                <button
                  key={iconId}
                  type="button"
                  onClick={() => handlePickBuiltin(iconId)}
                  disabled={disabled}
                  className={cn(
                    "flex aspect-square items-center justify-center rounded-md p-0.5 transition-colors",
                    active
                      ? "bg-zinc-200 dark:bg-zinc-600"
                      : "hover:bg-zinc-100 dark:hover:bg-zinc-700",
                  )}
                  title={iconId}
                >
                  <img
                    src={BUILTIN_ICONS[iconId] ?? ""}
                    alt={iconId}
                    draggable={false}
                    className="h-full w-full rounded-full object-cover"
                  />
                </button>
              );
            })}
          </div>
        </div>
      )}

      {/* Packaged info */}
      {value.kind === "packaged" && (
        <div className="flex items-center justify-between rounded-md border border-border-outer px-3 py-2 text-xs">
          <div className="min-w-0">
            <div className="font-mono text-text-secondary ">
              {value.relativePath}
            </div>
            <div className="text-10 text-text-tertiary">
              ship as part of the .agent package
            </div>
          </div>
          <div className="flex items-center gap-1">
            <button
              type="button"
              onClick={handlePickFile}
              disabled={disabled}
              className="rounded-md px-2 py-1 text-10 text-text-tertiary hover:bg-zinc-100 hover:text-zinc-800 disabled:opacity-50 dark:hover:bg-zinc-700 dark:hover:text-zinc-200"
            >
              Replace
            </button>
            <button
              type="button"
              onClick={handleClearPackaged}
              disabled={disabled}
              className="rounded-md px-2 py-1 text-10 text-text-tertiary hover:bg-zinc-100 hover:text-red-600 disabled:opacity-50 dark:hover:bg-zinc-700"
              aria-label={t("publishWizard.ariaLabelRemoveAvatar")}
            >
              <XIcon className="h-3.5 w-3.5" />
            </button>
          </div>
        </div>
      )}

      {/* None — random fallback note */}
      {value.kind === "none" && (
        <p className="rounded-md border border-dashed border-border-outer px-3 py-2 text-11 text-text-tertiary">
          The client will assign a random builtin icon on first install.
        </p>
      )}

      {pickerError && (
        <ErrorBox message={pickerError} onClose={() => setPickerError(null)} />
      )}

      {/* Hidden file input — currently unused (we use Tauri dialog instead)
          but kept here for potential future "drop a file here" affordance. */}
      <input
        ref={fileInputRef}
        type="file"
        accept="image/*"
        className="hidden"
      />
    </div>
  );
}

function ModeTab({
  active,
  onClick,
  disabled,
  children,
}: {
  active: boolean;
  onClick: () => void;
  disabled?: boolean;
  children: React.ReactNode;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled}
      className={cn(
        "rounded-md px-2 py-1 font-medium transition-colors disabled:opacity-50",
        active
          ? "bg-modal-surface text-text shadow-sm "
          : "text-text-tertiary hover:text-zinc-800  dark:hover:text-zinc-200",
      )}
    >
      {children}
    </button>
  );
}

function AvatarPreview({ selection, agentId }: { selection: AvatarSelection; agentId: string }) {
  if (selection.kind === "builtin") {
    const src = BUILTIN_ICONS[selection.iconId] ?? BUILTIN_ICONS["icon-01"];
    return (
      <div className="flex items-center gap-3 rounded-md border border-border-outer px-3 py-2">
        <img
          src={src}
          alt={selection.iconId}
          draggable={false}
          className="h-16 w-16 rounded-full object-cover ring-1 ring-zinc-300/60 dark:ring-zinc-600/60"
        />
        <div>
          <div className="text-xs font-medium text-text-secondary ">
            Builtin icon: <span className="font-mono">{selection.iconId}</span>
          </div>
          <div className="text-10 text-text-tertiary">
            stored as <span className="font-mono">builtin_avatar</span> in manifest.toml
          </div>
        </div>
      </div>
    );
  }
  if (selection.kind === "packaged") {
    // Preview the packaged image by hitting the gateway avatar endpoint.
    // The endpoint serves the file only if the manifest has avatar set; the
    // preview works after the user has uploaded the file (regardless of
    // whether manifest.toml has been updated yet).
    const url = resolveAgentAvatarUrl(agentId);
    return (
      <div className="flex items-center gap-3 rounded-md border border-border-outer px-3 py-2">
        <AuthedImage
          src={url}
          alt={selection.relativePath}
          className="h-16 w-16 rounded-full object-cover ring-1 ring-zinc-300/60 dark:ring-zinc-600/60"
          // Nothing to show until the file is readable — keep the slot.
          fallback={<div className="h-16 w-16" />}
        />
        <div>
          <div className="text-xs font-medium text-text-secondary ">
            Local image: <span className="font-mono">{selection.relativePath}</span>
          </div>
          <div className="text-10 text-text-tertiary">
            stored as <span className="font-mono">avatar</span> in manifest.toml
          </div>
        </div>
      </div>
    );
  }
  return (
    <div className="flex items-center gap-3 rounded-md border border-dashed border-border-outer px-3 py-2">
      <div className="flex h-16 w-16 items-center justify-center rounded-full bg-zinc-100 text-text-tertiary dark:bg-zinc-800">
        <ImagePlus className="h-6 w-6" />
      </div>
      <div>
        <div className="text-xs font-medium text-text-secondary ">
          No avatar selected
        </div>
        <div className="text-10 text-text-tertiary">
          clients will fall back to a random builtin icon at install time
        </div>
      </div>
    </div>
  );
}

// ── Helpers ────────────────────────────────────────────────────────────

/**
 * Upload a user-selected image file to the gateway's
 * `POST /api/agents/{id}/manifest/file` endpoint. The gateway restricts
 * the destination path to image extensions and canonicalises the path
 * to prevent escape from the install dir.
 */
async function uploadImageFile(agentId: string, filePath: string): Promise<void> {
  // Derive the relative path. We adopt `assets/avatar.<ext>` so the
  // manifest reference is stable. The server will create the `assets/`
  // directory if it doesn't exist.
  const ext = filePath.split(".").pop()?.toLowerCase() ?? "png";
  const relative = `assets/avatar.${ext}`;
  await invoke("upload_agent_file", {
    agentId,
    relativePath: relative,
    filePath,
  });
}
