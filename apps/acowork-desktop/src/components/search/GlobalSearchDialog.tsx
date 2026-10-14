/**
 * GlobalSearchDialog — ADR-081 application-wide search (Ctrl+Shift+F).
 *
 * Layout (3-zone dialog matching CloneDialog / PublishWizard chrome):
 *
 *   ┌─ Header  border-b  🔍 全局搜索                          ─┐
 *   ├─ Zone 1  border-b  [AgentPickerChip]│  🔍 input  [Aa][\b]─┤
 *   │                       (avatar only)   (search row)        │
 *   ├─ Zone 2  border-b?  [文件][git][doc][project][对话][记忆] ┤
 *   │                       ─────                               │
 *   │                       result rows (flex-1)                │
 *   ├─ Zone 3 (if hit focused)  PreviewPane                     ┤
 *   ├─ Footer border-t                              [关闭]      ┤
 *   └────────────────────────────────────────────────────────────┘
 *
 * Interaction:
 *   - Single click / Arrow keys → focused hit → preview pane renders
 *   - Double click / Enter       → activate the hit (open file, etc.)
 *   - Escape                    → close dialog
 *
 * Data ownership follows ADR-081 D1: per-agent sources (file/git) are
 * queried on the owning Agent Runtime via the Gateway reverse proxy;
 * doc/pm are global sources queried via their Gateway reverse proxies.
 *
 * - 文件 tab  : GET /api/agents/{id}/workspaces/search (ripgrep) → openFile
 * - git tab   : GET /api/agents/{id}/git/log + CommitPicker-style
 *               client-side subject/author/hash filter → virtual log file
 * - 文档 tab  : GET /api/doc/search (doc process) → DocEditor
 * - 项目 tab  : GET /api/pm/search (pm process) → ProjectBoard / TaskDetailDrawer
 */

import { useEffect, useMemo, useRef, useState, useCallback } from "react";
import { BookOpen, KanbanSquare, MessageSquare, RotateCw, Search } from "lucide-react";
import { useFileEditorStore } from "../../stores/fileEditorStore";
import { useAgentStore } from "../../stores/agentStore";
import { useSearchStore } from "../../stores/searchStore";
import { useGitStore, type GitCommitDto } from "../../stores/gitStore";
import { useSettingsStore } from "../../stores/settingsStore";
import { usePmProjectStore } from "../../stores/pm/projectStore";
import { usePmTaskDetailStore } from "../../stores/pm/taskDetailStore";
import { useDocEditorStore } from "../../stores/doc/editorStore";
import { useMemoryStore } from "../../stores/memoryStore";
import { useChatStore } from "../../stores/chatStore";
import { useWorkspaceStore } from "../../stores/workspaceStore";
import { searchDocs } from "../../lib/doc-api";
import { searchPm, type PmSearchHit } from "../../lib/pm-api";
import { DEFAULT_GATEWAY_URL } from "../../lib/config";
import { SetiIcon } from "../common/SetiIcon";
import { getFileIcon } from "../workspace/FileTree/fileIcons";
import { log } from "../../lib/logger";
import { useTranslation } from "../../i18n/useTranslation";
import { listRowHover, listRowSelected } from "../../lib/ui-styles";
import { AgentPickerChip } from "./AgentPickerChip";
import { PreviewPane } from "./PreviewPane";
import {
    runSearchSources,
    buildSummaryCandidates,
    requestSearchSummary,
    parseSummarySegments,
    SEARCH_SOURCES,
    SUMMARY_DEADLINE_MS,
    type SearchSource,
    type CandidateSet,
    type SummaryCandidate,
} from "../../lib/searchRun";

/* ─── Types ─────────────────────────────────────────────────────────── */

export type Tab = "file" | "git" | "doc" | "project" | "conversation" | "memory";

export interface SearchMatch {
    file: string;
    line: number;
    column: number;
    text: string;
}

export interface SearchResponse {
    matches: SearchMatch[];
    totalMatches: number;
    truncated: boolean;
}

/** ADR-081 §4.1 Runtime `/search` aggregate hit (Gateway reverse proxy). */
export interface GlobalSearchHit {
    scope: string;
    title: string;
    snippet: string;
    score: number;
    payload: {
        node_id?: number;
        node_type?: string;
        sub_type?: string | null;
        file?: string;
        line?: number;
        hash?: string;
        session_id?: string;
        message_index?: number;
        role?: string;
    };
}

export interface GlobalSearchResponse {
    hits: GlobalSearchHit[];
    scopes: Record<string, { status: string; count?: number }>;
    indexing: boolean;
}

/** ADR-081 D4 unified row. */
export interface SearchHit {
    type: Tab;
    id: string;
    title: string;
    snippet: string;
    /** Secondary line (path / commit meta). */
    sub: string;
}

/** File hits carry the ADR-081 D4 `meta` (locate info) on the row. */
export interface FileHit extends SearchHit {
    agentId: string;
    workspaceId?: string;
    path: string;
    line: number;
}

export interface DocHit extends SearchHit {
    docId: string;
}

export interface ProjectHit extends SearchHit {
    projectId: string;
    taskId?: string;
}

export interface MemoryHit extends SearchHit {
    nodeId: number;
    nodeType: string;
}

/** Conversation hits: one row per indexed message (ADR-081 §4.2). */
export interface ConversationHit extends SearchHit {
    sessionId: string;
    messageIndex: number;
    role: string;
}

export type RowHit =
    | FileHit
    | DocHit
    | ProjectHit
    | MemoryHit
    | ConversationHit
    | SearchHit;

/** ADR-093 summary state machine (design 27 §4.6). */
export type SummaryPhase = "idle" | "searching" | "summarizing" | "done" | "error" | "cancelled";

export interface GlobalSearchDialogProps {
    /** Switch the app-level view (projects/docs/chat) when a hit is opened. */
    onNavigate: (view: "projects" | "docs" | "chat") => void;
}

/* ─── Constants ─────────────────────────────────────────────────────── */

/** Lazy commits beyond the newest page can't be hit by the client-side
 *  filter — same ceiling CommitPicker accepts per page. Pagination chrome
 *  inside the dialog is a later phase (P0-2+). */
export const GIT_LOG_LIMIT = 100;
const FILE_MAX_RESULTS = 50;
const GLOBAL_SOURCE_LIMIT = 20;

