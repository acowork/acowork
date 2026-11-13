/**
 * Shared LSP utilities and types.
 *
 * Extracted from useLspClient.ts and useLspClientPool.ts so that the
 * LspConnection domain class can use them without importing from hooks.
 */

import type { IWebSocket } from "vscode-ws-jsonrpc";
import { getCachedLspRelayEndpoint, invalidateLspRelayEndpointCache } from "./gateway-api";
import { useAuthStore } from "../stores/authStore";
import { MonacoVscodeApiWrapper } from "monaco-languageclient/vscodeApiWrapper";
import type { MonacoVscodeApiConfig } from "monaco-languageclient/vscodeApiWrapper";
import { log } from "./logger";

// ── Types ──────────────────────────────────────────────────────────────

export type LspStatus =
    | "disconnected"
    | "connecting"
    | "connected"
    | "indexing"
    | "ready"
    | "error";

// ── VS Code API initialization ────────────────────────────────────────

let vscodeApiInitPromise: Promise<void> | null = null;
let vscodeApiInitDone = false;

/**
 * Upper bound on how long we are willing to wait for an already-running
 * global init before declaring it orphaned and resetting the global state.
 *
 * `MonacoVscodeApiWrapper.start()` (10.7.0) has no try/catch around its
 * init sequence — if any awaitable step throws between `markGlobalInit()`
 * and `markGlobalInitDone()`, the global init promise stays pending
 * forever and `vscodeApiInitialising` stays stuck on `true`.  Every
 * subsequent `wrapper.start()` then hits the "already ongoing" branch
 * which **returns immediately without doing anything**.  That is the
 * root cause of the "stuck at `connecting`" symptom.
 *
 * Waiting for the orphan would hang the UI permanently, so we race the
 * global promise against this timeout and reset state on expiry.
 */
const GLOBAL_INIT_TIMEOUT_MS = 30_000;

/**
 * Augmented fields that `MonacoVscodeApiWrapper` attaches to
 * `window.MonacoEnvironment` to co-ordinate global initialization
 * across multiple wrapper instances:
 *
 * - `vscodeApiGlobalInitAwait` — set by `markGlobalInit()` at the start
 *   of `wrapper.start()` and resolved by `markGlobalInitDone()`.  It is
 *   `undefined` both before the first start and after completion.
 * - `vscodeApiGlobalInitResolve` — the resolver paired with the await
 *   promise.  Held on the env object, so we can resolve an orphaned
 *   promise ourselves on the recovery path.
 * - `vscodeApiInitialised` — `true` once init succeeds.
 * - `vscodeApiInitialising` — `true` while init is running.
 *
 * Reading these on `window.MonacoEnvironment` lets us safely co-ordinate
 * with any other wrapper instance that may be initializing or have
 * already completed — without racing on the (non-idempotent) start()
 * flow which `MonacoVscodeApiWrapper` does not guard across instances.
 */
interface EnhancedMonacoEnvironment {
    vscodeApiGlobalInitAwait?: Promise<void>;
    vscodeApiGlobalInitResolve?: () => void;
    vscodeApiInitialised?: boolean;
    vscodeApiInitialising?: boolean;
    viewServiceType?: string;
}

function getEnhancedMonacoEnvironment(): EnhancedMonacoEnvironment {
    const w = window as unknown as { MonacoEnvironment?: EnhancedMonacoEnvironment };
    if (!w.MonacoEnvironment) w.MonacoEnvironment = {};
    return w.MonacoEnvironment;
}

/**
 * Reset the global init flags and resolve any orphaned promise.
 *
 * Called as a recovery path when we detect the library has left the
 * global init in an inconsistent state (typically: `wrapper.start()`
 * threw mid-sequence after `markGlobalInit()` but before
 * `markGlobalInitDone()`).
 *
 * After this runs, the next call to `ensureVscodeApiInitialized()`
 * will see no orphan, no `vscodeApiInitialising`, no
 * `vscodeApiInitialised` — and will safely re-enter Case C.
 */
function resetGlobalInitState(): void {
    const env = getEnhancedMonacoEnvironment();
    // Wake up anyone still awaiting the orphaned promise so they take
    // their own error path instead of hanging the UI.
    try {
        env.vscodeApiGlobalInitResolve?.();
    } catch {
        // resolve() must not throw, but defend against library changes
    }
    env.vscodeApiGlobalInitAwait = undefined;
    env.vscodeApiGlobalInitResolve = undefined;
    env.vscodeApiInitialising = false;
    env.vscodeApiInitialised = false;
}

