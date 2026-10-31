/**
 * PreviewPane — ADR-081 §4.3 single-click preview surface inside
 * GlobalSearchDialog.
 *
 * Rendered below the results list whenever the user single-clicks (or
 * arrow-keys into) a hit. The matched hit determines which renderer runs;
 * double-click / Enter activates the hit (open file, navigate, etc.) via
 * the parent dialog's `activate(idx)` — this pane only previews.
 *
 * Per-type:
 *   - file        → fetch the file body via `/api/agents/{id}/workspaces/file`
 *                  and show ±5 lines around the match with line numbers.
 *   - git         → render the commit's metadata (subject, author, date, hash).
 *   - doc         → render the search snippet + path (full doc body is TODO).
 *   - project     → look up project / task via the pm store and render
 *                  title + description.
 *   - conversation → render role + snippet (full message context is TODO).
 *   - memory      → fetch the full node via the memory store and render its
 *                  content + a compact meta dl. The right-panel
 *                  <MemoryNodeDetail> (delete / context menu / decay chart)
 *                  is intentionally NOT reused here — preview only shows
 *                  the main content, in the same chrome as the other tabs.
 *
 * Layout invariant: each preview is `flex h-full flex-col` →
 *   - <PreviewHeader />  (shrink-0, single line)
 *   - <div flex-1 min-h-0 overflow-auto>  ← all preview bodies. `min-h-0`
 *     lets the container shrink below its content size so the parent
 *     dialog's `max-h-[70vh]` + `overflow-hidden` actually clips, and
 *     content larger than the preview box scrolls instead of overflowing.
 */

import { useCallback, useEffect, useState } from "react";
import { Brain, Check, ChevronRight, Copy, FileText, GitCommit, Loader2 } from "lucide-react";
import type { GitCommitDto } from "../../stores/gitStore";
import type { MemoryNodeResponse } from "../../lib/types";
import { useTranslation } from "../../i18n/useTranslation";
import type {
    ConversationHit,
    DocHit,
    FileHit,
    MemoryHit,
    ProjectHit,
    SearchHit,
} from "./GlobalSearchDialog";
import { useMemoryStore } from "../../stores/memoryStore";
import { usePmProjectStore } from "../../stores/pm/projectStore";
import { usePmTaskDetailStore } from "../../stores/pm/taskDetailStore";
import { useSettingsStore } from "../../stores/settingsStore";
import { DEFAULT_GATEWAY_URL } from "../../lib/config";
import { log } from "../../lib/logger";

export type PreviewTab = "file" | "git" | "doc" | "project" | "conversation" | "memory";

export interface PreviewPaneProps {
    hit: SearchHit | null;
    tab: PreviewTab;
    agentId: string;
    workspaceId?: string;
    /** Required for git: the matched commit, looked up by the parent. */
    gitCommit?: GitCommitDto;
}

/** Response envelope from Runtime `GET /workspaces/file` (WorkspaceFileDto).
 *  The actual file body is in `content`; `size` / `mimeType` drive the
 *  binary-vs-text branching below. */
interface FileEnvelope {
    content?: string;
    size?: number;
    mimeType?: string;
    isFile?: boolean;
    isDir?: boolean;
    path?: string;
    modified?: string;
}

/** Treat the file as text only when the runtime's mime classification is
 *  one of these. Anything else (image, pdf, font, archive, …) gets the
 *  "binary file" placeholder so we don't dump raw bytes into the pre.
 *
 *  Source of truth: `core/acowork-runtime/src/usecases/workspace_query_impl.rs`
 *  `mime_type_for(rel_path)`. Keep in lockstep — when the runtime adds or
 *  renames a mime type, this list must change in the same commit, or the
 *  affected extension will silently fall back to the binary placeholder.
 *
 *  Ponytail ceiling: hand-rolled allow-list that duplicates the runtime's
 *  mime table. Upgrade path: extract `mime_type_for` into a shared
 *  `@acowork/protocol` package and import the JSON list here.
 */
const TEXT_APPLICATION_MIMES = new Set([
    "application/json",
    "application/javascript",
    "application/typescript", // .ts / .tsx — without this, TSX previews fall through to the binary branch.
    "application/xml",
    "application/yaml",
    "application/x-yaml", // legacy alias in case the runtime hasn't been rebuilt after the rename
    "application/toml",
    "application/x-sh", // runtime sends this for .sh
    "application/x-shellscript", // legacy alias
    "application/x-powershell", // .ps1
]);

