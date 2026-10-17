/**
 * CopyButton — the app's one copy affordance.
 *
 * Why this exists: five surfaces grew their own copy button
 * (`ErrorBox`, `CodeBlock`, `DebugPanel`, `PreviewPane`, `LspIndicator`),
 * each re-implementing the same three steps — write to the clipboard, flip
 * to a "copied" state, reset on a timer. They drifted: `ErrorBox` reset
 * after 1500 ms while the rest used 2000 ms, and `DebugPanel`/`LspIndicator`
 * hard-coded the English word "Copied" instead of reading i18n, so those
 * two buttons stayed English inside a Chinese UI. Every one of them also
 * forgot to clear the timer on unmount, so a copy-then-navigate left a
 * pending `setState` on a dead component.
 *
 * This component owns the whole lifecycle so those four differences cannot
 * come back. It does not prescribe colours — `tone` picks a palette and
 * callers override via `className` when they need a themed one.
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { Check, Copy } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { cn } from "../../lib/utils";

/** How long the "Copied" confirmation stays up. One value for the whole
 *  app; the old 1500/2000 split had no basis in user-visible need. */
const COPIED_RESET_MS = 2000;

export interface CopyButtonProps {
    /** Plain text written to the clipboard. Changing it while the "copied"
     *  confirmation is up does not restart the timer — the confirmation
     *  reports the click, not the content. */
    text: string;
    /** Accessible name. Defaults to the translated "Copy"; pass a more
     *  specific label when the target is ambiguous ("Copy summary"). */
    ariaLabel?: string;
    /** `ghost` matches the tertiary chrome used in preview/debug panes;
     *  `solid` is the higher-contrast variant for primary surfaces. */
    tone?: "ghost" | "solid";
    /** Icon-only. Drops the label and keeps the icon as the affordance;
     *  the `aria-label` is then required for screen readers. */
    iconOnly?: boolean;
    /** Rendered after the label — for an external marker such as a
     *  per-source citation counter. */
    children?: React.ReactNode;
    className?: string;
    testId?: string;
}

export function CopyButton({
    text,
    ariaLabel,
    tone = "ghost",
    iconOnly = false,
    children,
    className = "",
    testId,
}: CopyButtonProps) {
    const { t } = useTranslation();
    const [copied, setCopied] = useState(false);
    const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

    // A pending reset must not fire into an unmounted component.
    useEffect(() => {
        return () => {
            if (timerRef.current) clearTimeout(timerRef.current);
        };
    }, []);

    const handleCopy = useCallback(async () => {
        // Nothing to copy: leave the button inert rather than reporting a
        // success that copied nothing.
        if (!text) return;
        try {
            await navigator.clipboard.writeText(text);
        } catch {
            // The Clipboard API is unavailable in some sandboxed contexts
            // (and without a user gesture in older WebViews). Fall back to
            // the legacy textarea path so the user still gets the text out.
            const ta = document.createElement("textarea");
            ta.value = text;
            document.body.appendChild(ta);
            ta.select();
            document.execCommand("copy");
            document.body.removeChild(ta);
        }
        setCopied(true);
        if (timerRef.current) clearTimeout(timerRef.current);
        timerRef.current = setTimeout(() => setCopied(false), COPIED_RESET_MS);
    }, [text]);

    const solid = tone === "solid";
    const stateCls = copied
        ? "text-emerald-600 dark:text-emerald-400"
        : solid
          ? "text-text-secondary hover:bg-zinc-100 hover:text-text dark:hover:bg-zinc-700/50"
          : "text-text-tertiary hover:bg-zinc-100 hover:text-text-secondary dark:hover:bg-zinc-700/50";

    const label = copied ? t("common.copied") : t("common.copy");

    return (
        <button
            type="button"
            onClick={handleCopy}
            disabled={!text}
            aria-label={ariaLabel ?? label}
            title={copied ? undefined : ariaLabel}
            data-testid={testId}
            data-copied={copied ? "true" : undefined}
            className={cn(
                "inline-flex shrink-0 items-center rounded font-medium transition-colors",
                "disabled:cursor-not-allowed disabled:opacity-50 disabled:hover:bg-transparent",
                iconOnly ? "h-6 w-6 justify-center" : "h-6 gap-1 px-1.5",
                "text-11",
                stateCls,
                className,
            )}
        >
            {copied ? (
                <Check className="h-3 w-3" aria-hidden="true" />
            ) : (
                <Copy className="h-3 w-3" aria-hidden="true" />
            )}
            {!iconOnly && <span>{label}</span>}
            {children}
        </button>
    );
}
