import type { SkillDetailResponse } from "../../lib/types";
import { Loader2, ArrowLeft } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";

interface SkillDetailProps {
  detail: SkillDetailResponse | null;
  loading: boolean;
  onBack?: () => void;
}

export function SkillDetail({ detail, loading, onBack }: SkillDetailProps) {
  const { t } = useTranslation();
  if (loading) {
    return (
      <div className="flex h-full flex-col overflow-hidden">
        {onBack && (
          <div className="flex items-center gap-3 border-b border-border-divider px-4 py-3">
            <button
              onClick={onBack}
              className="inline-flex items-center gap-1.5 rounded p-1 text-xs text-text-tertiary hover:bg-[var(--color-accent)]/10  dark:hover:bg-[var(--color-accent)]/15"
              aria-label={t("common.backToList")}
            >
              <ArrowLeft className="h-4 w-4" />
              {t("common.back")}
            </button>
          </div>
        )}
        <div className="flex flex-1 items-center justify-center">
          <Loader2 className="h-6 w-6 animate-spin text-text-tertiary " />
        </div>
      </div>
    );
  }

  if (!detail) {
    return (
      <div className="flex h-full flex-col overflow-hidden">
        {onBack && (
          <div className="flex items-center gap-3 border-b border-border-divider px-4 py-3">
            <button
              onClick={onBack}
              className="inline-flex items-center gap-1.5 rounded p-1 text-xs text-text-tertiary hover:bg-[var(--color-accent)]/10  dark:hover:bg-[var(--color-accent)]/15"
              aria-label={t("common.backToList")}
            >
              <ArrowLeft className="h-4 w-4" />
              {t("common.back")}
            </button>
          </div>
        )}
        <div className="flex flex-1 items-center justify-center">
          <div className="text-center">
            <p className="text-sm text-text-tertiary ">
              Select a skill to view details
            </p>
          </div>
        </div>
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col overflow-hidden">
      {/* Back header */}
      {onBack && (
        <div className="flex items-center gap-3 border-b border-border-divider px-4 py-3">
          <button
            onClick={onBack}
            className="inline-flex items-center gap-1.5 rounded p-1 text-xs text-text-tertiary hover:bg-[var(--color-accent)]/10  dark:hover:bg-[var(--color-accent)]/15"
            aria-label={t("common.backToList")}
          >
            <ArrowLeft className="h-4 w-4" />
            {t("common.back")}
          </button>
        </div>
      )}
      <div className="flex-1 overflow-y-auto p-6">
      {/* Basic info card */}
      <div className="rounded-md border border-border-outer p-4">
        <h2 className="text-lg font-semibold text-text ">
          {detail.name}
        </h2>
        <p className="mt-1 text-sm text-text-tertiary ">
          {detail.description}
        </p>
        <div className="mt-3 flex flex-wrap gap-2 text-xs text-text-tertiary ">
          {detail.version && (
            <span className="rounded bg-zinc-100 px-2 py-0.5 dark:bg-zinc-800">
              Version: {detail.version}
            </span>
          )}
          {detail.author && (
            <span className="rounded bg-zinc-100 px-2 py-0.5 dark:bg-zinc-800">
              Author: {detail.author}
            </span>
          )}
        </div>
      </div>

      {/* Triggers */}
      {detail.triggers.length > 0 && (
        <div className="mt-5">
          <h3 className="text-xs font-semibold uppercase tracking-wider text-text-tertiary ">
            Triggers
          </h3>
          <div className="mt-2 flex flex-wrap gap-2">
            {detail.triggers.map((t) => (
              <span
                key={t}
                className="rounded-full px-2.5 py-0.5 text-xs font-medium border" style={{ backgroundColor: "color-mix(in srgb, var(--color-accent) 10%, transparent)", color: "var(--color-accent)", borderColor: "var(--color-accent)" }}
              >
                {t}
              </span>
            ))}
          </div>
        </div>
      )}

      {/* Tool dependencies */}
      {detail.tool_deps.length > 0 && (
        <div className="mt-5">
          <h3 className="text-xs font-semibold uppercase tracking-wider text-text-tertiary ">
            Tool Dependencies
          </h3>
          <div className="mt-2 flex flex-wrap gap-2">
            {detail.tool_deps.map((tool) => (
              <span
                key={tool}
                className="rounded-full bg-zinc-100 px-2.5 py-0.5 text-xs font-medium text-text-secondary dark:bg-zinc-800 "
              >
                {tool}
              </span>
            ))}
          </div>
        </div>
      )}

      {/* Instructions */}
      <div className="mt-5">
        <h3 className="text-xs font-semibold uppercase tracking-wider text-text-tertiary ">
          Instructions
        </h3>
        <div className="mt-2 rounded-md border border-border-outer bg-zinc-50 p-4  dark:bg-zinc-800/50">
          <pre className="whitespace-pre-wrap text-sm text-text-secondary ">
            {detail.instructions}
          </pre>
        </div>
      </div>

      {/* Execution stats */}
      <div className="mt-5">
        <h3 className="text-xs font-semibold uppercase tracking-wider text-text-tertiary ">
          Execution Stats
        </h3>
        <p className="mt-2 text-sm text-text-tertiary ">
          No execution history available
        </p>
      </div>
    </div>
    </div>
  );
}