/* ─── Pure helpers (unit-tested) ────────────────────────────────────── */

/** Case-insensitive keyword highlighting for result rows (ADR-081 4.5). */
function highlightParts(text: string, query: string): { text: string; hit: boolean }[] {
    const q = query.trim();
    if (!q) return [{ text, hit: false }];
    const lower = text.toLowerCase();
    const needle = q.toLowerCase();
    const parts: { text: string; hit: boolean }[] = [];
    let i = 0;
    let idx = lower.indexOf(needle, i);
    while (idx !== -1 && parts.length < 20) {
        if (idx > i) parts.push({ text: text.slice(i, idx), hit: false });
        parts.push({ text: text.slice(idx, idx + q.length), hit: true });
        i = idx + q.length;
        idx = lower.indexOf(needle, i);
    }
    if (i < text.length) parts.push({ text: text.slice(i), hit: false });
    return parts.length ? parts : [{ text, hit: false }];
}

function Highlight({ text, query }: { text: string; query: string }) {
    return (
        <>
            {highlightParts(text, query).map((p, i) =>
                p.hit ? (
                    <mark
                        key={i}
                        className="rounded-sm bg-[var(--color-accent)]/25 px-0.5 text-[var(--color-accent)]"
                    >
                        {p.text}
                    </mark>
                ) : (
                    <span key={i}>{p.text}</span>
                ),
            )}
        </>
    );
}

/** CommitPicker-compatible client-side commit filter (subject/author/hash). */
export function filterGitCommits(commits: GitCommitDto[], q: string): GitCommitDto[] {
    const term = q.trim().toLowerCase();
    if (!term) return commits;
    return commits.filter(
        (c) =>
            c.subject.toLowerCase().includes(term) ||
            c.author.toLowerCase().includes(term) ||
            c.shortHash.toLowerCase().startsWith(term) ||
            c.hash.toLowerCase().startsWith(term),
    );
}

export function toFileHits(agentId: string, workspaceId: string | undefined, matches: SearchMatch[]): FileHit[] {
    return matches.map((m) => {
        const fileName = m.file.split("/").pop() ?? m.file;
        return {
            type: "file",
            id: `${m.file}:${m.line}`,
            title: fileName,
            snippet: m.text,
            sub: m.file,
            agentId,
            workspaceId,
            path: m.file,
            line: m.line,
        };
    });
}

function toGitHits(commits: GitCommitDto[]): SearchHit[] {
    return commits.map((c) => ({
        type: "git",
        id: c.hash,
        title: c.subject,
        snippet: `${c.shortHash}  ${c.author}  ${c.date}`,
        sub: c.subject,
    }));
}

function toDocHits(hits: { doc_id: string; name: string; path: string; snippet: string }[]): DocHit[] {
    return hits.map((h) => ({
        type: "doc",
        id: h.doc_id,
        title: h.name,
        snippet: h.snippet,
        sub: h.path,
        docId: h.doc_id,
    }));
}

function toProjectHits(hits: PmSearchHit[]): ProjectHit[] {
    return hits.map((h) => ({
        type: "project",
        id: h.id,
        title: h.title,
        snippet: h.snippet,
        sub: h.kind === "task" ? `task · ${h.project_id}` : `project · ${h.project_id}`,
        projectId: h.project_id,
        taskId: h.task_id,
    }));
}

export function toMemoryHits(hits: GlobalSearchHit[]): MemoryHit[] {
    return hits.map((h) => ({
        type: "memory",
        id: String(h.payload.node_id ?? h.title),
        title: h.title,
        snippet: h.snippet,
        sub: h.payload.sub_type ? `${h.title} · ${h.payload.sub_type}` : h.title,
        nodeId: h.payload.node_id ?? 0,
        nodeType: h.title,
    }));
}

/** ADR-081 §4.2: Runtime `/search` conversation-scope hits → rows. */
export function toConversationHits(hits: GlobalSearchHit[]): ConversationHit[] {
    return hits
        .filter((h) => h.payload.session_id)
        .map((h) => ({
            type: "conversation",
            id: `${h.payload.session_id}:${h.payload.message_index ?? 0}`,
            title: h.payload.session_id!,
            snippet: h.snippet,
            sub: `${h.payload.role ?? ""} · #${h.payload.message_index ?? 0}`,
            sessionId: h.payload.session_id!,
            messageIndex: h.payload.message_index ?? 0,
            role: h.payload.role ?? "",
        }));
}

/* ─── Component ─────────────────────────────────────────────────────── */

