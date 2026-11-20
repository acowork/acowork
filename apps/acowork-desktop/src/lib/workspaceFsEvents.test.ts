/**
 * ADR-058 W5 frontend tests.
 *
 * Covers the workspace fs-changed handling wired into the stores:
 * 1. Per-parent-path incremental tree refresh (only cached dirs fetched).
 * 2. Editor conflict UX — dirty / clean / deleted paths + echo suppression
 *    + touch-metadata re-check before prompting a dirty conflict.
 * 3. Reconnect full-sync fallback (invalidate + root re-fetch).
 * 4. Wake-transition detection (review H-1: offline → online must sync).
 * 5. skipIfDirty reload guard (review M-3: in-flight edits never clobbered).
 */

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import {
    handleFsChanged,
    handleGitStatusChanged,
    isWakeTransition,
    scheduleFullTreeSync,
    FS_REFRESH_DEBOUNCE_MS,
    type WorkspaceFsChangeEvent,
} from "./workspaceFsEvents";
import { useWorkspaceStore } from "../stores/workspaceStore";
import { useFileTreeStore, treeKey, __seedTreeNode } from "../stores/fileTree";
import { useFileEditorStore, type OpenFile } from "../stores/fileEditorStore";
import { useAgentStore } from "../stores/agentStore";
import { useChatStore } from "../stores/chatStore";
import { resetVisualCanvasAutoOpenForTests } from "../components/editor/visualCanvas";

// ── Mocks ────────────────────────────────────────────────────────────────

/** URLs captured from the mocked global fetch. */
let fetchUrls: string[] = [];

function mockFetchOk(body: Record<string, unknown>) {
    return {
        ok: true,
        status: 200,
        json: () => Promise.resolve({ ...body }),
    } as Response;
}

function stubFetch() {
    fetchUrls = [];
    vi.stubGlobal(
        "fetch",
        vi.fn((url: string | URL) => {
            fetchUrls.push(String(url));
            // Default: tree response with two entries.
            return Promise.resolve(
                mockFetchOk({
                    root: "/tmp/ws",
                    path: "",
                    entries: [
                        { name: "a.txt", type: "file" },
                        { name: "sub", type: "directory" },
                    ],
                }),
            );
        }),
    );
}

function makeFile(overrides: Partial<OpenFile>): OpenFile {
    return {
        id: "agent-1:ws-1:notes.md",
        agentId: "agent-1",
        workspaceId: "ws-1",
        relPath: "notes.md",
        fileName: "notes.md",
        content: "hello",
        originalContent: "hello",
        loading: false,
        saving: false,
        language: "markdown",
        dirty: false,
        mode: "edit",
        kind: "file",
        ...overrides,
    };
}

function setFiles(files: OpenFile[]) {
    useFileEditorStore.setState({ openFiles: files, activeFileId: files[0]?.id ?? null });
}

function fsEvent(changes: Array<{ kind: string; path: string }>): WorkspaceFsChangeEvent {
    return {
        instance_id: "agent-1",
        workspace_id: "ws-1",
        changes: changes.map((c) => ({ ...c, timestamp_ms: Date.now() } as never)),
        window_end_ms: Date.now(),
    } as WorkspaceFsChangeEvent;
}

// ── Tests ────────────────────────────────────────────────────────────────

/**
 * Wait for the per-parent refresh debounce window to elapse so
 * debounced tree refreshes actually fire (real timers — see
 * FS_REFRESH_DEBOUNCE_MS).
 */
async function flushRefreshDebounce(): Promise<void> {
    await new Promise((r) => setTimeout(r, FS_REFRESH_DEBOUNCE_MS + 30));
}

beforeEach(() => {
    stubFetch();
    // Reset stores to a known state.
    useWorkspaceStore.setState({});
    useFileTreeStore.setState({ nodes: {} });
    setFiles([]);
});

/**
 * Seed the tree cache as if a tree fetch had already resolved. The fs
 * event listener only re-fetches parents that are already in the
 * cache, so test fixtures need to populate the cache directly. We
 * install a `kind:"ready"` node so the listener's `isReadyNode`
 * check passes — this writes through `__seedTreeNode` so the cache
 * (single source of truth) and the Zustand mirror agree.
 *
 * `fetchedAt` is set to the epoch so the cache treats the entry as
 * past `staleMs` and re-fetches on the next call (the listener's
 * purpose for re-fetching is precisely to invalidate stale data after
 * an fs change).
 */
