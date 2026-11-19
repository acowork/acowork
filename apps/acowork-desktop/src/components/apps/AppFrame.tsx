/**
 * Sandboxed host for an application's own front end (ADR-116).
 *
 * The Desktop contributes only an iframe container and a narrow postMessage
 * bridge. The app owns its DOM, routing and bundle; the Gateway owns the
 * session ticket and the reverse proxy. The frame always loads through
 * `getGatewayUrl()`, so a Desktop pointed at a remote Gateway still opens
 * the app running on that machine.
 *
 * `allow-same-origin` is deliberately absent: with an opaque origin the app
 * cannot reach the Desktop window's DOM, storage or tokens, while its
 * fetches still carry the session ticket. `allow-downloads` is present so
 * the app can save files through the browser download path.
 */
import { useEffect, useRef, useState } from "react";
import { getGatewayUrl } from "../../lib/config";
import { mintAppTicket, type AppDto } from "../../lib/apps-api";

interface BridgeMessage {
    channel?: string;
    type?: string;
    message?: string;
    [key: string]: unknown;
}

export function AppFrame({ app }: { app: AppDto }) {
    const frameRef = useRef<HTMLIFrameElement>(null);
    const [src, setSrc] = useState<string | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [notice, setNotice] = useState<string | null>(null);

    useEffect(() => {
        let cancelled = false;
        setSrc(null);
        setError(null);
        (async () => {
            try {
                const ticket = await mintAppTicket(app.id);
                if (cancelled) return;
                const base = `${getGatewayUrl()}/api/apps/${encodeURIComponent(app.id)}${app.ui_path}`;
                setSrc(`${base}#ticket=${encodeURIComponent(ticket)}`);
            } catch (e) {
                if (!cancelled) setError(e instanceof Error ? e.message : String(e));
            }
        })();
        return () => {
            cancelled = true;
        };
    }, [app.id, app.ui_path]);

    const pushTheme = () => {
        const win = frameRef.current?.contentWindow;
        if (!win) return;
        const root = document.documentElement;
        const accent =
            getComputedStyle(root).getPropertyValue("--color-accent").trim() || "#3b82f6";
        const theme = root.classList.contains("light") ? "light" : "dark";
        const locale =
            root.lang ||
            document.documentElement.getAttribute("lang") ||
            navigator.language ||
            "en";
        win.postMessage({ channel: "acowork", type: "theme", theme, accent, locale }, "*");
        win.postMessage({ channel: "acowork", type: "locale", locale }, "*");
    };

    useEffect(() => {
        const onMessage = (event: MessageEvent<BridgeMessage>) => {
            const data = event.data;
            if (!data || data.channel !== "acowork") return;
            // Only accept messages from the iframe we own. With an opaque
            // origin `event.origin` is "null", so source identity is the check.
            if (event.source !== frameRef.current?.contentWindow) return;
            if (data.type === "ready") pushTheme();
            if (data.type === "notify") setNotice(String(data.message ?? ""));
        };
        window.addEventListener("message", onMessage);
        return () => window.removeEventListener("message", onMessage);
    }, []);

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
                    <button
                        type="button"
                        onClick={() => setNotice(null)}
                        className="ml-2 text-text-tertiary"
                    >
                        dismiss
                    </button>
                </div>
            )}
            <iframe
                ref={frameRef}
                src={src}
                title={app.name}
                sandbox="allow-scripts allow-forms allow-popups allow-downloads"
                onLoad={pushTheme}
                className="min-h-0 w-full flex-1 border-0 bg-page-bg"
            />
        </div>
    );
}
