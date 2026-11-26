/**
 * LibraryIconPicker — 文档库图标选择器。
 *
 * 沿用 `ProjectIconPicker` 的交互语法（**预览即触发器** + 右下铅笔角标 +
 * 弹层网格），但数据源不同：文档库图标是 `LIBRARY_ICONS` 里的一枚文档语义
 * 字形，没有上传端点，所以弹层是一张静态网格 + 一个「默认」项，而不是项目
 * 那边的「已上传资产 + 上传按钮」。刻意不抽公共组件：两者的 `props` 形状、
 * 数据来源、持久化路径都不同，强行合并会把「草稿文件 / 服务端资产」的分支
 * 塞进文档库的只读静态网格里。
 *
 * **不列 `BUILTIN_ICONS`**：那是装 agent / 用户头像用的立绘集（人像、插画），
 * 挂在文档库上语义是错的。这里的网格是文档语义的字形，值域
 * `null`（默认 = 导航栏 docs 图标）或 `LIBRARY_ICONS` 的一个 key。
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { Check, Pencil } from "lucide-react";
import { LIBRARY_ICON_IDS } from "../../lib/libraryIcons";
import { OverlayScrollbar } from "../../components/common/OverlayScrollbar";
import { useTranslation } from "../../i18n/useTranslation";
import { cn } from "../../lib/utils";
import { LibraryIcon } from "./LibraryIcon";

export function LibraryIconPicker({
  value,
  onChange,
  size = 48,
  disabled,
}: {
  value: string | null;
  onChange: (icon: string | null) => void;
  size?: number;
  disabled?: boolean;
}) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const anchorRef = useRef<HTMLButtonElement>(null);
  const [pos, setPos] = useState<{ top: number; left: number } | null>(null);

  const computePos = useCallback(() => {
    const el = anchorRef.current;
    if (!el) return;
    const rect = el.getBoundingClientRect();
    setPos({ top: rect.bottom + 8, left: rect.left });
  }, []);

  useEffect(() => {
    if (!open) return;
    computePos();
    window.addEventListener("resize", computePos);
    window.addEventListener("scroll", computePos, true);
    return () => {
      window.removeEventListener("resize", computePos);
      window.removeEventListener("scroll", computePos, true);
    };
  }, [open, computePos]);

  const decide = (icon: string | null) => {
    onChange(icon);
    setOpen(false);
  };

  return (
    <>
      <button
        ref={anchorRef}
        type="button"
        disabled={disabled}
        onClick={() => {
          if (!open) computePos();
          setOpen((v) => !v);
        }}
        aria-label={t("doc.library.iconPickerTitle")}
        className="relative block rounded-lg ring-1 ring-border-strong/60 transition hover:ring-border-strong disabled:cursor-not-allowed disabled:opacity-50"
      >
        <LibraryIcon icon={value} size={size} className="rounded-md" />
        <span className="absolute -bottom-0.5 -right-0.5 flex h-5 w-5 items-center justify-center rounded-full bg-btn-solid text-white shadow-sm">
          <Pencil className="h-3 w-3" />
        </span>
      </button>

      {open &&
        pos &&
        createPortal(
          <>
            <div className="fixed inset-0 z-[60]" onClick={() => setOpen(false)} />
            <div
              className="fixed z-[61] w-64 rounded-md border border-border-outer bg-modal-surface p-2 shadow-xl"
              style={{ top: pos.top, left: pos.left }}
              role="dialog"
              aria-label={t("doc.library.iconPickerTitle")}
            >
              <OverlayScrollbar maxHeight="14rem" className="grid grid-cols-5 gap-1.5 p-0.5">
                <IconTile
                  label={t("doc.library.defaultIcon")}
                  selected={value === null}
                  onPick={() => decide(null)}
                >
                  <LibraryIcon icon={null} size={28} />
                </IconTile>
                {LIBRARY_ICON_IDS.map((id) => (
                  <IconTile
                    key={id}
                    label={id}
                    selected={value === id}
                    onPick={() => decide(id)}
                  >
                    <LibraryIcon icon={id} size={20} />
                  </IconTile>
                ))}
              </OverlayScrollbar>
            </div>
          </>,
          document.body,
        )}
    </>
  );
}

function IconTile({
  label,
  selected,
  onPick,
  children,
}: {
  label: string;
  selected: boolean;
  onPick: () => void;
  children: React.ReactNode;
}) {
  return (
    <button
      type="button"
      onClick={onPick}
      aria-label={label}
      aria-pressed={selected}
      title={label}
      className={cn(
        "relative flex items-center justify-center rounded-md p-1 transition-colors",
        selected
          ? "bg-accent/15 ring-1 ring-accent"
          : "hover:bg-[var(--color-accent)]/10",
      )}
    >
      {children}
      {selected && (
        <Check className="absolute -right-0.5 -top-0.5 h-3 w-3 rounded-full bg-btn-solid p-[1px] text-white" />
      )}
    </button>
  );
}