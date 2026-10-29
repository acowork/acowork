import { create } from "zustand";
import { useSettingsStore } from "./settingsStore";
import { useAuthStore } from "./authStore";
import { DEFAULT_GATEWAY_URL } from "../lib/config";
import { log } from "../lib/logger";
import type { GitCommitDto } from "./gitStore";

/**
 * Check if a string looks like a valid HTTP/HTTPS URL.
 * Returns the normalized URL (lowercased scheme) or null if not a valid http(s) URL.
 */
function normalizeUrl(candidate: string): string | null {
    if (!candidate) return null;
    const trimmed = candidate.trim();
    try {
        const url = new URL(trimmed);
        if (url.protocol === "http:" || url.protocol === "https:") {
            return url.href;
        }
        return null;
    } catch {
        return null;
    }
}

// ── Dispose registry ─────────────────────────────────────────────────────────
//
// FileEditorPanel owns the Monaco TextModel for each opened file (plus its
// LSP didClose side-effect). When any caller closes a file — UI X button,
// right-click menu, batch-confirm dialog, panel unmount, or an external
// trigger like workspaceFsEvents reacting to a disk-delete — the store
// must release the Monaco TextModel. But the store has no Monaco reference,
// and pushing Monaco knowledge down into the store would couple the data
// layer to the editor implementation.
//
// This module-level registry inverts the dependency: the owning component
// registers a disposer on mount, the close actions invoke every registered
// disposer for each removed file, and the store stays free of editor
// internals. New close paths (e.g. the disk-delete watcher) get correct
// disposal for free — no caller can forget to release.
//
// The registry is a Set rather than a single function because the panel
// may legitimately want to register multiple disposers (today only one is
// needed; future renderers — a separate LSP-tracker panel, a search-result
// preview — could each register their own).
type FileDisposer = (file: OpenFile) => void;
const fileDisposers = new Set<FileDisposer>();

/**
 * Register a disposer invoked once per file removed by closeFile,
 * closeOthers, or closeAllFiles. Returns an unregister callback for
 * use as the cleanup of a useEffect. Disposers run BEFORE the store
 * mutation, so a disposer that throws still leaves the store consistent
 * for the next call.
 */
export function registerFileDisposer(disposer: FileDisposer): () => void {
    fileDisposers.add(disposer);
    return () => {
        fileDisposers.delete(disposer);
    };
}

/** Invoke every registered disposer for each file in files. */
function disposeFiles(files: OpenFile[]): void {
    for (const file of files) {
        for (const disposer of fileDisposers) {
            try {
                disposer(file);
            } catch (err) {
                log.error("[FileEditorStore] file disposer threw:", err);
            }
        }
    }
}

/** Extension → Monaco language ID mapping */
const EXT_LANGUAGE_MAP: Record<string, string> = {
    rs: "rust",
    ts: "typescript",
    tsx: "typescript",
    js: "javascript",
    jsx: "javascript",
    json: "json",
    // Monaco ships no built-in TOML grammar; TOML syntax is close enough to
    // INI (comments, [sections], key = value) that we reuse "ini" for
    // highlighting. Must stay in lockstep with AgentSetupTab's
    // "TOML is mapped to ini language" when it opens shell_risk_rules.toml.
    toml: "ini",
    yaml: "yaml",
    yml: "yaml",
    md: "markdown",
    html: "html",
    htm: "html",
    css: "css",
    scss: "scss",
    less: "less",
    xml: "xml",
    sh: "shell",
    bash: "shell",
    zsh: "shell",
    ps1: "powershell",
    psm1: "powershell",
    psd1: "powershell",
    bat: "bat",
    cmd: "bat",
    py: "python",
    rb: "ruby",
    go: "go",
    java: "java",
    c: "c",
    h: "c",
    cpp: "cpp",
    cc: "cpp",
    cxx: "cpp",
    hpp: "cpp",
    cs: "csharp",
    swift: "swift",
    kt: "kotlin",
    kts: "kotlin",
    sql: "sql",
    graphql: "graphql",
    gql: "graphql",
    dockerfile: "dockerfile",
    ini: "ini",
    cfg: "ini",
    conf: "ini",
};

