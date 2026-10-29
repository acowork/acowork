//! ADR-104: the Create Agent wizard — five steps, one draft.
//!
//! ## What changed and why
//!
//! The wizard used to be four steps (basic / tools / skills / preview) and
//! silently dropped half of what a manifest can declare: no `[config]`
//! defaults, no `labels`, no `[[mcp_servers]]`, and `skills` were mapped 1:1
//! onto `[capabilities.*]` — a claim that is simply false (ADR-104: a skill is
//! an internal workflow, a capability is an outward contract).
//!
//! Two structural rules replace that:
//!
//! 1. **The preview is not a second implementation.** The old step 4
//!    hand-assembled TOML in TypeScript and began with a `[package]` table the
//!    manifest schema does not have. It now calls `preview_manifest(draft)`,
//!    which runs the same `ManifestDraft::build` that `create_agent` runs, so
//!    what the user reads is byte-identical to what gets written.
//! 2. **All decisions live in [lib/createWizardDraft.ts](src/lib/createWizardDraft.ts).**
//!    This component owns layout and in-flight state only; the mapping
//!    ("" ≠ 0, absent ≠ empty) and the per-step gate are pure functions with
//!    their own tests. A dialog that both decides and draws is untestable.
//!
//! ## Step contract
//!
//! | step    | declares                                   | may advance when            |
//! |---------|--------------------------------------------|-----------------------------|
//! | basic   | identity + `labels`                        | id/name valid, ≤12 labels   |
//! | config  | `[config]` defaults                        | every filled field in range |
//! | tools   | `[[tools]]` + `[[mcp_servers]]`            | always (all optional)        |
//! | skills  | skill ZIPs + `[capabilities.*]`            | zips parsed, actions valid  |
//! | preview | —                                          | Rust reports no error       |

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { cn } from "../../lib/utils";
import {
  Sparkles,
  Bot,
  SlidersHorizontal,
  Wrench,
  PackagePlus,
  Check,
  ChevronDown,
  ChevronRight,
  Loader2,
  Plus,
  X as XIcon,
} from "lucide-react";
import { StyledInput, StyledTextarea } from "../common/StyledInput";
import { TagInput } from "../common/TagInput";
import { useEscapeClose } from "../../hooks/useEscapeClose";
import { Switch } from "../common/Switch";
import { ListBox, ListRow, Badge } from "../common/list";
import { useTranslation } from "../../i18n/useTranslation";
import { ErrorBox } from "../common/ErrorBox";
import { fetchMcpPresets, groupPresetsByCategory, type McpPreset } from "../../lib/mcpPresets";
import {
  CAPABILITY_ACTION_RE,
  EMPTY_WIZARD_STATE,
  PLATFORM_HOSTED_MCP,
  stepIssues,
  toCreateAgentDraft,
  undeclaredToolDeps,
  type CapabilityInput,
  type McpPick,
  type SkillEntry,
  type StepIssue,
  type WizardState,
  type WizardStepKey,
} from "../../lib/createWizardDraft";
import { MAX_LABELS, configFieldError, isValidAgentId, isValidLabel } from "../../lib/manifestRules";

interface CreateWizardProps {
  open: boolean;
  onCreated: (agentId: string) => void;
  onClose: () => void;
}

interface BuiltinToolInfo {
  name: string;
  description: string;
  group?: string;
}

/** Result of `preview_manifest` (mirrors `ManifestPreview` in Rust). */
interface ManifestPreview {
  toml: string;
  errors: string[];
  warnings: string[];
}

/** One uploaded ZIP while it is being parsed / after a parse failure. */
interface SkillRow {
  uid: string;
  fileName: string;
  status: "parsing" | "ready" | "error";
  error?: string;
  entry?: SkillEntry;
}

type StepIcon = { key: WizardStepKey; icon: React.ElementType; i18nKey: string };
const STEP_ICONS: StepIcon[] = [
  { key: "basic", icon: Bot, i18nKey: "stepBasic" },
  { key: "config", icon: SlidersHorizontal, i18nKey: "stepConfig" },
  { key: "tools", icon: Wrench, i18nKey: "stepTools" },
  { key: "skills", icon: PackagePlus, i18nKey: "stepSkills" },
  { key: "preview", icon: Check, i18nKey: "stepPreview" },
];

let uidCounter = 0;
/** Local unique id for a React list row (never persisted, never in the manifest). */
function nextUid(prefix: string): string {
  uidCounter += 1;
  return `${prefix}-${uidCounter}`;
}