export function GlobalSearchDialog({ onNavigate }: GlobalSearchDialogProps) {
    const { t } = useTranslation();
    const open = useSearchStore((s) => s.open);
    const closeDialog = useSearchStore((s) => s.closeDialog);

    const [query, setQuery] = useState("");
    const [tab, setTab] = useState<Tab>("file");
    const [agentId, setAgentId] = useState<string>("");
    const [caseSensitive, setCaseSensitive] = useState(false);
    const [wholeWord, setWholeWord] = useState(false);

    const [fileHits, setFileHits] = useState<FileHit[]>([]);
    const [fileTotal, setFileTotal] = useState(0);
    const [loading, setLoading] = useState(false);
    const [error, setError] = useState<string | null>(null);

    const [commits, setCommits] = useState<GitCommitDto[]>([]);
    const [gitLoaded, setGitLoaded] = useState(false);
    const [gitError, setGitError] = useState<string | null>(null);

    const [docHits, setDocHits] = useState<DocHit[]>([]);
    const [docLoading, setDocLoading] = useState(false);
    const [docError, setDocError] = useState<string | null>(null);

    const [projHits, setProjHits] = useState<ProjectHit[]>([]);
    const [projLoading, setProjLoading] = useState(false);
    const [projError, setProjError] = useState<string | null>(null);

    const [memoryHits, setMemoryHits] = useState<MemoryHit[]>([]);
    const [memoryLoading, setMemoryLoading] = useState(false);
    const [memoryError, setMemoryError] = useState<string | null>(null);

    const [conversationHits, setConversationHits] = useState<ConversationHit[]>([]);
    const [conversationLoading, setConversationLoading] = useState(false);
    const [conversationError, setConversationError] = useState<string | null>(null);

    /* ── ADR-093 run/summary state machine (design 27 §4.6) ─────────── */
    // idle → searching → summarizing → done | error | cancelled.
    // "searching" covers the six-source fan-out (8 s deadline); the summary
    // stage (20 s deadline) is a separate phase so cancel/regenerate apply
    // to it alone.
    const [phase, setPhase] = useState<SummaryPhase>("idle");
    const [sourcesDone, setSourcesDone] = useState(0);
    const [summaryText, setSummaryText] = useState("");
    const [summaryError, setSummaryError] = useState<string | null>(null);
    const runCtrlRef = useRef<AbortController | null>(null);
    const summaryCtrlRef = useRef<AbortController | null>(null);
    const candidateRef = useRef<CandidateSet | null>(null);
    const lastSummaryReqRef = useRef<{
        query: string;
        candidates: SummaryCandidate[];
        partial: boolean;
        timedOutScopes: SearchSource[];
        failedScopes: SearchSource[];
    } | null>(null);
    // Landed rows per source, kept in a ref so citation clicks resolve the
    // exact row that fed the summary even before React state settles.
    const rowsRef = useRef<Record<SearchSource, RowHit[]>>({
        file: [], git: [], doc: [], project: [], memory: [], conversation: [],
    });

    const [focusedIdx, setFocusedIdx] = useState(0);
    const inputRef = useRef<HTMLInputElement>(null);
    const seqRef = useRef(0);

    const agents = useAgentStore((s) => s.agents);
    const selectedAgentId = useAgentStore((s) => s.selectedAgentId);
    const agentList = useMemo(
        () =>
            Object.values(agents).filter((a) => a.meta.alive),
        [agents],
    );

    // Default agent scope = currently selected agent (P0 single-agent).
    useEffect(() => {
        if (open && !agentId) {
            setAgentId(selectedAgentId ?? agentList[0]?.meta.instance_id ?? "");
        }
    }, [open, agentId, selectedAgentId, agentList]);

    // Workspace scope = the active session's selected workspace — the same
    // source as the right-side file tree / git banner (WorkspaceExplorer).
    // NOT the open editor file: a global search with no file open would
    // otherwise fall back to the agent home instead of the user's workspace.
    // "__agent_home__" is normalised to `undefined` so the file/git requests
    // omit `workspace_id` and the Runtime resolves the agent home itself.
    const activeSessionId = useChatStore((s) =>
        agentId ? s.getActiveSessionId(agentId) : null,
    );
    const workspaceId = useWorkspaceStore((s) => {
        if (!activeSessionId) return undefined;
        const ws = s.sessionWorkspaceMap[activeSessionId];
        return !ws || ws === "__agent_home__" ? undefined : ws;
    });

    /* ── Reset when opened ──────────────────────────────────────────── */
    useEffect(() => {
        if (open) {
            setQuery("");
            setFileHits([]);
            setFileTotal(0);
            setError(null);
            setGitLoaded(false);
            setCommits([]);
            setGitError(null);
            setDocHits([]);
            setDocError(null);
            setProjHits([]);
            setProjError(null);
            setMemoryHits([]);
            setMemoryError(null);
            setConversationHits([]);
            setConversationError(null);
            setFocusedIdx(0);
            setPhase("idle");
            setSourcesDone(0);
            setSummaryText("");
            setSummaryError(null);
            candidateRef.current = null;
            lastSummaryReqRef.current = null;
            rowsRef.current = { file: [], git: [], doc: [], project: [], memory: [], conversation: [] };
            requestAnimationFrame(() => inputRef.current?.focus());
        } else {
            runCtrlRef.current?.abort();
            summaryCtrlRef.current?.abort();
            seqRef.current += 1;
        }
    }, [open]);

    /* ── ADR-093: one run fans out to all six sources ───────────────── */
    // Design doc 27 §4.1/§5: each debounced query starts a new generation;
    // the previous generation's parent AbortController tears down every
    // in-flight request, and the seq check drops late results. Each source
    // updates its own tab as it lands — the user browses partial results
    // while the rest arrive.
    const runSummaryStage = useCallback(
        async (gen: number, parentCtrl: AbortController | null) => {
            const req = lastSummaryReqRef.current;
            if (!req) return;
            setPhase("summarizing");
            setSummaryError(null);
            const ctrl = new AbortController();
            summaryCtrlRef.current = ctrl;
            // 20 s deadline (ADR-093 D4). Manual cancel and deadline are
            // told apart by `timedOut` so the UI can say which happened.
            let timedOut = false;
            const timer = setTimeout(() => {
                timedOut = true;
                ctrl.abort();
            }, SUMMARY_DEADLINE_MS);
            const parentSignal = parentCtrl?.signal ?? undefined;
            const onParent = () => ctrl.abort();
            parentSignal?.addEventListener("abort", onParent);
            try {
                const baseUrl = useSettingsStore.getState().gatewayUrl || DEFAULT_GATEWAY_URL;
                const resp = await requestSearchSummary({
                    baseUrl,
                    agentId,
                    query: req.query,
                    candidates: req.candidates,
                    partial: req.partial,
                    timedOutScopes: req.timedOutScopes,
                    failedScopes: req.failedScopes,
                    signal: ctrl.signal,
                });
                if (gen !== seqRef.current) return;
                setSummaryText(resp.summary);
                setPhase("done");
            } catch (e: unknown) {
                if (gen !== seqRef.current) return;
                if ((e as Error)?.name === "AbortError") {
                    if (parentSignal?.aborted) return; // superseded by a new run
                    if (timedOut) {
                        setPhase("error");
                        setSummaryError(t("globalSearch.summaryTimeout"));
                    } else {
                        setPhase("cancelled");
                    }
                } else {
                    setPhase("error");
                    setSummaryError(t("globalSearch.summaryError"));
                    log.error("[GlobalSearch] summary error:", e);
                }
            } finally {
                clearTimeout(timer);
                parentSignal?.removeEventListener("abort", onParent);
            }
        },
        [agentId, t],
    );

    const runSearch = useCallback(
        async (q: string) => {
            runCtrlRef.current?.abort();
            summaryCtrlRef.current?.abort();
            const ctrl = new AbortController();
            runCtrlRef.current = ctrl;
            const gen = ++seqRef.current;

            setPhase("searching");
            setSourcesDone(0);
            setSummaryText("");
            setSummaryError(null);
            candidateRef.current = null;
            setLoading(true);
            setDocLoading(true);
            setProjLoading(true);
            setMemoryLoading(true);
            setConversationLoading(true);
            setError(null);
            setGitError(null);
            setDocError(null);
            setProjError(null);
            setMemoryError(null);
            setConversationError(null);

            const baseUrl = useSettingsStore.getState().gatewayUrl || DEFAULT_GATEWAY_URL;

            // Raw payloads the adapters fetch but that onSource applies to
            // state only after the generation check — a late-resolving
            // stale adapter must not write `fileTotal`/`commits` behind the
            // new run's back. onSource runs synchronously after the
            // adapter's promise settles, so these are safe per-run locals.
            let runFileTotal: number | undefined;
            let runGitCommits: GitCommitDto[] | undefined;

            const runtimeSearch = async (
                scope: "memory" | "conversation",
                signal: AbortSignal,
            ): Promise<RowHit[]> => {
                const params = new URLSearchParams();
                params.set("q", q);
                params.set("scopes", scope);
                params.set("mode", "hybrid");
                params.set("limit", String(GLOBAL_SOURCE_LIMIT));
                const resp = await fetch(`${baseUrl}/api/agents/${agentId}/search?${params}`, { signal });
                if (!resp.ok) throw new Error(`status ${resp.status}`);
                const data = (await resp.json()) as GlobalSearchResponse;
                return scope === "memory" ? toMemoryHits(data.hits ?? []) : toConversationHits(data.hits ?? []);
            };

            const results = await runSearchSources<RowHit[]>({
                parentSignal: ctrl.signal,
                adapters: {
                    file: async (signal) => {
                        const params = new URLSearchParams();
                        params.set("q", q);
                        if (workspaceId) params.set("workspace_id", workspaceId);
                        if (caseSensitive) params.set("case_sensitive", "true");
                        if (wholeWord) params.set("whole_word", "true");
                        params.set("max_results", String(FILE_MAX_RESULTS));
                        const resp = await fetch(`${baseUrl}/api/agents/${agentId}/workspaces/search?${params}`, { signal });
                        if (!resp.ok) throw new Error(`status ${resp.status}`);
                        const data = (await resp.json()) as SearchResponse;
                        runFileTotal = data.totalMatches;
                        return toFileHits(agentId, workspaceId, data.matches);
                    },
                    git: async (signal) => {
                        // Repo-wide history (empty path = no `-- <path>`
                        // filter), CommitPicker-style client-side subject/
                        // author/hash filter. Global search is not scoped
                        // to the open file.
                        const resp = await useGitStore
                            .getState()
                            .fetchLog(agentId, workspaceId ?? "", "", GIT_LOG_LIMIT, 0, signal);
                        runGitCommits = resp.commits;
                        return toGitHits(filterGitCommits(resp.commits, q));
                    },
                    doc: async (signal) => toDocHits(await searchDocs(q, GLOBAL_SOURCE_LIMIT, signal)),
                    project: async (signal) => toProjectHits(await searchPm(q, GLOBAL_SOURCE_LIMIT, signal)),
                    memory: (signal) => runtimeSearch("memory", signal),
                    conversation: (signal) => runtimeSearch("conversation", signal),
                },
                onSource: (source, r) => {
                    if (gen !== seqRef.current) return; // stale generation
                    const rows = r.data ?? [];
                    rowsRef.current[source] = rows;
                    switch (source) {
                        case "file":
                            setFileHits(rows as FileHit[]);
                            setLoading(false);
                            if (r.status === "ok" && runFileTotal !== undefined) {
                                setFileTotal(runFileTotal);
                            }
                            if (r.status !== "ok") setError(t("globalSearch.errorFileFailed"));
                            break;
                        case "git":
                            if (r.status === "ok") {
                                setCommits(runGitCommits ?? []);
                                setGitLoaded(true);
                            } else {
                                // Land the spinner either way — the tab
                                // falls back to the error/no-match rows.
                                setGitLoaded(true);
                                setGitError(t("globalSearch.errorGit"));
                            }
                            break;
                        case "doc":
                            setDocHits(rows as DocHit[]);
                            setDocLoading(false);
                            if (r.status !== "ok") setDocError(t("globalSearch.errorDocService"));
                            break;
                        case "project":
                            setProjHits(rows as ProjectHit[]);
                            setProjLoading(false);
                            if (r.status !== "ok") setProjError(t("globalSearch.errorProjectService"));
                            break;
                        case "memory":
                            setMemoryHits(rows as MemoryHit[]);
                            setMemoryLoading(false);
                            if (r.status !== "ok") setMemoryError(t("globalSearch.errorMemory"));
                            break;
                        case "conversation":
                            setConversationHits(rows as ConversationHit[]);
                            setConversationLoading(false);
                            if (r.status !== "ok") setConversationError(t("globalSearch.errorConversation"));
                            break;
                    }
                    setSourcesDone((n) => n + 1);
                },
            });

            if (gen !== seqRef.current || ctrl.signal.aborted) return;

            // ── Summary stage (design 27 §4.4): bounded candidates from
            // the six landed lists. `partial` when any source is missing;
            // timeouts and errors are listed separately so the model is
            // told what actually happened (Runtime search_summary.rs).
            const timedOutScopes = SEARCH_SOURCES.filter((s) => results[s].status === "timeout");
            const failedScopes = SEARCH_SOURCES.filter((s) => results[s].status === "error");
            const partial = timedOutScopes.length > 0 || failedScopes.length > 0;
            // RowHit carries title/snippet — directly the candidate shape.
            const set = buildSummaryCandidates(rowsRef.current);
            candidateRef.current = set;
            if (set.candidates.length === 0) {
                // Nothing to summarize — keep the lists browsable, no LLM call.
                setPhase("done");
                setSummaryText("");
                return;
            }
            lastSummaryReqRef.current = {
                query: q,
                candidates: set.candidates,
                partial,
                timedOutScopes,
                failedScopes,
            };
            await runSummaryStage(gen, ctrl);
        },
        [agentId, workspaceId, caseSensitive, wholeWord, t, runSummaryStage],
    );

    /* Display rows for the git tab stay derived (CommitPicker parity). */
    const gitHits = useMemo(() => toGitHits(filterGitCommits(commits, query)), [commits, query]);

    /* ── Debounced run (300ms) on query change ──────────────────────── */
    useEffect(() => {
        if (!open) return;
        if (!query.trim() || !agentId) {
            runCtrlRef.current?.abort();
            summaryCtrlRef.current?.abort();
            seqRef.current += 1;
            setPhase("idle");
            setSourcesDone(0);
            setSummaryText("");
            setSummaryError(null);
            candidateRef.current = null;
            rowsRef.current = { file: [], git: [], doc: [], project: [], memory: [], conversation: [] };
            setFileHits([]);
            setFileTotal(0);
            setDocHits([]);
            setProjHits([]);
            setMemoryHits([]);
            setConversationHits([]);
            return;
        }
        const timer = setTimeout(() => void runSearch(query), 300);
        return () => clearTimeout(timer);
    }, [open, query, agentId, runSearch]);

    /* ── Reset focus when results change ────────────────────────────── */
    const hits: RowHit[] =
        tab === "file" ? fileHits
        : tab === "git" ? gitHits
        : tab === "doc" ? docHits
        : tab === "project" ? projHits
        : tab === "conversation" ? conversationHits
        : memoryHits;
    useEffect(() => {
        setFocusedIdx(0);
    }, [hits.length, tab]);

    /* ── Actions ────────────────────────────────────────────────────── */
    const openFileHit = useCallback(
        (h: FileHit) => {
            void useFileEditorStore
                .getState()
                .openFile(h.agentId, h.workspaceId ?? "", h.path, h.line);
            closeDialog();
        },
        [closeDialog],
    );

    const openGitHit = useCallback(() => {
        const st = useFileEditorStore.getState();
        st.openVirtualFile({
            agentId,
            workspaceId: workspaceId ?? "",
            kind: "log",
            relPath: "",
            content:
                commits.map((c) => `${c.shortHash}  ${c.author}  ${c.date}\n    ${c.subject}`).join("\n\n") ||
                "No commits",
            language: "plaintext",
            loadedCommits: commits,
            displayedLimit: commits.length,
            reachedEnd: commits.length < GIT_LOG_LIMIT,
        });
        closeDialog();
    }, [agentId, workspaceId, commits, closeDialog]);

    const openDocHit = useCallback(
        (h: DocHit) => {
            void useDocEditorStore.getState().requestOpen(h.docId);
            onNavigate("docs");
            closeDialog();
        },
        [onNavigate, closeDialog],
    );

    const openProjectHit = useCallback(
        (h: ProjectHit) => {
            const pst = usePmProjectStore.getState();
            const ensure = () => {
                if (!pst.projects.some((p) => p.id === h.projectId)) {
                    return pst.loadProjects({ silent: false }).then(() => {
                        usePmProjectStore.getState().selectProject(h.projectId);
                    });
                }
                pst.selectProject(h.projectId);
                return Promise.resolve();
            };
            void ensure().then(() => {
                if (h.taskId) {
                    void usePmTaskDetailStore.getState().openTask(h.taskId);
                }
                onNavigate("projects");
            });
            closeDialog();
        },
        [onNavigate, closeDialog],
    );

    const openMemoryHit = useCallback(
        (h: MemoryHit) => {
            // Kept for the double-click path. The single-click preview path
            // routes through PreviewPane which calls fetchNode directly.
            void useMemoryStore.getState().fetchNode(agentId, h.nodeId).then(() => undefined);
        },
        [agentId],
    );

    const openConversationHit = useCallback(
        (h: ConversationHit) => {
            // Open the chat view at the hit session and scroll to the
            // exact message (ADR-081 §4.2): locateMessage aligns the
            // load window, then ChatPanel scrolls + highlights.
            onNavigate("chat");
            void useChatStore.getState().locateMessage(agentId, h.sessionId, h.messageIndex);
            closeDialog();
        },
        [agentId, onNavigate, closeDialog],
    );

    // Typed dispatcher — one hit, any source. Reused by the list rows
    // (Enter/double-click) AND the summary citation chips (ADR-093 D3):
    // a citation resolves to the exact landed row and routes the same open
    // action as if the user had clicked that row in its own tab.
    const activateHit = useCallback(
        (h: RowHit) => {
            if (h.type === "file") openFileHit(h as FileHit);
            else if (h.type === "git") openGitHit();
            else if (h.type === "doc") openDocHit(h as DocHit);
            else if (h.type === "project") openProjectHit(h as ProjectHit);
            else if (h.type === "conversation") openConversationHit(h as ConversationHit);
            else openMemoryHit(h as MemoryHit);
        },
        [openFileHit, openGitHit, openDocHit, openProjectHit, openConversationHit, openMemoryHit],
    );

    const activate = useCallback(
        (idx: number) => {
            const h = hits[idx];
            if (h) activateHit(h);
        },
        [hits, activateHit],
    );

    // A summary citation `[[rN]]` → the landed row it stands for → open.
    const openCitation = useCallback(
        (id: string) => {
            const set = candidateRef.current;
            if (!set) return;
            const source = set.byId[id];
            const rowIdx = set.indexOf[id];
            if (!source || rowIdx === undefined) return;
            const row = rowsRef.current[source]?.[rowIdx];
            if (row) activateHit(row);
        },
        [activateHit],
    );

    const onKeyDown = useCallback(
        (e: React.KeyboardEvent) => {
            if (e.key === "Escape") {
                e.preventDefault();
                e.stopPropagation();
                closeDialog();
            } else if (e.key === "ArrowDown") {
                e.preventDefault();
                setFocusedIdx((i) => Math.min(i + 1, hits.length - 1));
            } else if (e.key === "ArrowUp") {
                e.preventDefault();
                setFocusedIdx((i) => Math.max(i - 1, 0));
            } else if (e.key === "Enter") {
                e.preventDefault();
                activate(focusedIdx);
            }
        },
        [hits.length, focusedIdx, activate, closeDialog],
    );

    if (!open) return null;

    const TABS: { key: Tab; label: string }[] = [
        { key: "file", label: t("globalSearch.tabs.file") },
        { key: "git", label: t("globalSearch.tabs.git") },
        { key: "doc", label: t("globalSearch.tabs.doc") },
        { key: "project", label: t("globalSearch.tabs.project") },
        { key: "conversation", label: t("globalSearch.tabs.conversation") },
        { key: "memory", label: t("globalSearch.tabs.memory") },
    ];

    const focusedHit = hits[focusedIdx] ?? null;
    const focusedGit = focusedHit?.type === "git"
        ? commits.find((c) => c.hash === focusedHit.id)
        : undefined;

    return (
        <div
            className="fixed inset-0 z-50 flex items-center justify-center bg-modal-overlay"
            onMouseDown={() => closeDialog()}
        >
            <div
                className="relative z-10 flex max-h-[70vh] w-full max-w-3xl flex-col overflow-hidden rounded-md border border-border-outer bg-modal-surface text-text shadow-xl"
                onMouseDown={(e) => e.stopPropagation()}
                onKeyDown={onKeyDown}
            >
                {/* ── Header ──────────────────────────────────────────── */}
                <div className="flex shrink-0 items-center gap-2 border-b border-border-divider px-5 py-3 min-h-[var(--ui-dialog-zone-h)]">
                    <Search className="h-5 w-5 text-text-tertiary" />
                    <h2 className="text-sm font-semibold">{t("globalSearch.title")}</h2>
                </div>

                {/* ── Zone 1: agent picker + search input ─────────────── */}
                <div className="flex shrink-0 items-center border-b border-border-divider min-h-[var(--ui-dialog-zone-h)]">
                    <AgentPickerChip
                        agents={agentList.map((a) => a.meta)}
                        value={agentId}
                        onChange={(next) => {
                            setAgentId(next);
                            setGitLoaded(false);
                            setCommits([]);
                            setFileHits([]);
                            setMemoryHits([]);
                            setConversationHits([]);
                        }}
                    />
                    <div className="flex h-full flex-1 items-center gap-1.5 border-l border-zinc-200 px-5 py-1.5 dark:border-zinc-700">
                        <Search className="h-3 w-3 shrink-0 text-text-tertiary" />
                        <input
                            ref={inputRef}
                            type="text"
                            value={query}
                            onChange={(e) => setQuery(e.target.value)}
                            placeholder={t("globalSearch.searchInputPlaceholder")}
                            className="flex-1 bg-transparent text-xs text-text-secondary outline-none placeholder:text-text-tertiary"
                        />
                        {tab === "file" && (
                            <div className="flex items-center gap-1">
                                <ToggleBtn
                                    title={t("globalSearch.toggleCaseSensitive")}
                                    active={caseSensitive}
                                    onClick={() => setCaseSensitive((p) => !p)}
                                >
                                    Aa
                                </ToggleBtn>
                                <ToggleBtn
                                    title={t("globalSearch.toggleWholeWord")}
                                    active={wholeWord}
                                    onClick={() => setWholeWord((p) => !p)}
                                >
                                    \b
                                </ToggleBtn>
                            </div>
                        )}
                        {/* ADR-093: re-run — aborts the current generation
                            (search + summary) and starts a fresh one. */}
                        {query.trim() && (
                            <button
                                type="button"
                                data-testid="search-rerun"
                                title={t("globalSearch.rerun")}
                                aria-label={t("globalSearch.rerun")}
                                onMouseDown={(e) => e.preventDefault()}
                                onClick={() => void runSearch(query)}
                                className="flex h-6 w-6 shrink-0 items-center justify-center rounded text-text-tertiary transition-colors hover:bg-zinc-100 hover:text-text-secondary dark:hover:bg-zinc-700/50"
                            >
                                <RotateCw className="h-3.5 w-3.5" />
                            </button>
                        )}
                    </div>
                </div>

                {/* ── Zone 2: tabs + results list ────────────────────── */}
                {/* `min-h-[180px]` keeps the results region visible even
                    before the user has typed anything — without it Zone 2
                    shrinks to its content (an empty placeholder) and the
                    dialog jumps taller the moment a query produces rows.
                    `flex-1` lets it grow up to the 70vh dialog cap once
                    Zone 3 has claimed its fixed 140–300px. */}
                <div className="flex min-h-[180px] flex-1 flex-col overflow-hidden border-b border-border-divider">
                    <div className="flex shrink-0 items-center justify-center gap-1 border-b border-border-divider px-5 min-h-[var(--ui-dialog-zone-h)]">
                        {TABS.map((tabDef) => (
                            <button
                                key={tabDef.key}
                                data-testid={`tab-${tabDef.key}`}
                                onClick={() => setTab(tabDef.key)}
                                className={`rounded-full px-3 py-1.5 text-xs font-medium transition-colors ${
                                    tab === tabDef.key
                                        ? "bg-zinc-200 text-text dark:bg-zinc-700"
                                        : "text-text-tertiary hover:bg-zinc-100 dark:hover:bg-zinc-700/50"
                                }`}
                            >
                                {tabDef.label}
                            </button>
                        ))}
                    </div>

                    {/* ADR-093 progress strip (design 27 §4.6): stage +
                        "X/6 sources landed". No fake percentage — sources
                        arrive out of order, counts are the honest unit. */}
                    {phase !== "idle" && (
                        <div
                            data-testid="search-status-bar"
                            className="flex h-8 shrink-0 items-center gap-2 border-b border-border-divider px-5 text-11 text-text-tertiary"
                        >
                            {phase === "searching" && (
                                <span className="animate-pulse">
                                    {t("globalSearch.statusSearching", { done: sourcesDone, total: SEARCH_SOURCES.length })}
                                </span>
                            )}
                            {phase === "summarizing" && (
                                <>
                                    <span>{t("globalSearch.statusSearchDone", { total: SEARCH_SOURCES.length })}</span>
                                    <span aria-hidden>·</span>
                                    <span className="animate-pulse text-[var(--color-accent)]">{t("globalSearch.statusSummarizing")}</span>
                                </>
                            )}
                            {(phase === "done" || phase === "error" || phase === "cancelled") && (
                                <span>{t("globalSearch.statusSearchDone", { total: SEARCH_SOURCES.length })}</span>
                            )}
                        </div>
                    )}

                    <div className="flex-1 overflow-y-auto">
                        {tab === "file" && loading && <RowInfo>{t("globalSearch.searching")}</RowInfo>}
                        {tab === "file" && error && !loading && <RowInfo>{error}</RowInfo>}
                        {tab === "file" && !loading && !error && query.trim() && hits.length === 0 && (
                            <RowInfo>{t("globalSearch.fileNoMatch")}</RowInfo>
                        )}
                        {tab === "file" && !query.trim() && <RowInfo>{t("globalSearch.fileEmptyHint")}</RowInfo>}
                        {tab === "file" && fileTotal > 0 && !loading && (
                            <div className="px-3 pt-1 text-11 text-text-tertiary">
                                {t("globalSearch.matchCount", { total: fileTotal, shown: hits.length })}
                            </div>
                        )}

                        {tab === "git" && gitError && <RowInfo>{gitError}</RowInfo>}
                        {tab === "git" && !gitError && gitLoaded && hits.length === 0 && (
                            <RowInfo>{t("globalSearch.gitNoMatch")}</RowInfo>
                        )}
                        {tab === "git" && !gitError && !gitLoaded && phase !== "idle" && (
                            <RowInfo>{t("globalSearch.loadingGitHistory")}</RowInfo>
                        )}

                        {tab === "doc" && docLoading && <RowInfo>{t("globalSearch.searching")}</RowInfo>}
                        {tab === "doc" && docError && !docLoading && <RowInfo>{docError}</RowInfo>}
                        {tab === "doc" && !docLoading && !docError && query.trim() && hits.length === 0 && (
                            <RowInfo>{t("globalSearch.docNoMatch")}</RowInfo>
                        )}
                        {tab === "doc" && !query.trim() && <RowInfo>{t("globalSearch.docEmptyHint")}</RowInfo>}

                        {tab === "project" && projLoading && <RowInfo>{t("globalSearch.searching")}</RowInfo>}
                        {tab === "project" && projError && !projLoading && <RowInfo>{projError}</RowInfo>}
                        {tab === "project" && !projLoading && !projError && query.trim() && hits.length === 0 && (
                            <RowInfo>{t("globalSearch.projectNoMatch")}</RowInfo>
                        )}
                        {tab === "project" && !projLoading && !projError && !query.trim() && (
                            <RowInfo>{t("globalSearch.projectEmptyHint")}</RowInfo>
                        )}

                        {tab === "memory" && memoryLoading && <RowInfo>{t("globalSearch.searching")}</RowInfo>}
                        {tab === "memory" && memoryError && !memoryLoading && <RowInfo>{memoryError}</RowInfo>}
                        {tab === "memory" && !memoryLoading && !memoryError && query.trim() && hits.length === 0 && (
                            <RowInfo>{t("globalSearch.memoryNoMatch")}</RowInfo>
                        )}
                        {tab === "memory" && !memoryLoading && !memoryError && !query.trim() && (
                            <RowInfo>{t("globalSearch.memoryEmptyHint")}</RowInfo>
                        )}

                        {tab === "conversation" && conversationLoading && <RowInfo>{t("globalSearch.searching")}</RowInfo>}
                        {tab === "conversation" && conversationError && !conversationLoading && <RowInfo>{conversationError}</RowInfo>}
                        {tab === "conversation" && !conversationLoading && !conversationError && query.trim() && hits.length === 0 && (
                            <RowInfo>{t("globalSearch.conversationNoMatch")}</RowInfo>
                        )}
                        {tab === "conversation" && !conversationLoading && !conversationError && !query.trim() && (
                            <RowInfo>{t("globalSearch.conversationEmptyHint")}</RowInfo>
                        )}

                        <ul data-testid="result-list">
                            {hits.map((h, i) => (
                                <li
                                    key={h.id}
                                    data-testid={`result-row-${h.type}`}
                                    onMouseDown={(e) => {
                                        // Single click → preview.
                                        e.preventDefault();
                                        setFocusedIdx(i);
                                    }}
                                    onDoubleClick={(e) => {
                                        e.preventDefault();
                                        e.stopPropagation();
                                        activate(i);
                                    }}
                                    className={`flex cursor-pointer items-center gap-2 border-t border-border-divider px-3 py-1.5 text-xs first:border-t-0 ${
                                        i === focusedIdx
                                            ? `${listRowSelected} text-text`
                                            : `text-text-secondary ${listRowHover}`
                                    }`}
                                >
                                    <HitIcon type={h.type} title={h.title} focused={i === focusedIdx} />
                                    <div className="min-w-0 flex-1">
                                        <div className="truncate font-medium">
                                            <Highlight text={h.title} query={query} />
                                        </div>
                                        <div className="flex items-center gap-2 truncate text-11 text-text-tertiary">
                                            <span className="truncate">
                                                <Highlight text={localizeSub(h, t)} query={query} />
                                            </span>
                                            <span className="truncate font-mono">
                                                <Highlight text={h.snippet} query={query} />
                                            </span>
                                        </div>
                                    </div>
                                </li>
                            ))}
                        </ul>
                    </div>
                </div>

                {/* ── Zone 3: summary row (top) + preview row (bottom) ── */}
                {/* ADR-093 design 27 §4.6: the AI summary gets its own
                    always-visible row so progress/cancel/regenerate have a
                    home; the preview keeps its old slot below. Equal split,
                    each row scrolls independently. */}
                <div className="flex h-[35vh] max-h-[480px] min-h-[240px] shrink-0 flex-col overflow-hidden">
                    <div
                        data-testid="summary-pane"
                        className="flex min-h-0 flex-1 flex-col overflow-hidden"
                    >
                        <SummaryView
                            phase={phase}
                            summary={summaryText}
                            error={summaryError}
                            allowedIds={
                                new Set((candidateRef.current?.candidates ?? []).map((c) => c.id))
                            }
                            onCancel={() => summaryCtrlRef.current?.abort()}
                            onRegenerate={() => void runSummaryStage(seqRef.current, runCtrlRef.current)}
                            onCitation={openCitation}
                        />
                    </div>
                    <div
                        data-testid="preview-pane"
                        className="flex min-h-0 flex-1 flex-col overflow-hidden border-t border-border-divider"
                    >
                        <PreviewPane
                            hit={focusedHit}
                            tab={tab}
                            agentId={agentId}
                            workspaceId={workspaceId}
                            gitCommit={focusedGit}
                        />
                    </div>
                </div>

                {/* ── Footer ──────────────────────────────────────────── */}
                <div className="flex shrink-0 items-center justify-end border-t border-border-divider px-5 min-h-[var(--ui-dialog-zone-h)]">
                    <button
                        type="button"
                        onClick={() => closeDialog()}
                        className="rounded-md px-3 py-1.5 text-xs font-medium text-text-secondary hover:bg-zinc-100 dark:hover:bg-zinc-700"
                    >
                        {t("globalSearch.close")}
                    </button>
                </div>
            </div>
        </div>
    );
}

