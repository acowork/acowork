/**
 * Sandboxed host for an application's own front end (ADR-116 / ADR-121).
 *
 * The Desktop contributes only an iframe container and a narrow postMessage
 * bridge. The app owns its DOM, routing and bundle; the Gateway owns the
 * session ticket and the reverse proxy. The frame always loads through
 * `getGatewayUrl()`, so a Desktop pointed at a remote Gateway still opens
 * the app running on that machine.
 *
 * `allow-same-origin` is kept: an opaque origin makes `localStorage` and
 * IndexedDB throw, and a hosted app loses its preferences and its user fonts.
 * `allow-top-navigation` / `allow-modals` stay out — the app's own DOM is not
 * the Desktop's.
 *
 * Theme is re-pushed when the shell `light`/`dark` class changes. Optional
 * `search` / `openFile` are the visual-preview channel (workspace file in
 * the editor pane) — they must not be used to attach MCP to the left-nav
 * user canvas.
 */
import { useEffect, useRef, useState } from "react";
import { getGatewayUrl } from "../../lib/config";
import { mintAppTicket, type AppDto } from "../../lib/apps-api";

interface BridgeMessage {
    channel?: string;
    type?: string;
    message?: string;
    id?: number | string;
    bridge?: number;
    name?: string;
    bytes?: Uint8Array;
    saveAs?: boolean;
    error?: string;
}

export interface AppFrameOpenFile {
    name: string;
    bytes: Uint8Array;
}

/** The visual-preview INPUT form: bytes sit behind a getter so React's dev
 *  performance track never walks a multi-MB Uint8Array index-by-index. A large
 *  PDF passed as a plain `bytes` prop made the pane freeze 20s+ in dev (the
 *  track deep-inspects props and `for..in`s a typed array = O(file size) per
 *  render); a getter is a function, which it stringifies instead. Caller keeps
 *  the array in a ref. */
export interface AppFrameOpenFileInput {
    name: string;
    getBytes: () => Uint8Array;
}

/** The app's Save: its bytes, plus whether the user picked the destination. */
export interface AppFrameSaveFile extends AppFrameOpenFile {
    saveAs: boolean;
}

/** How long an app may take to answer the host before the pane says so. */
const BOOT_TIMEOUT_MS = 25_000;
/** A delivery is answered (`opened` / `openFailed`); unacked, it is retried. */
const DELIVERY_RETRY_MS = 2_000;
const DELIVERY_TRIES = 3;

