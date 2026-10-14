import { useState, useRef, useEffect } from "react";
import { Circle, CircleDot, Copy, Check, Play, Loader2, AlertTriangle } from "lucide-react";
import { Tooltip } from "../common/Tooltip";
import { cn } from "../../lib/utils";
import { getLspRelayUrl, lspAdminUrlFromRelayUrl, runLspInstall } from "../../lib/gateway-api";
import { useTranslation } from "../../i18n/useTranslation";
import { type LspStatus } from "../../lib/lspUtils";

// ── LSP Indicator Install Hints ───────────────────────────────────────
// Renamed from `LSP_INSTALL_HINTS` (the old `FileEditorPanel` local)
// to avoid name collision with `lib/lspUtils.ts:LSP_INSTALL_HINTS`,
// which is a different `Record<string, string>` table feeding the
// `formatLspError` helper. Kept here so the indicator is self-
// contained for re-use by `FileStatusCluster`.
export const LSP_INDICATOR_HINTS: Record<
    string,
    { name: string; command: string; url?: string }
> = {
    rust: {
        name: "rust-analyzer",
        command: "rustup component add rust-analyzer",
        url: "https://rustanalyzer.github.io/",
    },
    python: {
        name: "python-lsp-server",
        command: "pip install python-lsp-server",
        url: "https://github.com/python-languageserver/python-language-server",
    },
    typescript: {
        name: "typescript-language-server",
        command: "npm install -g typescript-language-server typescript",
        url: "https://github.com/typescript-language-server/typescript-language-server",
    },
    javascript: {
        name: "typescript-language-server",
        command: "npm install -g typescript-language-server typescript",
    },
    go: {
        name: "gopls",
        command: "go install golang.org/x/tools/gopls@latest",
        url: "https://pkg.go.dev/golang.org/x/tools/gopls",
    },
    cpp: {
        name: "clangd",
        command: "Windows: winget install LLVM.LLVM | Linux: apt install clangd | macOS: brew install llvm",
        url: "https://clangd.llvm.org/",
    },
};

// ── LSP Status Indicator ──────────────────────────────────────────────
// Extracted from `FileEditorPanel.tsx` as part of PR-2 (unified
// status-bar refactor). Pure, self-contained: takes `status`,
// `statusMessage`, and `language`; emits a status pill + optional
// install-hint popover. Behaviour and DOM are unchanged from the
// pre-refactor version. All user-facing strings now route through
// `fileStatus.lsp.*` i18n keys (PR-3).