/* ─── Helpers used inside the component ─────────────────────────────── */

/** ADR-081 D4: row secondary line is raw (kind, role) so the renderer
 *  applies the locale-aware label here. */
function localizeSub(h: RowHit, t: (k: string) => string): string {
    if (h.type === "project") {
        const ph = h as ProjectHit;
        const isTask = ph.sub.startsWith("task");
        const label = t(isTask ? "globalSearch.subTask" : "globalSearch.subProject");
        return `${label} · ${ph.projectId}`;
    }
    if (h.type === "conversation") {
        const ch = h as ConversationHit;
        const isAssistant = ch.role === "assistant";
        const label = t(isAssistant ? "globalSearch.subAssistant" : "globalSearch.subUser");
        return `${label} · #${ch.messageIndex}`;
    }
    return h.sub;
}

function HitIcon({ type, title, focused }: { type: Tab; title: string; focused: boolean }) {
    const cls = `h-4 w-4 shrink-0 ${focused ? "text-text" : "text-text-tertiary"}`;
    if (type === "file") {
        return <SetiIcon {...getFileIcon(title)} size={16} className={cls} />;
    }
    if (type === "doc") return <BookOpen className={cls} />;
    if (type === "project") return <KanbanSquare className={cls} />;
    if (type === "conversation") return <MessageSquare className={cls} />;
    if (type === "memory") {
        return (
            <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.2" className={cls}>
                <ellipse cx="8" cy="5" rx="5" ry="3" />
                <path d="M3 5v4c0 1.7 2.2 3 5 3s5-1.3 5-3V5" />
                <path d="M3 9v2c0 1.7 2.2 3 5 3s5-1.3 5-3V9" />
            </svg>
        );
    }
    // git
    return <GitCommitRowIcon focused={focused} />;
}

