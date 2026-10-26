import React, { createElement, useCallback, useEffect, useLayoutEffect, useMemo, useRef, Children, isValidElement } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeRaw from "rehype-raw";
import { convertFileSrc } from "@tauri-apps/api/core";
import { Loader2 } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { CodeBlock } from "../chat/CodeBlock";
import { useAgentStore } from "../../stores/agentStore";
import { useFileEditorStore, type OpenFile } from "../../stores/fileEditorStore";
import { useWorkspaceStore } from "../../stores/workspaceStore";
import { useFileTreeStore, treeKey } from "../../stores/fileTree";
import { cn } from "../../lib/utils";
import {
    PASSTHROUGH_SCHEMES,
    resolveAssetAcrossWorkspaces,
    openFirstResolved,
    notifyLinkNotFound,
    type ResolvedAsset,
} from "./markdownLinkResolver";

/** ReactMarkdown `pre` override — code blocks with title bar (mirrors ChatPanel). */
const markdownComponents = {
    pre: ({ children }: { children?: React.ReactNode }) => {
        const childArray = Children.toArray(children);
        const codeEl = childArray.find(
            (child): child is React.ReactElement<{ className?: string; children?: React.ReactNode }> =>
                isValidElement(child) && child.type === "code",
        );
        if (codeEl) {
            const { className, children: codeContent } = codeEl.props;
            const language = className?.replace(/^language-/, "") || "";
            const code = Children.toArray(codeContent).join("");
            return <CodeBlock language={language} code={code} />;
        }
        return <pre>{children}</pre>;
    },
    /** Wrap <table> in a horizontally-scrollable container so an
     *  oversized column (URL / hash / path) doesn't squeeze the
     *  title column mid-word. Mirrors the same override in
     *  MessageBubble / CompactionCard; the wrapper is a pure
     *  scroll viewport, visual chrome (border / rounded corners)
     *  lives on the <table> itself in globals.css. */
    table: ({ children, ...rest }: React.TableHTMLAttributes<HTMLTableElement>) => (
        <div className="prose-table-scroll">
            <table {...rest}>{children}</table>
        </div>
    ),
};

interface MarkdownPreviewViewProps {
    file: OpenFile;
}

/**
 * Creates a ReactMarkdown component override that reads deprecated
 * `align` and `valign` attributes from the raw HAST node and applies
 * them via `useLayoutEffect` directly on the DOM element. This
 * approach bypasses React's prop filtering AND CSS specificity —
 * `el.style.setProperty(…, 'important')` overrides any stylesheet rule.
 */
function toAlignComponent(tag: string): React.ComponentType<any> {
    const AlignComponent = (props: any) => {
        const ref = useRef<HTMLElement>(null!);
        const nodeAlign = props.node?.properties?.align as string | undefined;
        const nodeValign = props.node?.properties?.valign as string | undefined;

        useLayoutEffect(() => {
            const el = ref.current;
            if (!el) return;
            if (nodeAlign === "center" || nodeAlign === "right" || nodeAlign === "left") {
                el.style.setProperty("text-align", nodeAlign, "important");
                el.setAttribute("data-align", nodeAlign);
            }
            if (nodeValign === "top" || nodeValign === "middle" || nodeValign === "bottom") {
                el.style.setProperty("vertical-align", nodeValign, "important");
                el.setAttribute("data-valign", nodeValign);
            }
        }, [nodeAlign, nodeValign]);

        const { node: _, style: __, ...rest } = props;
        return createElement(tag, { ...rest, ref });
    };
    return AlignComponent;
}

/**
 * Decide whether a markdown `href` should open in edit mode (`openFile`)
 * or read-only preview mode (`openPreview`). Markdown/Markdown-variant
 * links go to preview so the user sees rendered output; everything else
 * (source code, config, etc.) opens in the editor.
 *
 * Centralising this avoids drift between `a` and `img` handlers.
 */
function pickOpenAction(href: string): "openFile" | "openPreview" {
    return /\.m(?:d|arkdown|down)$/i.test(href) ? "openPreview" : "openFile";
}

/**
 * Module-level `<img>` wrapper used by the ReactMarkdown override map.
 *
 * Lives outside `MarkdownPreviewView` so its React identity is stable
 * across renders — react-markdown passes props through `components`,
 * and a freshly-allocated function component on every render would
 * remount the underlying `<img>` and break image caching/selection.
 *
 * Behaviour:
 *  - Pass-through schemes (http/https/data/asset/blob) → render as-is.
 *  - No workspace can resolve the src → render with the raw src and
 *    skip click-to-open.
 *  - Otherwise → render the first candidate's `asset://` URL and let
 *    clicks run the cross-workspace open resolver.
 */
interface ImageWithClickProps extends Omit<React.ImgHTMLAttributes<HTMLImageElement>, "src"> {
    src?: string;
    candidates: ResolvedAsset[];
    onClickImage: (src: string) => Promise<boolean>;
    clickHint: string;
}

