/**
 * `DocEntryRef` — 桌面侧「一条文档库条目」的唯一形状，以及由它派生的
 * **命名规则**与**类型判定**（Markdown 正文 vs 导入文件字节）。
 *
 * ## 为什么要有它
 *
 * 同一条文档条目在桌面上有三条进入路径：文档库列表 / 目录树（拿到的是
 * `DocRead` / `DocMeta`）、聊天的交付物卡片（拿到的是 runtime 戳进
 * `metadata.docs` 的 `{docId, path, name}`）、Markdown 链接（P3，拿到的是
 * `/refs/resolve` 的响应）。三条路径过去各自拼 `{docId, path, title}` 的松散
 * 字面量，于是「这条目到底是正文还是字节」这个判定在每个调用点各写一遍，且
 * 随时间漂移（例如把 `.pdf` 当成 Markdown 打开，得到一个空白编辑器）。
 *
 * 收敛办法是把**数据形状**固定成这一个 `DocEntryRef`，把两条规则放在它旁边：
 *
 *   - `docEntryFileName()` —— 条目的显示名 / 另存为名 / 缓存 key 的末段，
 *     与磁盘上的文件名同源（服务端 `DocMeta::file_name()` = `{name}.{ext ?? "md"}`）。
 *   - `docEntryKind()` —— 决定这个条目该进**文本视图**还是**字节 viewer**。
 *     唯一依据是扩展名，而扩展名优先取显式字段、其次取路径末段的后缀 —— 因为
 *     聊天交付物卡片与链接解析拿不到 `ext` 字段，只拿得到人读路径。
 *
 * ## 边界
 *
 * 这个模块是**纯函数 + 类型**：不认识 HTTP（地址在 `doc-api.ts`）、不认识字节
 * 缓存（坐标在 `docFileBlob.ts`）、不认识 tab（分派在 `fileEditorStore.ts` 的
 * `openDocEntry`）。正因如此它可以在任何一层被 import 而不引入环。
 */
import type { DocMeta, DocRead } from "./doc-types";

/** 一条文档库条目在桌面上的唯一形状。 */
export interface DocEntryRef {
    /** `doc-…`，跨改名 / 移动稳定 —— tab 身份与字节地址都只认它。 */
    docId: string;
    /** 人读库内路径，形如 `研发/纪要.md`；末段即 `docEntryFileName()`。 */
    path: string;
    /** 标题，**不含**扩展名（服务端 `DocMeta::name`）。 */
    name: string;
    /** 导入文件的扩展名（小写、不带点）；Markdown 文档为 `null`/缺省。 */
    ext?: string | null;
}

const MARKDOWN_EXTS = new Set(["md", "markdown"]);

/** 路径 / 文件名最后一个点之后的后缀（小写）；没有点或点是末字符时为 null。 */
function suffixOf(fileName: string): string | null {
    const dot = fileName.lastIndexOf(".");
    if (dot <= 0 || dot === fileName.length - 1) return null;
    return fileName.slice(dot + 1).toLowerCase();
}

/**
 * 条目的文件名（显示名 / 另存为名 / 字节缓存坐标的 `relPath`）。
 *
 * 优先取人读路径的末段：它就是服务端渲染出来的 `DocMeta::file_name()`，也就是
 * 磁盘上的真名，**不需要**在这里重新拼一次 `name + "." + ext`。只有路径缺失时
 * （理论上不该发生）才退回用 `name` / `ext` 拼。
 */
export function docEntryFileName(entry: DocEntryRef): string {
    const last = entry.path.split("/").filter((s) => s.length > 0).pop();
    if (last) return last;
    const ext = entry.ext?.trim().toLowerCase();
    return `${entry.name}.${ext ?? "md"}`;
}

/** 条目的扩展名：显式字段优先，否则取文件名后缀。Markdown 文档可能返回 null。 */
export function docEntryExt(entry: DocEntryRef): string | null {
    const explicit = entry.ext?.trim().toLowerCase();
    if (explicit) return explicit;
    return suffixOf(docEntryFileName(entry));
}

/**
 * 该条目该用哪一类视图打开。
 *
 * `"markdown"` = 有正文可渲染（`.md` / 无扩展名），进只读 Markdown tab；
 * `"file"`     = 导入文件（pdf/docx/xlsx/pptx/图片…），进字节 viewer —— 与工作区
 *                同一个 `renderDocumentViewer` 分派表。
 */
export function docEntryKind(entry: DocEntryRef): "markdown" | "file" {
    const ext = docEntryExt(entry);
    if (!ext || MARKDOWN_EXTS.has(ext)) return "markdown";
    return "file";
}

/** 快捷判定：是不是「导入文件」（没有可编辑正文）。 */
export function isImportedEntry(entry: DocEntryRef): boolean {
    return docEntryKind(entry) === "file";
}

/** `GET /docs/{id}` 的响应 → 条目形状（文档库面板那条路径）。 */
export function docEntryFromRead(read: DocRead): DocEntryRef {
    return {
        docId: read.meta.doc_id,
        path: read.path,
        name: read.meta.name,
        ext: read.meta.ext ?? null,
    };
}

/** 只有元数据、路径来自别处时（列表 / 树 / 搜索命中）用这个。 */
export function docEntryFromMeta(meta: DocMeta, path: string): DocEntryRef {
    return { docId: meta.doc_id, path, name: meta.name, ext: meta.ext ?? null };
}