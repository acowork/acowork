import { useCallback, useState, useEffect, useRef } from "react";
import { AlertTriangle, Check, ChevronDown, ChevronRight, Copy, X } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { cn } from "../../lib/utils";

interface ErrorBoxProps {
    /** Short headline message — shown in the box body. */
    message: string;
    /** Optional long-form details (stack trace, raw error body, etc.).
     *  When provided, a "Show details" disclosure is rendered. */
    details?: string;
    /** Optional close handler. When provided, an X button is rendered. */
    onClose?: () => void;
    /** Extra classes for the outer container. */
    className?: string;
}

/**
 * ErrorBox — standardized inline error display.
 *
 * Replaces the dozens of hand-rolled `<div class="bg-danger-surface ... text-danger-text">{error}</div>`
 * blocks across the app. Colour now comes from the `--color-danger-*` tokens
 * in @theme, so a theme pack recolours it by overriding variables — no
 * `dark:` double-write, no literal hex in the component. Adds:
 *   - `user-select: text` + `data-error` (CSS hook to opt out of body user-select:none)
 *   - One-click Copy button with copied feedback
 *   - Optional collapsible details for long error bodies
 *   - Optional close button
 *   - role="alert" for accessibility
 */
export function ErrorBox({ message, details, onClose, className }: ErrorBoxProps) {
    const { t } = useTranslation();
    const [copied, setCopied] = useState(false);
    const [showDetails, setShowDetails] = useState(false);
    const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

    useEffect(() => {
        return () => {
            if (timerRef.current) clearTimeout(timerRef.current);
        };
    }, []);

    const handleCopy = useCallback(async () => {
        // Combine message + details so the user gets the full context.
        const full = details ? `${message}\n\n${details}` : message;
        try {
            await navigator.clipboard.writeText(full);
            setCopied(true);
            if (timerRef.current) clearTimeout(timerRef.current);
            timerRef.current = setTimeout(() => setCopied(false), 1500);
        } catch {
            // Clipboard API can fail in some sandboxed contexts; fail silently.
        }
    }, [message, details]);

    const hasDetails = Boolean(details);

    return (
        <div
            role="alert"
            data-error="true"
            className={cn(
                "rounded-md border border-danger-border bg-danger-surface p-3 text-xs text-danger-text select-text",
                className,
            )}
        >
            <div className="flex items-start gap-2">
                <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0 text-danger-icon" />
                <div className="min-w-0 flex-1">
                    <p className="break-words whitespace-pre-wrap">{message}</p>
                    {hasDetails && (
                        <button
                            type="button"
                            onClick={() => setShowDetails((v) => !v)}
                            className="mt-1 inline-flex items-center gap-0.5 text-11 font-medium text-danger-text hover:text-danger-text-strong"
                        >
                            {showDetails ? (
                                <ChevronDown className="h-3 w-3" />
                            ) : (
                                <ChevronRight className="h-3 w-3" />
                            )}
                            {showDetails ? t("common.hideDetails") : t("common.showDetails")}
                        </button>
                    )}
                </div>
                <div className="flex shrink-0 items-center gap-1">
                    <button
                        type="button"
                        onClick={handleCopy}
                        aria-label={t("common.ariaLabelCopyError")}
                        title={t("common.copy")}
                        className={cn(
                            "inline-flex h-6 items-center gap-1 rounded px-1.5 text-11 font-medium transition-colors",
                            copied
                                ? "text-ok"
                                : "text-danger-text hover:bg-danger-surface-hover",
                        )}
                    >
                        {copied ? (
                            <>
                                <Check className="h-3 w-3" />
                                {t("common.copied")}
                            </>
                        ) : (
                            <>
                                <Copy className="h-3 w-3" />
                                {t("common.copy")}
                            </>
                        )}
                    </button>
                    {onClose && (
                        <button
                            type="button"
                            onClick={onClose}
                            aria-label={t("common.ariaLabelDismiss")}
                            className="inline-flex h-6 w-6 items-center justify-center rounded text-danger-text hover:bg-danger-surface-hover"
                        >
                            <X className="h-3 w-3" />
                        </button>
                    )}
                </div>
            </div>
            {hasDetails && showDetails && (
                <pre className="mt-2 max-h-48 overflow-auto rounded bg-danger-surface-hover/60 p-2 text-11 leading-relaxed text-danger-text break-all whitespace-pre-wrap">
                    {details}
                </pre>
            )}
        </div>
    );
}