/**
 * ServiceOfflineBanner — 离线降级提示条（T2-13）。
 *
 * 对齐 UX 设计 §7：PM 服务不可达（连续 3 次健康检查失败）时，
 * 在视图顶部显示琥珀色提示条 + 重试按钮。写操作由各组件
 * 通过 `usePmHealthStore` 的 `healthy` 状态自行禁用。
 */

import { usePmHealthStore } from "../../stores/pm/healthStore";
import { useTranslation } from "../../i18n/useTranslation";

export function ServiceOfflineBanner() {
  const { t } = useTranslation();
  const healthy = usePmHealthStore((s) => s.healthy);
  const check = usePmHealthStore((s) => s.check);

  // healthy === null（未探测）时不显示；false 时显示
  if (healthy !== false) return null;

  return (
    <div
      role="alert"
      className="flex shrink-0 items-center justify-between gap-2 border-b border-warning-border/60 bg-warning-surface px-4 py-2 text-xs text-warning-quiet"
    >
      <span className="flex min-w-0 items-center gap-2">
        <span aria-hidden>⚠️</span>
        <span className="truncate">{t("pm.offlineHint")}</span>
      </span>
      <button
        type="button"
        onClick={() => void check()}
        className="shrink-0 rounded-md px-2 py-0.5 text-11 font-medium text-warning-quiet hover:bg-warning-surface-hover"
      >
        {t("common.retry")}
      </button>
    </div>
  );
}