export function isTextMime(mimeType: string | undefined): boolean {
    if (!mimeType) return true; // Runtime didn't classify; default to text.
    if (mimeType.startsWith("text/")) return true;
    if (TEXT_APPLICATION_MIMES.has(mimeType)) return true;
    // SVG is XML markup; the runtime serves the raw text payload so the
    // editor can show syntax-highlighted markup (see `is_binary_path`'s
    // explicit SVG exclusion in workspace_query_impl.rs).
    if (mimeType === "image/svg+xml") return true;
    // Runtime's "unknown extension" fallback. If ripgrep matched content
    // here, `env.content` is raw text — render it.
    if (mimeType === "application/octet-stream") return true;
    return false;
}

function formatBytes(n: number): string {
    if (n < 1024) return `${n} B`;
    if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
    if (n < 1024 * 1024 * 1024) return `${(n / (1024 * 1024)).toFixed(1)} MB`;
    return `${(n / (1024 * 1024 * 1024)).toFixed(1)} GB`;
}

export function PreviewPane({ hit, tab, agentId, workspaceId, gitCommit }: PreviewPaneProps) {
    const { t } = useTranslation();
    if (!hit) {
        return (
            <div className="flex flex-1 select-text items-center justify-center px-6 py-6 text-xs text-text-tertiary">
                {t("globalSearch.previewPlaceholder")}
            </div>
        );
    }
    if (hit.type !== tab) {
        // Defensive: focusedIdx reset on tab change, but if it ever drifts,
        // don't render a cross-tab preview.
        return null;
    }
    switch (hit.type) {
        case "file":
            return <FilePreview hit={hit as FileHit} agentId={agentId} workspaceId={workspaceId} />;
        case "git":
            return <GitPreview commit={gitCommit} fallback={hit} />;
        case "doc":
            return <DocPreview hit={hit as DocHit} />;
        case "project":
            return <ProjectPreview hit={hit as ProjectHit} />;
        case "conversation":
            return <ConversationPreview hit={hit as ConversationHit} />;
        case "memory":
            return <MemoryPreview hit={hit as MemoryHit} agentId={agentId} />;
    }
}

/* ── File preview ───────────────────────────────────────────────────── */

const PREVIEW_CONTEXT_LINES = 5;

/** `path:line` for a content hit, plain `path` for a file-name hit —
 *  a name hit has no line to point at. */
function fileLocationLabel(hit: FileHit): string {
    return hit.line === undefined ? hit.path : `${hit.path}:${hit.line}`;
}