function seedTreeNode(agentId: string, workspaceId: string, relPath: string, root: string) {
    __seedTreeNode(treeKey(agentId, workspaceId, relPath), {
        kind: "ready",
        entries: [],
        root,
        fetchedAt: 0,
    });
}

describe("ADR-058: per-parent-path incremental tree refresh", () => {
    it("re-fetches only parents present in the cache", async () => {
        // Seed: root and sub/ cached; other/ NOT cached.
        seedTreeNode("agent-1", "ws-1", "", "/tmp/ws");
        seedTreeNode("agent-1", "ws-1", "sub", "/tmp/ws");

        await handleFsChanged(
            fsEvent([
                { kind: "created", path: "sub/new-file.txt" },
                { kind: "modified", path: "other/uncached.txt" },
            ]),
        );
        await flushRefreshDebounce();

        const treeFetches = fetchUrls.filter((u) => u.includes("/workspaces/tree"));
        // sub (cached) is fetched; other (uncached) is NOT; root ("") is
        // NOT fetched because the change's parent resolution for
        // "sub/new-file.txt" is "sub" only (a root fetch would have NO
        // query string — treeClient omits the empty `path` param).
        expect(treeFetches.some((u) => u.includes("path=sub"))).toBe(true);
        expect(treeFetches.some((u) => u.includes("path=other"))).toBe(false);
        expect(treeFetches.some((u) => !u.includes("?"))).toBe(false);
    });

    it("re-fetches the root when a top-level file changes", async () => {
        seedTreeNode("agent-1", "ws-1", "", "/tmp/ws");

        await handleFsChanged(fsEvent([{ kind: "deleted", path: "top-level.txt" }]));
        await flushRefreshDebounce();

        const treeFetches = fetchUrls.filter((u) => u.includes("/workspaces/tree"));
        // Root fetch: no path param (path="").
        expect(treeFetches.length).toBe(1);
        expect(treeFetches[0].includes("path=")).toBe(false);
    });

    it("coalesces a burst of fs-changed events for the same parent into ONE refresh", async () => {
        seedTreeNode("agent-1", "ws-1", "", "/tmp/ws");
        seedTreeNode("agent-1", "ws-1", "sub", "/tmp/ws");

        // A bulk operation (e.g. paste of 10 files into sub/) arrives as
        // several events in quick succession. Each would previously
        // abort+refetch `sub`; the debounce must coalesce them.
        await handleFsChanged(fsEvent([{ kind: "created", path: "sub/f1.txt" }]));
        await new Promise((r) => setTimeout(r, 100)); // inside debounce window
        await handleFsChanged(fsEvent([{ kind: "created", path: "sub/f2.txt" }]));
        await new Promise((r) => setTimeout(r, 100)); // still inside window
        await handleFsChanged(fsEvent([{ kind: "created", path: "sub/f3.txt" }]));
        await flushRefreshDebounce();

        const subFetches = fetchUrls.filter((u) => u.includes("/workspaces/tree") && u.includes("path=sub"));
        // Exactly ONE refresh for sub despite three events.
        expect(subFetches.length).toBe(1);
    });

    it("refreshes different parents independently within a burst", async () => {
        seedTreeNode("agent-1", "ws-1", "", "/tmp/ws");
        seedTreeNode("agent-1", "ws-1", "a", "/tmp/ws");
        seedTreeNode("agent-1", "ws-1", "b", "/tmp/ws");

        await handleFsChanged(fsEvent([{ kind: "created", path: "a/f1.txt" }]));
        await new Promise((r) => setTimeout(r, 100));
        await handleFsChanged(fsEvent([{ kind: "created", path: "b/f1.txt" }]));
        await flushRefreshDebounce();

        const treeFetches = fetchUrls.filter((u) => u.includes("/workspaces/tree"));
        expect(treeFetches.filter((u) => u.includes("path=a")).length).toBe(1);
        expect(treeFetches.filter((u) => u.includes("path=b")).length).toBe(1);
    });
});