/** Initialize VS Code API services (required by MonacoLanguageClient v10+). */
export async function ensureVscodeApiInitialized(): Promise<void> {
    if (vscodeApiInitDone) return;

    const env = getEnhancedMonacoEnvironment();

    // Case A: another wrapper instance is currently initializing.
    // `MonacoVscodeApiWrapper.start()` returns immediately in its
    // "already ongoing" branch WITHOUT awaiting the real init — so we
    // must await the global init promise ourselves to guarantee
    // `defaultApi` (set by the local-extension-host post-participant)
    // is populated before `new MonacoLanguageClient()` runs.
    //
    // If that promise has been orphaned by a prior failed init it will
    // never resolve, so we race it against a timeout and recover.
    const ongoing = env.vscodeApiGlobalInitAwait;
    if (ongoing) {
        if (!vscodeApiInitPromise) vscodeApiInitPromise = ongoing;
        try {
            await Promise.race([
                ongoing,
                new Promise<void>((_, reject) =>
                    setTimeout(
                        () =>
                            reject(
                                new Error(
                                    `VS Code API global init timed out after ${GLOBAL_INIT_TIMEOUT_MS}ms — orphaned from prior failed init`,
                                ),
                            ),
                        GLOBAL_INIT_TIMEOUT_MS,
                    ),
                ),
            ]);
            vscodeApiInitDone = true;
            return;
        } catch (err) {
            // Orphaned (or otherwise failed) global init.  Clear the
            // global flags and our local promise so the next caller can
            // re-enter Case C cleanly instead of getting stuck forever
            // in the library's "already ongoing" early-return branch.
            log.error(
                "[LSP] VS Code API global init wait failed — resetting orphan state:",
                err,
            );
            resetGlobalInitState();
            vscodeApiInitPromise = null;
            throw err;
        }
    }

    // Case B: init already completed globally (by a prior wrapper on this
    // page, or by our own previous run).  Nothing to do — `defaultApi` is
    // already set in the shared `vscode/localExtensionHost` module.
    if (env.vscodeApiInitialised === true) {
        vscodeApiInitDone = true;
        return;
    }

    // Case C: we are the first to initialize.  Guard with our own promise
    // so concurrent callers within this module coalesce onto one start().
    if (!vscodeApiInitPromise) {
        log.debug("[LSP] Initializing VS Code API services (first time)...");
        const t0 = performance.now();
        vscodeApiInitPromise = (async () => {
            try {
                const config: MonacoVscodeApiConfig = {
                    $type: "classic",
                    viewsConfig: { $type: "EditorService" },
                };
                const wrapper = new MonacoVscodeApiWrapper(config);
                await wrapper.start({ caller: "LspConnection" });

                // ── Silence Monaco's built-in TypeScript worker diagnostics ──
                // Monaco spawns its own TS worker that uses default compiler
                // options (no tsconfig.json).  With `moduleResolution: "bundler"`
                // in the project, the worker's default options cause false
                // "Cannot find module" errors for packages without `exports`.
                // We disable the worker's diagnostics entirely — the external
                // tsserver (via LSP relay) is the source of truth and reports
                // diagnostics via textDocument/publishDiagnostics.
                const monaco = await import("monaco-editor");
                // monaco.languages.typescript is marked deprecated in type defs
                // but exists at runtime.  Use `any` to bypass TS errors.
                // eslint-disable-next-line @typescript-eslint/no-explicit-any
                const ts = (monaco.languages as any).typescript;
                ts.typescriptDefaults.setDiagnosticsOptions({
                    noSemanticValidation: true,
                    noSyntaxValidation: true,
                    noSuggestionDiagnostics: true,
                });
                ts.javascriptDefaults.setDiagnosticsOptions({
                    noSemanticValidation: true,
                    noSyntaxValidation: true,
                    noSuggestionDiagnostics: true,
                });
                log.debug("[LSP] Monaco built-in TS diagnostics disabled — using tsserver");

                vscodeApiInitDone = true;
                log.debug(
                    "[LSP] VS Code API services initialized successfully",
                    `elapsed: ${Math.round(performance.now() - t0)}ms`,
                );
            } catch (err) {
                // Library bug: `wrapper.start()` may throw AFTER
                // `markGlobalInit()` but BEFORE `markGlobalInitDone()`,
                // leaving an orphaned pending promise on the global env.
                // Subsequent `wrapper.start()` calls would then hit the
                // "already ongoing" early-return branch forever.  We must
                // clear that state so retries actually run.
                log.error("[LSP] VS Code API initialization failed:", err);
                resetGlobalInitState();
                vscodeApiInitPromise = null;
                throw err;
            }
        })();
    }
    await vscodeApiInitPromise;
}

// ── WebSocket adapter ──────────────────────────────────────────────────

/** Adapt a browser WebSocket to vscode-ws-jsonrpc's IWebSocket interface. */
export function adaptWebSocket(ws: WebSocket): IWebSocket {
    const listeners: Array<() => void> = [];
    return {
        send(content: string): void {
            ws.send(content);
        },
        onMessage(cb: (data: unknown) => void): void {
            const handler = (e: MessageEvent) => cb(e.data);
            ws.addEventListener("message", handler);
            listeners.push(() => ws.removeEventListener("message", handler));
        },
        onError(cb: (reason: unknown) => void): void {
            const handler = () => cb(undefined);
            ws.addEventListener("error", handler);
            listeners.push(() => ws.removeEventListener("error", handler));
        },
        onClose(cb: (code: number, reason: string) => void): void {
            const handler = (e: CloseEvent) => cb(e.code, e.reason);
            ws.addEventListener("close", handler);
            listeners.push(() => ws.removeEventListener("close", handler));
        },
        dispose(): void {
            for (const remove of listeners) remove();
            listeners.length = 0;
        },
    };
}