function FilePreview({ hit, agentId, workspaceId }: { hit: FileHit; agentId: string; workspaceId?: string }) {
    const [state, setState] = useState<
        | { kind: "loading" }
        | { kind: "ok"; lines: string[]; matchedLine: number }
        | { kind: "binary"; mimeType: string; size: number; snippet: string }
        | { kind: "error"; message: string }
    >({ kind: "loading" });

    // A file-name hit has no line to centre on. The preview then shows the
    // head of the file instead of an arbitrary line — the user matched the
    // name, so the name is what they want to see the file for.
    const isNameHit = hit.kind === "name";

    useEffect(() => {
        // Reset state at the start of every effect run so the DOM doesn't
        // briefly render the previous hit's `state.lines` under the new
        // hit's path/line (the "click any file, content is identical" bug).
        // The `cancelled` flag below then blocks the in-flight fetch's
        // .then/.catch from calling setState once we've moved on.
        setState({ kind: "loading" });
        const ctrl = new AbortController();
        let cancelled = false;
        const url = buildFileUrl(agentId, workspaceId, hit.path);
        fetch(url, { signal: ctrl.signal })
            .then(async (resp) => {
                if (cancelled) return;
                if (!resp.ok) {
                    // Read the error body so the user can see why the file
                    // couldn't be opened (e.g. "permission denied",
                    // "file not found") instead of a bare HTTP status.
                    let detail = "";
                    try {
                        detail = (await resp.json()).error ?? "";
                    } catch {
                        /* not JSON, leave detail empty */
                    }
                    throw new Error(detail || `HTTP ${resp.status}`);
                }
                // Runtime serves `/workspaces/file` as a JSON envelope
                // { content, size, mimeType, isFile, isDir, path, modified? }
                // (see WorkspaceFileDto in core/acowork-runtime/.../workspace_query.rs).
                // Reading it as text and splitting by lines used to render
                // the JSON wrapper as the file body — every preview looked
                // like {"content":"...",...}. Parse and unwrap here.
                const env = (await resp.json()) as FileEnvelope;
                if (cancelled) return;
                if (typeof env.content !== "string") {
                    // The `/workspaces/file` endpoint is supposed to return a
                    // JSON envelope `{ content, size, mimeType, ... }`. If the
                    // `content` field is missing or not a string, the backend
                    // is either on an older protocol or genuinely broken.
                    // Surface that as a clear error instead of pretending
                    // `env.content` is a file body.
                    throw new Error("Malformed file envelope (missing `content` field)");
                }
                if (!isTextMime(env.mimeType)) {
                    // Binary file (image / pdf / compiled artefact, etc.).
                    // Show metadata + ripgrep snippet instead of dumping
                    // raw bytes; the snippet still tells the user what
                    // was matched.
                    setState({
                        kind: "binary",
                        mimeType: env.mimeType ?? "application/octet-stream",
                        size: env.size ?? 0,
                        snippet: hit.snippet,
                    });
                    return;
                }
                const lines = env.content.split(/\r?\n/);
                setState({ kind: "ok", lines, matchedLine: hit.line ?? 1 });
            })
            .catch((e) => {
                if (cancelled) return;
                if ((e as Error)?.name === "AbortError") return;
                log.error("[PreviewPane] file fetch failed:", e);
                setState({ kind: "error", message: String((e as Error)?.message ?? e) });
            });
        return () => {
            cancelled = true;
            ctrl.abort();
        };
    }, [agentId, workspaceId, hit.path, hit.line, hit.snippet]);

    if (state.kind === "loading") {
        return <PreviewLoading icon={FileText} label={hit.path} />;
    }
    if (state.kind === "error") {
        return (
            <PreviewEmpty
                icon={FileText}
                title={hit.title}
                sub={fileLocationLabel(hit)}
                snippet={hit.snippet}
                footer={state.message}
            />
        );
    }

    if (state.kind === "binary") {
        return (
            <PreviewEmpty
                icon={FileText}
                title={hit.title}
                sub={fileLocationLabel(hit)}
                snippet={state.snippet}
                footer={`二进制文件 (${state.mimeType || "unknown"}, ${formatBytes(state.size)}) — 仅显示命中片段`}
            />
        );
    }

    const totalLines = state.lines.length;
    // File was emptied between search and preview (ripgrep matched when the
    // file had content; the fetch now returns 0 lines). Fall back to the
    // ripgrep snippet so the preview is never blank.
    if (totalLines === 0 || (totalLines === 1 && state.lines[0] === "")) {
        return (
            <PreviewEmpty
                icon={FileText}
                title={hit.title}
                sub={fileLocationLabel(hit)}
                snippet={hit.snippet}
                footer="文件已无内容（0 字节），以下是命中片段"
            />
        );
    }
    // Defensive: ripgrep returned `hit.line` from a snapshot; the file may
    // have shrunk between search and preview, or `line` could be reported
    // past EOF (e.g. symlink re-resolution, CRLF normalisation drift).
    // Without this clamp, `start > end` makes `Array.from({length: <neg>})`
    // produce an empty array and the preview renders blank.
    const matchedClamped = Math.min(Math.max(1, state.matchedLine), totalLines);
    const start = Math.max(1, matchedClamped - PREVIEW_CONTEXT_LINES);
    const end = Math.min(totalLines, matchedClamped + PREVIEW_CONTEXT_LINES);
    const width = String(end).length;
    const matchedOutOfRange = !isNameHit && matchedClamped !== state.matchedLine;
    // Reconstruct the rendered preview as plain text: "<line>: <content>"
    // per row, matching the gutter + content columns the user sees on
    // screen. Used by the header's Copy button.
    const copyText = Array.from({ length: end - start + 1 }, (_, i) => {
        const n = start + i;
        const line = state.lines[n - 1] ?? "";
        return `${String(n).padStart(width, " ")}: ${line}`;
    }).join("\n");

    return (
        <div className="flex h-full flex-col select-text">
            <PreviewHeader
                icon={FileText}
                title={hit.title}
                sub={fileLocationLabel(hit)}
                copyText={copyText}
            />
            {matchedOutOfRange && (
                <div className="border-b border-border-divider px-4 py-1.5 text-10 text-text-tertiary">
                    匹配行 {state.matchedLine} 已超出当前文件范围（{totalLines} 行），显示附近行
                </div>
            )}
            <pre className="m-0 min-h-0 flex-1 overflow-auto px-4 py-2 font-mono text-11 leading-relaxed">
                {Array.from({ length: end - start + 1 }, (_, i) => {
                    const n = start + i;
                    const isMatch = n === matchedClamped;
                    return (
                        <div
                            key={n}
                            className={
                                isMatch
                                    ? "bg-yellow-100/60 text-text dark:bg-yellow-500/15"
                                    : "text-text-secondary"
                            }
                        >
                            <span className="mr-3 inline-block w-10 select-none text-right text-text-tertiary">
                                {String(n).padStart(width, " ")}
                            </span>
                            <span className="whitespace-pre">{state.lines[n - 1] ?? ""}</span>
                        </div>
                    );
                })}
            </pre>
        </div>
    );
}