function GitCommitRowIcon({ focused }: { focused: boolean }) {
    const cls = `h-4 w-4 shrink-0 ${focused ? "text-text" : "text-text-tertiary"}`;
    return <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.2" className={cls}>
        <circle cx="8" cy="4" r="2" />
        <line x1="8" y1="6" x2="8" y2="14" />
        <circle cx="8" cy="12" r="2" />
    </svg>;
}

function RowInfo({ children }: { children: React.ReactNode }) {
    return (
        <div className="px-3 py-3 text-center text-xs text-text-tertiary">{children}</div>
    );
}

/**
 * ADR-093 summary row (design 27 §4.6). Owns no fetching — the dialog
 * drives the state machine; this renders per phase. Citation chips are
 * accessible buttons carrying `data-citation-id`; the summary text itself
 * is rendered as plain text segments (LLM output is never interpreted as
 * HTML/URLs — ADR-093 D3).
 */
function SummaryView({
    phase,
    summary,
    error,
    allowedIds,
    onCancel,
    onRegenerate,
    onCitation,
}: {
    phase: SummaryPhase;
    summary: string;
    error: string | null;
    allowedIds: ReadonlySet<string>;
    onCancel: () => void;
    onRegenerate: () => void;
    onCitation: (id: string) => void;
}) {
    const { t } = useTranslation();

    const cornerBtn = (label: string, onClick: () => void) => (
        <button
            type="button"
            onClick={onClick}
            className="absolute right-2 rounded px-2 py-0.5 text-11 text-text-tertiary transition-colors hover:bg-zinc-100 hover:text-text-secondary dark:hover:bg-zinc-700/50"
        >
            {label}
        </button>
    );

    if (phase === "idle") {
        return <RowInfo>{t("globalSearch.summaryIdleHint")}</RowInfo>;
    }
    if (phase === "searching") {
        return <RowInfo>{t("globalSearch.summaryWaiting")}</RowInfo>;
    }
    if (phase === "summarizing") {
        return (
            <div className="relative flex h-full items-center justify-center px-3 py-3 text-xs text-text-tertiary">
                <span className="animate-pulse" data-testid="summary-working">
                    {t("globalSearch.summaryWorking")}
                </span>
                {cornerBtn(t("globalSearch.cancelSummary"), onCancel)}
            </div>
        );
    }
    if (phase === "error" || phase === "cancelled") {
        return (
            <div className="relative flex h-full items-center justify-center px-3 py-3 text-xs text-text-tertiary">
                <span>{error ?? t("globalSearch.summaryCancelled")}</span>
                {cornerBtn(t("globalSearch.regenerate"), onRegenerate)}
            </div>
        );
    }
    // phase === "done"
    if (!summary) {
        return <RowInfo>{t("globalSearch.summaryNoCandidates")}</RowInfo>;
    }
    const segments = parseSummarySegments(summary, allowedIds);
    return (
        <div className="relative h-full">
            <div
                data-testid="summary-text"
                className="h-full overflow-y-auto px-4 py-2.5 text-xs leading-relaxed text-text-secondary"
            >
                {segments.map((s, i) =>
                    s.kind === "text" ? (
                        <span key={i}>{s.text}</span>
                    ) : (
                        <button
                            key={i}
                            type="button"
                            data-citation-id={s.id}
                            aria-label={t("globalSearch.citationAria", { id: s.id })}
                            onClick={() => onCitation(s.id)}
                            className="mx-0.5 rounded-sm bg-[var(--color-accent)]/15 px-1 font-mono text-11 text-[var(--color-accent)] transition-colors hover:bg-[var(--color-accent)]/30"
                        >
                            {s.id}
                        </button>
                    ),
                )}
            </div>
            {cornerBtn(t("globalSearch.regenerate"), onRegenerate)}
        </div>
    );
}

function ToggleBtn({
    title,
    active,
    onClick,
    children,
}: {
    title: string;
    active: boolean;
    onClick: () => void;
    children: React.ReactNode;
}) {
    return (
        <button
            type="button"
            title={title}
            onMouseDown={(e) => e.preventDefault()}
            onClick={onClick}
            className={`flex h-6 min-w-[1.5rem] items-center justify-center rounded px-1.5 text-11 font-medium transition-colors ${
                active
                    ? "bg-[var(--color-accent)]/20 text-[var(--color-accent)]"
                    : "text-text-tertiary hover:bg-zinc-100 dark:hover:bg-zinc-700/50"
            }`}
        >
            {children}
        </button>
    );
}

