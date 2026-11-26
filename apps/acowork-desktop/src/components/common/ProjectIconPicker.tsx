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
 *
 * 两种数据源（`mode` 判别，视觉同一份 markup）：
 * - `mode="project"`：已有项目，资产列表与上传都走 PM 端点（按 pid 寻址）。
 * - `mode="draft"`：**新建项目对话框** —— 项目还不存在，没有 pid，
 *   `POST /projects/:pid/icon` 无从调用。所以 "+" 只把 `File` 交给调用方
 *   （本地 object URL 预览），网格里的"那一张"就是它；真正上传发生在项目
 *   落地之后（见 `stores/pm/projectStore.ts` 的 createProject 编排）。
 *   发明第二套图标选择 UI 的代价就是两处视觉漂移，这里刻意只有一份。
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { AuthedImage } from "./AuthedImage";
import { ProjectAvatar } from "./ProjectAvatar";
import {
  DEFAULT_PROJECT_ICON,
  resolveProjectIconUrl,
  type ProjectIconRef,
} from "../../lib/projectIcon";
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

/**
 * 草稿模式数据源 —— 选中的 `File` 由调用方持有（创建后才上传），本组件只
 * 负责预览与"选/清"两个动作。
 */
export interface DraftIconSource {
  /** 本地预览 URL（object URL）；null = 还没选图，触发器显示默认图标。 */
  previewUrl: string | null;
  /** "+" 选完文件；`null` = 清除（回落默认图标）。 */
  onPickFile: (file: File | null) => void;
}

type CommonProps = {
  /** 预览边长（px）。默认 64（与头像设置一致）。 */
  size?: number;
  /** 离线等场景禁用。 */
  disabled?: boolean;
};

export type ProjectIconPickerProps = CommonProps &
  (
    | {
        mode: "project";
        project: PmProject;
        /** 当前图标引用（用于高亮选中项）。 */
        value: ProjectIconRef | null;
        /** 选中 / 清除回调（`null` = 用默认图标）。 */
        onChange: (ref: ProjectIconRef | null) => void;
      }
    | {
        mode: "draft";
        draft: DraftIconSource;
      }
  );

export function ProjectIconPicker(props: ProjectIconPickerProps) {
  const { disabled, size = 64 } = props;
  const draft = props.mode === "draft" ? props.draft : null;
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
  // 草稿模式没有 pid，服务端资产无从谈起 —— 网格只放手里那一张。
  const pid = props.mode === "project" ? props.project.id : null;
  useEffect(() => {
    if (!open) return;
    computePos();
    if (pid) {
      fetchProjectIconAssets(pid)
        .then(setAssets)
        .catch((err) => log.warn("[ProjectIconPicker] fetch assets failed:", err));
    }
    window.addEventListener("resize", computePos);
    window.addEventListener("scroll", computePos, true);
    return () => {
      window.removeEventListener("resize", computePos);
      window.removeEventListener("scroll", computePos, true);
    };
  }, [open, pid, computePos]);

  const select = useCallback(
    (ref: ProjectIconRef | null) => {
      if (draft) {
        // 草稿期"取消选中"的语义就是清掉手里那个 File；点本地 tile 是无操作
        // （它已经是选中的那张）。
        if (ref === null) draft.onPickFile(null);
      } else if (props.mode === "project") {
        props.onChange(ref);
      }
      setOpen(false);
    },
    [draft, props],
  );

  const handleUpload = useCallback(
    async (file: File) => {
      if (draft) {
        // 不上传，只把 File 交给调用方（object URL 预览由调用方维护）。
        draft.onPickFile(file);
        setOpen(false);
        return;
      }
      if (props.mode !== "project") return;
      setBusy(true);
      try {
        const asset = await uploadProjectIcon(props.project.id, file);
        // 刷新网格 + 立即选中刚上传的（前端回写 metadata 由父组件负责）。
        setAssets((prev) => [...prev, asset]);
        select({ kind: "custom", path: asset.relative_path });
      } catch (err) {
        log.warn("[ProjectIconPicker] upload failed:", err);
      } finally {
        setBusy(false);
      }
    },
    [draft, props, select],
  );

  // 网格内容归一：project = 服务端资产（AuthedImage 取字节）；draft = 本地
  // object URL（普通 <img>，blob: 不该走 bearer fetch）。
  const tiles = draft
    ? draft.previewUrl
      ? [{ key: "local", url: draft.previewUrl, selected: true, local: true }]
      : []
    : props.mode === "project"
      ? assets.map((asset) => ({
          key: asset.relative_path,
          url: resolveProjectIconUrl(props.project.id, asset.relative_path),
          selected:
            props.value?.kind === "custom" && props.value.path === asset.relative_path,
          local: false,
        }))
      : [];

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
        className="relative block rounded-lg ring-1 ring-border-strong/60 transition hover:ring-border-strong disabled:cursor-not-allowed disabled:opacity-50"
      >
        {draft ? (
          <img
            src={draft.previewUrl ?? DEFAULT_PROJECT_ICON}
            alt=""
            draggable={false}
            style={{ width: size, height: size }}
            className="rounded-md object-cover"
          />
        ) : props.mode === "project" ? (
          <ProjectAvatar project={props.project} size={size} />
        ) : null}
        <span className="absolute -bottom-0.5 -right-0.5 flex h-5 w-5 items-center justify-center rounded-full bg-btn-solid text-white shadow-sm">
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
                    className="flex aspect-square items-center justify-center rounded-md border border-dashed border-border-strong text-text-tertiary transition-colors hover:border-border-strong hover:text-text-tertiary disabled:opacity-50"
                  >
                    <span className="text-lg">+</span>
                  </button>
                  {tiles.map((tile) => (
                    <div
                      key={tile.key}
                      className={`relative aspect-square overflow-hidden rounded-md border-2 transition-colors ${
                        tile.selected
                          ? "border-text"
                          : "border-transparent hover:border-border-strong"
                      }`}
                    >
                      {tile.local ? (
                        <img
                          src={tile.url}
                          alt={tile.key}
                          draggable={false}
                          className="h-full w-full cursor-pointer object-cover"
                          onClick={() => select(null)}
                        />
                      ) : (
                        <AuthedImage
                          src={tile.url}
                          alt={tile.key}
                          className="h-full w-full cursor-pointer object-cover"
                          onClick={() =>
                            select({ kind: "custom", path: tile.key })
                          }
                        />
                      )}
                    </div>
                  ))}
                </div>

                {/* 清除（回落默认图标）。草稿期没选图时没什么可清。 */}
                {(!draft || draft.previewUrl) && (
                  <button
                    onClick={() => select(null)}
                    disabled={busy}
                    className="mt-3 w-full rounded-md px-3 py-1.5 text-xs font-medium text-text-tertiary hover:bg-[var(--color-accent)]/10 disabled:opacity-50 dark:hover:bg-[var(--color-accent)]/15"
                  >
                    {t("pm.iconPicker.clear")}
                  </button>
                )}
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
