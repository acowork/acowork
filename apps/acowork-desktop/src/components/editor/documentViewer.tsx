// SPDX-License-Identifier: Apache-2.0
// Copyright the ACowork project contributors.
//
// 「这个文件用哪个 viewer」的唯一一张表。
//
// 右侧工作区的编辑器 tab 和文档库里的导入文件预览必须长得一样、行为一样 ——
// 它们读的是同一批组件、同一份字节。所以分派只有这一处：两个调用方各自传
// (agentId, workspaceId, relPath, fileName)，由这里决定挂哪个组件。
//
// 为什么不是「抽一个 PreivewSwitch 组件」：分派结果是一个 ReactNode，调用方
// 的 `<Suspense>` 边界、tab 状态、聊天投递都在自己那一层，包成组件只会多一层
// 包装而没有共享任何东西。共享的是那张表，不是一个壳。
//
// 文档库那条路复用它时传的是一组 `__doc__` 哨兵坐标（见
// `lib/workspaceFileBlob.ts::docFileRef`），viewer 完全不知道字节来自文档服务。

import { lazy } from "react";
import type { ReactNode } from "react";
import type { PdfChatProps } from "./PdfViewer";

const PdfViewer = lazy(() => import("./PdfViewer"));
const SheetViewer = lazy(() => import("./SheetViewer"));
const DocxViewer = lazy(() => import("./DocxViewer"));
const PptxViewer = lazy(() => import("./PptxViewer"));
const UnsupportedDocViewer = lazy(() => import("./UnsupportedDocViewer"));
const OfficePdfViewer = lazy(() => import("./OfficePdfViewer"));

const OFFICE_EXT_RE = /\.(?:docx|doc|pptx|ppt|xlsx|xls)$/i;

/** Everything a document viewer needs to fetch the bytes it renders. */
export interface DocumentViewerTarget {
    agentId: string;
    workspaceId: string;
    relPath: string;
    fileName: string;
    /** Bumped to force a re-read after the file changed on disk. */
    previewSeq?: number;
}

export interface DocumentViewerMeta {
    /** Byte size, shown by the Office print-layout state. */
    size?: number;
    /** Disk mtime, invalidates a cached print-layout PDF. */
    mtimeMs?: number;
}

/**
 * Pick the viewer component for one document. `docChat` is the "send this to the
 * agent" hand-off — an empty object is legitimate: a surface without a composer
 * (the doc-library preview) mounts the same viewers with the pins disabled.
 */
export function renderDocumentViewer(
    file: DocumentViewerTarget,
    docChat: PdfChatProps,
    meta?: DocumentViewerMeta,
): ReactNode {
    const viewerFile = {
        agentId: file.agentId,
        workspaceId: file.workspaceId,
        relPath: file.relPath,
        fileName: file.fileName,
        previewSeq: file.previewSeq,
    };
    if (/\.pdf$/i.test(file.fileName)) return <PdfViewer {...viewerFile} {...docChat} />;
    // CSV keeps the grid (see OFFICE_EXT_RE); every other Office format tries
    // the print-layout PDF path first and keeps its JS renderer as the
    // fallback, so a machine without an engine — or one bad file — still
    // previews exactly as it did before.
    if (/\.csv$/i.test(file.fileName)) return <SheetViewer {...viewerFile} />;
    if (OFFICE_EXT_RE.test(file.fileName)) {
        const ext = file.fileName.toLowerCase().split(".").pop() ?? "";
        // docx-preview / SheetJS / the pptx card stay reachable — they are what
        // renders when this machine has no engine, and they are the only
        // renderers for the intermediate loading-free states.
        const fallback =
            ext === "xlsx" || ext === "xls" ? (
                <SheetViewer {...viewerFile} />
            ) : ext === "docx" ? (
                <DocxViewer {...viewerFile} />
            ) : ext === "pptx" ? (
                <PptxViewer {...viewerFile} />
            ) : (
                // Legacy .doc / .ppt — no in-app renderer by plan; the card
                // routes the user to save-as / open-externally.
                <UnsupportedDocViewer {...viewerFile} />
            );
        return (
            <OfficePdfViewer
                {...viewerFile}
                {...docChat}
                fallback={fallback}
                size={meta?.size}
                mtimeMs={meta?.mtimeMs}
            />
        );
    }
    // Anything else in the document set (nothing today) keeps the card.
    return <UnsupportedDocViewer {...viewerFile} />;
}