export function LspIndicator({
    status,
    statusMessage,
    language,
    agentId,
}: {
    status: LspStatus;
    statusMessage: string;
    language: string;
    /** Agent hosting the active file — resolves which node's LSP relay to use (ADR-055 §6.7) */
    agentId?: string;
}) {
    const { t } = useTranslation();
    const [showPopover, setShowPopover] = useState(false);
    const [copied, setCopied] = useState(false);
    const [installing, setInstalling] = useState(false);
    const [installResult, setInstallResult] = useState<{ success: boolean; text: string } | null>(null);
    const popoverRef = useRef<HTMLDivElement>(null);

    const isUnavailable = status === "disconnected" || status === "error";
    const hint = LSP_INDICATOR_HINTS[language];

    // Close popover on outside click or Escape
    useEffect(() => {
        if (!showPopover) return;

        const handleClickOutside = (e: MouseEvent) => {
            if (popoverRef.current && !popoverRef.current.contains(e.target as Node)) {
                setShowPopover(false);
            }
        };
        const handleEscape = (e: KeyboardEvent) => {
            if (e.key === "Escape") setShowPopover(false);
        };

        document.addEventListener("mousedown", handleClickOutside);
        document.addEventListener("keydown", handleEscape);
        return () => {
            document.removeEventListener("mousedown", handleClickOutside);
            document.removeEventListener("keydown", handleEscape);
        };
    }, [showPopover]);

    const handleClick = () => {
        if (isUnavailable && hint) {
            setShowPopover((v) => !v);
        }
    };

    const copyToClipboard = () => {
        if (!hint) return;
        void navigator.clipboard.writeText(hint.command).then(() => {
            setCopied(true);
            setTimeout(() => setCopied(false), 2000);
        });
    };

    const runInstall = async () => {
        if (!language || installing) return;
        setInstalling(true);
        setInstallResult(null);
        try {
            // ADR-091 D15: the Node is the only installer. The relay's own
            // `/api/lsp/install` is gone, so this goes to the registry-admin
            // proxy next to the relay passthrough — same node, different
            // surface (`can_manage` is checked there, not here).
            const relayUrl = await getLspRelayUrl(agentId);
            const adminUrl = relayUrl ? lspAdminUrlFromRelayUrl(relayUrl) : null;
            if (!adminUrl) {
                setInstallResult({
                    success: false,
                    text: "LSP Relay not available",
                });
                return;
            }
            const data = await runLspInstall(adminUrl, language);
            if (data.success) {
                setInstallResult({
                    success: true,
                    // No relay restart for an install (§5.10): the Node fires
                    // a forced probe that re-reads PATH, so an editor session
                    // that already gave up on this language needs a reconnect.
                    text: data.stdout || "Installation completed. Reopen the file to start the server.",
                });
            } else {
                // Show stderr first, then stdout, then fallback
                const detail =
                    data.stderr ||
                    data.failure ||
                    data.stdout ||
                    `Install failed (exit code: ${data.exitCode ?? "n/a"})`;
                setInstallResult({
                    success: false,
                    text: detail,
                });
            }
        } catch (err: any) {
            setInstallResult({
                success: false,
                text: err?.message || "Failed to run install script",
            });
        } finally {
            setInstalling(false);
        }
    };

    // Render the status text
    let content: React.ReactNode;
    if (status === "disconnected") {
        content = (
            <span className="flex items-center gap-1 text-text-tertiary">
                <Circle className="h-2 w-2" />
                <span>{t("fileStatus.lsp.unavailable", { language })}</span>
            </span>
        );
    } else if (status === "connecting") {
        content = (
            <span className="flex items-center gap-1 text-text-tertiary">
                <Circle className="h-2 w-2 animate-pulse" />
                <span>{t("fileStatus.lsp.connecting", { language })}</span>
            </span>
        );
    } else if (status === "indexing") {
        content = (
            <span className="flex items-center gap-1 text-amber-500 dark:text-amber-400">
                <Circle className="h-2 w-2 animate-pulse" />
                <span>
                    {statusMessage
                        ? t("fileStatus.lsp.indexing", { language, statusMessage })
                        : t("fileStatus.lsp.indexingDefault", { language })}
                </span>
            </span>
        );
    } else if (status === "connected") {
        // Handshake done, but indexing has not started/finished yet —
        // hover/definition results may be incomplete.
        content = (
            <span className="flex items-center gap-1 text-emerald-500/70 dark:text-emerald-400/70">
                <Circle className="h-2 w-2" />
                <span>{t("fileStatus.lsp.connected", { language })}</span>
            </span>
        );
    } else if (status === "ready") {
        content = (
            <span className="flex items-center gap-1 text-emerald-600 dark:text-emerald-400">
                <CircleDot className="h-2 w-2" />
                <span>{t("fileStatus.lsp.ready", { language })}</span>
            </span>
        );
    } else {
        // error
        const tooltip = statusMessage || "unknown error";
        content = (
            <Tooltip content={tooltip} variant="plain">
                <span className="flex items-center gap-1 text-amber-500">
                    <Circle className="h-2 w-2" />
                    <span>{t("fileStatus.lsp.unavailable", { language })}</span>
                </span>
            </Tooltip>
        );
    }

    return (
        <div className="relative" ref={popoverRef}>
            <button
                type="button"
                onClick={handleClick}
                className={cn(
                    "flex items-center",
                    isUnavailable && hint ? "cursor-pointer hover:opacity-80" : "cursor-default",
                )}
            >
                {content}
            </button>

            {/* Install hint popover */}
            {showPopover && hint && (
                <div className="absolute bottom-full left-0 z-50 mb-1 w-72 rounded-md border border-border-outer bg-modal-surface p-3 shadow-lg text-xs">
                    <div className="font-medium text-text-secondary  mb-1.5">
                        {t("fileStatus.lsp.installTitle", { name: hint.name })}
                    </div>
                    <div className="flex items-center gap-1.5 rounded bg-zinc-100 dark:bg-zinc-900 px-2 py-1.5 font-mono text-11">
                        <span className="flex-1 select-all break-all text-text-secondary ">
                            {hint.command}
                        </span>
                        <Tooltip content={t("fileEditor.copy")} variant="plain">
                            <button
                                type="button"
                                onClick={copyToClipboard}
                                className="shrink-0 rounded p-0.5 text-text-tertiary hover:text-zinc-600 dark:hover:text-zinc-200 transition-colors"
                            >
                                {copied ? <Check className="h-3 w-3 text-emerald-500" /> : <Copy className="h-3 w-3" />}
                            </button>
                        </Tooltip>
                    </div>

                    {/* Install button */}
                    <button
                        type="button"
                        onClick={runInstall}
                        disabled={installing}
                        className={cn(
                            "mt-2 flex w-full items-center justify-center gap-1.5 rounded px-3 py-1.5 text-11 font-medium transition-colors",
                            installing
                                ? "bg-zinc-200 text-text-tertiary dark:bg-zinc-700  cursor-not-allowed"
                                : "bg-accent-strong text-white hover:opacity-90",
                        )}
                    >
                        {installing ? (
                            <>
                                <Loader2 className="h-3 w-3 animate-spin" />
                                {t("fileStatus.lsp.installing")}
                            </>
                        ) : (
                            <>
                                <Play className="h-3 w-3" />
                                {t("fileStatus.lsp.runInstall")}
                            </>
                        )}
                    </button>

                    {/* Install result */}
                    {installResult && (
                        <div
                            className={cn(
                                "mt-2 rounded px-2 py-1.5 text-11 leading-relaxed",
                                installResult.success
                                    ? "bg-emerald-50 text-emerald-700 dark:bg-emerald-900/30 dark:text-emerald-400"
                                    : "bg-amber-50 text-amber-700 dark:bg-amber-50 dark:text-amber-400",
                            )}
                        >
                            <div className="flex items-start gap-1">
                                {installResult.success ? (
                                    <Check className="h-3 w-3 mt-0.5 shrink-0" />
                                ) : (
                                    <AlertTriangle className="h-3 w-3 mt-0.5 shrink-0" />
                                )}
                                <span className="whitespace-pre-wrap break-all">{installResult.text}</span>
                            </div>
                        </div>
                    )}

                    {hint.url && (
                        <a
                            href={hint.url}
                            target="_blank"
                            rel="noopener noreferrer"
                            className="mt-2 inline-block text-[var(--color-accent)] hover:underline text-11"
                        >
                            {t("fileStatus.lsp.docsLink")}
                        </a>
                    )}
                </div>
            )}
        </div>
    );
}