/** A file opened in the editor */
export interface OpenFile {
    /** Unique ID: `${agentId}:${workspaceId}:${relPath}` for files, `${agentId}:url:${url}` for URLs */
    id: string;
    agentId: string;
    workspaceId: string;
    relPath: string;
    fileName: string;
    /** Current content (may differ from originalContent when dirty) */
    content: string;
    /** Original content loaded from disk */
    originalContent: string;
    loading: boolean;
    saving: boolean;
    /** Monaco language ID (e.g. "typescript", "rust") */
    language: string;
    /** Whether the file has unsaved changes */
    dirty: boolean;
    /** Set when the last save attempt failed. Cleared on next successful save or on content edit. */
    saveError?: string;
    /** If set, editor should reveal this line (1-based) after mount */
    cursorLine?: number;
    /** "edit" = Monaco editor; "preview" = read-only Markdown render. */
    mode: "edit" | "preview";
    /** "file" = workspace file; "url" = external URL loaded in an iframe;
     *  "diff" / "log" = read-only virtual files from the Git Status Bar
     *  (ADR-078 decision 7) — never saved, never dirty, no fs-watch;
     *  "attachment" = read-only virtual preview of an inbox chat attachment
     *  (ADR-076 blob owned by the Gateway, not a workspace file) — also
     *  never saved, never dirty, no fs-watch. */
    kind: "file" | "url" | "diff" | "log" | "attachment";
    /** The URL to load (only for kind === "url") */
    url?: string;
    /** ADR-078: git diff classification for kind === "diff" virtual
     *  tabs ("modified" | "untracked" | "deleted" | "binary" | "nochange"). */
    gitDiffKind?: string;
    /** ADR-078 extension: base revision for the diff (default "HEAD").
     *  Set when the user picks a different base via the diff banner. */
    diffBaseRef?: string;
    /** ADR-078 extension: compare revision for the diff.
     *  "" (default) means the working tree; any git ref is accepted.
     *  Set when the user picks a different compare via the diff banner. */
    diffHeadRef?: string;
    /** ADR-078 diff-mode toggle: when true (default for new diff
     *  tabs — seeded by openVirtualFile), render the virtual diff tab
     *  in Monaco's INLINE form (one pane, +/- colour-coded blocks).
     *  When false, the side-by-side two-pane layout is used.
     *  User flips via the GitVirtualNav toggle; persists with the tab
     *  until closed. Inline is the default because single-file +/- diff
     *  is denser on narrow editor widths and matches the "what
     *  changed here" reading the user does in 90% of cases. */
    diffInlineMode?: boolean;
    /** Pagination state for kind === "log" virtual tabs.
     *
     *  - `loadedCommits` is the cache of every commit we've fetched for
     *    this tab so far (newest first). It only GROWS — backward
     *    navigation never trims it, so the user can flip forward again
     *    without a network round-trip. Capped at LOG_MAX_LIMIT (200,
     *    matches Runtime's `core/.../git_query_impl.rs` LOG_MAX_LIMIT).
     *  - `displayedLimit` is the size of the window currently visible
     *    (= commits.slice(0, displayedLimit).length). Ranged between
     *    LOG_STEP (50, the initial page) and LOG_MAX_LIMIT (200).
     *
     *  Both fields are read by GitVirtualNav's Prev / Next page buttons. */
    loadedCommits?: GitCommitDto[];
    /** See `loadedCommits` — current display window size. */
    displayedLimit?: number;
    /** Set true once a `fetchLog` call returned fewer commits than
     *  asked for — i.e. we've reached the end of the file's history
     *  and another "Next" click would just return the same tail. The
     *  Next button in GitVirtualNav reads this and disables itself;
     *  the cache (`loadedCommits`) is preserved so Prev still works.
     *  Cleared by closeFile / workspace switch (handled by the file
     *  being replaced wholesale). */
    reachedEnd?: boolean;
    /** MIME type from Gateway response (e.g. "image/png", "text/html") */
    mimeType?: string;
    // ── ADR-058: external-modification conflict tracking ──
    /** Disk mtime (RFC3339 string) as of the last read/save — used to
     *  distinguish real external writes from touch/chmod noise. */
    diskModified?: string;
    /** Disk size (bytes) as of the last read/save — pairs with diskModified. */
    diskSize?: number;
    /** Save-success moment (Desktop local clock, epoch ms). Used to
     *  suppress the echo of our own write coming back via MQTT. */
    lastSavedAtMs?: number;
    /** Set when the file was deleted on disk while the tab is dirty. */
    diskDeleted?: boolean;
    /** External-change conflict state ("modified" | "deleted"). */
    diskConflict?: "modified" | "deleted";
}

interface FileEditorState {
    openFiles: OpenFile[];
    activeFileId: string | null;

