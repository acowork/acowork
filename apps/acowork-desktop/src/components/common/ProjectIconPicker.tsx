/**
 * ProjectIconPicker — 项目图标选择器（仅自定义上传）。
 *
 * 对标头像的自定义图标设置（[ProfileTab.tsx]）：**图标预览本身即触发器**
 * （点击弹出选择弹层，右下角铅笔角标提示可编辑），不额外放"更换图标"按钮。
 * 弹层经 portal 渲染到 `document.body`、`position:fixed` 锚定预览 rect，点击
 * 遮罩关闭；选中 / 清除即回调 `onChange(ProjectIconRef | null)` 由父组件持久化
 * 到 `metadata.icon`（`null` → 清除，回落默认图标）。
 *
 * 只有 **Custom**（内置头像图标集不适合项目语义）："+" 上传（隐藏
 * `<input type=file>`，复用 PM multipart 端点）+ 已上传资产网格；上传成功后
 * **立即选中**返回的相对路径（前端回写 metadata）。
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { AuthedImage } from "./AuthedImage";
import { ProjectAvatar } from "./ProjectAvatar";
import { resolveProjectIconUrl, type ProjectIconRef } from "../../lib/projectIcon";
import {
  fetchProjectIconAssets,
  uploadProjectIcon,
  type PmIconAsset,
} from "../../lib/pm-api";
import { OverlayScrollbar } from "./OverlayScrollbar";
import { useTranslation } from "../../i18n/useTranslation";
import { log } from "../../lib/logger";
import type { PmProject } from "../../lib/pm-types";

const IMAGE_EXTENSIONS = ["png", "jpg", "jpeg", "gif", "webp", "svg"];

interface ProjectIconPickerProps {
  project: PmProject;
  /** 当前图标引用（用于高亮选中项）。 */
  value: ProjectIconRef | null;
  /** 离线等场景禁用。 */
  disabled?: boolean;
  /** 选中 / 清除回调（`null` = 用默认图标）。 */
  onChange: (ref: ProjectIconRef | null) => void;
  /** 预览边长（px）。默认 64（与头像设置一致）。 */
  size?: number;
}