export function AppFrame({
    app,
    search,
    openFile,
    onSaveFile,
}: {
    app: AppDto;
    /** Extra query string (ticket stays in the fragment). */
    search?: string;
    /** Workspace file bytes for a visual preview iframe (behind a getter so a
     *  large file never lands in React props — see AppFrameOpenFileInput). */
    openFile?: AppFrameOpenFileInput | null;
    /** Write-back: the app's Save, landed on the workspace file it came from. */
    onSaveFile?: (file: AppFrameSaveFile) => Promise<void>;
}) {
    const frameRef = useRef<HTMLIFrameElement>(null);
    const [src, setSrc] = useState<string | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [notice, setNotice] = useState<string | null>(null);
    const [bootError, setBootError] = useState<string | null>(null);
    const [painted, setPainted] = useState(false);
    const [docInFlight, setDocInFlight] = useState(false);
    /** Bumped to reload the frame after a failed boot. */
    const [attempt, setAttempt] = useState(0);
    const pushThemeRef = useRef<() => void>(() => {});
    const lastThemeKeyRef = useRef<string | null>(null);
    const openFileRef = useRef(openFile);
    openFileRef.current = openFile;
    const onSaveFileRef = useRef(onSaveFile);
    onSaveFileRef.current = onSaveFile;
    /** The app answered the host: it is up and can be handed a document. */
    const bootedRef = useRef(false);
    /** Bridge version the app announced (`hello` = 2; `ready` alone = 1). */
    const bridgeRef = useRef(0);
    /** What the app already took, so the same bytes do not re-open it. */
    const ackedRef = useRef<AppFrameOpenFileInput | null>(null);
    /** What the app asked us to save — it comes back as `openFile`. */
    const lastSavedRef = useRef<AppFrameOpenFile | null>(null);
    const deliveryRef = useRef<{
        id: number;
        timer: ReturnType<typeof setTimeout> | null;
    } | null>(null);
    const deliverySeq = useRef(0);

    useEffect(() => {
        let cancelled = false;
        setSrc(null);
        setError(null);
        setBootError(null);
        setPainted(false);
        setDocInFlight(false);
        bootedRef.current = false;
        bridgeRef.current = 0;
        ackedRef.current = null;
        deliveryRef.current = null;
        lastThemeKeyRef.current = null;
        // A frame that never says hello is a black rectangle: say so, and let
        // the user retry, instead of a loader that never ends.
        const watchdog = setTimeout(() => {
            if (cancelled || bootedRef.current) return;
            setBootError(`The app did not start. Its page never answered the host (bridge v2 expected).`);
        }, BOOT_TIMEOUT_MS);
        (async () => {
            try {
                const ticket = await mintAppTicket(app.id);
                if (cancelled) return;
                const base = `${getGatewayUrl()}/api/apps/${encodeURIComponent(app.id)}${app.ui_path}`;
                const extra = withControlParam(search);
                // Version-key the entry URL: a heuristic-cached index from an older install
                // keeps requesting superseded hashed assets, which the SPA fallback answers
                // with HTML ("expected magic word … found 3c 21 64 6f").
                const ver = encodeURIComponent(app.installed_version ?? app.version);
                setSrc(`${base}${extra}&v=${ver}#ticket=${encodeURIComponent(ticket)}`);
            } catch (e) {
                if (!cancelled) setError(e instanceof Error ? e.message : String(e));
            }
        })();
        return () => {
            cancelled = true;
            clearTimeout(watchdog);
        };
    }, [app.id, app.ui_path, app.installed_version, app.version, search, attempt]);

    const post = (message: Record<string, unknown>) => {
        frameRef.current?.contentWindow?.postMessage({ channel: "acowork", ...message }, "*");
    };

    const pushTheme = () => {
        const win = frameRef.current?.contentWindow;
        if (!win) return;
        const root = document.documentElement;
        const accent =
            getComputedStyle(root).getPropertyValue("--color-accent").trim() || "#3b82f6";
        // settingsStore only toggles `.dark` on <html>; there is no `.light`.
        const theme = root.classList.contains("dark") ? "dark" : "light";
        // The shell's `<html lang>` IS the interface's language — the i18n
        // module owns that attribute — and it is the locale a hosted app must
        // render in. The OS locale is only a fallback for a shell that never
        // said one.
        const locale = root.lang || navigator.language || "en";
        const key = `${theme}|${accent}|${locale}`;
        if (lastThemeKeyRef.current === key) return;
        lastThemeKeyRef.current = key;
        win.postMessage({ channel: "acowork", type: "theme", theme, accent, locale }, "*");
        win.postMessage({ channel: "acowork", type: "locale", locale }, "*");
    };
    pushThemeRef.current = pushTheme;

    /** Stop retrying: the app took the document, or it refused it. */
    const settleDelivery = () => {
        const delivery = deliveryRef.current;
        if (delivery?.timer) clearTimeout(delivery.timer);
        deliveryRef.current = null;
    };

    /**
     * A hand-over is not fire-and-forget: the app answers `opened`, so an
     * unacked delivery is the same document sent again under the same id —
     * never a reopen of one that worked, and never a loader left hanging.
     */
    const armDelivery = (id: number, tries: number) => {
        const timer = setTimeout(() => {
            if (deliveryRef.current?.id !== id) return;
            const payload = openFileRef.current;
            if (!payload || tries >= DELIVERY_TRIES - 1) {
                settleDelivery();
                setDocInFlight(false);
                setNotice(`The app did not take ${payload?.name ?? "the document"}. Reopen it to try again.`);
                return;
            }
            post({ type: "openFile", name: payload.name, bytes: payload.getBytes(), id });
            armDelivery(id, tries + 1);
        }, DELIVERY_RETRY_MS);
        deliveryRef.current = { id, timer };
    };

    const pushOpenFile = () => {
        const payload = openFileRef.current;
        // Nothing goes into a frame that has not answered: an app that is not
        // drawn yet (or not listening) would drop the document on the floor.
        if (!frameRef.current?.contentWindow || !payload) return;
        if (!bootedRef.current) return;
        if (sameBytes(payload, lastSavedRef.current)) return; // it just wrote these
        if (sameBytes(payload, ackedRef.current)) return; // already in, and it drew them
        if (deliveryRef.current) return; // one document in flight at a time
        const id = ++deliverySeq.current;
        deliveryRef.current = { id, timer: null };
        post({ type: "openFile", name: payload.name, bytes: payload.getBytes(), id });
        setDocInFlight(true);
        // A v1 package answers nothing: one hand-over and no verdict.
        if (bridgeRef.current >= 2) armDelivery(id, 0);
    };

    useEffect(() => {
        const onMessage = (event: MessageEvent<BridgeMessage>) => {
            const data = event.data;
            if (!data || data.channel !== "acowork") return;
            // Only accept messages from the iframe we own. With an opaque
            // origin `event.origin` is "null", so source identity is the check.
            if (event.source !== frameRef.current?.contentWindow) return;
            if (data.type === "hello" || data.type === "ready") {
                // `hello` carries the bridge version; `ready` is what a package
                // built before that contract still sends.
                bridgeRef.current = typeof data.bridge === "number" ? data.bridge : 1;
                bootedRef.current = true;
                setBootError(null);
                pushTheme();
                setPainted(true);
                pushOpenFile();
            }
            if (data.type === "opened") {
                if (deliveryRef.current?.id === data.id) {
                    settleDelivery();
                    ackedRef.current = openFileRef.current ?? null;
                    setNotice(null);
                    // The ack says the app took the bytes, not that it has
                    // painted them: uncovering on the ack itself shows its start
                    // screen for a frame or two.
                    requestAnimationFrame(() => requestAnimationFrame(() => setDocInFlight(false)));
                }
            }
            if (data.type === "openFailed") {
                if (deliveryRef.current?.id === data.id) {
                    settleDelivery();
                    setDocInFlight(false);
                    setNotice(String(data.error ?? "the app refused the document"));
                }
            }
            if (data.type === "error") {
                // The app's own exception. It goes to the Desktop log so a
                // user's report has evidence behind it.
                console.error(`[app ${app.id}] ${String(data.message ?? "")}`);
                setNotice(String(data.message ?? ""));
            }
            if (data.type === "notify") setNotice(String(data.message ?? ""));
            if (data.type === "saveFile") {
                const save = onSaveFileRef.current;
                const id = data.id;
                const reply = (type: string, extra: Record<string, unknown> = {}) => {
                    post({ type, ...(id === undefined ? {} : { id }), ...extra });
                };
                if (!save || !data.bytes) {
                    reply("saveFailed", { error: "no workspace file to save into" });
                    return;
                }
                const name = String(data.name || "document");
                // Guests may hand save bytes over as a transferred ArrayBuffer; the write
                // path needs a Uint8Array (an ArrayBuffer has no `.length`).
                const bytes = data.bytes instanceof Uint8Array ? data.bytes : new Uint8Array(data.bytes);
                lastSavedRef.current = { name, bytes };
                void save({ name, bytes, saveAs: Boolean(data.saveAs) }).then(
                    () => reply("saved"),
                    (e) => reply("saveFailed", { error: e instanceof Error ? e.message : String(e) }),
                );
            }
        };
        window.addEventListener("message", onMessage);
        return () => window.removeEventListener("message", onMessage);
    }, [app.id]);

    useEffect(() => {
        const observer = new MutationObserver(() => pushThemeRef.current());
        observer.observe(document.documentElement, {
            attributes: true,
            attributeFilter: ["class", "lang"],
        });
        return () => observer.disconnect();
    }, []);

    useEffect(() => {
        pushOpenFile();
    }, [openFile, src]);

    if (error) {
        return (
            <div className="flex h-full flex-col items-center justify-center gap-2 p-6 text-xs text-text-tertiary">
                <span>Failed to open {app.name}</span>
                <span className="text-text-tertiary/80">{error}</span>
            </div>
        );
    }
    if (!src) {
        return (
            <div className="flex h-full items-center justify-center text-xs text-text-tertiary">
                Opening {app.name}…
            </div>
        );
    }

    return (
        <div className="flex h-full min-h-0 flex-col">
            {notice && (
                <div className="shrink-0 border-b border-border-divider px-3 py-1.5 text-xs text-text-secondary">
                    {notice}
                    <button type="button" onClick={() => setNotice(null)} className="ml-2 text-text-tertiary">
                        dismiss
                    </button>
                </div>
            )}
            <div className="relative min-h-0 flex-1">
                <iframe
                    ref={frameRef}
                    src={src}
                    title={app.name}
                    sandbox="allow-scripts allow-same-origin allow-forms allow-popups allow-downloads"
                    onLoad={() => {
                        pushTheme();
                        pushOpenFile();
                    }}
                    className="h-full w-full border-0 bg-page-bg"
                />
                {(!painted || docInFlight) && (
                    <div className="absolute inset-0 flex flex-col items-center justify-center gap-2 bg-page-bg px-6 text-center text-xs text-text-tertiary">
                        {bootError ? (
                            <>
                                <span>{bootError}</span>
                                <button
                                    type="button"
                                    onClick={() => setAttempt((n) => n + 1)}
                                    className="rounded border border-border-divider px-2 py-1 text-text-secondary"
                                >
                                    Try again
                                </button>
                            </>
                        ) : (
                            <>
                                <div className="h-5 w-5 animate-spin rounded-full border-2 border-border-strong border-t-zinc-600 dark:border-t-zinc-300" />
                                <span>
                                    {docInFlight
                                        ? `Opening ${openFile?.name ?? "document"}…`
                                        : `Opening ${app.name}…`}
                                </span>
                            </>
                        )}
                    </div>
                )}
            </div>
        </div>
    );
}

function sameBytes(
    a: AppFrameOpenFileInput | AppFrameOpenFile | null,
    b: AppFrameOpenFileInput | AppFrameOpenFile | null,
): boolean {
    if (!a || !b) return false;
    if (a.name !== b.name) return false;
    const ab = "getBytes" in a ? a.getBytes() : a.bytes;
    const bb = "getBytes" in b ? b.getBytes() : b.bytes;
    if (ab.length !== bb.length) return false;
    for (let i = 0; i < ab.length; i += 1) if (ab[i] !== bb[i]) return false;
    return true;
}

function normalizeSearch(search?: string): string {
    if (!search) return "";
    const trimmed = search.trim();
    if (!trimmed) return "";
    return trimmed.startsWith("?") ? trimmed : `?${trimmed}`;
}

/** FigCraft's `window.figcraft` control API is opt-in via `?control`. */
function withControlParam(search?: string): string {
    const extra = normalizeSearch(search);
    const params = new URLSearchParams(extra.startsWith("?") ? extra.slice(1) : extra);
    if (!params.has("control")) params.set("control", "1");
    return `?${params.toString()}`;
}