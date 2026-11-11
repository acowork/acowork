/**
 * PriorityBadge — 优先级徽章（实底胶囊 + 白字 + 文字语义）。
 * 对齐 UX 设计 §5.2 / §8.4：不依赖纯颜色传达信息。
 *
 * 底色取 `--color-pill-*`（全局高亮色体系的 rose / amber / blue 三档加深版，
 * 白字在深浅两套主题下都 ≥4.5:1）；中性档是 zinc，因为「低」没有颜色语义。
 */

import { useTranslation } from "../../i18n/useTranslation";
import { cn } from "../../lib/utils";
import type { Priority } from "../../lib/pm-types";

const STYLES: Record<Priority, string> = {
  urgent: "bg-pill-red",
  high: "bg-pill-amber",
  normal: "bg-pill-blue",
  low: "bg-pill-neutral",
};

/** 无障碍文案里的英文语义（徽章本身按当前语言渲染）。 */
const A11Y_LABELS: Record<Priority, string> = {
  urgent: "URGENT",
  high: "HIGH",
  normal: "MEDIUM",
  low: "LOW",
};

export function PriorityBadge({
  priority,
  className,
}: {
  priority: Priority;
  className?: string;
}) {
  const { t } = useTranslation();
  return (
    <span
      className={cn(
        "inline-flex items-center rounded-full px-1.5 py-0.5 text-9 font-semibold uppercase tracking-wide text-white",
        STYLES[priority],
        className,
      )}
      aria-label={`${A11Y_LABELS[priority]} priority`}
    >
      {t(`pm.priority.${priority}`)}
    </span>
  );
}
