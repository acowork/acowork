import { useState } from "react";
import type { ModelInfo, ModelCapabilitiesInfo, ModelCapabilitiesMap } from "../../lib/types";
import { cn } from "../../lib/utils";
import { listRowHover } from "../../lib/ui-styles";
import { StyledInput } from "../common/StyledInput";
import { Dropdown } from "../common/Dropdown";
import { OverlayScrollbar } from "../common/OverlayScrollbar";
import { ModelCapEditor } from "./ModelCapEditor";
import { ModelProtocolEditor } from "./ModelProtocolEditor";
import { ProtocolChip } from "./ProtocolChip";
import { useTranslation } from "../../i18n/useTranslation";
import { protocolOptionOf } from "../../lib/modelProtocol";
import type { ModelProtocolState } from "./useModelProtocolState";

/** A capability filter chip toggle. */
export type CapabilityFilter = "tool_call" | "reasoning" | "image";

export interface ModelMultiSelectProps {
  // ── Data ──
  /** Available models (typically from Gateway `fetchProviderModels`). */
  models: ModelInfo[];
  /** Show loading placeholder when true. */
  loading?: boolean;

  // ── Selection (controlled) ──
  /** Selected model IDs. */
  selected: string[];
  /** Called when the selection changes. */
  onSelectedChange: (next: string[]) => void;

  // ── Per-model capabilities (optional — used by local/custom providers) ──
  /** Map of modelId → capabilities for the currently selected models. */
  caps?: ModelCapabilitiesMap;
  /** Called when a capability field is changed or a model is added/removed.
   *  Receives the FULLY UPDATED map (caller should treat as immutable). */
  onCapsChange?: (next: ModelCapabilitiesMap) => void;
  /** Which model IDs currently have their ModelCapEditor expanded. */
  expandedModels?: Set<string>;
  /** Toggle a single model's editor expansion state. */
  onExpandedToggle?: (modelId: string) => void;
  /** Render the per-model capability editors (needs `caps` + `onCapsChange`).
   *  Off by default — only meaningful for local or custom providers. */
  showModelCapEditor?: boolean;

  // ── ADR-103 D11 per-model protocol (optional) ──
  /** Protocol selection state from `useModelProtocolState`. When provided, the
   *  per-model cards grow a protocol dropdown / verify button, and the list
   *  rows grow the `Ⓡ Responses` chip. */
  protocols?: ModelProtocolState;
  /** Provider id used for the "re-verify" call; without it the button is
   *  disabled (an unsaved provider has nothing to probe). */
  protocolProviderId?: string;

  // ── Compact model selector (optional) ──
  compactModel?: string;
  onCompactModelChange?: (m: string) => void;
  /** Render a "Compact Model" <select> when at least one model is selected.
   *  Off by default. */
  showCompactModel?: boolean;

  // ── UI feature toggles ──
  /** Show tool_call / reasoning / image filter chips. Default `true`. */
  showCapabilityFilter?: boolean;
  /** Show ctx / max / emoji metadata in list rows. Default `true`. */
  showMetadata?: boolean;
  /** Show "type custom name + Enter" free-text input. Default `true`. */
  showCustomInput?: boolean;
  /** Show "N selected" counter next to the label. Default `true`. */
  showSelectedCount?: boolean;

  // ── Helpers ──
  /** Factory for default caps when a model is newly selected and no stored
   *  caps exist. Default: sensible 128k/16k/text in+out with tool-call on,
   *  reasoning off — merged with live `ModelInfo` when available. */
  makeDefaultCaps?: (mi: ModelInfo | undefined) => ModelCapabilitiesInfo;

  // ── Styling ──
  className?: string;
}

/** Default capability factory — exported so callers can reuse it. */
export function defaultMakeCaps(mi: ModelInfo | undefined): ModelCapabilitiesInfo {
  if (mi && (mi.context_window || mi.max_tokens)) {
    return {
      context_window: mi.context_window ?? 128000,
      max_output_tokens: mi.max_tokens ?? 16384,
      supports_tool_calling: mi.tool_call ?? true,
      supports_reasoning: mi.reasoning ?? false,
      modalities: {
        input: mi.input_modalities ?? ["text"],
        output: mi.output_modalities ?? ["text"],
      },
    };
  }
  return {
    context_window: 128000,
    max_output_tokens: 16384,
    supports_tool_calling: true,
    supports_reasoning: false,
    modalities: { input: ["text"], output: ["text"] },
  };
}