function buildFileUrl(agentId: string, workspaceId: string | undefined, path: string): string {
    const base = useSettingsStore.getState().gatewayUrl || DEFAULT_GATEWAY_URL;
    const params = new URLSearchParams();
    params.set("path", path);
    if (workspaceId) params.set("workspace_id", workspaceId);
    return `${base}/api/agents/${encodeURIComponent(agentId)}/workspaces/file?${params.toString()}`;
}

/* ── Git preview ────────────────────────────────────────────────────── */

function GitPreview({ commit, fallback }: { commit?: GitCommitDto; fallback: SearchHit }) {
    if (!commit) {
        return (
            <PreviewEmpty
                icon={GitCommit}
                title={fallback.title}
                sub={fallback.sub}
                snippet={fallback.snippet}
            />
        );
    }
    return (
        <div className="flex h-full flex-col select-text">
            <PreviewHeader
                icon={GitCommit}
                title={commit.subject}
                sub={commit.shortHash}
                copyText={`${commit.subject}\n\nhash: ${commit.hash}\nauthor: ${commit.author}\ndate: ${commit.date}`}
            />
            <div className="min-h-0 flex-1 overflow-auto px-4 py-3 text-xs">
                <dl className="grid grid-cols-[5rem_1fr] gap-x-3 gap-y-1.5 text-xs">
                    <dt className="text-text-tertiary">hash</dt>
                    <dd className="break-all font-mono text-text">{commit.hash}</dd>
                    <dt className="text-text-tertiary">author</dt>
                    <dd className="text-text">{commit.author}</dd>
                    <dt className="text-text-tertiary">date</dt>
                    <dd className="text-text-secondary">{commit.date}</dd>
                </dl>
            </div>
        </div>
    );
}

/* ── Doc preview (snippet only — full body is TODO) ─────────────────── */

function DocPreview({ hit }: { hit: DocHit }) {
    return (
        <PreviewEmpty
            icon={FileText}
            title={hit.title}
            sub={hit.sub}
            snippet={hit.snippet}
            footer="完整文档正文将在后续阶段接入（ADR-081 P1）"
        />
    );
}

/* ── Project preview ────────────────────────────────────────────────── */

function ProjectPreview({ hit }: { hit: ProjectHit }) {
    const project = usePmProjectStore((s) => s.projects.find((p) => p.id === hit.projectId));
    const openTask = usePmTaskDetailStore((s) => s.openTask);
    const loadProjects = usePmProjectStore((s) => s.loadProjects);
    useEffect(() => {
        if (!project) void loadProjects({ silent: true });
        if (hit.taskId) void openTask(hit.taskId);
    }, [project, loadProjects, hit.taskId, openTask]);

    if (!project) {
        return <PreviewLoading icon={FileText} label={hit.sub} />;
    }
    return (
        <div className="flex h-full flex-col select-text">
            <PreviewHeader
                icon={FileText}
                title={project.title}
                sub={hit.taskId ? `task · ${hit.taskId}` : `project · ${project.id}`}
                copyText={
                    [
                        `${project.title}\n`,
                        project.description ?? "（无描述）",
                        hit.taskId ? `关联任务:${hit.taskId}` : "",
                    ]
                        .filter(Boolean)
                        .join("\n")
                }
            />
            <div className="min-h-0 flex-1 overflow-auto px-4 py-3 text-xs">
                {project.description ? (
                    <p className="whitespace-pre-wrap text-text-secondary">
                        {project.description}
                    </p>
                ) : (
                    <p className="text-text-tertiary">（无描述）</p>
                )}
                {hit.taskId ? (
                    <div className="mt-3 flex items-center gap-1 text-11 text-text-tertiary">
                        <ChevronRight className="h-3 w-3" />
                        <span>关联任务：{hit.taskId}</span>
                    </div>
                ) : null}
            </div>
        </div>
    );
}

