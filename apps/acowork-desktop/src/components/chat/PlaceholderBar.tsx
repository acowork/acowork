//! Neutral loading strip shown while the Runtime is resolving global LLM
//! resources (bootstrap not READY, vault not populated). Mirrors the visual
//! weight of the misconfigured banner but with grey styling so users
//! understand the system is working, not broken.

import { Loader2 } from "lucide-react";

interface Props {
  /** Short message shown next to the spinner (i18n key preferred). */
  text?: string;
}

export function PlaceholderBar({ text }: Props) {
  return (
    <div
      className="flex items-center gap-2 border-b border-border-outer bg-surface-subtle px-4 py-2 rounded-t-lg"
      role="status"
      aria-live="polite"
      data-testid="llm-availability-placeholder"
    >
      <Loader2 className="h-4 w-4 animate-spin text-text-tertiary" />
      <span className="text-xs text-text-tertiary">
        {text ?? "正在同步 LLM 配置…"}
      </span>
    </div>
  );
}