describe("ADR-058: editor conflict UX", () => {
    it("clean file modified on disk → silent reload", async () => {
        setFiles([makeFile({})]);
        await handleFsChanged(fsEvent([{ kind: "modified", path: "notes.md" }]));
        // refreshFile re-read the file content.
        expect(fetchUrls.some((u) => u.includes("/workspaces/file?"))).toBe(true);
        expect(useFileEditorStore.getState().openFiles[0].dirty).toBe(false);
    });

    it("echo of our own save is suppressed (lastSavedAtMs window)", async () => {
        setFiles([makeFile({ lastSavedAtMs: Date.now() })]);
        await handleFsChanged(fsEvent([{ kind: "modified", path: "notes.md" }]));
        // No file re-read, no conflict marker.
        expect(fetchUrls.filter((u) => u.includes("/workspaces/file?")).length).toBe(0);
        expect(useFileEditorStore.getState().openFiles[0].diskConflict).toBeUndefined();
    });

    it("dirty file: pure metadata change (modified+size unchanged) → no toast, baseline adopted", async () => {
        setFiles([makeFile({ dirty: true, diskModified: "2026-01-01T00:00:00+00:00", diskSize: 5 })]);
        // statDiskFile re-check fetch returns the SAME modified+size.
        vi.stubGlobal(
            "fetch",
            vi.fn(() =>
                Promise.resolve(
                    mockFetchOk({
                        content: "hello",
                        size: 5,
                        mimeType: "text/markdown",
                        modified: "2026-01-01T00:00:00+00:00",
                    }),
                ),
            ),
        );
        const dispatchSpy = vi.spyOn(window, "dispatchEvent");

        await handleFsChanged(fsEvent([{ kind: "modified", path: "notes.md" }]));

        expect(dispatchSpy).not.toHaveBeenCalledWith(expect.objectContaining({ type: "acowork:toast" }));
        expect(useFileEditorStore.getState().openFiles[0].diskConflict).toBeUndefined();
        dispatchSpy.mockRestore();
    });

    it("dirty file: real external write → conflict marker + toast with Reload action", async () => {
        setFiles([makeFile({ dirty: true, diskModified: "2026-01-01T00:00:00+00:00", diskSize: 5 })]);
        // statDiskFile re-check fetch returns CHANGED modified+size.
        vi.stubGlobal(
            "fetch",
            vi.fn(() =>
                Promise.resolve(
                    mockFetchOk({
                        content: "hello world",
                        size: 11,
                        mimeType: "text/markdown",
                        modified: "2026-02-02T00:00:00+00:00",
                    }),
                ),
            ),
        );
        const dispatchSpy = vi.spyOn(window, "dispatchEvent");

        await handleFsChanged(fsEvent([{ kind: "modified", path: "notes.md" }]));

        expect(useFileEditorStore.getState().openFiles[0].diskConflict).toBe("modified");
        const toastCalls = dispatchSpy.mock.calls.filter(
            (args) => (args[0] as Event).type === "acowork:toast",
        );
        expect(toastCalls.length).toBe(1);
        const detail = (toastCalls[0][0] as CustomEvent).detail as { action?: { label: string } };
        expect(detail.action?.label).toBe("Reload");
        dispatchSpy.mockRestore();
    });

    it("clean file deleted → tab closed", async () => {
        setFiles([makeFile({})]);
        const dispatchSpy = vi.spyOn(window, "dispatchEvent");

        await handleFsChanged(fsEvent([{ kind: "deleted", path: "notes.md" }]));

        expect(useFileEditorStore.getState().openFiles.length).toBe(0);
        const toastCalls = dispatchSpy.mock.calls.filter(
            (args) => (args[0] as Event).type === "acowork:toast",
        );
        expect(toastCalls.length).toBe(1);
        dispatchSpy.mockRestore();
    });

    it("dirty file deleted → tab kept, diskConflict = deleted", async () => {
        setFiles([makeFile({ dirty: true })]);
        const dispatchSpy = vi.spyOn(window, "dispatchEvent");

        await handleFsChanged(fsEvent([{ kind: "deleted", path: "notes.md" }]));

        expect(useFileEditorStore.getState().openFiles.length).toBe(1);
        expect(useFileEditorStore.getState().openFiles[0].diskConflict).toBe("deleted");
        expect(useFileEditorStore.getState().openFiles[0].diskDeleted).toBe(true);
        dispatchSpy.mockRestore();
    });
});