const ImageWithClick = React.memo(function ImageWithClick({
    src,
    alt,
    candidates,
    onClickImage,
    clickHint,
    style,
    ...rest
}: ImageWithClickProps) {
    if (!src) return <img alt={alt} {...rest} />;
    if (PASSTHROUGH_SCHEMES.test(src)) {
        return <img src={src} alt={alt} style={style} {...rest} />;
    }
    if (candidates.length === 0) {
        return <img src={src} alt={alt} style={style} {...rest} />;
    }

    // Render the first (highest-priority) candidate. Cross-workspace
    // rendering fallback on broken-image is intentionally omitted here:
    // it adds `onError` state churn for the rare case where a stale
    // current-workspace root misses the file, and the click handler
    // already does the right thing for that case.
    const current = candidates[0];
    const handleClick = (e: React.MouseEvent<HTMLImageElement>) => {
        e.preventDefault();
        e.stopPropagation();
        void onClickImage(src).then((opened) => {
            if (!opened) notifyLinkNotFound(src);
        });
    };
    return (
        <img
            src={convertFileSrc(current.absPath)}
            alt={alt}
            onClick={handleClick}
            style={{ cursor: "pointer", ...style }}
            title={clickHint}
            {...rest}
        />
    );
});

export function MarkdownPreviewView({ file }: MarkdownPreviewViewProps) {
    const { t } = useTranslation();
    const openFile = useFileEditorStore((s) => s.openFile);
    // Tree cache subscription kept so React re-renders the preview when the
    // workspace tree finishes loading (the cross-workspace resolver inside
    // `openResolved` reads the tree on demand, not through this variable).
    // Subscribing only to THIS file's workspace root (instead of the whole
    // `nodes` map) keeps the re-render scoped: tree transitions in other
    // agents / workspaces no longer touch the preview.
    useFileTreeStore((s) => s.nodes[treeKey(file.agentId, file.workspaceId, "")]);

    /** Switch the current tab from preview mode back to edit mode. */
    const handleOpenAsEditor = useCallback(() => {
        void openFile(file.agentId, file.workspaceId, file.relPath);
    }, [openFile, file.agentId, file.workspaceId, file.relPath]);

    // Ensure the workspace list is loaded so cross-workspace link/image
    // resolution can see all candidates. The store keeps a cache, so
    // this is a no-op after the first successful fetch.
    useEffect(() => {
        const aid = file.agentId;
        if (!aid) return;
        // "Already loaded" means loaded *for this agent*: the list is one
        // global slot and can be holding another agent's entries, which would
        // both skip the fetch and feed the resolver foreign workspace roots.
        if (useWorkspaceStore.getState().workspacesAgentId === aid) return;
        void useWorkspaceStore.getState().fetchWorkspaces(aid);
    }, [file.agentId]);

    /**
     * Run the cross-workspace open resolver for `rawSrc`. Returns `true`
     * if a tab was opened, `false` if every candidate HEAD-probed as 404
     * (caller should toast). Captured by both `a` and `img` handlers
     * below, so this is the single source of truth for "click to open".
     */
    const openResolved = useCallback(
        async (rawSrc: string): Promise<boolean> => {
            const candidates = resolveAssetAcrossWorkspaces(
                file.agentId,
                file.workspaceId,
                file.relPath,
                rawSrc,
            );
            if (candidates.length === 0) return false;
            const action = pickOpenAction(rawSrc);
            const result = await openFirstResolved(file.agentId, candidates, action);
            return result.opened;
        },
        [file.agentId, file.workspaceId, file.relPath],
    );

    /**
     * Synchronous `onClick` for markdown anchor tags.
     *
     * Routing:
     *  - http(s) → existing URL-preview tab (iframe), preserved verbatim.
     *  - same-page anchor (`#…`) → no preventDefault, webview scrolls.
     *  - pass-through schemes (mailto, tel, data, asset, blob) → no
     *    preventDefault, webview handles.
     *  - everything else → resolve across workspaces (HEAD probe each
     *    candidate) and open the first match; current workspace wins
     *    on ties.
     *
     * preventDefault is only called for cases we actually handle. Other
     * schemes fall through to the webview's default behaviour, which
     * preserves the historical "weird URL → webview tries" path.
     */
    const handleLinkClick = useCallback(
        (e: React.MouseEvent<HTMLAnchorElement>, href: string | undefined) => {
            if (!href) return;

            if (/^https?:\/\//i.test(href)) {
                e.preventDefault();
                const aid = useAgentStore.getState().selectedAgentId;
                if (aid) useFileEditorStore.getState().openUrl(aid, href);
                return;
            }
            if (href.startsWith("#")) return;
            if (PASSTHROUGH_SCHEMES.test(href)) return;

            // Local file path. Always take over navigation so the
            // webview never falls back to `tauri://…` for non-URL hrefs
            // (which is what was triggering the spurious "restart").
            e.preventDefault();
            void openResolved(href).then((opened) => {
                if (!opened) notifyLinkNotFound(href);
            });
        },
        [openResolved],
    );

    /**
     * Component overrides for the preview's ReactMarkdown instance.
     * Re-created only when the markdown file's agent/workspace/path or
     * the open-resolver identity changes — these are the inputs the
     * `a` and `img` resolvers depend on.
     */
    const previewComponents = useMemo(() => {
        const clickHint = t("fileEditor.markdownImageClickHint");
        return {
            ...markdownComponents,
            /**
             * React-markdown strips deprecated HTML attributes (align, valign, etc.)
             * from React props. We read them from the raw HAST node injected by
             * react-markdown (`node`) and translate them into inline styles, which
             * are guaranteed to work across all rendering paths.
             */
            p: toAlignComponent("p"),
            h1: toAlignComponent("h1"),
            h2: toAlignComponent("h2"),
            h3: toAlignComponent("h3"),
            td: toAlignComponent("td"),
            th: toAlignComponent("th"),
            /**
             * Anchor click dispatcher — see `handleLinkClick` for the full
             * routing matrix. Wrapping the inline handler in a stable
             * named closure keeps React reconciliation happy when the
             * parent re-renders for unrelated reasons.
             */
            a: ({ href, children, ...rest }: React.AnchorHTMLAttributes<HTMLAnchorElement>) => {
                const onClick = (e: React.MouseEvent<HTMLAnchorElement>) =>
                    handleLinkClick(e, href);
                return (
                    <a href={href} onClick={onClick} {...rest}>
                        {children}
                    </a>
                );
            },
            /**
             * Image: render the first cross-workspace-resolved candidate
             * and make the image clickable so the user can open it in a
             * preview tab. Click handling goes through the same
             * `openResolved` resolver as link clicks for consistency.
             */
            img: (props: React.ImgHTMLAttributes<HTMLImageElement>) => {
                const src = props.src ?? "";
                if (!src || PASSTHROUGH_SCHEMES.test(src)) {
                    return <img {...props} />;
                }
                const candidates = resolveAssetAcrossWorkspaces(
                    file.agentId,
                    file.workspaceId,
                    file.relPath,
                    src,
                );
                return (
                    <ImageWithClick
                        {...props}
                        candidates={candidates}
                        onClickImage={openResolved}
                        clickHint={clickHint}
                    />
                );
            },
        };
    }, [file.agentId, file.workspaceId, file.relPath, openResolved, handleLinkClick, t]);

    if (file.loading) {
        return (
            <div className="flex h-full items-center justify-center gap-2 text-xs text-text-tertiary ">
                <Loader2 className="h-4 w-4 animate-spin" />
                {t("fileEditor.previewLoading")}
            </div>
        );
    }

    return (
        // `bg-editor-canvas` on the preview root paints the surface
        // with the same Monaco `vs` / `vs-dark` background the editor
        // uses, so the right-hand preview column reads as one unified
        // "editor canvas" distinct from the left ChatPanel bg (`#FAFAFA`
        // / zinc-900). Without it, the preview inherits the
        // FileEditorPanel wrapper's bg and becomes visually
        // indistinguishable from the chat panel.
        // Token is registered in globals.css @theme + .dark block; do
        // not hard-code #FFFFFF / #1E1E1E here — keep in sync with Monaco.
        <div
            className={cn(
                "markdown-preview prose prose-sm prose-zinc max-w-none h-full overflow-y-auto px-5 py-4 bg-editor-canvas",
                "dark:prose-invert",
            )}
            onDoubleClick={handleOpenAsEditor}
            title={t("fileEditor.previewDoubleClickHint")}
        >
            {/* Injected CSS — bypasses Tailwind/LightningCSS processor to guarantee
                that deprecated HTML attributes (align, valign) are honored.
                The same rules also live in globals.css as a static fallback. */}
            <style>{`
                .markdown-preview [data-align="center"] { text-align: center !important; }
                .markdown-preview [data-align="right"]  { text-align: right  !important; }
                .markdown-preview [data-align="left"]   { text-align: left   !important; }
                .markdown-preview [data-valign="top"]    { vertical-align: top    !important; }
                .markdown-preview [data-valign="middle"] { vertical-align: middle !important; }
                .markdown-preview [data-valign="bottom"] { vertical-align: bottom !important; }
                /* Keep images inline inside paragraphs (Tailwind typography adds large margins) */
                .markdown-preview p img,
                .markdown-preview p a img { display: inline !important; margin-top: 0 !important; margin-bottom: 0 !important; vertical-align: middle; }
                /* Single-image paragraphs — center as block */
                .markdown-preview p > img:only-child { display: block !important; margin-left: auto !important; margin-right: auto !important; }
            `}</style>
            <ReactMarkdown
                remarkPlugins={[remarkGfm]}
                rehypePlugins={[rehypeRaw]}
                components={previewComponents as any}
            >
                {file.content}
            </ReactMarkdown>
        </div>
    );
}