/* ── Conversation preview (snippet only — full message context is TODO) ─ */

function ConversationPreview({ hit }: { hit: ConversationHit }) {
    return (
        <PreviewEmpty
            icon={FileText}
            title={`会话 ${hit.sessionId}`}
            sub={`${hit.role || "?"} · #${hit.messageIndex}`}
            snippet={hit.snippet}
            footer="完整消息上下文将在后续阶段接入（ADR-081 P1）"
        />
    );
}

/* ── Memory preview ─────────────────────────────────────────────────── */

function MemoryPreview({ hit, agentId }: { hit: MemoryHit; agentId: string }) {
    const [node, setNode] = useState<MemoryNodeResponse | null>(null);
    const [loading, setLoading] = useState(true);
    const fetchNode = useMemoryStore((s) => s.fetchNode);

    useEffect(() => {
        let cancelled = false;
        setLoading(true);
        fetchNode(agentId, hit.nodeId)
            .then((n) => {
                if (cancelled) return;
                setNode(n);
            })
            .finally(() => {
                if (!cancelled) setLoading(false);
            });
        return () => {
            cancelled = true;
        };
    }, [agentId, hit.nodeId, fetchNode]);

    if (loading) {
        return <PreviewLoading icon={Brain} label={hit.sub} />;
    }
    // A resolved-but-absent node is not "still loading". `fetchNode` returns
    // null on any error, and folding that into the loading branch turned every
    // detail-fetch failure (including a missing locate id) into an endless
    // spinner with nothing to read. Fall back to what the search result already
    // carries so the user sees the snippet and why it is not the full node.
    if (!node) {
        return (
            <PreviewEmpty
                icon={Brain}
                title={hit.title}
                sub={hit.sub}
                snippet={hit.snippet}
                footer="未能读取该记忆节点的详情（节点可能已被合并或删除，或搜索结果缺少定位信息）"
            />
        );
    }

    // Show the node's content (the main thing) + a compact dl with the
    // fields a search preview is expected to surface. The full panel UI
    // (delete button, decay chart, type chips, context menu) lives in
    // <MemoryNodeDetail> and is intentionally NOT used here — preview is
    // read-only and shares the chrome of every other tab.
    const confidenceLabel =
        node.node_type === "Episodic" ? "importance" : "confidence";
    const confidenceValue =
        node.node_type === "Episodic" ? node.importance : node.confidence;

    return (
        <div className="flex h-full flex-col select-text">
            <PreviewHeader
                icon={Brain}
                title={node.node_type}
                sub={node.sub_type ? `${node.sub_type} · #${node.node_id}` : `#${node.node_id}`}
                copyText={
                    [
                        node.content,
                        "",
                        `status: ${node.status}`,
                        `${confidenceLabel}: ${(confidenceValue * 100).toFixed(1)}%`,
                        `accessed: ${node.access_count} · last ${
                            node.last_accessed_at > 0
                                ? new Date(node.last_accessed_at * 1000).toLocaleString()
                                : "—"
                        }`,
                    ].join("\n")
                }
            />
            <div className="min-h-0 flex-1 overflow-auto px-4 py-3 text-xs">
                <p className="whitespace-pre-wrap text-text">{node.content}</p>
                <dl className="mt-3 grid grid-cols-[5rem_1fr] gap-x-3 gap-y-1 text-11">
                    <dt className="text-text-tertiary">status</dt>
                    <dd className="text-text-secondary">{node.status}</dd>
                    <dt className="text-text-tertiary">{confidenceLabel}</dt>
                    <dd className="text-text-secondary">
                        {(confidenceValue * 100).toFixed(1)}%
                    </dd>
                    <dt className="text-text-tertiary">accessed</dt>
                    <dd className="text-text-secondary">
                        {node.access_count} · last{" "}
                        {node.last_accessed_at > 0
                            ? new Date(node.last_accessed_at * 1000).toLocaleString()
                            : "—"}
                    </dd>
                </dl>
            </div>
        </div>
    );
}

