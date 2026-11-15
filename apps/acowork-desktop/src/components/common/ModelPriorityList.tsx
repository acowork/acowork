/**
 * ModelPriorityList — ordered (provider, model) candidate editor.
 *
 * Shared by the Harness "global default compact model" card and the
 * memory-panel distiller-model card (ADR-056 list semantics): the
 * runtime walks the list top-to-bottom and uses the first candidate
 * that passes availability / context-window checks.
 *
 * The component is controlled: `items` is the source of truth and every
 * mutation (add / move / remove) emits the full new list through
 * `onChange` — callers PUT the whole array (last write wins, no
 * per-row PATCH endpoint exists).
 */
import { useMemo } from "react";
import { ArrowDown, ArrowUp, X } from "lucide-react";
import type { CompactModelRef } from "../../lib/types";
import { useTranslation } from "../../i18n/useTranslation";
import { Dropdown } from "./Dropdown";
import { cn } from "../../lib/utils";

export interface ModelPriorityOption {
  key: string;
  providerId: string;
  modelId: string;
}

export interface ModelPriorityListProps {
  /** Ordered candidate list — first entry has highest priority. */
  items: CompactModelRef[];
  /** Selectable (provider, model) pairs built from configured keys. */
  options: ModelPriorityOption[];
  /** provider_id → display name (falls back to raw id). */
  providerNameById: Map<string, string>;
  disabled?: boolean;
  onChange: (next: CompactModelRef[]) => void;
  className?: string;
}

const refKey = (r: CompactModelRef) => `${r.provider_id}::${r.model_id}`;
// Em-space padding around "·" — matches the historical option-label
// grammar of the compact-model pickers.
const SEP = "\u2003\u00b7\u2003";

export function ModelPriorityList({
  items,
  options,
  providerNameById,
  disabled,
  onChange,
  className,
}: ModelPriorityListProps) {
  const { t } = useTranslation();

  const labelFor = (ref: CompactModelRef): string => {
    const provider = providerNameById.get(ref.provider_id) ?? ref.provider_id;
    const stale = !options.some(
      (o) => o.providerId === ref.provider_id && o.modelId === ref.model_id,
    );
    return stale
      ? `${ref.model_id}${SEP}${provider} (${t("common.modelList.unavailable")})`
      : `${ref.model_id}${SEP}${provider}`;
  };

  // Options not already in the list — the "add" dropdown only offers
  // what isn't picked yet (list entries are unique by design; the
  // Gateway also dedups server-side).
  const addable = useMemo(() => {
    const taken = new Set(items.map(refKey));
    return options
      .filter((o) => !taken.has(`${o.providerId}::${o.modelId}`))
      .map((o) => ({
        value: o.key,
        label: `${o.modelId}${SEP}${providerNameById.get(o.providerId) ?? o.providerId}`,
      }));
  }, [items, options, providerNameById]);

  const move = (idx: number, delta: number) => {
    const next = [...items];
    const j = idx + delta;
    if (j < 0 || j >= next.length) return;
    [next[idx], next[j]] = [next[j], next[idx]];
    onChange(next);
  };

  const iconBtn =
    "p-0.5 rounded text-text-tertiary hover:text-text hover:bg-[var(--color-accent)]/10 disabled:opacity-40 disabled:pointer-events-none";

  return (
    // `text-11` is this component's own size step; every child (rows, the
    // add-dropdown and its native option list) inherits it, so a caller can
    // retime the whole editor with one font-size class in `className` — e.g.
    // the memory-panel distiller card passes `text-xs` to line the model rows
    // up with the `StyledInput` fields below them.
    <div className={cn("flex flex-col gap-1 text-11", className)}>
      {items.length === 0 && (
        <span className="text-10 text-text-tertiary">
          {t("common.modelList.empty")}
        </span>
      )}
      {items.map((ref, i) => (
        <div
          key={refKey(ref)}
          className="flex items-center gap-1.5 rounded border border-border-divider bg-panel px-2 py-1"
        >
          <span className="w-4 shrink-0 text-center text-text-tertiary tabular-nums">
            {i + 1}
          </span>
          <span className="min-w-0 flex-1 truncate" title={labelFor(ref)}>
            {labelFor(ref)}
          </span>
          <button
            type="button"
            className={iconBtn}
            disabled={disabled || i === 0}
            onClick={() => move(i, -1)}
            aria-label={t("common.modelList.moveUp")}
            title={t("common.modelList.moveUp")}
          >
            <ArrowUp size={12} />
          </button>
          <button
            type="button"
            className={iconBtn}
            disabled={disabled || i === items.length - 1}
            onClick={() => move(i, +1)}
            aria-label={t("common.modelList.moveDown")}
            title={t("common.modelList.moveDown")}
          >
            <ArrowDown size={12} />
          </button>
          <button
            type="button"
            className={iconBtn}
            disabled={disabled}
            onClick={() => onChange(items.filter((_, j) => j !== i))}
            aria-label={t("common.modelList.remove")}
            title={t("common.modelList.remove")}
          >
            <X size={12} />
          </button>
        </div>
      ))}
      <Dropdown
        value=""
        disabled={disabled || addable.length === 0}
        placeholder={{
          value: "",
          label: t("common.modelList.add"),
          selectable: true,
        }}
        options={addable}
        onChange={(v) => {
          if (!v) return;
          const o = options.find((opt) => opt.key === v);
          if (!o) return;
          onChange([...items, { provider_id: o.providerId, model_id: o.modelId }]);
        }}
        // Follow the container's font-size instead of Dropdown's own
        // `size="small"` step, so the trigger and the native option list
        // stay the same size as the rows above them.
        className="text-[length:inherit]"
        size="small"
      />
    </div>
  );
}
