/**
 * 任务面上的小格式化工具 —— 从 `TaskEditDialog` 提出来的那份。
 *
 * 单独成模块的原因：提交历史（`TaskActivity.tsx`）和任务表单
 * （`TaskEditDialog.tsx`）都要把时间戳和字节数写成同一种样子。留在
 * `TaskEditDialog` 里就得复制一份，或者让新组件反向依赖对话框模块
 * （循环 import）。
 */

/** `2026-10-30 16:41`；解析失败时原样回显（不猜、不吞）。 */
export function formatDateTime(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/** 附件体积：`812 B` / `14.2 KB` / `3.1 MB`。 */
export function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}

/**
 * 截止日期短格式：本年内 `MM-DD`，跨年带年份。
 *
 * 列表视图的截止日期列用这个（看板卡片里是同一份规则的私有实现）。
 */
export function formatDueDate(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso.slice(0, 10);
  const sameYear = d.getFullYear() === new Date().getFullYear();
  const mm = String(d.getMonth() + 1).padStart(2, "0");
  const dd = String(d.getDate()).padStart(2, "0");
  return sameYear ? `${mm}-${dd}` : `${d.getFullYear()}-${mm}-${dd}`;
}

/**
 * 逾期判定：终态任务（已完成 / 已取消 / 已驳回）不算逾期。
 *
 * 与看板卡片同规则 —— 一个已完成的历史任务挂着红字是噪音。
 */
export function isDueOverdue(iso: string, status: string): boolean {
  if (status === "done" || status === "cancelled" || status === "rejected") return false;
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return false;
  const today = new Date();
  const endOfToday = new Date(
    today.getFullYear(),
    today.getMonth(),
    today.getDate(),
    23,
    59,
    59,
    999,
  );
  return d.getTime() < endOfToday.getTime();
}

/** 距截止还有几天（负数 = 已逾期 N 天）；无截止 → `null`。 */
export function dueDaysLeft(iso: string | null): number | null {
  if (!iso) return null;
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return null;
  const today = new Date();
  const a = new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
  const b = new Date(today.getFullYear(), today.getMonth(), today.getDate()).getTime();
  return Math.round((a - b) / 86_400_000);
}