/* ── Shared chrome ──────────────────────────────────────────────────── */

function PreviewHeader({
    icon: Icon,
    title,
    sub,
    copyText,
}: {
    icon: React.ElementType;
    title: string;
    sub: string;
    /** Plain-text snapshot of the entire preview body. When provided,
     *  a Copy button is rendered on the right side of the header that
     *  writes this string to the clipboard (with a 2s "Copied" affordance).
     *  The PreviewPane family follows the same pattern as DebugPanel's
     *  snapshot-copy button. */
    copyText?: string;
}) {
    const { t } = useTranslation();
    const [copied, setCopied] = useState(false);
    const handleCopy = useCallback(async () => {
        if (!copyText) return;
        try {
            await navigator.clipboard.writeText(copyText);
        } catch {
            // Clipboard API can fail in some sandboxed contexts; fall back
            // to the legacy textarea + execCommand path so the user still
            // gets their text out (same fallback DebugPanel uses).
            const ta = document.createElement("textarea");
            ta.value = copyText;
            document.body.appendChild(ta);
            ta.select();
            document.execCommand("copy");
            document.body.removeChild(ta);
        }
        setCopied(true);
        setTimeout(() => setCopied(false), 2000);
    }, [copyText]);

    return (
        <div className="flex shrink-0 items-center gap-2 border-b border-border-divider px-4 py-2">
            <Icon className="h-4 w-4 shrink-0 text-text-tertiary" />
            <span className="truncate text-xs font-medium text-text">{title}</span>
            <span className="truncate font-mono text-10 text-text-tertiary">{sub}</span>
            {copyText !== undefined && (
                <button
                    type="button"
                    onClick={handleCopy}
                    aria-label={t("common.copy")}
                    className="ml-auto inline-flex shrink-0 items-center gap-1 rounded px-1.5 py-0.5 text-11 text-text-tertiary transition-colors hover:bg-zinc-100 dark:hover:bg-zinc-700/50"
                >
                    {copied ? (
                        <>
                            <Check className="h-3 w-3 text-emerald-600 dark:text-emerald-400" aria-hidden="true" />
                            {t("common.copied")}
                        </>
                    ) : (
                        <>
                            <Copy className="h-3 w-3" aria-hidden="true" />
                            {t("common.copy")}
                        </>
                    )}
                </button>
            )}
        </div>
    );
}

function PreviewLoading({ icon: Icon, label }: { icon: React.ElementType; label: string }) {
    return (
        <div className="flex h-full items-center justify-center gap-2 text-xs text-text-tertiary">
            <Loader2 className="h-3.5 w-3.5 animate-spin" />
            <Icon className="h-3.5 w-3.5" />
            <span className="truncate">{label}</span>
        </div>
    );
}

function PreviewEmpty({
    icon: Icon,
    title,
    sub,
    snippet,
    footer,
}: {
    icon: React.ElementType;
    title: string;
    sub: string;
    snippet: string;
    footer?: string;
}) {
    // Mirror the visible body: the <pre> snippet is what the user sees,
    // footer is a one-liner below. Concatenating the two preserves the
    // "copy what you see" guarantee.
    const copyText = [snippet, footer].filter(Boolean).join("\n\n") || snippet;
    return (
        <div className="flex h-full flex-col select-text">
            <PreviewHeader icon={Icon} title={title} sub={sub} copyText={copyText} />
            <div className="min-h-0 flex-1 overflow-auto px-4 py-3">
                {snippet ? (
                    <pre className="m-0 whitespace-pre-wrap font-mono text-11 leading-relaxed text-text-secondary">
                        {snippet}
                    </pre>
                ) : (
                    <p className="text-xs text-text-tertiary">（无内容）</p>
                )}
                {footer ? (
                    <p className="mt-3 text-10 text-text-tertiary">{footer}</p>
                ) : null}
            </div>
        </div>
    );
}