/**
 * DocFilePreview — 文档库里「导入文件」条目的预览面板。
 *
 * 预览方式与右侧工作区**完全一致**，因为这根本就是同一段代码：
 * `components/editor/documentViewer.tsx::renderDocumentViewer` 决定挂哪个
 * viewer（PdfViewer / SheetViewer / DocxViewer / PptxViewer / OfficePdfViewer /
 * UnsupportedDocViewer），图片用工作区那个 ImageViewer（缩放 + 像素坐标）。
 * 桥接点只有一个：`docFileRef()` 把文档条目映射成 viewers 要的
 * `(agentId, workspaceId, relPath)` 三元组，`__doc__` 哨兵让 URL 构造器指向
 * `GET /api/doc/docs/{id}/file`。所以这里没有任何一个 viewer 需要被改动。
 *
 * 唯一的差别是「加入对话」那一组手势：文档库条目没有工作区路径可交给 agent，
 * 所以在 ImageViewer 上禁用（带原因 tooltip），文档类 viewer 拿到空的 chat
 * hand-off —— 与工作区里一个没有 composer 的会话表现一致。
 */
import { Suspense } from "react";
import { Loader2 } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { useWorkspaceFileBlob } from "../../hooks/useWorkspaceFileBlob";
import { docFileRef } from "../../lib/workspaceFileBlob";
import { renderDocumentViewer } from "../../components/editor/documentViewer";
import { ImageViewer } from "../../components/editor/ImageViewer";
import type { DocMeta } from "../../lib/doc-types";

/** 文档库预览没有工作区路径，所以图像 viewer 的对话手势不可用。 */
const IMAGE_EXT_RE = /^(?:png|jpe?g|gif|webp|bmp|svg)$/;

export function DocFilePreview({ doc }: { doc: DocMeta }) {
  const { t } = useTranslation();
  const file = docFileRef(doc);
  const ext = (doc.ext ?? "").toLowerCase();
  const isImage = IMAGE_EXT_RE.test(ext);
  // 图片要 blob URL（canvas 裁剪 / <img src>）；文档类 viewer 自己拉字节。
  const imageBlob = useWorkspaceFileBlob(isImage ? file : null, { mode: "url" });

  if (isImage) {
    if (imageBlob.error) return <ErrorState message={imageBlob.error} />;
    if (!imageBlob.blobUrl) return <Busy />;
    return (
      <ImageViewer
        src={imageBlob.blobUrl}
        fileName={file.fileName}
        addToChatDisabledTitle={t("doc.filePreview.addToChatUnavailable")}
        // 上面那个 title 让这两项不可点，所以这里永远不会被调到。
        onAddPointToChat={noop}
        onAddRegionToChat={noop}
      />
    );
  }

  return (
    <Suspense fallback={<Busy label={t("common.loading")} />}>
      {renderDocumentViewer(file, {})}
    </Suspense>
  );
}

function noop() {}

function Busy({ label }: { label?: string }) {
  return (
    <div className="flex h-full flex-col items-center justify-center gap-2 text-text-tertiary">
      <Loader2 className="h-5 w-5 animate-spin" aria-hidden />
      {label ? <span className="text-xs">{label}</span> : null}
    </div>
  );
}

function ErrorState({ message }: { message: string }) {
  return (
    <div className="flex h-full items-center justify-center p-4 text-xs text-error-quiet">
      {message}
    </div>
  );
}