    /** Open a file in edit mode (or activate if already open). Fetches content from Gateway.
     * @param line - Optional 1-based line number to reveal after opening */
    openFile: (agentId: string, workspaceId: string, relPath: string, line?: number) => Promise<void>;
    /** Open a file in read-only preview mode. Re-fetches content from disk.
     *  - If the file is already open in preview mode, just activates it.
     *  - If the file is open in edit mode, switches that tab to preview mode. */
    openPreview: (agentId: string, workspaceId: string, relPath: string) => Promise<void>;
    /** Open a file with pre-loaded content (skips Gateway fetch). Used by LSP cross-file navigation. */
    openFileWithContent: (agentId: string, workspaceId: string, relPath: string, content: string, language: string) => void;
    /** Open a group's `chat.md` (the editable body only — the non-trimmable
     *  `# Safety` tail is not shown, the server re-appends it on save) as a
     *  Monaco tab. The session id scopes the tab; Save routes back through the
     *  chat-prompt endpoint (`saveFile` detects the sentinel workspace). */
    openGroupChatMd: (sessionId: string, content: string) => void;
    /** Open a read-only virtual tab for an inbox chat attachment (ADR-076).
     *  Content is supplied by the caller (already fetched from the chat
     *  blob route) — no workspace, no Gateway file fetch, never dirty,
     *  never saved. Tabs are addressed as
     *  `attachment:${chatId}:${attachmentId}`, so opening the same
     *  attachment again activates the existing tab instead of stacking
     *  duplicates. Keyed on the chat, not the account: the group surface
     *  never holds an owning `userId`, and a chat id is already unique. */
    openAttachmentPreview: (opts: {
        chatId: string;
        attachmentId: string;
        fileName: string;
        content: string;
        language: string;
    }) => void;
    /** Open a URL in a new tab (rendered in an iframe). Does nothing if the URL is invalid. */
    openUrl: (agentId: string, url: string) => void;
    /** ADR-078: open a read-only virtual diff/log tab (Monaco DiffEditor
     *  or plaintext). Content is supplied directly — no Gateway fetch,
     *  no disk, never dirty. Tabs are addressed as
     *  `git:${agentId}:${workspaceId}:${kind}:${relPath}` so switching
     *  agent/workspace never collides with a real file tab. */
    openVirtualFile: (opts: {
        agentId: string;
        workspaceId: string;
        kind: "diff" | "log";
        relPath: string;
        content: string;
        original?: string;
        gitDiffKind?: string;
        language: string;
        /** ADR-XXX: log pagination seed (kind === "log" only). Sets
         *  the initial commit cache + displayed window so GitVirtualNav
         *  can navigate Prev / Next without an extra fetch on first
         *  open. Pass the array returned by the same fetchLog call
         *  that produced `content`, plus the limit used. `reachedEnd`
         *  is seeded true when the initial fetch returned fewer
         *  commits than the limit (small history). */
        loadedCommits?: GitCommitDto[];
        displayedLimit?: number;
        reachedEnd?: boolean;
        /** ADR-XXX: diff ref override — when set, the diff tab will
         *  render against these revisions instead of HEAD vs working
         *  tree. Set by the diff banner commit picker. */
        diffBaseRef?: string;
        diffHeadRef?: string;
    }) => void;
    /** Close a file tab. Returns false if dirty (caller should confirm first). */
    closeFile: (fileId: string, force?: boolean) => boolean;
    /** Close all tabs except the one with `keepFileId`.
     *  Returns `false` if any non-kept file is dirty and `force` is not set. */
    closeOthers: (keepFileId: string, force?: boolean) => boolean;
    /** Set the active (focused) file */
    setActiveFile: (fileId: string) => void;
    /** Update file content (marks as dirty) */
    updateContent: (fileId: string, content: string) => void;
    /** Replace the content of a virtual (kind === "diff" | "log") tab
     *  WITHOUT flipping the `dirty` flag. Virtual tabs are never saved,
     *  so dirty is meaningless there — using `updateContent` would
     *  spuriously mark them dirty and trigger a stray save-conflict
     *  badge. Used by GitVirtualNav's Prev / Next commit pagination
     *  buttons on log tabs to swap the displayed window in/out. */
    setVirtualFileContent: (
        fileId: string,
        content: string,
        extras?: {
            loadedCommits?: GitCommitDto[];
            displayedLimit?: number;
            reachedEnd?: boolean;
        },
    ) => void;
    /** ADR-078 extension: change the diff base / head refs in place
     *  (no new tab). The actual content re-fetch is the caller's job
     *  — this only updates which refs the tab is bound to so the
     *  banner labels render correctly and a later commit-picker
     *  selection knows what to highlight. */
    updateDiffRefs: (fileId: string, baseRef?: string, headRef?: string) => void;
    /** Flip a diff tab between side-by-side (default) and inline
     *  (single-pane +/- colour blocks). No-op for non-diff tabs. */
    setDiffInlineMode: (fileId: string, inline: boolean) => void;
    /** Save file content to Gateway */
    saveFile: (fileId: string) => Promise<void>;
    /** Re-fetch file content from disk and replace both content and originalContent.
     *  Resets dirty=false. URL-preview tabs (kind === "url") are skipped.
     *  No-op while a refresh is already in flight for this file.
     *  `opts.skipIfDirty` (ADR-058 review M-3): skip instead of overwrite
     *  when the tab turned dirty while the fetch was in flight — used by
     *  fs-event-triggered silent reloads so user input is never clobbered. */
    refreshFile: (fileId: string, opts?: { skipIfDirty?: boolean }) => Promise<void>;
    /** ADR-058: clear the external-change conflict markers on a tab
     *  (the "Keep mine" answer to a disk-change toast). Purely local —
     *  the disk state itself is untouched. */
    clearDiskConflict: (fileId: string) => void;
    /** Close all open files. If `force` is false and any file is dirty,
     *  no files are closed and the function returns `false`. */
    closeAllFiles: (force?: boolean) => boolean;
}

function getGatewayUrl(): string {
    return useSettingsStore.getState().gatewayUrl || DEFAULT_GATEWAY_URL;
}

/**
 * Build the Gateway endpoint URL for a workspace file (read/write).
 * Centralised so all callers (openFile, openPreview, saveFile, refreshFile)
 * stay in sync on workspace_id handling and the `path` query param.
 */