describe("ADR-058: reconnect / wake full-sync fallback", () => {
    it("invalidates the tree cache and re-fetches every cached workspace root", async () => {
        seedTreeNode("agent-1", "ws-1", "", "/tmp/ws");
        seedTreeNode("agent-1", "ws-1", "sub", "/tmp/ws");
        seedTreeNode("agent-2", "ws-2", "", "/tmp/other");

        scheduleFullTreeSync("test");

        // The two agents whose entries we seeded both get their root
        // re-fetched. We don't assert the post-sync `nodes` shape
        // here because `scheduleFullTreeSync` is fire-and-forget on
        // the fetch — by the time the assertion would run, the
        // triggered fetches have already transitioned the entries
        // through `loading` (which the mirror keeps) toward `ready`.
        // The behavioural contract under test is "all cached roots
        // are re-fetched", verified by the URL capture.
        const treeFetches = fetchUrls.filter((u) => u.includes("/workspaces/tree"));
        expect(treeFetches.some((u) => u.includes("workspace_id=ws-1"))).toBe(true);
        expect(treeFetches.some((u) => u.includes("workspace_id=ws-2"))).toBe(true);
        // ws-2 must have invalidated too (otherwise it would still be
        // in the cache as `ready`).
        await new Promise((r) => setTimeout(r, 5));
        expect(Object.keys(useFileTreeStore.getState().nodes).length).toBeLessThan(3);
    });

    it("dedupes syncs fired within the debounce window", async () => {
        seedTreeNode("agent-1", "ws-1", "", "/tmp/ws");
        scheduleFullTreeSync("first");
        const fetchesAfterFirst = fetchUrls.filter((u) => u.includes("/workspaces/tree")).length;
        scheduleFullTreeSync("second-within-window");
        const fetchesAfterSecond = fetchUrls.filter((u) => u.includes("/workspaces/tree")).length;
        expect(fetchesAfterSecond).toBe(fetchesAfterFirst);
    });
});

// ── ADR-058 review H-1: wake-transition detection ────────────────────────
//
// Auto-sleep was retired in Sept 2026 — `AgentStatusSnapshot.sleeping`
// is gone. A wake is now simply `offline → online` (e.g. crash + restart).

describe("ADR-058 review H-1: isWakeTransition", () => {
    it("cold start (retained online, no previous state) is not a wake", () => {
        expect(isWakeTransition(undefined, { online: true })).toBe(false);
    });

    it("online → online (status re-publish) is not a wake", () => {
        expect(isWakeTransition({ online: true }, { online: true })).toBe(false);
    });

    it("offline → online (Will-message path) is a wake", () => {
        expect(isWakeTransition({ online: false }, { online: true })).toBe(true);
    });
});

// ── Review M-3: skipIfDirty reload guard ──────────────────────────────────

describe("ADR-058 review M-3: fs-triggered reload never clobbers in-flight edits", () => {
    const FILE_ID = "agent-1:ws-1:notes.md";

    it("skips the reload when the tab turned dirty while the fetch was in flight", async () => {
        setFiles([makeFile({})]);

        let resolveFetch!: (r: Response) => void;
        vi.stubGlobal(
            "fetch",
            vi.fn(
                () =>
                    new Promise<Response>((resolve) => {
                        resolveFetch = resolve;
                    }),
            ),
        );

        const refresh = useFileEditorStore.getState().refreshFile(FILE_ID, { skipIfDirty: true });
        // User starts typing while the fetch is in flight.
        useFileEditorStore.getState().updateContent(FILE_ID, "user typing");
        resolveFetch(
            mockFetchOk({
                content: "disk version",
                size: 12,
                mimeType: "text/markdown",
            }) as Response,
        );
        await refresh;

        const f = useFileEditorStore.getState().openFiles[0];
        expect(f.content).toBe("user typing"); // edits preserved
        expect(f.dirty).toBe(true);
        expect(f.loading).toBe(false); // loading flag still cleaned up
    });

    it("manual refreshFile (no opts) still overwrites — explicit user intent", async () => {
        setFiles([makeFile({ dirty: true, content: "my draft" })]);

        let resolveFetch!: (r: Response) => void;
        vi.stubGlobal(
            "fetch",
            vi.fn(
                () =>
                    new Promise<Response>((resolve) => {
                        resolveFetch = resolve;
                    }),
            ),
        );

        const refresh = useFileEditorStore.getState().refreshFile(FILE_ID);
        resolveFetch(
            mockFetchOk({
                content: "disk version",
                size: 12,
                mimeType: "text/markdown",
            }) as Response,
        );
        await refresh;

        const f = useFileEditorStore.getState().openFiles[0];
        expect(f.content).toBe("disk version"); // explicit reload wins
        expect(f.dirty).toBe(false);
    });
});

