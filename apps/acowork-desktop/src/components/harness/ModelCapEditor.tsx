import type { ReactNode } from "react";
import type { ModelCapabilitiesInfo } from "../../lib/types";
import { StyledInput } from "../common/StyledInput";
import { Dropdown } from "../common/Dropdown";
import { useTranslation } from "../../i18n/useTranslation";

interface ModelCapEditorProps {
  modelId: string;
  /** Capability fields; omit to render a protocol-only card (ADR-103 D11). */
  caps?: ModelCapabilitiesInfo;
  /**
   * ADR-103 D11 position 3+4: the protocol dropdown / base_url / verify block.
   * Injected as a slot so this component stays ignorant of protocol semantics.
   */
  protocolSection?: ReactNode;
  /** Trailing header hint (e.g. the `Ⓡ Responses` chip). */
  headerHint?: ReactNode;
  expanded: boolean;
  onToggle: () => void;
  onUpdate?: (field: keyof ModelCapabilitiesInfo, value: unknown) => void;
}

const INPUT_MODALITIES = ["text", "image", "audio", "video"] as const;
const OUTPUT_MODALITIES = ["text", "image"] as const;

/** Reusable expandable per-model capability editor card. */
export function ModelCapEditor({
  modelId,
  caps,
  protocolSection,
  headerHint,
  expanded,
  onToggle,
  onUpdate,
}: ModelCapEditorProps) {
  const { t } = useTranslation();
  const update = (field: keyof ModelCapabilitiesInfo, value: unknown) => {
    onUpdate?.(field, value);
  };

  return (
    <div className="rounded border border-border-outer">
      <button
        type="button"
        onClick={onToggle}
        className="flex w-full items-center gap-1 px-2 py-1.5 text-xs text-text-secondary "
      >
        <span className="text-text-tertiary">{expanded ? "\u25BC" : "\u25B6"}</span>
        <span className="flex-1 truncate text-left">{modelId}</span>
        {headerHint}
        <span className="text-text-tertiary">{caps?.context_window ? `${(caps.context_window / 1000).toFixed(0)}K ctx` : ""}</span>
      </button>
      {expanded && (
        <div className="border-t border-border-divider px-2 py-2">
          {protocolSection}
          {caps && (
          <>
          <div className="flex gap-2">
            <div className="flex-1">
              <label className="mb-0.5 block text-xs text-text-tertiary">{t("harness.contextWindow")}</label>
              <StyledInput
                type="number"
                value={caps.context_window?.toString() ?? ""}
                onChange={(e) => update("context_window", parseInt(e.target.value) || 0)}
                placeholder="e.g. 128000"
                className="dark:border-zinc-700 dark:bg-zinc-900 "
              />
            </div>
            <div className="flex-1">
              <label className="mb-0.5 block text-xs text-text-tertiary">{t("harness.maxOutputTokens")}</label>
              <StyledInput
                type="number"
                value={caps.max_output_tokens?.toString() ?? ""}
                onChange={(e) => update("max_output_tokens", parseInt(e.target.value) || 0)}
                placeholder="e.g. 16384"
                className="dark:border-zinc-700 dark:bg-zinc-900 "
              />
            </div>
          </div>
          <div className="mt-1.5 flex flex-wrap items-center gap-3">
            <label className="flex items-center gap-1.5 text-xs text-text-tertiary">
              <input
                type="checkbox"
                checked={caps.supports_tool_calling ?? false}
                onChange={(e) => update("supports_tool_calling", e.target.checked)}
                className="accent-[var(--color-accent)]"
              />
              {t("harness.supportsToolCalling")}
            </label>
            <label className="flex items-center gap-1.5 text-xs text-text-tertiary">
              <input
                type="checkbox"
                checked={caps.supports_reasoning ?? false}
                onChange={(e) => update("supports_reasoning", e.target.checked)}
                className="accent-[var(--color-accent)]"
              />
              {t("harness.reasoning")}
            </label>
          </div>
          <div className="mt-1.5 flex gap-4">
            <div>
              <label className="mb-0.5 block text-xs text-text-tertiary">Input</label>
              <div className="flex gap-2">
                {INPUT_MODALITIES.map(mod => (
                  <label key={mod} className="flex items-center gap-1 text-xs text-text-tertiary">
                    <input
                      type="checkbox"
                      checked={caps.modalities?.input?.includes(mod) ?? false}
                      onChange={(e) => {
                        const current = caps.modalities?.input ?? [];
                        const nextMod = e.target.checked
                          ? [...current, mod]
                          : current.filter(m => m !== mod);
                        update("modalities", { ...caps.modalities, input: nextMod });
                      }}
                      className="accent-[var(--color-accent)]"
                    />
                    {mod}
                  </label>
                ))}
              </div>
            </div>
            <div>
              <label className="mb-0.5 block text-xs text-text-tertiary">Output</label>
              <div className="flex gap-2">
                {OUTPUT_MODALITIES.map(mod => (
                  <label key={mod} className="flex items-center gap-1 text-xs text-text-tertiary">
                    <input
                      type="checkbox"
                      checked={caps.modalities?.output?.includes(mod) ?? false}
                      onChange={(e) => {
                        const current = caps.modalities?.output ?? [];
                        const nextMod = e.target.checked
                          ? [...current, mod]
                          : current.filter(m => m !== mod);
                        update("modalities", { ...caps.modalities, output: nextMod });
                      }}
                      className="accent-[var(--color-accent)]"
                    />
                    {mod}
                  </label>
                ))}
              </div>
            </div>
          </div>
          {caps.supports_reasoning && (
            <div className="mt-1.5">
              <label className="mb-0.5 block text-xs text-text-tertiary">{t("harness.defaultReasoningEffort")}</label>
              <Dropdown
                value={caps.default_reasoning_effort ?? "auto"}
                onChange={(v) => update("default_reasoning_effort", v)}
                options={[
                  { value: "auto", label: "Auto" },
                  { value: "off", label: "Off" },
                  { value: "low", label: "Low" },
                  { value: "medium", label: "Medium" },
                  { value: "high", label: "High" },
                ]}
              />
            </div>
          )}
          </>
          )}
        </div>
      )}
    </div>
  );
}
