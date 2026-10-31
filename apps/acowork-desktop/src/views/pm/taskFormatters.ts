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