// ── ADR-078: git status refresh wiring ───────────────────────────────────

import { useGitStore, gitGroupKey } from "../stores/gitStore";

describe("ADR-078: fs-changed → git status refresh wiring", () => {
    beforeEach(() => {
        useGitStore.setState({
            expandedKey: null,
            status: {},
            _inflight: {},
            _fsTimer: {},
        });
    });

    it("debounced-refreshes status when the event hits the expanded group", async () => {
        useGitStore.setState({ expandedKey: gitGroupKey("agent-1", "ws-1") });
        await handleFsChanged(
            fsEvent([{ kind: "modified", path: "src/a.ts" }]),
        );
        // No immediate refresh — it is debounced.
        expect(fetchUrls.some((u) => u.includes("/git/status"))).toBe(false);
        await flushRefreshDebounce();
        expect(fetchUrls.some((u) => u.includes("/git/status"))).toBe(true);
    });

    it("coalesces a burst of fs-changed events into ONE status refresh", async () => {
        useGitStore.setState({ expandedKey: gitGroupKey("agent-1", "ws-1") });
        await handleFsChanged(
            fsEvent([{ kind: "modified", path: "src/a.ts" }]),
        );
        await handleFsChanged(
            fsEvent([{ kind: "modified", path: "src/b.ts" }]),
        );
        await flushRefreshDebounce();

        const statusFetches = fetchUrls.filter((u) =>
            u.includes("/git/status"),
        );
        expect(statusFetches).toHaveLength(1);
    });

    it("ignores fs-changed events while the panel is collapsed", async () => {
        // expandedKey is null (collapsed) — no refresh triggered.
        await handleFsChanged(
            fsEvent([{ kind: "modified", path: "src/a.ts" }]),
        );
        await flushRefreshDebounce();
        expect(fetchUrls.some((u) => u.includes("/git/status"))).toBe(false);
    });

    it("ignores events for a different workspace than the expanded group", async () => {
        useGitStore.setState({ expandedKey: gitGroupKey("agent-1", "ws-2") });
        await handleFsChanged(
            fsEvent([{ kind: "modified", path: "src/a.ts" }]),
        );
        await flushRefreshDebounce();
        expect(fetchUrls.some((u) => u.includes("/git/status"))).toBe(false);
    });
});