function buildFileUrl(agentId: string, workspaceId: string, relPath: string): string {
    const baseUrl = getGatewayUrl();
    const params = new URLSearchParams();
    if (workspaceId && workspaceId !== "__agent_home__") {
        params.set("workspace_id", workspaceId);
    }
    params.set("path", relPath);
    return `${baseUrl}/api/agents/${agentId}/workspaces/file?${params.toString()}`;
}

/**
 * The shell risk rules file lives at `<work_dir>/config/shell_risk_rules.toml`,
 * which is NOT inside any workspace — so the generic workspace file API
 * (`/workspaces/file`) cannot write to it. The Runtime exposes a dedicated
 * endpoint `/agents/{id}/shell-risk-rules` that handles persistence and
 * live-reload of the in-memory rule cache.
 *
 * Detect this special file by its path so saveFile routes to the right URL.
 */
const SHELL_RISK_RULES_PATH = "config/shell_risk_rules.toml";
function isShellRiskRulesFile(workspaceId: string, relPath: string): boolean {
    return workspaceId === "__agent_home__" && relPath === SHELL_RISK_RULES_PATH;
}
function buildShellRiskRulesUrl(agentId: string): string {
    return `${getGatewayUrl()}/api/agents/${agentId}/shell-risk-rules`;
}

/**
 * A group's `chat.md` is a second non-workspace Monaco-editable file, opened
 * by the group settings panel via `openFileWithContent` under a sentinel
 * workspace with the session id encoded in the relPath
 * (`chat-sessions/{sessionId}/chat.md`). Like the shell risk rules it is NOT
 * under any workspace, so the generic `/workspaces/file` endpoint can neither
 * read nor write it — the chat-prompt route owns it instead:
 * `/api/chat-sessions/{sessionId}/chat-md`.
 *
 * Two differences from the workspace save path, both handled in `saveFile`:
 *   - the user-service route authenticates from the bearer token the Gateway
 *     forwards, so the save MUST attach `Authorization` (workspace saves do
 *     not — they ride the Gateway's localhost-trusted agent surface);
 *   - the stored body always carries the non-trimmable `# Safety` tail the
 *     server re-appends, but the GET/PUT response hides it, so the tab holds
 *     the editable body only and saving it is round-trip stable.
 */
const GROUP_CHAT_MD_WORKSPACE = "__chat_group__";
const GROUP_CHAT_MD_PREFIX = "chat-sessions/";
function isGroupChatMd(workspaceId: string): boolean {
    return workspaceId === GROUP_CHAT_MD_WORKSPACE;
}
/** Extract the `{sessionId}` from a `chat-sessions/{sessionId}/chat.md` path. */
function groupChatMdSessionId(relPath: string): string | null {
    if (!relPath.startsWith(GROUP_CHAT_MD_PREFIX)) return null;
    const rest = relPath.slice(GROUP_CHAT_MD_PREFIX.length);
    const slash = rest.indexOf("/");
    return slash > 0 ? rest.slice(0, slash) : null;
}
function buildGroupChatMdUrl(sessionId: string): string {
    return `${getGatewayUrl()}/api/chat-sessions/${encodeURIComponent(sessionId)}/chat-md`;
}

function detectLanguage(fileName: string): string {
    const ext = fileName.split(".").pop()?.toLowerCase() || "";
    // Handle special filenames without extension
    const baseName = fileName.toLowerCase();
    if (baseName === "dockerfile") return "dockerfile";
    if (baseName === "makefile") return "makefile";
    if (baseName === ".gitignore" || baseName === ".editorconfig") return "ini";
    return EXT_LANGUAGE_MAP[ext] || "plaintext";
}

