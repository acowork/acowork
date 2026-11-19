/**
 * ADR-058 follow-up: demand-driven workspace fs-watch set (Desktop frontend).
 *
 * The Runtime's fs-watcher is now purely demand-driven: it only watches the
 * paths the frontend is actually showing (ADR-058 §3.6+). This module derives
 * that "visible set" from the frontend state and pushes it to the Runtime via
 * `PUT /api/agents/{id}/workspaces/{ws_id}/fs-watch` (full-replace semantics:
 * the Runtime diffs against its current set).
 *
 * What counts as "visible" (strictly mirrors the UI):
 *   1. Every open editor tab (`kind === "file"`) — a file stays watched even
 *      when the workspace panel is closed / another tab is active. This is the
 *      "file open, tree closed → file keeps watching" rule.
 *   2. When the right-side workspace panel is visible — `workspacePanelVisible`
 *      in `components/layout/rightPanelGate.ts`, which counts the pinned panel
 *      AND the hover drawer that previews it while the layout stays collapsed:
 *      the root of the current agent's current session workspace (""), plus
 *      every directory currently expanded in the file tree (per-session
 *      `treeExpandedPaths`).
 *
 * The module is framework-agnostic: it subscribes to the relevant Zustand
 * stores and reports on a debounce. A group that disappears from the derived
 * set is reported as `{ paths: [] }` to clear its watches (no leaks when the
 * user collapses a dir, switches workspace, or closes the panel).
 */

import { useFileEditorStore } from "../stores/fileEditorStore";
import { useLayoutStore } from "../stores/layoutStore";
import { workspacePanelVisible } from "../components/layout/rightPanelGate";
import { useChatStore } from "../stores/chatStore";
import { useAgentStore } from "../stores/agentStore";
import { useWorkspaceStore } from "../stores/workspaceStore";
import { useGitStore } from "../stores/gitStore";
import { getGatewayUrl } from "./config";
import { log } from "./logger";

/** Debounce window (ms) for coalescing bursts of frontend state changes. */
export const WATCH_REPORT_DEBOUNCE_MS = 300;

/** Group key: `${agentId}\u0000${workspaceId}`. */
type GroupKey = string;

let _unsubscribers: Array<() => void> = [];
let _reportTimer: ReturnType<typeof setTimeout> | null = null;
let _lastReported = new Map<GroupKey, string[]>();

/**
 * Derive the current "visible" watch set from frontend state, grouped by
 * (agent, workspace). Empty string represents the workspace root.
 *
 * @internal Exported for tests — production entry is `initWorkspaceWatchReporter`.
 */
export function deriveWatchGroups(): Map<GroupKey, string[]> {
    const groups = new Map<GroupKey, Set<string>>();

    const addPath = (agentId: string, workspaceId: string, relPath: string) => {
        const key = `${agentId}\u0000${workspaceId}`;
        let set = groups.get(key);
        if (!set) {
            set = new Set();
            groups.set(key, set);
        }
        set.add(relPath);
    };

    // 1. Open editor tabs (files only — URL previews have no fs path).
    for (const f of useFileEditorStore.getState().openFiles) {
        if (f.kind !== "file") continue;
        addPath(f.agentId, f.workspaceId, f.relPath);
    }

    // 2. Visible workspace panel → root + expanded directories.
    //
    // "Visible" is `workspacePanelVisible`, not `activePanelTab === "workspace"
    // && !rightPanelCollapsed`: the hover drawer previews the tree while the
    // layout stays collapsed, and a tree the user is looking at has to keep
    // updating — otherwise the drawer shows a snapshot and the file they
    // hovered for is missing from it.
    const layout = useLayoutStore.getState();
    if (workspacePanelVisible(layout)) {
        const agentId = useAgentStore.getState().selectedAgentId;
        if (agentId) {
            const sessionId = useChatStore.getState().getActiveSessionId(agentId);
            if (sessionId) {
                const workspaceId = useWorkspaceStore
                    .getState()
                    .getSessionWorkspaceId(sessionId);
                addPath(agentId, workspaceId, ""); // root
                const ss =
                    useChatStore.getState().agentStates[agentId]?.sessionStates[sessionId];
                for (const p of ss?.treeExpandedPaths ?? []) {
                    if (p) addPath(agentId, workspaceId, p);
                }
            }
        }
    }

    // 3. Git Status panel expanded → watch the workspace root, OUTSIDE the
    //    workspace-panel visibility guard (ADR-078 decision 8): the
    //    GitStatusBar/Panel lives at the bottom of the WorkspaceExplorer, so
    //    its root watch must be independent of whether the file tree's root
    //    is expanded or the file tree is even visible — only "git panel
    //    expanded" matters. (2026-XX revision: the panel moved from
    //    FileEditorPanel to WorkspaceExplorer; the orthogonality invariant
    //    remains, the trigger is now "git panel visible" rather than
    //    "editor open".)
    const git = useGitStore.getState();
    if (git.expandedKey) {
        const sep = git.expandedKey.indexOf("\u0000");
        if (sep !== -1) {
            addPath(
                git.expandedKey.slice(0, sep),
                git.expandedKey.slice(sep + 1),
                "",
            );
        }
    }

    const out = new Map<GroupKey, string[]>();
    for (const [key, set] of groups) out.set(key, [...set]);
    return out;
}