export function ProjectIconPicker({ project, value, disabled, onChange, size = 64 }: ProjectIconPickerProps) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const [assets, setAssets] = useState<PmIconAsset[]>([]);
  const [busy, setBusy] = useState(false);

  const anchorRef = useRef<HTMLDivElement>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const [popupPos, setPopupPos] = useState<{ top: number; left: number } | null>(null);

  const computePos = useCallback(() => {
    const el = anchorRef.current;
    if (!el) return;
    const rect = el.getBoundingClientRect();
    setPopupPos({ top: rect.bottom + 8, left: rect.left });
  }, []);

  // 打开时拉取已上传资产 + 挂 resize/scroll 跟踪（capture 阶段捕获面板滚动）。
  useEffect(() => {
    if (!open) return;
    computePos();
    fetchProjectIconAssets(project.id)
      .then(setAssets)
      .catch((err) => log.warn("[ProjectIconPicker] fetch assets failed:", err));
    window.addEventListener("resize", computePos);
    window.addEventListener("scroll", computePos, true);
    return () => {
      window.removeEventListener("resize", computePos);
      window.removeEventListener("scroll", computePos, true);
    };
  }, [open, project.id, computePos]);

  const select = useCallback(
    (ref: ProjectIconRef | null) => {
      onChange(ref);
      setOpen(false);
    },
    [onChange],
  );

  const handleUpload = useCallback(
    async (file: File) => {
      setBusy(true);
      try {
        const asset = await uploadProjectIcon(project.id, file);
        // 刷新网格 + 立即选中刚上传的（前端回写 metadata 由父组件负责）。
        setAssets((prev) => [...prev, asset]);
        select({ kind: "custom", path: asset.relative_path });
      } catch (err) {
        log.warn("[ProjectIconPicker] upload failed:", err);
      } finally {
        setBusy(false);
      }
    },
    [project.id, select],
  );

  return (
    <div className="relative" ref={anchorRef}>
      {/* 图标预览 = 触发器（点击弹出选择器），右下铅笔角标 */}
      <button
        type="button"
        disabled={disabled || busy}
        onClick={() => {
          if (!open) computePos();
          setOpen((v) => !v);
        }}
        aria-label={t("pm.iconPicker.title")}
        className="relative block rounded-lg ring-1 ring-zinc-300/60 transition hover:ring-zinc-400 disabled:cursor-not-allowed disabled:opacity-50 dark:ring-zinc-600/60 dark:hover:ring-zinc-400"
      >
        <ProjectAvatar project={project} size={size} />
        <span className="absolute -bottom-0.5 -right-0.5 flex h-5 w-5 items-center justify-center rounded-full bg-zinc-800 text-white shadow-sm dark:bg-zinc-600">
          <svg viewBox="0 0 16 16" className="h-3 w-3 fill-current" xmlns="http://www.w3.org/2000/svg">
            <path d="M11.013 1.427a1.75 1.75 0 0 1 2.474 0l1.086 1.086a1.75 1.75 0 0 1 0 2.474l-8.61 8.61c-.21.21-.47.364-.756.445l-3.251.93a.75.75 0 0 1-.927-.928l.929-3.25c.081-.286.235-.547.445-.758l8.61-8.61Zm.176 4.823L11.5 7l-3-3-.31.31a.75.75 0 0 0-.177.764l.93 3.251a.75.75 0 0 1-.927.928l-3.251-.93Z" />
          </svg>
        </span>
      </button>

      {open &&
        createPortal(
          <>
            {/* 点击遮罩关闭 */}
            <div className="fixed inset-0 z-40" onClick={() => setOpen(false)} />
            <div
              className="fixed z-50"
              style={{ top: popupPos?.top ?? 0, left: popupPos?.left ?? 0 }}
            >
              <OverlayScrollbar
                maxHeight="min(calc(100vh-120px),460px)"
                className="p-3 overscroll-contain"
                containerClassName="w-72 overflow-hidden rounded-lg border border-border-outer bg-modal-surface shadow-lg"
              >
                <div className="grid grid-cols-4 gap-2">
                  <button
                    onClick={() => fileInputRef.current?.click()}
                    disabled={busy}
                    aria-label={t("pm.iconPicker.upload")}
                    className="flex aspect-square items-center justify-center rounded-md border border-dashed border-zinc-300 text-text-tertiary transition-colors hover:border-zinc-400 hover:text-zinc-600 disabled:opacity-50 dark:border-zinc-600 dark:hover:border-zinc-400"
                  >
                    <span className="text-lg">+</span>
                  </button>
                  {assets.map((asset) => (
                    <div
                      key={asset.relative_path}
                      className={`relative aspect-square overflow-hidden rounded-md border-2 transition-colors ${
                        value?.kind === "custom" && value.path === asset.relative_path
                          ? "border-zinc-800 dark:border-zinc-200"
                          : "border-transparent hover:border-zinc-300 dark:hover:border-zinc-600"
                      }`}
                    >
                      <AuthedImage
                        src={resolveProjectIconUrl(project.id, asset.relative_path)}
                        alt={asset.relative_path}
                        className="h-full w-full cursor-pointer object-cover"
                        onClick={() => select({ kind: "custom", path: asset.relative_path })}
                      />
                    </div>
                  ))}
                </div>

                {/* 清除（回落默认图标） */}
                <button
                  onClick={() => select(null)}
                  disabled={busy}
                  className="mt-3 w-full rounded-md px-3 py-1.5 text-xs font-medium text-text-tertiary hover:bg-zinc-100 disabled:opacity-50 dark:hover:bg-zinc-700"
                >
                  {t("pm.iconPicker.clear")}
                </button>
              </OverlayScrollbar>
            </div>
          </>,
          document.body,
        )}

      {/* 隐藏的选图 input（accept 与 IMAGE_EXTENSIONS 对齐） */}
      <input
        ref={fileInputRef}
        type="file"
        accept={IMAGE_EXTENSIONS.map((e) => `.${e}`).join(",")}
        className="hidden"
        onChange={(e) => {
          const file = e.target.files?.[0];
          e.target.value = "";
          if (file) void handleUpload(file);
        }}
      />
    </div>
  );
}
