/**
 * 文档库条目 ↔ 字节缓存的桥。
 *
 * ## 为什么需要一座桥
 *
 * 文档库的导入条目（pdf / docx / xlsx / pptx / 图片）必须与右侧工作区**完全
 * 一样地预览**：同一批 viewer（`documentViewer.tsx` 的分派表）、同一套保存到
 * 本地 / 外部打开手势、同一份 blob LRU。而那些 viewer 全部按工作区坐标
 * `(agentId, workspaceId, relPath)` 取字节，缓存也按这个三元组做键。
 *
 * 所以桥只做一件事：把一条文档条目映射成那个三元组。文档条目没有工作区，
 * 于是用哨兵把两个坐标填成 doc 自己的身份（`__agent_home__` 早已用同样的办法
 * 代表「没有 workspace 的 home」）：
 *
 *     agentId     = `__doc__`       （来源标记，缓存与 viewer 只当它是坐标）
 *     workspaceId = docId           （条目身份，跨改名 / 移动稳定）
 *     relPath     = 文件名           （`docEntry.ts::docEntryFileName()` = 服务端
 *                                    的 `DocMeta::file_name()`，显示名 + 另存为名）
 *
 * ## 边界
 *
 * 这个模块属于中间件，只做「条目 → 缓存坐标」这一件事：条目形状和命名规则来自
 * `docEntry.ts`，它不重新拼 `name + "." + ext`；字节地址来自
 * `doc-api.ts::getDocFileUrl`（R1 的唯一 URL 构造者），它不拼 `/api/doc`；
 * **不认识**任何业务（链接、聊天、交付物、tab）。
 */
import { DOC_FILE_AGENT_ID, getDocFileUrl } from "./doc-api";
import { docEntryFileName, type DocEntryRef } from "./docEntry";
import { primeWorkspaceFileBlob } from "./workspaceFileBlob";

export { DOC_FILE_AGENT_ID };

/** viewer / blob 缓存要的三元组 + 显示用文件名。 */
export interface DocFileRef {
    agentId: string;
    workspaceId: string;
    relPath: string;
    fileName: string;
}

/**
 * 文档条目 → 字节缓存的坐标。
 *
 * 文件名只由 `docEntryFileName()` 给出（路径末段 = 服务端渲染的
 * `DocMeta::file_name()`），这个模块不重新拼 `name + "." + ext` —— 拼法有第二份，
 * 就会出现「tab 叫 `报告.pdf`、缓存 key 叫 `报告`」这种同一份字节两个身份的问题。
 */
export function docFileRef(entry: DocEntryRef): DocFileRef {
    const fileName = docEntryFileName(entry);
    return {
        agentId: DOC_FILE_AGENT_ID,
        workspaceId: entry.docId,
        relPath: fileName,
        fileName,
    };
}

/**
 * 把**已经是字节**的文档写回 blob 缓存（app 保存回写用）。
 *
 * 回写后必须刷新缓存，否则下一次打开拿到的还是旧字节 —— 而这个刷新只有缓存
 * 自己的 key 规则知道怎么做，所以由这里代劳，调用方不必知道三元组。
 */
export function primeDocFileBlob(entry: DocEntryRef, bytes: Uint8Array): void {
    const ref = docFileRef(entry);
    primeWorkspaceFileBlob(ref.agentId, ref.workspaceId, ref.relPath, bytes);
}

/**
 * 只有 `fileName` 时（viewer 的坐标里就这些）刷新缓存。
 *
 * 文件名 → 条目的拆解不再由本模块承担：`DocEntryRef` 允许只给 `path`（末段就是
 * 文件名），所以这里直接拿 `fileName` 当路径用 —— 命名规则仍然只有
 * `docEntry.ts::docEntryFileName()` 一份。
 */
export function primeDocFileBlobFromFileName(
    docId: string,
    fileName: string,
    bytes: Uint8Array,
): void {
    primeDocFileBlob({ docId, path: fileName, name: fileName }, bytes);
}

/** 文档条目的字节地址（唯一构造者是 `doc-api.ts`）。 */
export function docEntryFileUrl(docId: string): string {
    return getDocFileUrl(docId);
}