export const useFileEditorStore = create<FileEditorState>((set, get) => ({
    openFiles: [],
    activeFileId: null,

    openFile: async (agentId: string, workspaceId: string, relPath: string, line?: number) => {
        const fileId = `${agentId}:${workspaceId}:${relPath}`;
        const existing = get().openFiles.find((f) => f.id === fileId);
        if (existing) {
            // Already open — activate, switch to edit mode if it was preview,
            // and jump to line if specified
            set({
                activeFileId: fileId,
                openFiles: get().openFiles.map((f) =>
                    f.id === fileId
                        ? {
                            ...f,
                            mode: "edit",
                            ...(line !== undefined ? { cursorLine: line } : {}),
                        }
                        : f,
                ),
            });
            return;
        }

        const fileName = relPath.split("/").pop() || relPath;
        const language = detectLanguage(fileName);

        // Add placeholder file
        const newFile: OpenFile = {
            id: fileId,
            agentId,
            workspaceId,
            relPath,
            fileName,
            content: "",
            originalContent: "",
            loading: true,
            saving: false,
            language,
            dirty: false,
            mode: "edit",
            ...(line !== undefined ? { cursorLine: line } : {}),
            kind: "file",
        };

        set((state) => ({
            openFiles: [...state.openFiles, newFile],
            activeFileId: fileId,
        }));

        // Fetch content from Gateway
        try {
            const url = buildFileUrl(agentId, workspaceId, relPath);
            const resp = await fetch(url);
            if (!resp.ok) {
                log.error("[FileEditorStore] read_file failed:", resp.status);
                // Remove the file on error
                set((state) => ({
                    openFiles: state.openFiles.filter((f) => f.id !== fileId),
                    activeFileId: state.activeFileId === fileId ? null : state.activeFileId,
                }));
                return;
            }
            const data = (await resp.json()) as { content: string; size: number; mimeType: string; modified?: string };
            set((state) => ({
                openFiles: state.openFiles.map((f) =>
                    f.id === fileId
                        ? { ...f, content: data.content, originalContent: data.content, loading: false, mimeType: data.mimeType,
                            // ADR-058: cache disk metadata for conflict checks
                            diskModified: data.modified, diskSize: data.size }
                        : f,
                ),
            }));
        } catch (e) {
            log.error("[FileEditorStore] openFile error:", e);
            set((state) => ({
                openFiles: state.openFiles.map((f) =>
                    f.id === fileId ? { ...f, loading: false } : f,
                ),
            }));
        }
    },

    openPreview: async (agentId: string, workspaceId: string, relPath: string) => {
        const fileId = `${agentId}:${workspaceId}:${relPath}`;
        const fileName = relPath.split("/").pop() || relPath;

        // If already open — just activate and switch to preview mode
        const existing = get().openFiles.find((f) => f.id === fileId);
        if (existing) {
            set({
                activeFileId: fileId,
                openFiles: get().openFiles.map((f) =>
                    f.id === fileId ? { ...f, mode: "preview" } : f,
                ),
            });
            return;
        }

        // Add a preview-mode placeholder
        const newFile: OpenFile = {
            id: fileId,
            agentId,
            workspaceId,
            relPath,
            fileName,
            content: "",
            originalContent: "",
            loading: true,
            saving: false,
            language: "markdown",
            dirty: false,
            mode: "preview",
            kind: "file",
        };

        set((state) => ({
            openFiles: [...state.openFiles, newFile],
            activeFileId: fileId,
        }));

        // Fetch content from Gateway
        try {
            const url = buildFileUrl(agentId, workspaceId, relPath);
            const resp = await fetch(url);
            if (!resp.ok) {
                log.error("[FileEditorStore] openPreview read failed:", resp.status);
                set((state) => ({
                    openFiles: state.openFiles.filter((f) => f.id !== fileId),
                    activeFileId: state.activeFileId === fileId ? null : state.activeFileId,
                }));
                return;
            }
            const data = (await resp.json()) as { content: string; size: number; mimeType: string; modified?: string };
            set((state) => ({
                openFiles: state.openFiles.map((f) =>
                    f.id === fileId
                        ? { ...f, content: data.content, originalContent: data.content, loading: false, mimeType: data.mimeType,
                            // ADR-058: cache disk metadata for conflict checks
                            diskModified: data.modified, diskSize: data.size }
                        : f,
                ),
            }));
        } catch (e) {
            log.error("[FileEditorStore] openPreview error:", e);
            set((state) => ({
                openFiles: state.openFiles.map((f) =>
                    f.id === fileId ? { ...f, loading: false } : f,
                ),
            }));
        }
    },

    openFileWithContent: (agentId: string, workspaceId: string, relPath: string, content: string, language: string) => {
        const fileId = `${agentId}:${workspaceId}:${relPath}`;
        const existing = get().openFiles.find((f) => f.id === fileId);
        if (existing) {
            // Already open, just activate
            set({ activeFileId: fileId });
            return;
        }

        const fileName = relPath.split("/").pop() || relPath;
        const newFile: OpenFile = {
            id: fileId,
            agentId,
            workspaceId,
            relPath,
            fileName,
            content,
            originalContent: content,
            loading: false, // Already have content, no need to fetch
            saving: false,
            language,
            dirty: false,
            mode: "edit",
            kind: "file",
        };

        set((state) => ({
            openFiles: [...state.openFiles, newFile],
            activeFileId: fileId,
        }));
    },

    openGroupChatMd: (sessionId: string, content: string) => {
        // The agentId slot carries the session id so two groups never share a
        // tab key; the workspace + relPath are what `saveFile` matches on.
        get().openFileWithContent(
            sessionId,
            GROUP_CHAT_MD_WORKSPACE,
            `${GROUP_CHAT_MD_PREFIX}${sessionId}/chat.md`,
            content,
            "markdown",
        );
    },

    openAttachmentPreview: ({ chatId, attachmentId, fileName, content, language }) => {
        const fileId = `attachment:${chatId}:${attachmentId}`;
        if (get().openFiles.some((f) => f.id === fileId)) {
            // Already open, just activate — mirrors openFileWithContent.
            set({ activeFileId: fileId });
            return;
        }

        const newFile: OpenFile = {
            id: fileId,
            // Chat attachments are Gateway-owned blobs (ADR-076), not
            // workspace files: there is no agent, no workspace root and
            // nothing to write back to. Empty ids keep the shape happy and
            // every `kind === "file"` gate in FileEditorPanel (save,
            // refresh, LSP, tab context menu, fs-watch) skips the tab.
            agentId: "",
            workspaceId: "",
            // Same trick as the git virtual tabs: prefix the model path so
            // Monaco can never collide this model with a workspace file's,
            // while the tab title stays the attachment name. The id segment
            // keeps two same-named attachments in different chats distinct.
            relPath: `attachment:${chatId}/${attachmentId}/${fileName}`,
            fileName,
            content,
            originalContent: content,
            loading: false,
            saving: false,
            language,
            dirty: false,
            mode: "edit",
            kind: "attachment",
        };

        set((state) => ({
            openFiles: [...state.openFiles, newFile],
            activeFileId: fileId,
        }));
    },

    openVirtualFile: ({ agentId, workspaceId, kind, relPath, content, original, gitDiffKind, language, loadedCommits, displayedLimit, reachedEnd, diffBaseRef, diffHeadRef }) => {
        const fileId = `git:${agentId}:${workspaceId}:${kind}:${relPath}`;
        const existing = get().openFiles.find((f) => f.id === fileId);
        if (existing) {
            // Already open, just activate
            set({ activeFileId: fileId });
            return;
        }

        const fileName = relPath.split("/").pop() || relPath;
        // Monaco addresses the model by `relPath` — for virtual tabs we prefix
        // it with a URI-safe scheme (`diff:src/foo.ts` parses as scheme "diff")
        // so the model never collides with the real file's model. The human
        // readable prefix lives in the tab title too.
        const monacoRelPath = `${kind}:${relPath}`;
        const newFile: OpenFile = {
            id: fileId,
            agentId,
            workspaceId,
            relPath: monacoRelPath,
            fileName: kind === "diff" ? `diff: ${fileName}` : `log: ${fileName}`,
            content,
            originalContent: original ?? content,
            loading: false,
            saving: false,
            // Pagination seed for log tabs — caller (GitStatusPanel.openLog)
            // passes the exact `commits` array + limit it used to build
            // `content`, so GitVirtualNav's Prev / Next buttons can
            // navigate without a refetch on first open. Subsequent
            // navigations update both fields via setVirtualFileContent.
            loadedCommits: kind === "log" ? loadedCommits : undefined,
            displayedLimit: kind === "log" ? displayedLimit : undefined,
            reachedEnd: kind === "log" ? reachedEnd : undefined,
            language,
            dirty: false,
            mode: "edit",
            kind,
            gitDiffKind,
            diffBaseRef,
            diffHeadRef,
            // Diff tabs open in INLINE mode by default — single-pane +/-
            // colour blocks. User can flip to side-by-side via the
            // GitVirtualNav toggle (which writes back through
            // setDiffInlineMode). Log tabs ignore this field.
            diffInlineMode: kind === "diff" ? true : undefined,
        };

        set((state) => ({
            openFiles: [...state.openFiles, newFile],
            activeFileId: fileId,
        }));
    },

    closeFile: (fileId: string, force?: boolean) => {
        const file = get().openFiles.find((f) => f.id === fileId);
        if (!file) return true;
        if (file.dirty && !force) return false;

        // Release Monaco TextModel + LSP didClose BEFORE mutating store state.
        disposeFiles([file]);

        set((state) => {
            const nextFiles = state.openFiles.filter((f) => f.id !== fileId);
            let nextActive = state.activeFileId;
            if (state.activeFileId === fileId) {
                // Activate adjacent tab or null
                const idx = state.openFiles.findIndex((f) => f.id === fileId);
                nextActive = nextFiles.length > 0
                    ? nextFiles[Math.min(idx, nextFiles.length - 1)].id
                    : null;
            }
            return { openFiles: nextFiles, activeFileId: nextActive };
        });
        return true;
    },

    closeOthers: (keepFileId: string, force?: boolean) => {
        const state = get();
        if (!state.openFiles.some((f) => f.id === keepFileId)) return true;
        if (!force) {
            const hasDirty = state.openFiles.some(
                (f) => f.id !== keepFileId && f.dirty,
            );
            if (hasDirty) return false;
        }
        const others = state.openFiles.filter((f) => f.id !== keepFileId);
        // Release Monaco TextModels for every non-kept tab BEFORE the store mutation.
        disposeFiles(others);
        set({
            openFiles: state.openFiles.filter((f) => f.id === keepFileId),
            // Keep the requested tab active; if it wasn't the active one,
            // promote it so the surviving single tab is clearly focused.
            activeFileId: keepFileId,
        });
        return true;
    },

    setActiveFile: (fileId: string) => {
        set({ activeFileId: fileId });
    },

    updateContent: (fileId: string, content: string) => {
        set((state) => ({
            openFiles: state.openFiles.map((f) =>
                f.id === fileId
                    ? {
                        ...f,
                        content,
                        dirty: content !== f.originalContent,
                        // Clear stale save error — user is editing to recover.
                        saveError: undefined,
                    }
                    : f,
            ),
        }));
    },

    setVirtualFileContent: (fileId: string, content: string, extras) => {
        set((state) => ({
            openFiles: state.openFiles.map((f) => {
                if (f.id !== fileId) return f;
                // Only virtual tabs may be mutated this way — callers
                // shouldn't reach into a real file's content through here.
                if (f.kind !== "diff" && f.kind !== "log") return f;
                return {
                    ...f,
                    content,
                    // Keep originalContent in lockstep so `dirty` stays
                    // false (matches openVirtualFile's seed semantics —
                    // virtual tabs are never dirty).
                    originalContent: content,
                    // Log pagination state — `loadedCommits` is the
                    // ever-growing cache, `displayedLimit` is the
                    // current window. Backward navigation only shrinks
                    // `displayedLimit`, never `loadedCommits`, so the
                    // user can re-flip forward without a refetch.
                    // `reachedEnd` is set when the server returned fewer
                    // commits than asked for — Next button then
                    // disables itself without losing the cache.
                    loadedCommits: extras?.loadedCommits ?? f.loadedCommits,
                    displayedLimit: extras?.displayedLimit ?? f.displayedLimit,
                    reachedEnd: extras?.reachedEnd ?? f.reachedEnd,
                };
            }),
        }));
    },

    updateDiffRefs: (fileId, baseRef, headRef) => {
        set((state) => ({
            openFiles: state.openFiles.map((f) =>
                f.id === fileId
                    ? {
                          ...f,
                          diffBaseRef: baseRef ?? f.diffBaseRef,
                          diffHeadRef: headRef ?? f.diffHeadRef,
                      }
                    : f,
            ),
        }));
    },

    /** Flip a diff tab between inline (default) and side-by-side.
     *  No-op for non-diff tabs — keeps the toggle from accidentally
     *  marking a real file or log tab as "diff mode set". */
    setDiffInlineMode: (fileId, inline) => {
        set((state) => ({
            openFiles: state.openFiles.map((f) =>
                f.id === fileId && f.kind === "diff"
                    ? { ...f, diffInlineMode: inline }
                    : f,
            ),
        }));
    },

    saveFile: async (fileId: string) => {
        const file = get().openFiles.find((f) => f.id === fileId);
        // ADR-078: virtual diff/log tabs are never saved (no disk target).
        if (!file || file.saving || file.kind !== "file") return;

        set((state) => ({
            openFiles: state.openFiles.map((f) =>
                f.id === fileId ? { ...f, saving: true, saveError: undefined } : f,
            ),
        }));

        try {
            // Route by file identity. A group `chat.md` and the shell risk
            // rules file both live outside any workspace, so neither can go
            // through the generic `/workspaces/file` write.
            const chatMdSession = isGroupChatMd(file.workspaceId)
                ? groupChatMdSessionId(file.relPath)
                : null;
            if (isGroupChatMd(file.workspaceId) && !chatMdSession) {
                // Sentinel workspace but a path we cannot parse — bail rather
                // than PUT to a wrong session.
                set((state) => ({
                    openFiles: state.openFiles.map((f) =>
                        f.id === fileId
                            ? { ...f, saving: false, saveError: "malformed group chat.md path" }
                            : f,
                    ),
                }));
                return;
            }
            const isRiskRules = isShellRiskRulesFile(file.workspaceId, file.relPath);
            const url = chatMdSession
                ? buildGroupChatMdUrl(chatMdSession)
                : isRiskRules
                    ? buildShellRiskRulesUrl(file.agentId)
                    : buildFileUrl(file.agentId, file.workspaceId, file.relPath);
            // The user-service chat-prompt route authenticates from the bearer
            // the Gateway forwards; workspace / agent saves ride the trusted
            // localhost surface and carry no token.
            const headers: Record<string, string> = { "Content-Type": "application/json" };
            if (chatMdSession) {
                const token = useAuthStore.getState().accessToken;
                if (token) headers.Authorization = `Bearer ${token}`;
            }
            const resp = await fetch(url, {
                method: "PUT",
                headers,
                body: JSON.stringify({ content: file.content }),
            });
            if (!resp.ok) {
                // Surface the error body so the editor toast can show a
                // useful message (e.g. TOML parse error from Runtime).
                const errText = await resp.text().catch(() => "");
                const saveError = errText || `HTTP ${resp.status}`;
                log.error("[FileEditorStore] saveFile failed:", resp.status, errText);
                set((state) => ({
                    openFiles: state.openFiles.map((f) =>
                        f.id === fileId ? { ...f, saving: false, saveError } : f,
                    ),
                }));
                return;
            }
            // ADR-058: parse the post-write disk metadata echoed by the
            // Runtime (modified RFC3339 + size) and stamp lastSavedAtMs
            // (Desktop local clock) for echo suppression in the
            // workspace fs-changed listener.
            const saved = (await resp.json().catch(() => ({}))) as {
                modified?: string;
                size?: number;
            };
            set((state) => ({
                openFiles: state.openFiles.map((f) =>
                    f.id === fileId
                        ? {
                            ...f,
                            saving: false,
                            originalContent: f.content,
                            dirty: false,
                            saveError: undefined,
                            ...(saved.modified !== undefined ? { diskModified: saved.modified } : {}),
                            ...(saved.size !== undefined ? { diskSize: saved.size } : {}),
                            lastSavedAtMs: Date.now(),
                            diskConflict: undefined,
                            diskDeleted: undefined,
                        }
                        : f,
                ),
            }));
        } catch (e) {
            log.error("[FileEditorStore] saveFile error:", e);
            set((state) => ({
                openFiles: state.openFiles.map((f) =>
                    f.id === fileId ? { ...f, saving: false, saveError: String(e) } : f,
                ),
            }));
        }
    },

    refreshFile: async (fileId: string, opts?: { skipIfDirty?: boolean }) => {
        const file = get().openFiles.find((f) => f.id === fileId);
        if (!file) return;
        // Only workspace files have a Gateway file endpoint to refresh against.
        if (file.kind !== "file") return;
        // A group `chat.md` has no workspace fs source; the settings panel owns
        // its freshness (a fresh GET on each open), so a disk refresh is a
        // no-op here rather than a 404 against `/workspaces/file`.
        if (isGroupChatMd(file.workspaceId)) return;
        // Skip if a load is already running (avoids interleaved responses
        // clobbering the in-flight state).
        if (file.loading) return;

        set((state) => ({
            openFiles: state.openFiles.map((f) =>
                f.id === fileId ? { ...f, loading: true } : f,
            ),
        }));

        try {
            const url = buildFileUrl(file.agentId, file.workspaceId, file.relPath);
            const resp = await fetch(url);
            if (!resp.ok) {
                log.error("[FileEditorStore] refreshFile failed:", resp.status);
                set((state) => ({
                    openFiles: state.openFiles.map((f) =>
                        f.id === fileId ? { ...f, loading: false } : f,
                    ),
                }));
                return;
            }
            const data = (await resp.json()) as { content: string; size: number; mimeType: string; modified?: string };
            set((state) => ({
                openFiles: state.openFiles.map((f) => {
                    if (f.id !== fileId) return f;
                    // ADR-058 (review M-3): a reload triggered by an fs
                    // event must never clobber edits the user started
                    // while the fetch was in flight — skip instead of
                    // overwriting when the tab turned dirty meanwhile.
                    if (opts?.skipIfDirty && f.dirty) return { ...f, loading: false };
                    return {
                        ...f,
                        content: data.content,
                        originalContent: data.content,
                        mimeType: data.mimeType,
                        loading: false,
                        // Resetting both content and originalContent means dirty must be false.
                        dirty: false,
                        // ADR-058: refresh resolves any pending disk
                        // conflict (the reload is now authoritative)
                        // and re-caches disk metadata.
                        diskModified: data.modified,
                        diskSize: data.size,
                        diskConflict: undefined,
                        diskDeleted: undefined,
                    };
                }),
            }));
        } catch (e) {
            log.error("[FileEditorStore] refreshFile error:", e);
            set((state) => ({
                openFiles: state.openFiles.map((f) =>
                    f.id === fileId ? { ...f, loading: false } : f,
                ),
            }));
        }
    },

    clearDiskConflict: (fileId: string) => {
        set((state) => ({
            openFiles: state.openFiles.map((f) =>
                f.id === fileId ? { ...f, diskConflict: undefined, diskDeleted: undefined } : f,
            ),
        }));
    },

    closeAllFiles: (force?: boolean) => {
        const state = get();
        if (!force && state.openFiles.some((f) => f.dirty)) return false;
        // Release Monaco TextModels for every open tab BEFORE the store mutation.
        disposeFiles(state.openFiles);
        set({ openFiles: [], activeFileId: null });
        return true;
    },

    openUrl: (agentId: string, url: string) => {
        const normalized = normalizeUrl(url);
        if (!normalized) {
            log.warn("[FileEditorStore] openUrl — skipping invalid URL:", url);
            return;
        }

        // Use the URL itself as a unique file ID (scoped to agent)
        const fileId = `${agentId}:url:${normalized}`;
        const existing = get().openFiles.find((f) => f.id === fileId);
        if (existing) {
            // Already open — just activate it
            set({ activeFileId: fileId });
            return;
        }

        const fileName = new URL(normalized).hostname; // e.g. "example.com"
        const newFile: OpenFile = {
            id: fileId,
            agentId,
            workspaceId: "",
            relPath: normalized,
            fileName,
            content: "",
            originalContent: "",
            loading: false,
            saving: false,
            language: "plaintext",
            dirty: false,
            mode: "preview",
            kind: "url",
            url: normalized,
        };

        set((state) => ({
            openFiles: [...state.openFiles, newFile],
            activeFileId: fileId,
        }));
    },
}));

/**
 * Real workspace-relative path for an OpenFile.
 *
 * Virtual tabs (`kind === "diff" | "log" | "attachment"`) prefix `relPath`
 * with the kind for monaco URI disambiguation (see `openVirtualFile` /
 * `openAttachmentPreview`), so consumers that need the user-facing path —
 * FileTree locate, banner display, tab tooltip — must strip the prefix via
 * this helper. For `file` / `url` tabs `relPath` is already the workspace
 * path.
 */
export function sourceRelPath(file: Pick<OpenFile, "kind" | "relPath">): string {
    if (file.kind === "diff" || file.kind === "log" || file.kind === "attachment") {
        return file.relPath.replace(/^(diff|log|attachment):/, "");
    }
    return file.relPath;
}
