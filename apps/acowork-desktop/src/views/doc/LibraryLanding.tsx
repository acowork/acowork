/**
 * LibraryLanding — 文档视图的「未打开文档」落地页：文档库卡片网格。
 *
 * 为什么要有它：根层语义从「一组目录」变成「一组文档库」之后，空态还只
 * 显示一句「请从左侧选择文档」就把新语义藏起来了。这里像项目列表一样把
 * 库摆出来 —— 图标、名称、描述、所有者、成员数、文档数，一眼能认出
 * 「这就是一个像项目一样的容器」。
 *
 * 数据源是已加载的根树（`nodes[root].dirs`），不额外发请求：根树本来
 * 就在进入视图时加载，库的图标/描述/creator/ACL 都在 DirMeta 上（服务端
 * 显示缓存），所以卡片渲染零新端点。
 */

import { useEffect, useMemo, useState } from "react";
import { Plus } from "lucide-react";
import { OverlayScrollbar } from "../../components/common/OverlayScrollbar";
import { useTranslation } from "../../i18n/useTranslation";
import { useDocTreeStore } from "../../stores/doc/treeStore";
import { useDocHealthStore } from "../../stores/doc/healthStore";
import { useDocEditorStore } from "../../stores/doc/editorStore";
import { DOC_ROOT_DIR_ID } from "../../lib/doc-types";
import type { LibraryMeta } from "../../lib/doc-types";
import { useResolvedMembers } from "../pm/pmMembers";
import { LibraryIcon } from "./LibraryIcon";
import { DocLibraryDialog } from "./DocLibraryDialog";

export function LibraryLanding() {
  const { t } = useTranslation();
  const root = useDocTreeStore((s) => s.nodes[DOC_ROOT_DIR_ID]);
  const loadDir = useDocTreeStore((s) => s.loadDir);
  const expanded = useDocTreeStore((s) => s.expanded);
  const health = useDocHealthStore((s) => s.healthy);
  const [creating, setCreating] = useState(false);

  // 根树还没加载（首次进入 / 落地页先于侧栏挂载）时兜底拉一次；
  // loadDir 自身有缓存与防重入。副作用不能写在 render 里。
  useEffect(() => {
    if (!root) void loadDir(DOC_ROOT_DIR_ID);
  }, [root, loadDir]);

  const libraries = useMemo<LibraryMeta[]>(() => root?.dirs ?? [], [root]);

  // 所有 owner 一次解析（hooks 不能在 map 里逐个调）。
  const ownerMembers = useMemo(
    () =>
      libraries
        .map((l) => l.creator)
        .filter((id): id is string => !!id)
        .map((id) => ({ instance_id: id, kind: "user" as const, added_at: "" })),
    [libraries],
  );
  const owners = useResolvedMembers(ownerMembers);
  const ownerName = (id: string | null | undefined) => {
    if (!id) return null;
    const hit = owners.find((o) => o.instance_id === id);
    return hit?.displayName ?? id;
  };

  const openLibrary = (lib: LibraryMeta) => {
    useDocTreeStore.getState().selectNode({ kind: "dir", id: lib.dir_id, name: lib.name });
    if (!expanded[lib.dir_id]) void useDocTreeStore.getState().expandDir(lib.dir_id);
    const firstDoc = useDocTreeStore.getState().nodes[lib.dir_id]?.files[0];
    if (firstDoc) void useDocEditorStore.getState().requestOpen(firstDoc.doc_id);
  };

  return (
    <div className="flex h-full flex-col overflow-hidden">
      <div className="flex min-h-[var(--ui-list-header-h)] shrink-0 items-center justify-between border-b border-border-divider px-4">
        <span className="text-xs font-medium text-text-secondary">
          {t("doc.library.allLibraries")}
          {libraries.length > 0 ? ` · ${libraries.length}` : ""}
        </span>
        <button
          type="button"
          onClick={() => setCreating(true)}
          disabled={health === false}
          className="flex items-center gap-1 rounded-md px-2 py-1 text-xs font-medium text-text-secondary transition-colors hover:bg-[var(--color-accent)]/10 disabled:cursor-not-allowed disabled:opacity-40"
        >
          <Plus className="h-3.5 w-3.5" />
          {t("doc.library.newLibrary")}
        </button>
      </div>

      <OverlayScrollbar fill="flex" className="p-4">
        {libraries.length === 0 ? (
          <div className="flex h-full flex-col items-center justify-center gap-2 text-text-tertiary">
            <LibraryIcon icon={null} size={40} className="opacity-60" />
            <p className="text-xs">{t("doc.library.emptyTitle")}</p>
            <p className="max-w-sm text-center text-11">{t("doc.library.emptyHint")}</p>
            <button
              type="button"
              onClick={() => setCreating(true)}
              className="mt-1 rounded-md bg-btn-solid px-3 py-1.5 text-xs font-medium text-white hover:bg-btn-solid-hover"
            >
              {t("doc.library.newLibrary")}
            </button>
          </div>
        ) : (
          <div className="grid grid-cols-[repeat(auto-fill,minmax(220px,1fr))] gap-3">
            {libraries.map((lib) => (
              <button
                key={lib.dir_id}
                type="button"
                onClick={() => openLibrary(lib)}
                className="group flex min-h-[104px] flex-col gap-2 rounded-md border border-border-outer bg-modal-surface p-3 text-left transition-colors hover:border-border-strong hover:bg-surface-hover"
              >
                <span className="flex items-center gap-2">
                  <LibraryIcon icon={lib.icon} size={28} />
                  <span className="min-w-0 flex-1 truncate text-xs font-medium text-text">
                    {lib.name}
                  </span>
                </span>
                <span className="line-clamp-2 min-h-[2.4em] text-11 text-text-tertiary">
                  {lib.description || t("doc.library.noDescription")}
                </span>
                <span className="mt-auto flex items-center justify-between gap-2 text-10 text-text-tertiary">
                  <span className="truncate">
                    {t("doc.library.owner")}:{" "}
                    {ownerName(lib.creator) ?? t("doc.library.unknownOwner")}
                  </span>
                  <span className="shrink-0">
                    {t("doc.library.memberCount", { count: lib.acl?.length ?? 0 })}
                  </span>
                </span>
              </button>
            ))}
          </div>
        )}
      </OverlayScrollbar>

      {creating && (
        <DocLibraryDialog mode="create" onClose={() => setCreating(false)} />
      )}
    </div>
  );
}