// ── WebSocket URL builder ──────────────────────────────────────────────

/**
 * Build the LSP Relay WebSocket URL for a given language.
 *
 * Queries the Gateway for the LSP Relay endpoint of the node hosting
 * `agentId` (cached), then constructs a direct WebSocket URL to the
 * relay process. The relay's WebSocket handler expects `workspace_root`
 * as a query parameter.
 *
 * @throws if the LSP Relay is not available (or no agent id was given).
 */
export async function buildLspWsUrl(
    language: string,
    workspaceRoot?: string,
    agentId?: string,
): Promise<string> {
    const relayUrl = agentId ? await getCachedLspRelayEndpoint(agentId) : null;
    if (!relayUrl) {
        throw new Error("LSP Relay not available");
    }
    // The endpoint is the Gateway relay-proxy base URL (ADR-089 D3,
    // e.g. "http://127.0.0.1:19876/api/nodes/{id}/lsp"); swap the
    // scheme for the WebSocket transport.
    const wsUrl = relayUrl.replace(/^http/, "ws");
    let url = `${wsUrl}/lsp/${encodeURIComponent(language)}`;
    const params = new URLSearchParams();
    if (workspaceRoot) params.set("workspace_root", workspaceRoot);
    // ADR-089 D2: the proxy sits behind the Gateway bearer gate and a
    // browser WebSocket cannot set headers — the token rides as a
    // query parameter (the Gateway accepts it for upgrade requests only
    // and redacts it from logs). ADR-114: no account-mode branch.
    const auth = useAuthStore.getState();
    if (auth.accessToken) {
        params.set("access_token", auth.accessToken);
    }
    const qs = params.toString();
    const result = qs ? `${url}?${qs}` : url;
    log.debug(
        "[LSP] buildLspWsUrl — relay endpoint:",
        relayUrl,
        "→ result:",
        result,
    );
    return result;
}

// ── Absolute URI builder ───────────────────────────────────────────────

/**
 * Build an absolute LSP file URI from a workspace root and relative path.
 *
 * Handles the Windows extended-length path prefix (\\?\) that
 * std::fs::canonicalize() produces on Windows.
 */
export function buildAbsoluteUri(workspaceRoot: string, relPath: string): string {
    let root = workspaceRoot.replace(/\\/g, "/");
    root = root.replace(/^\/\/\?\//, "").replace(/^\/\?\//, "");
    const rel = relPath.replace(/\\/g, "/");
    const absPath = `${root}/${rel}`;
    if (/^[A-Za-z]:/.test(absPath)) {
        return `file:///${absPath}`;
    }
    return `file://${absPath}`;
}

// ── Language ID mapping ────────────────────────────────────────────────

/**
 * Map a Monaco model's language ID to the correct LSP language ID.
 *
 * Monaco uses "typescript" for both .ts and .tsx, and "javascript" for
 * both .js and .jsx.  tsserver (typescript-language-server) needs explicit
 * "typescriptreact" / "javascriptreact" to enable JSX parsing.  Without
 * this mapping, .tsx files are parsed as plain TypeScript and JSX syntax
 * produces spurious "'>' expected" errors.
 */
export function toLspLanguageId(modelUri: string, monacoLanguageId: string): string {
    const ext = modelUri.split("?")[0].split(".").pop()?.toLowerCase();
    if (monacoLanguageId === "typescript" && ext === "tsx") {
        return "typescriptreact";
    }
    if (monacoLanguageId === "javascript" && ext === "jsx") {
        return "javascriptreact";
    }
    return monacoLanguageId;
}

// ── Error formatting ───────────────────────────────────────────────────

/** Language-specific LSP install hints shown in error messages. */
export const LSP_INSTALL_HINTS: Record<string, string> = {
    typescript: "npm install -g typescript-language-server typescript",
    javascript: "npm install -g typescript-language-server typescript",
    ts: "npm install -g typescript-language-server typescript",
    js: "npm install -g typescript-language-server typescript",
    rust: "rustup component add rust-analyzer",
    python: "pip install python-lsp-server",
    go: "go install golang.org/x/tools/gopls@latest",
    json: "npm install -g vscode-json-languageserver",
    yaml: "npm install -g yaml-language-server",
    yml: "npm install -g yaml-language-server",
    html: "npm install -g vscode-html-languageserver",
    css: "npm install -g vscode-css-languageserver",
    scss: "npm install -g vscode-css-languageserver",
    less: "npm install -g vscode-css-languageserver",
    markdown: "Install marksman: https://github.com/artempyanykh/marksman",
    md: "Install marksman: https://github.com/artempyanykh/marksman",
};

export function formatLspError(language: string, reason: string): string {
    const hint = LSP_INSTALL_HINTS[language.toLowerCase()];
    if (hint) {
        return `${reason}. Install: ${hint}`;
    }
    return reason;
}

/** Re-export for convenience. */
export { invalidateLspRelayEndpointCache };