function samePathSet(a: string[], b: string[]): boolean {
    if (a.length !== b.length) return false;
    const sb = new Set(b);
    return a.every((p) => sb.has(p));
}

async function reportGroup(agentId: string, workspaceId: string, paths: string[]): Promise<void> {
    const baseUrl = getGatewayUrl();
    try {
        const resp = await fetch(
            `${baseUrl}/api/agents/${encodeURIComponent(agentId)}/workspaces/${encodeURIComponent(workspaceId)}/fs-watch`,
            {
                method: "PUT",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify({ paths }),
            },
        );
        if (!resp.ok) {
            // 404 = agent not running (stopped between derivation and report)
            // or workspace removed — nothing to watch, stay quiet.
            if (resp.status !== 404) {
                log.warn(
                    "[WorkspaceFsWatch] fs-watch PUT failed",
                    resp.status,
                    resp.statusText,
                );
            }
        }
    } catch (e) {
        log.warn("[WorkspaceFsWatch] fs-watch PUT error:", e);
    }
}

/**
 * Compare the freshly derived groups against the last reported set and push
 * every diff. Groups that vanished are reported as empty (clears the Runtime's
 * watches for that workspace).
 */
async function flushWatchReport(): Promise<void> {
    const derived = deriveWatchGroups();

    const reports: Array<[string, string, string[]]> = [];
    for (const [key, paths] of derived) {
        const prev = _lastReported.get(key);
        if (prev && samePathSet(prev, paths)) continue;
        const [agentId, workspaceId] = key.split("\u0000");
        reports.push([agentId, workspaceId, paths]);
        _lastReported.set(key, paths);
    }
    // Groups we reported before but no longer derive → clear them.
    for (const [key, prev] of _lastReported) {
        if (derived.has(key)) continue;
        if (prev.length === 0) {
            _lastReported.delete(key);
            continue;
        }
        const [agentId, workspaceId] = key.split("\u0000");
        reports.push([agentId, workspaceId, []]);
        _lastReported.delete(key);
    }

    if (reports.length === 0) return;
    await Promise.all(reports.map(([a, w, p]) => reportGroup(a, w, p)));
}

function scheduleWatchReport(): void {
    if (_reportTimer) clearTimeout(_reportTimer);
    _reportTimer = setTimeout(() => {
        _reportTimer = null;
        flushWatchReport().catch((e) =>
            log.error("[WorkspaceFsWatch] flushWatchReport error:", e),
        );
    }, WATCH_REPORT_DEBOUNCE_MS);
}

/** Subscribe to every store whose state feeds the derived watch set. */
function subscribeStores(): void {
    _unsubscribers.push(useFileEditorStore.subscribe(() => scheduleWatchReport()));
    _unsubscribers.push(useLayoutStore.subscribe(() => scheduleWatchReport()));
    _unsubscribers.push(useChatStore.subscribe(() => scheduleWatchReport()));
    _unsubscribers.push(useAgentStore.subscribe(() => scheduleWatchReport()));
    _unsubscribers.push(useWorkspaceStore.subscribe(() => scheduleWatchReport()));
    // ADR-078 decision 8: expanding/collapsing the Git Status panel adds /
    // removes the workspace root from the derived watch set.
    _unsubscribers.push(useGitStore.subscribe(() => scheduleWatchReport()));
}

/**
 * Initialise the watch-set reporter. Idempotent — a recovery reload re-inits
 * without duplicate subscriptions. Also performs an initial push so the
 * Runtime converges even if no state changed after mount.
 */
export function initWorkspaceWatchReporter(): void {
    disposeWorkspaceWatchReporter();
    subscribeStores();
    // Initial push on mount (async, not awaited by callers).
    void flushWatchReport();
}

/** Tear down subscriptions and any pending debounce. */
export function disposeWorkspaceWatchReporter(): void {
    for (const unsub of _unsubscribers) unsub();
    _unsubscribers = [];
    if (_reportTimer) {
        clearTimeout(_reportTimer);
        _reportTimer = null;
    }
    _lastReported.clear();
}
