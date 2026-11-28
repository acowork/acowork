/**
 * `lib/linkResolver.ts` — 一个 Markdown `href` 到底是什么，以及它指向的文档库
 * 条目。
 *
 * ## 为什么是一个独立模块
 *
 * 链接点击过去分散在三处：聊天气泡（`handleChatMarkdownLinkClick`）、Markdown
 * 预览（`MarkdownPreviewView.handleLinkClick`）、交付物卡片。它们各写一遍
 * 「http(s)？锚点？透传协议？本地路径？」的判定，而且**都只认工作区文件** ——
 * 于是指向文档库的链接（agent 交付物的自然形状：`项目管理/p-x/评审/y.md`、
 * `.pdf`、`.pptx`）一律落到「未找到」的 toast，用户读到的就是「点了没反应」。
 *
 * 这个模块回答两个**纯问题**（不认识 store、不认识 tab、不认识 toast）：
 *
 *   1. `classifyMarkdownLink(href)` —— 这个 href 该由谁处理：页内锚点 / 外部
 *      URL / 透传协议 / 本地路径（含 `#L42` 行号与 Windows 盘符形态）。
 *   2. `resolveDocLink(ref)` —— 这个库内地址指向哪一条目，或者为什么指不到
 *      （`denied` 与 `missing` 必须分开：前者是权限，后者是不存在）。
 *
 * 打开与提示留在调用方（它们才知道自己那一层的手势与文案）。
 *
 * ## 不做什么
 *
 * 不猜：`missing` 就是「工作区与文档库都没有」，不做 basename 模糊匹配、不做
 * 关键字搜索；文档库地址必须完整且合法（`doc-…` 或末段严格等于
 * `DocMeta::file_name()` 的库内路径）。解析器在服务端只有一份
 * （`resolve_entry`），这里只是它的桌面侧客户端。
 */
import { resolveDocEntries } from "./doc-api";
import type { DocEntryRef } from "./docEntry";

/** 直接交给 webview 的协议：不认识、也不该由我们打开。 */
export const PASSTHROUGH_SCHEMES = /^(https?:|data:|asset:|blob:|mailto:|tel:)/i;

/** 绝对路径前缀（POSIX `/…` 或 Windows `C:\…` / `C:/…`）。 */
export const ABSOLUTE_PATH = /^([/\\]|[A-Za-z]:[/\\])/;

/** 一个 Markdown href 的处理归属。 */
export type MarkdownLinkKind = "anchor" | "external" | "passthrough" | "path";

export interface ClassifiedMarkdownLink {
    kind: MarkdownLinkKind;
    /** 原始 href（提示文案里要原样回显，用户才能认出自己点的是哪一条）。 */
    href: string;
    /** kind === "external" 时可直接交给 URL tab 的地址。 */
    url?: string;
    /** kind === "path" 时的本地 / 库内地址。 */
    path?: string;
    /** `#L42` / `#L42-L67` 的起始行（仅 kind === "path"，且仅对源码文件有意义）。 */
    line?: number;
}

/**
 * 百分号解码，失败就原样返回。
 *
 * 为什么必须解码：react-markdown 会把 `<a href>` 里的非 ASCII 编成
 * `%E8%AE%BE%E8%AE%A1`，而文档库里的路径基本都是中文（`研发/设计.md`）。不解码
 * 就等于「所有中文路径都找不到」—— 一个只在点击时才暴露的静默失败。手写的、含
 * 裸 `%` 的 href 解码会抛，那种情况保持原样，让服务端按非法地址报错。
 */
export function decodePathSafe(path: string): string {
    if (!path.includes("%")) return path;
    try {
        return decodeURIComponent(path);
    } catch {
        return path;
    }
}

/** 解析后的本地路径形态：路径 + 可选行号。 */
export interface ChatLinkTarget {
    /** 路径部分；URL 化的盘符形态（`/D:/…`）已去掉前导斜杠。 */
    path: string;
    /** `#L42` / `#L42-L67` 的起始行（1-based），没有锚点时为 undefined。 */
    line?: number;
}

/**
 * 把 href 拆成「路径 + 可选行号」，并归一两种形态：
 *   - `/D:/repo/a.md` —— `file://` 转换的残留，以及把盘符 URL 化的编辑器；
 *   - `D:/repo/a.md` —— 原样盘符路径，工作区解析器按根前缀映射。
 * 相对路径（`docs/a.md`）原样通过；`#foo` 这类非行号锚点直接丢弃。
 */
export function parseChatLinkHref(href: string): ChatLinkTarget {
    const [rawPath, fragment = ""] = href.split("#", 2);
    let path = decodePathSafe(rawPath);
    if (/^\/[A-Za-z]:[/\\]/.test(path)) path = path.slice(1);
    const m = fragment.match(/^L(\d+)(?:-L(\d+))?$/);
    return { path, line: m ? parseInt(m[1], 10) : undefined };
}

