/**
 * DocImportTargetDialog — 「把文件加到文档库」时选目标位置。
 *
 * 两步合成一个对话框：先选文档库（根层节点），再在库里选目录（默认库本身）。
 * 目录浏览器是懒展开的（`getTree(dir_id)` 返回直接子项），与左侧 DocTreeSidebar
 * 同一套数据源、同一套语义——不复制一棵树，只是同一份数据换了个选择用途。
 *
 * 只负责「选目标并确认」，落盘（字节从哪来、怎么上传）留给调用方：
 * 本机上传和工作区导入的来源不同，唯一相同的是目标目录。这样对话框不需要
 * 知道 File / workspace 的存在，测试也不需要 mock blob。
 */
import { useEffect, useState } from "react";
import { ChevronRight, Folder, Loader2 } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { cn } from "../../lib/utils";
import { listLibraries, getTree } from "../../lib/doc-api";
import type { DirMeta, DocTreeNode } from "../../lib/doc-types";

interface Props {
  /** 浮动提示里显示的文件名（可空：调用方可能一次传多个）。 */
  fileName?: string;
  onCancel: () => void;
  /** 选定的目标目录 id（document library 本身即合法目标）。 */
  onConfirm: (dirId: string) => void;
}

export function DocImportTargetDialog({ fileName, onCancel, onConfirm }: Props) {
  const { t } = useTranslation();
  const [libraries, setLibraries] = useState<DirMeta[] | null>(null);
  const [libraryId, setLibraryId] = useState<string | null>(null);
  const [nodes, setNodes] = useState<Record<string, DocTreeNode>>({});
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});
  const [targetDirId, setTargetDirId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  // 加载文档库列表，默认选中第一个（并把它当目标）。
  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const list = await listLibraries();
        if (cancelled) return;
        setLibraries(list);
        const first = list[0];
        if (first) {
          setLibraryId(first.dir_id);
          setTargetDirId(first.dir_id);
          setExpanded({ [first.dir_id]: true });
          // 首屏就把默认库的直接子项拉下来：不拉的话目录面板只剩一行「库根目录」，
          // 看起来像这个库是空的，用户得先点一下根行才看到目录。
          const node = await getTree(first.dir_id);
          if (!cancelled) setNodes({ [first.dir_id]: node });
        }
      } catch (e) {
        if (!cancelled) setError(e instanceof Error ? e.message : String(e));
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  const loadDir = async (dirId: string) => {
    if (nodes[dirId]) return;
    try {
      const node = await getTree(dirId);
      setNodes((n) => ({ ...n, [dirId]: node }));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  const pickLibrary = (dirId: string) => {
    setLibraryId(dirId);
    setTargetDirId(dirId);
    setExpanded({ [dirId]: true });
    void loadDir(dirId);
  };

  const toggleDir = (dirId: string) => {
    const open = !expanded[dirId];
    setExpanded((e) => ({ ...e, [dirId]: open }));
    if (open) void loadDir(dirId);
  };

  const renderDirRows = (dirId: string, depth: number) => {
    const node = nodes[dirId];
    if (!node) return null;
    return node.dirs
      .filter((d) => !d.deleted)
      .map((d) => {
        const open = !!expanded[d.dir_id];
        const selected = targetDirId === d.dir_id;
        return (
          <div key={d.dir_id}>
            <div
              className={cn(
                "flex items-center gap-1 rounded-md py-1 pr-1 text-xs",
                selected ? "bg-[var(--color-accent)]/15 text-text" : "hover:bg-[var(--color-accent)]/10",
              )}
              style={{ paddingLeft: depth * 14 }}
            >
              <button
                type="button"
                aria-label={t(open ? "doc.collapseDir" : "doc.expandDir")}
                className="flex h-4 w-4 shrink-0 items-center justify-center text-text-tertiary"
                onClick={() => toggleDir(d.dir_id)}
              >
                <ChevronRight className={cn("h-3 w-3 transition-transform", open && "rotate-90")} />
              </button>
              <button
                type="button"
                className="flex min-w-0 flex-1 items-center gap-1 text-left"
                onClick={() => setTargetDirId(d.dir_id)}
                onDoubleClick={() => toggleDir(d.dir_id)}
              >
                <Folder className="h-3 w-3 shrink-0 text-text-tertiary" aria-hidden />
                <span className="truncate">{d.name}</span>
              </button>
            </div>
            {open ? renderDirRows(d.dir_id, depth + 1) : null}
          </div>
        );
      });
  };

  const activeLibrary = libraries?.find((l) => l.dir_id === libraryId) ?? null;

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      <div className="absolute inset-0 bg-modal-overlay" onClick={onCancel} />
      <div
        className="relative z-10 flex max-h-[70vh] w-full max-w-lg flex-col rounded-md border border-border-outer bg-modal-surface shadow-xl"
        role="dialog"
        aria-modal="true"
        aria-label={t("doc.importTarget.title")}
      >
        <h3 className="flex items-center border-b border-border-divider px-5 py-3 text-sm font-semibold text-text">
          {t("doc.importTarget.title")}
        </h3>

        <div className="flex min-h-0 flex-1 flex-col gap-2 px-3 py-2">
          {fileName ? (
            <p className="truncate px-1 text-11 text-text-tertiary">
              {t("doc.importTarget.file", { name: fileName })}
            </p>
          ) : null}

          {error ? (
            <p className="px-1 text-11 text-error-quiet">{error}</p>
          ) : libraries === null ? (
            <div className="flex items-center gap-2 px-1 py-3 text-xs text-text-tertiary">
              <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden />
              {t("common.loading")}
            </div>
          ) : libraries.length === 0 ? (
            <p className="px-1 py-3 text-xs text-text-tertiary">
              {t("doc.importTarget.noLibrary")}
            </p>
          ) : (
            // 两栏：左边文档库列表（竖排、可滚动），右边选中库的目录树。
            // 库多的时候横排 chip 只能横向翻滚，竖列表才是「有多少滚多少」。
            <div className="flex min-h-0 flex-1 gap-2">
              <div className="flex w-32 shrink-0 flex-col">
                <span className="px-1 pb-1 text-10 uppercase tracking-wide text-text-tertiary">
                  {t("doc.importTarget.library")}
                </span>
                <div className="min-h-0 flex-1 overflow-y-auto rounded-md border border-border-divider p-1">
                  {libraries.map((lib) => (
                    <button
                      key={lib.dir_id}
                      type="button"
                      onClick={() => pickLibrary(lib.dir_id)}
                      title={lib.name}
                      className={cn(
                        "block w-full truncate rounded-md px-2 py-1 text-left text-xs",
                        libraryId === lib.dir_id
                          ? "bg-[var(--color-accent)] text-white"
                          : "text-text-secondary hover:bg-[var(--color-accent)]/10",
                      )}
                    >
                      {lib.name}
                    </button>
                  ))}
                </div>
              </div>

              {/* 目录选择（默认 = 库本身） */}
              <div className="min-h-0 flex-1 overflow-auto rounded-md border border-border-divider p-1">
                <div
                  className={cn(
                    "flex items-center gap-1 rounded-md px-1 py-1 text-xs",
                    targetDirId === libraryId
                      ? "bg-[var(--color-accent)]/15 text-text"
                      : "hover:bg-[var(--color-accent)]/10",
                  )}
                >
                  <button
                    type="button"
                    className="flex min-w-0 flex-1 items-center gap-1 text-left"
                    onClick={() => {
                      if (libraryId) {
                        setTargetDirId(libraryId);
                        setExpanded((e) => ({ ...e, [libraryId]: true }));
                        void loadDir(libraryId);
                      }
                    }}
                  >
                    <Folder className="h-3 w-3 shrink-0 text-text-tertiary" aria-hidden />
                    <span className="truncate">
                      {activeLibrary?.name ?? t("doc.importTarget.libraryRoot")}
                      <span className="ml-1 text-10 text-text-tertiary">
                        {t("doc.importTarget.libraryRoot")}
                      </span>
                    </span>
                  </button>
                </div>
                {libraryId ? renderDirRows(libraryId, 1) : null}
              </div>
            </div>
          )}
        </div>

        <div className="flex items-center justify-end gap-2 border-t border-border-divider px-5 py-2">
          <button
            type="button"
            onClick={onCancel}
            className="rounded-md px-3 py-1.5 text-xs font-medium text-text-secondary hover:bg-[var(--color-accent)]/10"
          >
            {t("common.cancel")}
          </button>
          <button
            type="button"
            disabled={!targetDirId}
            onClick={() => targetDirId && onConfirm(targetDirId)}
            className="btn-accent rounded-md px-3 py-1.5 text-xs font-medium disabled:opacity-40"
          >
            {t("doc.importTarget.confirm")}
          </button>
        </div>
      </div>
    </div>
  );
}