export function CreateWizard({ open, onCreated, onClose }: CreateWizardProps) {
  const { t } = useTranslation();
  const STEPS = STEP_ICONS.map((s) => ({ ...s, label: t(`createWizard.${s.i18nKey}`) }));

  const [step, setStep] = useState<WizardStepKey>("basic");
  const [state, setState] = useState<WizardState>({ ...EMPTY_WIZARD_STATE });
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  /** A label the user typed that `manifest::is_valid_label` would refuse. */
  const [labelError, setLabelError] = useState<string | null>(null);

  const [builtinTools, setBuiltinTools] = useState<BuiltinToolInfo[]>([]);
  const [presets, setPresets] = useState<McpPreset[]>([]);
  const [presetsError, setPresetsError] = useState<string | null>(null);

  const [skillRows, setSkillRows] = useState<SkillRow[]>([]);
  /** `[capabilities.*]` rows the user expanded the JSON area of. */
  const [openSchema, setOpenSchema] = useState<Set<string>>(new Set());

  const [preview, setPreview] = useState<ManifestPreview | null>(null);
  const [previewError, setPreviewError] = useState<string | null>(null);
  const [previewBusy, setPreviewBusy] = useState(false);

  // Reset on open. Every piece of wizard state resets together: leaving one
  // behind (the old code forgot `skills` on a failed create) means the next
  // open shows a draft the user never made.
  useEffect(() => {
    if (!open) return;
    setStep("basic");
    setState({ ...EMPTY_WIZARD_STATE, config: { ...EMPTY_WIZARD_STATE.config } });
    setBusy(false);
    setError(null);
    setLabelError(null);
    setSkillRows([]);
    setOpenSchema(new Set());
    setPreview(null);
    setPreviewError(null);
  }, [open]);

  // Static catalogs, loaded once per open: the builtin tool list comes from
  // `acowork-core` (shared with the Runtime), the preset registry from the
  // Gateway (shared with every client).
  useEffect(() => {
    if (!open) return;
    let cancelled = false;
    invoke<BuiltinToolInfo[]>("list_builtin_tools")
      .then((list) => {
        if (!cancelled) setBuiltinTools(list);
      })
      .catch((e) => {
        if (!cancelled) setError(`Failed to load builtin tool catalog: ${e}`);
      });
    fetchMcpPresets()
      .then((list) => {
        if (!cancelled) setPresets(list);
      })
      .catch((e: unknown) => {
        // A missing registry must not block authoring: step 3 still offers the
        // platform-hosted servers, which need no catalog entry.
        if (!cancelled) setPresetsError(e instanceof Error ? e.message : String(e));
      });
    return () => {
      cancelled = true;
    };
  }, [open]);

  useEscapeClose(open, onClose, !busy);

  /** Ready skills are the ones the manifest may reference. */
  const skills = useMemo(
    () => skillRows.filter((r): r is SkillRow & { entry: SkillEntry } => r.status === "ready" && !!r.entry).map((r) => r.entry),
    [skillRows],
  );

  /**
   * The state every pure helper sees: `state.skills` is derived from the parsed
   * rows, so a half-uploaded ZIP can never leak into the draft.
   */
  const draftState: WizardState = useMemo(() => ({ ...state, skills }), [state, skills]);

  const issues = stepIssues(step, draftState);
  /** A parse in flight or a failed parse blocks leaving step 4. */
  const skillsUnparsed = skillRows.some((r) => r.status !== "ready");
  const canAdvance =
    issues.length === 0 && (step !== "skills" || !skillsUnparsed) && !busy;

  const stepIndex = STEPS.findIndex((s) => s.key === step);
  const isLast = stepIndex === STEPS.length - 1;

  const patch = useCallback((p: Partial<WizardState>) => {
    setState((prev) => ({ ...prev, ...p }));
  }, []);

  /* ---- labels ---------------------------------------------------------- */

  const addLabel = (raw: string) => {
    const token = raw.trim().toLowerCase();
    // `manifestRules.isValidLabel` mirrors `manifest::is_valid_label`; a token it
    // refuses would make `validate()` fail at the end of an otherwise fine wizard.
    if (!isValidLabel(token)) {
      setLabelError(t("createWizard.labelInvalid", { label: token }));
      return;
    }
    setLabelError(null);
    setState((prev) =>
      prev.labels.includes(token) || prev.labels.length >= MAX_LABELS
        ? prev
        : { ...prev, labels: [...prev.labels, token] },
    );
  };

  const removeLabel = (index: number) => {
    setLabelError(null);
    setState((prev) => ({ ...prev, labels: prev.labels.filter((_, i) => i !== index) }));
  };

  /* ---- skills ---------------------------------------------------------- */

  /**
   * Parse each dropped ZIP through the backend (`parse_skill_zip_preview`), the
   * same parser `create_agent` uses — so the row's name/description/`tool_deps`
   * cannot disagree with what actually gets installed.
   */
  const addSkillFiles = async (files: File[]) => {
    const rows: SkillRow[] = files.map((f) => ({
      uid: nextUid("skill"),
      fileName: f.name,
      status: "parsing",
    }));
    setSkillRows((prev) => [...prev, ...rows]);

    await Promise.all(
      files.map(async (file, i) => {
        const row = rows[i];
        try {
          const buf = new Uint8Array(await file.arrayBuffer());
          const parsed = await invoke<{ name: string; description: string; tool_deps: string[] }>(
            "parse_skill_zip_preview",
            { zipBytes: Array.from(buf) },
          );
          setSkillRows((prev) => {
            // Two ZIPs with the same SKILL.md name would produce two
            // `skills/<name>/` directories — `create_agent` refuses it, so the
            // row does too instead of failing at the very end.
            const clash = prev.some(
              (r) => r.uid !== row.uid && r.status === "ready" && r.entry?.name === parsed.name,
            );
            return prev.map((r) =>
              r.uid === row.uid
                ? clash
                  ? { ...r, status: "error", error: `Duplicate skill name '${parsed.name}'` }
                  : {
                      ...r,
                      status: "ready",
                      entry: {
                        name: parsed.name,
                        description: parsed.description,
                        toolDeps: parsed.tool_deps ?? [],
                        bytes: Array.from(buf),
                      },
                    }
                : r,
            );
          });
        } catch (e) {
          setSkillRows((prev) =>
            prev.map((r) =>
              r.uid === row.uid ? { ...r, status: "error", error: String(e) } : r,
            ),
          );
        }
      }),
    );
  };

  const removeSkill = (uid: string) => {
    setSkillRows((prev) => prev.filter((r) => r.uid !== uid));
    // A removed skill must not stay linked from a capability: Rust turns a
    // dangling edge into a hard error, and the user would not see why.
    setState((prev) => {
      const remaining = new Set(
        skillRows.filter((r) => r.uid !== uid && r.entry).map((r) => r.entry!.name),
      );
      return {
        ...prev,
        capabilities: prev.capabilities.map((c) => ({
          ...c,
          skills: c.skills.filter((s) => remaining.has(s)),
        })),
      };
    });
  };

  const undeclared = useMemo(() => undeclaredToolDeps(draftState), [draftState]);

  /* ---- capabilities ---------------------------------------------------- */

  const addCapability = () => {
    setState((prev) => ({
      ...prev,
      capabilities: [
        ...prev.capabilities,
        {
          uid: nextUid("cap"),
          action: "",
          description: "",
          skills: [],
          inputSchema: "",
          outputSchema: "",
        },
      ],
    }));
  };

  const patchCapability = (uid: string, p: Partial<CapabilityInput>) => {
    setState((prev) => ({
      ...prev,
      capabilities: prev.capabilities.map((c) => (c.uid === uid ? { ...c, ...p } : c)),
    }));
  };

  const removeCapability = (uid: string) => {
    setState((prev) => ({ ...prev, capabilities: prev.capabilities.filter((c) => c.uid !== uid) }));
  };

  const toggleCapabilitySkill = (uid: string, skill: string) => {
    setState((prev) => ({
      ...prev,
      capabilities: prev.capabilities.map((c) =>
        c.uid === uid
          ? {
              ...c,
              skills: c.skills.includes(skill)
                ? c.skills.filter((s) => s !== skill)
                : [...c.skills, skill],
            }
          : c,
      ),
    }));
  };

  /* ---- builtin tools --------------------------------------------------- */

  const toggleTool = (name: string) => {
    setState((prev) => ({
      ...prev,
      tools: prev.tools.includes(name)
        ? prev.tools.filter((n) => n !== name)
        : [...prev.tools, name],
    }));
  };

  /* ---- MCP picks ------------------------------------------------------- */

  const toggleHosted = (id: string) => {
    setState((prev) => {
      const exists = prev.mcp.some((p) => p.kind === "hosted" && p.id === id);
      return {
        ...prev,
        mcp: exists
          ? prev.mcp.filter((p) => !(p.kind === "hosted" && p.id === id))
          : [
              ...prev.mcp,
              {
                kind: "hosted",
                id,
                why: "",
                required: false,
                // Platform-hosted servers need no user-supplied secret.
                requiredSecrets: [],
                label: hostedLabel(id, t),
              },
            ],
      };
    });
  };

  const togglePreset = (preset: McpPreset) => {
    setState((prev) => {
      const exists = prev.mcp.some((p) => p.kind === "preset" && p.id === preset.id);
      return {
        ...prev,
        mcp: exists
          ? prev.mcp.filter((p) => !(p.kind === "preset" && p.id === preset.id))
          : [
              ...prev.mcp,
              {
                kind: "preset",
                id: preset.id,
                why: "",
                required: false,
                // Secrets are the preset's fact, not the user's input (ADR-090 D4).
                requiredSecrets: [...preset.required_env],
                label: preset.name,
              },
            ],
      };
    });
  };

  const patchPick = (kind: McpPick["kind"], id: string, p: Partial<McpPick>) => {
    setState((prev) => ({
      ...prev,
      mcp: prev.mcp.map((pick) =>
        pick.kind === kind && pick.id === id ? { ...pick, ...p } : pick,
      ),
    }));
  };

  /* ---- preview --------------------------------------------------------- */

  /**
   * Build the preview from the backend on entering step 5 (and after any edit
   * made from it). One request, no debounce: the step is entered once per
   * visit, and `preview_manifest` is pure CPU over an in-memory draft.
   */
  const loadPreview = useCallback(async () => {
    setPreviewBusy(true);
    setPreviewError(null);
    try {
      const result = await invoke<ManifestPreview>("preview_manifest", {
        draft: toCreateAgentDraft(draftState),
      });
      setPreview(result);
    } catch (e) {
      // `build` rejects a draft it cannot turn into a valid manifest, so the
      // blocking problems arrive as one joined message — that IS the error list.
      setPreview(null);
      setPreviewError(String(e));
    } finally {
      setPreviewBusy(false);
    }
  }, [draftState]);

  useEffect(() => {
    if (!open || step !== "preview") return;
    void loadPreview();
  }, [open, step, loadPreview]);

  const handleCreate = async () => {
    setBusy(true);
    setError(null);
    try {
      const agentId = await invoke<string>("create_agent", {
        draft: toCreateAgentDraft(draftState),
      });
      onCreated(agentId);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const handleNext = () => {
    if (isLast) {
      void handleCreate();
      return;
    }
    const next = STEPS[stepIndex + 1];
    if (next) setStep(next.key);
  };

  const handleBack = () => {
    const prev = STEPS[stepIndex - 1];
    if (prev) setStep(prev.key);
  };

  if (!open) return null;

  /** Blocking issues for the current step, as translated lines. */
  const issueLines = issues.map((issue) => issueText(issue, t)).filter((s): s is string => !!s);

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      <div className="absolute inset-0 bg-modal-overlay" onClick={busy ? undefined : onClose} />

      <div className="relative z-10 flex max-h-[90vh] w-full max-w-2xl flex-col rounded-md border border-border-outer bg-modal-surface shadow-xl">
        {/* Header */}
        <div className="flex items-center justify-between border-b border-border-divider px-5 py-3 min-h-[var(--ui-dialog-zone-h)]">
          <div className="flex items-center gap-2">
            <Sparkles className="h-5 w-5 text-text-tertiary" />
            <h2 className="text-sm font-semibold text-text">{t("createWizard.headerTitle")}</h2>
          </div>
          <button
            onClick={busy ? undefined : onClose}
            disabled={busy}
            aria-label={t("agentDetailDialog.ariaLabelClose")}
            className="text-text-tertiary hover:text-zinc-600 disabled:opacity-50 dark:hover:text-zinc-300"
          >
            <XIcon className="h-4 w-4" />
          </button>
        </div>

        {/* Step rail */}
        <div className="flex items-center gap-0 overflow-x-auto border-b border-border-divider px-5 min-h-[var(--ui-dialog-zone-h)]">
          {STEPS.map((s, i) => {
            const Icon = s.icon;
            const active = s.key === step;
            const passed = i < stepIndex;
            return (
              <div key={s.key} className="flex shrink-0 items-center">
                <div
                  className={cn(
                    "flex items-center gap-1.5 rounded-full px-2.5 py-1 text-xs font-medium transition-colors",
                    active && "bg-zinc-200 text-text dark:bg-zinc-700",
                    passed && "bg-green-100 text-green-700 dark:bg-green-900/30 dark:text-green-400",
                    !active && !passed && "text-text-tertiary",
                  )}
                >
                  {passed ? <Check className="h-3 w-3" /> : <Icon className="h-3 w-3" />}
                  {s.label}
                </div>
                {i < STEPS.length - 1 && (
                  <div
                    className={cn(
                      "mx-1 h-px w-4",
                      i < stepIndex ? "bg-green-300 dark:bg-green-600" : "bg-zinc-200 dark:bg-zinc-600",
                    )}
                  />
                )}
              </div>
            );
          })}
        </div>

        {/* Body — `min-h-0` lets the flex child shrink so `overflow-y-auto` wins
            over pushing the dialog off-screen. */}
        <div className="flex-1 min-h-0 space-y-4 overflow-y-auto px-5 py-4">
          {step === "basic" && (
            <div className="space-y-3">
              <Field label={t("createWizard.fieldAgentId")} required hint={t("createWizard.fieldAgentIdHint")}>
                <StyledInput
                  type="text"
                  value={state.agentId}
                  onChange={(e) => patch({ agentId: e.target.value })}
                  placeholder="com.example.myagent"
                  aria-label={t("createWizard.fieldAgentId")}
                  aria-invalid={state.agentId.trim() !== "" && !isValidAgentId(state.agentId) || undefined}
                  className={cn(
                    state.agentId.trim() !== "" && !isValidAgentId(state.agentId) && "border-red-400",
                  )}
                  fontMono
                />
              </Field>
              <div className="grid grid-cols-2 gap-3">
                <Field label={t("createWizard.fieldName")} required>
                  <StyledInput
                    type="text"
                    value={state.name}
                    onChange={(e) => patch({ name: e.target.value })}
                    placeholder={t("createWizard.placeholderAgentName")}
                    aria-label={t("createWizard.fieldName")}
                  />
                </Field>
                <Field label={t("createWizard.fieldRole")}>
                  <StyledInput
                    type="text"
                    value={state.role}
                    onChange={(e) => patch({ role: e.target.value })}
                    placeholder={t("createWizard.fieldRolePlaceholder")}
                    aria-label={t("createWizard.fieldRole")}
                  />
                </Field>
              </div>
              <div className="grid grid-cols-2 gap-3">
                <Field label={t("createWizard.fieldVersion")}>
                  <StyledInput
                    type="text"
                    value={state.version}
                    onChange={(e) => patch({ version: e.target.value })}
                    placeholder="0.1.0"
                    aria-label={t("createWizard.fieldVersion")}
                  />
                </Field>
                <Field label={t("createWizard.fieldAuthor")}>
                  <StyledInput
                    type="text"
                    value={state.author}
                    onChange={(e) => patch({ author: e.target.value })}
                    placeholder={t("createWizard.placeholderYourName")}
                    aria-label={t("createWizard.fieldAuthor")}
                  />
                </Field>
              </div>
              <Field label={t("createWizard.fieldDescription")}>
                <StyledTextarea
                  value={state.description}
                  onChange={(e) => patch({ description: e.target.value })}
                  placeholder={t("createWizard.placeholderDescribe")}
                  rows={2}
                  className="resize-none"
                  aria-label={t("createWizard.fieldDescription")}
                />
              </Field>
              <Field label={t("createWizard.fieldLabels")} hint={t("createWizard.labelsHint")}>
                <TagInput
                  value={state.labels}
                  onAdd={addLabel}
                  onRemove={removeLabel}
                  placeholder={t("createWizard.labelPlaceholder")}
                  removeLabel={t("createWizard.labelRemove")}
                  ariaLabel={t("createWizard.fieldLabels")}
                  disabled={busy}
                  invalid={!!labelError}
                />
                {labelError && <p className="mt-1 text-10 text-red-500">{labelError}</p>}
              </Field>
            </div>
          )}

          {step === "config" && (
            <div className="space-y-3">
              <p className="text-xs text-text-tertiary">{t("createWizard.configHint")}</p>
              <ConfigNumberField
                label={t("createWizard.configTemperature")}
                desc={t("createWizard.configTemperatureDesc")}
                fallback="0.3"
                value={state.config.temperature}
                error={configFieldError("temperature", state.config.temperature)}
                onChange={(v) => patch({ config: { ...state.config, temperature: v } })}
                disabled={busy}
              />
              <ConfigNumberField
                label={t("createWizard.configContextWindow")}
                desc={t("createWizard.configContextWindowDesc")}
                fallback="200000"
                value={state.config.context_window}
                error={configFieldError("context_window", state.config.context_window)}
                onChange={(v) => patch({ config: { ...state.config, context_window: v } })}
                disabled={busy}
              />
              <ConfigNumberField
                label={t("createWizard.configMaxOutputTokens")}
                desc={t("createWizard.configMaxOutputTokensDesc")}
                fallback="200000"
                value={state.config.max_output_tokens}
                error={configFieldError("max_output_tokens", state.config.max_output_tokens)}
                onChange={(v) => patch({ config: { ...state.config, max_output_tokens: v } })}
                disabled={busy}
              />
              <ConfigNumberField
                label={t("createWizard.configMaxIterations")}
                desc={t("createWizard.configMaxIterationsDesc")}
                fallback="500"
                value={state.config.max_iterations}
                error={configFieldError("max_iterations", state.config.max_iterations)}
                onChange={(v) => patch({ config: { ...state.config, max_iterations: v } })}
                disabled={busy}
              />
              <ConfigNumberField
                label={t("createWizard.configCompression")}
                desc={t("createWizard.configCompressionDesc")}
                fallback="0.9"
                value={state.config.compression_ratio_threshold}
                error={configFieldError(
                  "compression_ratio_threshold",
                  state.config.compression_ratio_threshold,
                )}
                onChange={(v) =>
                  patch({ config: { ...state.config, compression_ratio_threshold: v } })
                }
                disabled={busy}
              />
            </div>
          )}

          {step === "tools" && (
            <div className="space-y-4">
              <section className="space-y-2">
                <p className="text-xs text-text-tertiary">{t("createWizard.toolsHint")}</p>
                {builtinTools.length === 0 ? (
                  <div className="text-xs text-text-tertiary">{t("common.loading")}</div>
                ) : (
                  <ListBox variant="plain" maxHeight={220}>
                    {builtinTools.map((tool) => {
                      const checked = state.tools.includes(tool.name);
                      return (
                        // The Switch is the only control: a clickable row would
                        // nest its <label> inside the row's <button> and toggle
                        // twice per click (the same reason the right panel's
                        // Builtin Tools list has no row-level onClick).
                        <ListRow
                          key={tool.name}
                          selected={checked}
                          padding="default"
                          trailing={
                            <Switch
                              checked={checked}
                              onChange={() => toggleTool(tool.name)}
                              disabled={busy}
                              size="sm"
                              aria-label={tool.name}
                            />
                          }
                        >
                          <div className="min-w-0 flex-1">
                            <p className="truncate font-mono text-11 font-medium text-text-secondary">
                              {tool.name}
                            </p>
                            <p className="truncate text-10 text-text-tertiary">{tool.description}</p>
                          </div>
                        </ListRow>
                      );
                    })}
                  </ListBox>
                )}
              </section>

              <section className="space-y-2">
                <div>
                  <p className="text-xs font-medium text-text-secondary">{t("createWizard.mcpTitle")}</p>
                  <p className="text-10 text-text-tertiary">{t("createWizard.mcpHint")}</p>
                </div>

                <p className="text-10 font-medium uppercase tracking-wide text-text-tertiary">
                  {t("createWizard.mcpHostedGroup")}
                </p>
                <ListBox variant="plain">
                  {PLATFORM_HOSTED_MCP.map((id) => {
                    const checked = state.mcp.some((p) => p.kind === "hosted" && p.id === id);
                    return (
                      <ListRow
                        key={id}
                        selected={checked}
                        padding="default"
                        trailing={
                          <Switch
                            checked={checked}
                            onChange={() => toggleHosted(id)}
                            disabled={busy}
                            size="sm"
                            aria-label={id}
                          />
                        }
                      >
                        <div className="min-w-0 flex-1">
                          <p className="truncate font-mono text-11 font-medium text-text-secondary">{id}</p>
                          <p className="truncate text-10 text-text-tertiary">{hostedLabel(id, t)}</p>
                        </div>
                      </ListRow>
                    );
                  })}
                </ListBox>

                <p className="text-10 font-medium uppercase tracking-wide text-text-tertiary">
                  {t("createWizard.mcpPresetGroup")}
                </p>
                {presetsError ? (
                  <p className="text-10 text-text-tertiary">{presetsError}</p>
                ) : presets.length === 0 ? (
                  <p className="text-10 text-text-tertiary">{t("common.loading")}</p>
                ) : (
                  groupPresetsByCategory(presets).map(([category, list]) => (
                    <div key={category} className="space-y-1">
                      <p className="text-10 text-text-tertiary">{category}</p>
                      <ListBox variant="plain">
                        {list.map((preset) => {
                          const checked = state.mcp.some(
                            (p) => p.kind === "preset" && p.id === preset.id,
                          );
                          return (
                            <ListRow
                              key={preset.id}
                              selected={checked}
                              padding="default"
                              trailing={
                                <Switch
                                  checked={checked}
                                  onChange={() => togglePreset(preset)}
                                  disabled={busy}
                                  size="sm"
                                  aria-label={preset.name}
                                />
                              }
                            >
                              <div className="min-w-0 flex-1">
                                <p className="truncate text-11 font-medium text-text-secondary">
                                  {preset.name}
                                </p>
                                <p className="truncate text-10 text-text-tertiary">
                                  {preset.description}
                                  {preset.required_env.length > 0 &&
                                    ` · ${preset.required_env.join(", ")}`}
                                </p>
                              </div>
                            </ListRow>
                          );
                        })}
                      </ListBox>
                    </div>
                  ))
                )}

                {state.mcp.length > 0 && (
                  <div className="space-y-2 pt-1">
                    <p className="text-10 font-medium uppercase tracking-wide text-text-tertiary">
                      {t("createWizard.mcpSelectedCount", { count: state.mcp.length })}
                    </p>
                    {state.mcp.map((pick) => (
                      <div
                        key={`${pick.kind}:${pick.id}`}
                        className="space-y-2 rounded-md border border-border-divider px-3 py-2"
                      >
                        <div className="flex items-center justify-between gap-2">
                          <span className="truncate font-mono text-11 text-text-secondary">
                            {pick.label}
                            <span className="ml-1 text-text-tertiary">({pick.id})</span>
                          </span>
                          <Switch
                            checked={pick.required}
                            onChange={() =>
                              patchPick(pick.kind, pick.id, { required: !pick.required })
                            }
                            disabled={busy}
                            size="sm"
                            label={t("createWizard.mcpRequiredLabel")}
                          />
                        </div>
                        <StyledInput
                          type="text"
                          value={pick.why}
                          onChange={(e) => patchPick(pick.kind, pick.id, { why: e.target.value })}
                          placeholder={t("createWizard.mcpWhyPlaceholder")}
                          aria-label={`${t("createWizard.mcpWhyLabel")} ${pick.id}`}
                          disabled={busy}
                        />
                        {pick.required && (
                          <p className="text-10 text-text-tertiary">{t("createWizard.mcpRequiredHint")}</p>
                        )}
                      </div>
                    ))}
                    <p className="text-10 text-text-tertiary">{t("createWizard.mcpToolsHint")}</p>
                  </div>
                )}
              </section>
            </div>
          )}

          {step === "skills" && (
            <div className="space-y-4">
              <p className="text-xs text-text-tertiary">{t("createWizard.skillsHint")}</p>
              <SkillDropzone
                rows={skillRows}
                onAdd={addSkillFiles}
                onRemove={removeSkill}
                disabled={busy}
                undeclared={undeclared}
              />

              <section className="space-y-2">
                <div className="flex items-start justify-between gap-2">
                  <div>
                    <p className="text-xs font-medium text-text-secondary">
                      {t("createWizard.capabilityTitle")}
                    </p>
                    <p className="text-10 text-text-tertiary">{t("createWizard.capabilityHint")}</p>
                  </div>
                  <button
                    type="button"
                    onClick={addCapability}
                    disabled={busy}
                    className="flex shrink-0 items-center gap-1 rounded border border-border-outer px-2 py-1 text-10 font-medium text-text-secondary hover:bg-zinc-100 disabled:opacity-50 dark:hover:bg-zinc-700"
                  >
                    <Plus className="h-3 w-3" />
                    {t("createWizard.capabilityAdd")}
                  </button>
                </div>

                {state.capabilities.length === 0 ? (
                  <p className="text-10 text-text-tertiary">{t("createWizard.capabilityEmpty")}</p>
                ) : (
                  state.capabilities.map((cap) => {
                    const schemaOpen = openSchema.has(cap.uid);
                    const actionInvalid =
                      cap.action.trim() !== "" && !CAPABILITY_ACTION_RE.test(cap.action.trim());
                    return (
                      <div
                        key={cap.uid}
                        className="space-y-2 rounded-md border border-border-divider px-3 py-2"
                      >
                        <div className="flex items-end gap-2">
                          <div className="flex-1">
                            <Field label={t("createWizard.capabilityAction")}>
                              <StyledInput
                                type="text"
                                value={cap.action}
                                onChange={(e) => patchCapability(cap.uid, { action: e.target.value })}
                                placeholder={t("createWizard.capabilityActionPlaceholder")}
                                aria-label={t("createWizard.capabilityAction")}
                                aria-invalid={actionInvalid || undefined}
                                className={cn("font-mono", actionInvalid && "border-red-400")}
                                disabled={busy}
                              />
                            </Field>
                          </div>
                          <button
                            type="button"
                            aria-label={t("createWizard.capabilityRemove")}
                            onClick={() => removeCapability(cap.uid)}
                            disabled={busy}
                            className="mb-1 p-1 text-text-tertiary hover:text-red-500 disabled:opacity-50"
                          >
                            <XIcon className="h-3.5 w-3.5" />
                          </button>
                        </div>
                        <Field label={t("createWizard.capabilityDescription")}>
                          <StyledInput
                            type="text"
                            value={cap.description}
                            onChange={(e) =>
                              patchCapability(cap.uid, { description: e.target.value })
                            }
                            placeholder={t("createWizard.capabilityDescriptionPlaceholder")}
                            aria-label={t("createWizard.capabilityDescription")}
                            disabled={busy}
                          />
                        </Field>
                        <Field label={t("createWizard.capabilitySkills")}>
                          {skills.length === 0 ? (
                            <p className="text-10 text-text-tertiary">
                              {t("createWizard.capabilitySkillsEmpty")}
                            </p>
                          ) : (
                            <div className="flex flex-wrap gap-1">
                              {skills.map((skill) => {
                                const on = cap.skills.includes(skill.name);
                                return (
                                  <button
                                    key={skill.name}
                                    type="button"
                                    aria-pressed={on}
                                    onClick={() => toggleCapabilitySkill(cap.uid, skill.name)}
                                    disabled={busy}
                                    className={cn(
                                      "rounded px-1.5 py-0.5 text-10 font-medium",
                                      on
                                        ? "bg-[var(--color-accent)]/15 text-[var(--color-accent)]"
                                        : "bg-zinc-100 text-text-tertiary dark:bg-zinc-700",
                                    )}
                                  >
                                    {skill.name}
                                  </button>
                                );
                              })}
                            </div>
                          )}
                        </Field>
                        <button
                          type="button"
                          onClick={() =>
                            setOpenSchema((prev) => {
                              const next = new Set(prev);
                              if (next.has(cap.uid)) next.delete(cap.uid);
                              else next.add(cap.uid);
                              return next;
                            })
                          }
                          className="flex items-center gap-1 text-10 text-text-tertiary hover:text-text-secondary"
                        >
                          {schemaOpen ? (
                            <ChevronDown className="h-3 w-3" />
                          ) : (
                            <ChevronRight className="h-3 w-3" />
                          )}
                          {t("createWizard.capabilityAdvanced")}
                        </button>
                        {schemaOpen && (
                          <div className="space-y-2">
                            <Field label={t("createWizard.capabilityInputSchema")}>
                              <StyledTextarea
                                value={cap.inputSchema}
                                onChange={(e) =>
                                  patchCapability(cap.uid, { inputSchema: e.target.value })
                                }
                                rows={3}
                                className="resize-none"
                                fontMono
                                aria-label={t("createWizard.capabilityInputSchema")}
                                disabled={busy}
                              />
                            </Field>
                            <Field label={t("createWizard.capabilityOutputSchema")}>
                              <StyledTextarea
                                value={cap.outputSchema}
                                onChange={(e) =>
                                  patchCapability(cap.uid, { outputSchema: e.target.value })
                                }
                                rows={3}
                                className="resize-none"
                                fontMono
                                aria-label={t("createWizard.capabilityOutputSchema")}
                                disabled={busy}
                              />
                            </Field>
                          </div>
                        )}
                      </div>
                    );
                  })
                )}
              </section>
            </div>
          )}

          {step === "preview" && (
            <div className="space-y-3">
              <h3 className="text-xs font-medium text-text-secondary">{t("createWizard.previewTitle")}</h3>
              {previewBusy ? (
                <p className="text-xs text-text-tertiary">{t("createWizard.previewLoading")}</p>
              ) : previewError ? (
                <ErrorBox message={previewError} />
              ) : preview ? (
                <>
                  <div className="max-h-[320px] overflow-auto rounded-md bg-zinc-50 px-4 py-3 dark:bg-zinc-700/50">
                    <pre className="whitespace-pre-wrap font-mono text-xs text-text-secondary">
                      {preview.toml}
                    </pre>
                  </div>
                  {preview.errors.length > 0 && (
                    <IssueList
                      tone="danger"
                      title={t("createWizard.previewErrors", { count: preview.errors.length })}
                      items={preview.errors}
                    />
                  )}
                  {preview.warnings.length > 0 && (
                    <IssueList
                      tone="warning"
                      title={t("createWizard.previewWarnings", { count: preview.warnings.length })}
                      items={preview.warnings}
                    />
                  )}
                  {preview.errors.length === 0 && preview.warnings.length === 0 && (
                    <p className="text-10 text-text-tertiary">{t("createWizard.previewClean")}</p>
                  )}
                </>
              ) : null}
            </div>
          )}

          {/* Step-level blocking issues, in the user's language. */}
          {issueLines.length > 0 && step !== "preview" && (
            <IssueList tone="danger" title={t("createWizard.previewErrors", { count: issueLines.length })} items={issueLines} />
          )}

          {error && <ErrorBox message={error} onClose={() => setError(null)} />}
        </div>

        {/* Footer */}
        <div className="flex items-center justify-between border-t border-border-divider px-5 min-h-[var(--ui-dialog-zone-h)]">
          <button
            onClick={stepIndex === 0 ? onClose : handleBack}
            disabled={busy}
            className="rounded-md px-3 py-1.5 text-xs font-medium text-text-secondary hover:bg-zinc-100 disabled:opacity-50 dark:hover:bg-zinc-700"
          >
            {stepIndex === 0 ? t("common.cancel") : t("common.back")}
          </button>

          <button
            onClick={handleNext}
            disabled={!canAdvance || (isLast && (previewBusy || !!previewError))}
            className="flex items-center gap-2 rounded btn-accent px-3 py-1.5 text-xs font-medium disabled:cursor-not-allowed disabled:opacity-50"
          >
            {busy ? (
              <>
                <Loader2 className="h-3.5 w-3.5 animate-spin" />
                {t("createWizard.creating")}
              </>
            ) : isLast ? (
              t("createWizard.buttonCreateAgent")
            ) : (
              t("createWizard.buttonNext")
            )}
          </button>
        </div>
      </div>
    </div>
  );
}

/* -------------------------------------------------------------------------- */
/* Presentation helpers                                                       */
/* -------------------------------------------------------------------------- */

/** Label + optional hint + optional red-asterisk, the shape every field uses. */
function Field({
  label,
  hint,
  required,
  children,
}: {
  label: string;
  hint?: string;
  required?: boolean;
  children: React.ReactNode;
}) {
  return (
    <div>
      <label className="mb-1 block text-xs font-medium text-text-tertiary">
        {label}
        {required && <span className="ml-0.5 text-red-500">*</span>}
        {hint && <span className="ml-1 font-normal text-text-tertiary">({hint})</span>}
      </label>
      {children}
    </div>
  );
}

/** One `[config]` row: input, the platform fallback, and the range error. */
function ConfigNumberField({
  label,
  desc,
  fallback,
  value,
  error,
  onChange,
  disabled,
}: {
  label: string;
  desc: string;
  fallback: string;
  value: string;
  error: "range" | "integer" | null;
  onChange: (v: string) => void;
  disabled?: boolean;
}) {
  const { t } = useTranslation();
  return (
    <div>
      <div className="flex items-center justify-between gap-3">
        <label className="text-xs font-medium text-text-tertiary">{label}</label>
        <span className="text-10 text-text-tertiary">
          {t("createWizard.configDefault", { value: fallback })}
        </span>
      </div>
      <StyledInput
        type="text"
        inputMode="decimal"
        value={value}
        onChange={(e) => onChange(e.target.value)}
        aria-label={label}
        aria-invalid={!!error || undefined}
        className={cn(error && "border-red-400")}
        disabled={disabled}
      />
      <p className={cn("mt-1 text-10", error ? "text-red-500" : "text-text-tertiary")}>
        {error === "range"
          ? t("createWizard.issueConfigRange")
          : error === "integer"
            ? t("createWizard.issueConfigInteger")
            : desc}
      </p>
    </div>
  );
}

/** Red / yellow block of problems. Never a dialog — these do not interrupt. */
function IssueList({ tone, title, items }: { tone: "danger" | "warning"; title: string; items: string[] }) {
  return (
    <div className="space-y-1">
      <div className="flex items-center gap-1.5">
        <Badge tone={tone}>{title}</Badge>
      </div>
      <ul className="list-disc space-y-0.5 pl-4">
        {items.map((item, i) => (
          <li key={`${i}-${item}`} className="text-10 text-text-tertiary">
            {item}
          </li>
        ))}
      </ul>
    </div>
  );
}

/**
 * Skills drop zone + rows.
 *
 * Split out only for readability: the row shows three things the wizard needs
 * while the user can still act on them — the parsed name, the skill's
 * `tool_deps`, and whether step 3 left one of those deps unreachable.
 */
function SkillDropzone({
  rows,
  onAdd,
  onRemove,
  disabled,
  undeclared,
}: {
  rows: SkillRow[];
  onAdd: (files: File[]) => Promise<void>;
  onRemove: (uid: string) => void;
  disabled?: boolean;
  undeclared: Array<{ skill: string; tool: string }>;
}) {
  const { t } = useTranslation();
  const fileInputRef = useRef<HTMLInputElement>(null);
  const [dragging, setDragging] = useState(false);

  const pick = async (fileList: FileList | null) => {
    const files = Array.from(fileList ?? []).filter((f) => f.name.toLowerCase().endsWith(".zip"));
    if (files.length > 0) await onAdd(files);
  };

  return (
    <div className="space-y-2">
      <div
        role="button"
        tabIndex={0}
        aria-label={t("createWizard.skillsDropzonePrompt")}
        onClick={() => !disabled && fileInputRef.current?.click()}
        onKeyDown={(e) => {
          if ((e.key === "Enter" || e.key === " ") && !disabled) {
            e.preventDefault();
            fileInputRef.current?.click();
          }
        }}
        onDragOver={(e) => {
          e.preventDefault();
          if (!disabled) setDragging(true);
        }}
        onDragLeave={() => setDragging(false)}
        onDrop={async (e) => {
          e.preventDefault();
          setDragging(false);
          if (!disabled) await pick(e.dataTransfer.files);
        }}
        className={cn(
          "flex cursor-pointer items-center justify-center rounded-md border border-dashed px-4 py-6 text-center text-xs",
          dragging
            ? "border-[var(--color-accent)] bg-[var(--color-accent)]/5 text-[var(--color-accent)]"
            : "border-border-outer text-text-tertiary hover:border-[var(--color-accent)]",
          disabled && "cursor-not-allowed opacity-50",
        )}
      >
        {t("createWizard.skillsDropzonePrompt")}
      </div>
      <input
        ref={fileInputRef}
        type="file"
        accept=".zip"
        multiple
        className="hidden"
        onChange={(e) => {
          void pick(e.target.files);
          // Allow re-picking the same file after a failed parse.
          e.target.value = "";
        }}
      />

      {rows.map((row) => {
        const missing = row.entry
          ? undeclared.filter((u) => u.skill === row.entry!.name)
          : [];
        return (
          <div
            key={row.uid}
            className="flex items-start justify-between gap-2 rounded-md border border-border-divider px-3 py-2"
          >
            <div className="min-w-0 flex-1">
              <p className="truncate font-mono text-11 font-medium text-text-secondary">
                {row.entry?.name ?? row.fileName}
                {row.status === "parsing" && (
                  <Loader2 className="ml-1 inline h-3 w-3 animate-spin text-text-tertiary" />
                )}
              </p>
              {row.status === "error" ? (
                <p className="text-10 text-red-500">{row.error}</p>
              ) : row.entry ? (
                <>
                  <p className="truncate text-10 text-text-tertiary">{row.entry.description}</p>
                  {row.entry.toolDeps.length > 0 && (
                    <p className="text-10 text-text-tertiary">
                      {t("createWizard.skillDeps", { tools: row.entry.toolDeps.join(" · ") })}
                      {missing.length > 0 && (
                        <span className="ml-1 text-[var(--color-warning)]">
                          {t("createWizard.skillDepsUndeclared")}
                        </span>
                      )}
                    </p>
                  )}
                </>
              ) : null}
            </div>
            <button
              type="button"
              aria-label={t("createWizard.removeSkillAriaLabel")}
              onClick={() => onRemove(row.uid)}
              disabled={disabled}
              className="p-1 text-text-tertiary hover:text-red-500 disabled:opacity-50"
            >
              <XIcon className="h-3.5 w-3.5" />
            </button>
          </div>
        );
      })}
    </div>
  );
}

/** Display name for a platform-hosted MCP id (`pm` / `doc` / `memory`). */
function hostedLabel(id: string, t: (key: string) => string): string {
  switch (id) {
    case "pm":
      return t("createWizard.mcpHostedPm");
    case "doc":
      return t("createWizard.mcpHostedDoc");
    case "memory":
      return t("createWizard.mcpHostedMemory");
    default:
      return id;
  }
}

/** Translated message for a step gate, or `null` if the code has no copy. */
function issueText(issue: StepIssue, t: (key: string, opts?: Record<string, unknown>) => string): string | null {
  switch (issue) {
    case "agentIdInvalid":
      return t("createWizard.issueAgentIdInvalid");
    case "nameMissing":
      return t("createWizard.issueNameMissing");
    case "tooManyLabels":
      return t("createWizard.issueTooManyLabels", { max: MAX_LABELS });
    case "configRange":
      return t("createWizard.issueConfigRange");
    case "configInteger":
      return t("createWizard.issueConfigInteger");
    case "capabilityActionInvalid":
      return t("createWizard.issueCapabilityActionInvalid");
    case "capabilityActionDuplicate":
      return t("createWizard.issueCapabilityActionDuplicate");
    case "capabilitySchemaInvalid":
      return t("createWizard.issueCapabilitySchemaInvalid");
    default:
      return null;
  }
}