/**
 * Reusable multi-select model picker used by:
 *  - Harness → AddProviderFlow (add / custom steps)
 *  - Harness → Edit dialog
 *  - Onboarding → ApiKeyStep
 *
 * Renders:
 *  - (optional) capability filter chips (tool_call / reasoning / image)
 *  - selected model tags with remove buttons
 *  - search input
 *  - checkbox list of available models with metadata
 *  - (optional) free-text custom model input
 *  - (optional) per-model capability editors (ModelCapEditor)
 *  - (optional) Compact Model selector
 *
 * Selection state, caps state, and expansion state are all controlled
 * so the parent component stays the source of truth.
 */
export function ModelMultiSelect({
  models,
  loading = false,
  selected,
  onSelectedChange,
  caps,
  onCapsChange,
  expandedModels,
  onExpandedToggle,
  showModelCapEditor = false,
  protocols,
  protocolProviderId,
  compactModel,
  onCompactModelChange,
  showCompactModel = false,
  showCapabilityFilter = true,
  showMetadata = true,
  showCustomInput = true,
  showSelectedCount = true,
  makeDefaultCaps = defaultMakeCaps,
  className,
}: ModelMultiSelectProps) {
  const { t } = useTranslation();

  // Local UI state (not user-persisted) ───────────────────────────────
  const [searchTerm, setSearchTerm] = useState("");
  const [capabilityFilter, setCapabilityFilter] = useState<CapabilityFilter[]>([]);

  // ── Selection helpers ─────────────────────────────────────────────
  const toggleModel = (model: string) => {
    if (selected.includes(model)) {
      // Remove from selection and (if caps tracked) drop the caps entry
      const nextSelected = selected.filter((m) => m !== model);
      onSelectedChange(nextSelected);
      if (caps && onCapsChange) {
        const nextCaps = { ...caps };
        delete nextCaps[model];
        onCapsChange(nextCaps);
      }
    } else {
      // Add to selection and (if caps tracked) seed default caps
      onSelectedChange([...selected, model]);
      if (caps && onCapsChange) {
        const mi = models.find((m) => m.id === model);
        onCapsChange({ ...caps, [model]: makeDefaultCaps(mi) });
      }
    }
  };

  const addCustomModel = (rawValue: string) => {
    const val = rawValue.trim();
    if (!val || selected.includes(val)) return;
    onSelectedChange([...selected, val]);
    if (caps && onCapsChange) {
      onCapsChange({ ...caps, [val]: makeDefaultCaps(undefined) });
    }
  };

  const toggleCapabilityFilter = (filter: CapabilityFilter) => {
    setCapabilityFilter((prev) =>
      prev.includes(filter) ? prev.filter((f) => f !== filter) : [...prev, filter],
    );
  };

  const updateModelCap = (modelId: string, field: keyof ModelCapabilitiesInfo, value: unknown) => {
    if (!caps || !onCapsChange) return;
    onCapsChange({
      ...caps,
      [modelId]: { ...caps[modelId], [field]: value },
    });
  };

  // ── Filter the model list ─────────────────────────────────────────
  const visibleModels = models.filter((m) => {
    // Text search (id OR name)
    if (searchTerm) {
      const term = searchTerm.toLowerCase();
      if (!m.id.toLowerCase().includes(term) && !m.name.toLowerCase().includes(term)) {
        return false;
      }
    }
    // Capability filters (ALL must match — AND semantics)
    if (capabilityFilter.length > 0) {
      for (const filter of capabilityFilter) {
        if (filter === "tool_call" && m.tool_call !== true) return false;
        if (filter === "reasoning" && m.reasoning !== true) return false;
        if (filter === "image" && !(m.input_modalities?.includes("image") ?? false)) return false;
      }
    }
    return true;
  });

  return (
    <div className={className}>
      {/* Header label + selected counter */}
      <label className="mb-1 block text-xs text-text-tertiary">
        {t("harness.defaultModel")}
        {showSelectedCount && selected.length > 0 && (
          <span className="text-accent-green">
            ({selected.length} {t("harness.selected")})
          </span>
        )}
      </label>

      {/* Capability filter chips */}
      {showCapabilityFilter && (
        <div className="mb-2 flex gap-2">
          <button
            type="button"
            onClick={() => toggleCapabilityFilter("tool_call")}
            className={cn(
              "rounded px-2 py-0.5 text-xs font-medium",
              capabilityFilter.includes("tool_call")
                ? "bg-accent-green/10 text-accent-green"
                : "bg-zinc-100 text-text-secondary hover:bg-zinc-200 dark:bg-zinc-700 ",
            )}
          >
            🔧 {t("harness.toolCalling")}
          </button>
          <button
            type="button"
            onClick={() => toggleCapabilityFilter("reasoning")}
            className={cn(
              "rounded px-2 py-0.5 text-xs font-medium",
              capabilityFilter.includes("reasoning")
                ? "bg-purple-100 text-purple-700 dark:bg-purple-900 dark:text-purple-300"
                : "bg-zinc-100 text-text-secondary hover:bg-zinc-200 dark:bg-zinc-700 ",
            )}
          >
            🧠 {t("harness.reasoning")}
          </button>
          <button
            type="button"
            onClick={() => toggleCapabilityFilter("image")}
            className={cn(
              "rounded px-2 py-0.5 text-xs font-medium",
              capabilityFilter.includes("image")
                ? "bg-sky-100 text-sky-700 dark:bg-sky-900 dark:text-sky-300"
                : "bg-zinc-100 text-text-secondary hover:bg-zinc-200 dark:bg-zinc-700 ",
            )}
          >
            🖼️ {t("harness.image")}
          </button>
        </div>
      )}

      {/* Selected model tags */}
      {selected.length > 0 && (
        <div className="mb-1 flex flex-wrap gap-1">
          {selected.map((m) => (
            <span
              key={m}
              className="inline-flex items-center gap-1 rounded bg-accent-green/10 px-2 py-0.5 text-xs text-accent-green"
            >
              {m}
              <button
                type="button"
                onClick={() => toggleModel(m)}
                className="text-accent-green/60 hover:text-accent-green"
              >
                ×
              </button>
            </span>
          ))}
        </div>
      )}

      {/* Search input */}
      <StyledInput
        type="text"
        value={searchTerm}
        onChange={(e) => setSearchTerm(e.target.value)}
        placeholder={t("harness.searchModels")}
      />

      {/* Model list */}
      <OverlayScrollbar
        maxHeight={160}
        containerClassName="mt-1 overflow-hidden rounded border border-border-outer"
      >
        {loading ? (
          <div className="px-3 py-2 text-xs text-text-tertiary">{t("harness.loadingModels")}</div>
        ) : visibleModels.length === 0 ? (
          <div className="px-3 py-2 text-xs text-text-tertiary">{t("harness.noModelsFound")}</div>
        ) : (
          visibleModels.map((m) => (
            <label
              key={m.id}
              className={cn("flex cursor-pointer items-center gap-2 px-3 py-1.5 text-xs", listRowHover)}
            >
              <input
                type="checkbox"
                checked={selected.includes(m.id)}
                onChange={() => toggleModel(m.id)}
                className="accent-[var(--color-accent)]"
              />
              <div className="flex flex-1 flex-col gap-0.5">
                <span className="truncate">{m.name || m.id}</span>
                {showMetadata && (
                  <div className="flex gap-2 text-xs text-text-tertiary">
                    {m.context_window != null && (
                      <span>{(m.context_window / 1000).toFixed(0)}K {t("harness.context")}</span>
                    )}
                    {m.max_tokens != null && (
                      <span>{(m.max_tokens / 1000).toFixed(1)}K {t("harness.maxOutput")}</span>
                    )}
                    {m.reasoning && <span>🧠 {t("harness.reasoning")}</span>}
                    {m.tool_call && <span>🔧 {t("harness.tools")}</span>}
                    {m.input_modalities?.includes("image") && <span>🖼️ {t("harness.image")}</span>}
                    {protocols && (
                      <ProtocolChip badge={protocols.badgeFor(m)} />
                    )}
                  </div>
                )}
              </div>
            </label>
          ))
        )}
      </OverlayScrollbar>

      {/* Free-text custom model input */}
      {showCustomInput && (
        <div className="mt-2 flex gap-1">
          <StyledInput
            type="text"
            placeholder={t("harness.customModelPlaceholder")}
            className="flex-1"
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                const val = (e.target as HTMLInputElement).value.trim();
                if (val) {
                  addCustomModel(val);
                  (e.target as HTMLInputElement).value = "";
                }
              }
            }}
          />
        </div>
      )}

      {/* Per-model cards: capability editor (local/custom providers only) and
          the ADR-103 D11 protocol editor (every provider — a built-in provider
          can host a Responses-capable model too). */}
      {(showModelCapEditor || protocols) &&
        expandedModels &&
        onExpandedToggle &&
        selected.length > 0 && (
        <div className="mt-2">
          <label className="mb-1 block text-xs text-text-tertiary">
            {showModelCapEditor ? t("harness.modelCapabilities") : t("harness.protocolSection")}
            {showModelCapEditor && (
              <span className="ml-1 text-xs text-amber-500">({t("harness.manualInputRequired")})</span>
            )}
          </label>
          {/* D11: the request count must be visible before saving. Rendered in
              the section header so both provider dialogs get it for free;
              `models`/`requests` (not `count`) keeps i18next out of its plural
              key convention. */}
          {protocols && (() => {
            const est = protocols.estimate(models);
            if (est.models === 0) return null;
            return (
              <p className="mb-1 text-xs text-amber-500">
                {t("harness.probeSaveCost", {
                  models: est.models,
                  requests: est.requests,
                })}
              </p>
            );
          })()}
          <div className="space-y-1">
            {selected.map((modelId) => {
              const modelCaps = caps?.[modelId];
              const mi = models.find((m) => m.id === modelId);
              // A caps-only card without capability data has nothing to show.
              if (!modelCaps && !protocols) return null;
              const badge = protocols && mi ? protocols.badgeFor(mi) : undefined;
              return (
                <ModelCapEditor
                  key={modelId}
                  modelId={modelId}
                  caps={modelCaps}
                  headerHint={badge ? <ProtocolChip badge={badge} /> : undefined}
                  protocolSection={
                    protocols ? (
                      <ModelProtocolEditor
                        modelId={modelId}
                        selection={
                          protocols.selections[modelId] ?? protocolOptionOf(mi?.protocol_selected)
                        }
                        onSelectionChange={(next) => protocols.setSelection(modelId, next)}
                        baseUrl={protocols.baseUrls[modelId] ?? ""}
                        onBaseUrlChange={(next) => protocols.setBaseUrl(modelId, next)}
                        showBaseUrl={protocols.needsBaseUrl(modelId)}
                        badge={badge ?? { kind: "none" }}
                        probe={mi?.protocol_probe}
                        outcome={protocols.outcomes[modelId]}
                        verifying={protocols.verifying === modelId}
                        canVerify={Boolean(protocolProviderId)}
                        onVerify={() => {
                          if (protocolProviderId) {
                            void protocols.verify(protocolProviderId, modelId);
                          }
                        }}
                      />
                    ) : undefined
                  }
                  expanded={expandedModels.has(modelId)}
                  onToggle={() => onExpandedToggle(modelId)}
                  onUpdate={(field, value) => updateModelCap(modelId, field, value)}
                />
              );
            })}
          </div>
        </div>
      )}

      {/* Compact model selector */}
      {showCompactModel && onCompactModelChange && selected.length > 0 && (
        <div className="mt-2">
          <label className="mb-1 block text-xs text-text-tertiary">
            {t("harness.compactModel")}
          </label>
          <Dropdown
            value={compactModel ?? ""}
            onChange={(v) => onCompactModelChange?.(v)}
            placeholder={{ value: "", label: t("harness.useCurrentModel") }}
            options={selected.map((m) => ({ value: m, label: m }))}
          />
        </div>
      )}
    </div>
  );
}