describe("ADR-078 follow-up: git-changed nudge → git status refresh", () => {
    beforeEach(() => {
        useGitStore.setState({
            expandedKey: null,
            status: {},
            _inflight: {},
            _fsTimer: {},
        });
    });

    it("debounced-refreshes status on the expanded group", async () => {
        useGitStore.setState({ expandedKey: gitGroupKey("agent-1", "ws-1") });
        handleGitStatusChanged({
            instance_id: "agent-1",
            workspace_id: "ws-1",
            window_end_ms: Date.now(),
        });
        // Debounced, like the fs path.
        expect(fetchUrls.some((u) => u.includes("/git/status"))).toBe(false);
        // Drain the pending timer: the store's debounce lives outside
        // Zustand's state, so a timer left running here would fire
        // during a later test and be counted against it.
        await flushRefreshDebounce();
    });

    it("coalesces a burst of nudges into ONE status refresh", async () => {
        useGitStore.setState({ expandedKey: gitGroupKey("agent-1", "ws-1") });
        for (let i = 0; i < 3; i++) {
            handleGitStatusChanged({
                instance_id: "agent-1",
                workspace_id: "ws-1",
                window_end_ms: Date.now(),
            });
        }
        await flushRefreshDebounce();
        expect(
            fetchUrls.filter((u) => u.includes("/git/status")),
        ).toHaveLength(1);
    });

    // The nudge carries no git state; the refresh re-reads GET /git/status,
    // which is the authoritative source. Guards the regression where
    // someone "helpfully" threads the proto's agent_id in instead of the
    // topic-derived instance_id (ADR-073) — that addresses a group that
    // never matches, and the banner silently stops refreshing.
    it("addresses the group by topic instance_id, not the proto agent_id", async () => {
        useGitStore.setState({ expandedKey: gitGroupKey("agent-1", "ws-1") });
        handleGitStatusChanged({
            instance_id: "agent-1",
            workspace_id: "ws-1",
            window_end_ms: Date.now(),
        });
        await flushRefreshDebounce();
        const statusFetches = fetchUrls.filter((u) => u.includes("/git/status"));
        expect(statusFetches).toHaveLength(1);
        expect(statusFetches[0]).toContain("agent-1");
    });

    it("ignores nudges while the panel is collapsed", async () => {
        handleGitStatusChanged({
            instance_id: "agent-1",
            workspace_id: "ws-1",
            window_end_ms: Date.now(),
        });
        await flushRefreshDebounce();
        expect(fetchUrls.some((u) => u.includes("/git/status"))).toBe(false);
    });

    it("ignores nudges for a different workspace than the expanded group", async () => {
        useGitStore.setState({ expandedKey: gitGroupKey("agent-1", "ws-2") });
        handleGitStatusChanged({
            instance_id: "agent-1",
            workspace_id: "ws-1",
            window_end_ms: Date.now(),
        });
        await flushRefreshDebounce();
        expect(fetchUrls.some((u) => u.includes("/git/status"))).toBe(false);
    });
});

describe("ADR-118: auto-open visual canvas preview", () => {
    const realOpenPreview = useFileEditorStore.getState().openPreview;

    beforeEach(() => {
        resetVisualCanvasAutoOpenForTests();
        useAgentStore.setState({ selectedAgentId: "agent-1" });
        useChatStore.setState({
            agentStates: {
                "agent-1": {
                    sessionStates: {},
                    activeSessionId: "sess-1",
                    openSessionIds: ["sess-1"],
                    lastLoadedSessionId: null,
                    isSessionInitLoading: false,
                    preferredModel: null,
                    preferredProvider: null,
                    llmAvailability: "unspecified",
                    tabsRestoredFromRuntime: false,
                },
            },
        } as never);
        useWorkspaceStore.setState({ sessionWorkspaceMap: { "sess-1": "ws-1" } });
        useFileEditorStore.setState({ openPreview: vi.fn().mockResolvedValue(undefined) });
    });

    afterEach(() => {
        resetVisualCanvasAutoOpenForTests();
        useAgentStore.setState({ selectedAgentId: null });
        useFileEditorStore.setState({ openPreview: realOpenPreview });
    });

    it("opens preview once for a created .figcraft in the active workspace", async () => {
        await handleFsChanged(fsEvent([{ kind: "created", path: "designs/home.figcraft" }]));
        const openPreview = useFileEditorStore.getState().openPreview;
        expect(openPreview).toHaveBeenCalledTimes(1);
        expect(openPreview).toHaveBeenCalledWith("agent-1", "ws-1", "designs/home.figcraft");

        await handleFsChanged(fsEvent([{ kind: "modified", path: "designs/home.figcraft" }]));
        expect(openPreview).toHaveBeenCalledTimes(1);
    });

    it("ignores figcraft writes in another workspace or agent", async () => {
        useWorkspaceStore.setState({ sessionWorkspaceMap: { "sess-1": "ws-other" } });
        await handleFsChanged(fsEvent([{ kind: "created", path: "home.figcraft" }]));
        expect(useFileEditorStore.getState().openPreview).not.toHaveBeenCalled();
    });
});