/**
 * href 归属判定。**唯一实现**：聊天气泡、Markdown 预览、以及将来的任何
 * markdown 渲染面都调用它，而不是各自写一遍正则。
 *
 * 顺序即优先级：锚点（页内滚动）→ 外部 URL → 透传协议 → 本地路径。
 */
export function classifyMarkdownLink(href: string): ClassifiedMarkdownLink {
    const raw = href ?? "";
    if (!raw || raw.startsWith("#")) return { kind: "anchor", href: raw };
    if (/^https?:\/\//i.test(raw)) {
        return { kind: "external", href: raw, url: raw };
    }
    if (PASSTHROUGH_SCHEMES.test(raw)) return { kind: "passthrough", href: raw };
    const parsed = parseChatLinkHref(raw);
    return { kind: "path", href: raw, path: parsed.path, line: parsed.line };
}

/**
 * 把一个可能相对的库内地址，按「当前文档所在的目录」解析成库根路径。
 *
 * 为什么需要：文档正文里的互相引用写的是相对形式（`./设计.md`、`../评审/y.md`），
 * 而库内地址的语法是**从库根起**的。这里做的是纯粹的路径算术（`.`、`..`、重复
 * 斜杠、根外钳制），不是模糊匹配 —— 结果要么是一条完整合法的地址，要么就解析
 * 不出来，交给调用方报「没有」。
 */
export function resolveLibraryPath(basePath: string, href: string): string {
    const target = (href ?? "").trim();
    if (!target) return "";
    const isRooted = target.startsWith("/");
    const segments: string[] = [];
    if (!isRooted) {
        const dir = (basePath ?? "").split("/").slice(0, -1);
        segments.push(...dir);
    }
    for (const seg of target.replace(/^\/+/, "").split("/")) {
        if (!seg || seg === ".") continue;
        if (seg === "..") {
            // 越过库根不再是合法地址：钳制掉，而不是解析成 `../x`
            // （那会被服务端当非法地址，错误却指向调用方）。
            segments.pop();
            continue;
        }
        segments.push(seg);
    }
    return segments.join("/");
}

/**
 * 一个库内链接值得尝试的地址，按优先级去重。
 *
 * 两条都是**完整合法**的读法，不是猜测：先按「相对当前文档」解析（正文里的
 * 自然写法），再按「库根起」解析（agent 直接抄交付物路径时的写法）。仍然不做
 * basename 匹配，也不搜索。
 */
export function docLinkRefs(basePath: string, href: string): string[] {
    const parsed = parseChatLinkHref(href);
    const raw = parsed.path;
    const out: string[] = [];
    for (const candidate of [resolveLibraryPath(basePath, raw), raw.replace(/^\/+/, "")]) {
        if (candidate && !out.includes(candidate)) out.push(candidate);
    }
    return out;
}

/**
 * 文档库解析结果。
 *
 * `missing` 合并了服务端的 `not_found` 与 `invalid`：对点击者而言「这条地址在
 * 库里没有」是同一件事，区分它们只会逼业务层去编两套文案。`denied` 必须独立 ——
 * 条目存在但无权查看，说成「不存在」既误导也不安全。
 */
export type DocLinkOutcome =
    | { status: "found"; entry: DocEntryRef }
    | { status: "denied" }
    | { status: "missing" };

/**
 * 把一个库内地址解析成可打开的条目。
 *
 * 网络 / 服务错误按 `missing` 返回：链接点击的兜底语义就是「这里打不开」，抛异常
 * 出去只会让点击处理器多一条没人 catch 的路径。
 */
export async function resolveDocLink(ref: string): Promise<DocLinkOutcome> {
    const target = ref.trim();
    if (!target) return { status: "missing" };
    let results;
    try {
        results = await resolveDocEntries([target]);
    } catch {
        return { status: "missing" };
    }
    const hit = results[0];
    if (!hit) return { status: "missing" };
    if (hit.status === "denied") return { status: "denied" };
    if (hit.status !== "found" || !hit.doc_id) return { status: "missing" };
    return {
        status: "found",
        entry: {
            docId: hit.doc_id,
            // 路径渲染失败时退回用户点的那一条：它至少是人能读的东西，
            // 而内部 id 不是（服务端已保证 `path` 不含 `dir_id`）。
            path: hit.path ?? target,
            name: hit.name ?? hit.path ?? target,
            // `ext` 在 `found` 上恒常存在（`null` = Markdown），这里保持"三态"：
            // 缺字段 = 服务端未给出 → 交给 `docEntryKind` 从路径后缀推。
            ext: hit.ext ?? null,
        },
    };
}