import { useEffect, useRef, useState } from "react";
import { Check } from "lucide-react";
import { useStatusBarStore } from "../../stores/statusBarStore";
import { useTranslation } from "../../i18n/useTranslation";
import { Tooltip } from "../common/Tooltip";
import { cn } from "../../lib/utils";

/**
 * The app's transient status message — Gateway outage, MQTT loss, debug
 * output — rendered as a pill in the title bar's right cluster.
 *
 * Why the title bar: the bottom status bar is gone, and the title bar is
 * the only surface that is visible on EVERY view without stealing
 * content height. The Gateway outage pill already lives here for the same
 * reason (see `GatewaySwitcherMenu`), so a second pill next to it reads
 * as one family instead of a stray badge.
 *
 * Click-to-copy survives the move: the message is often a stack trace or
 * a URL the user wants to paste, and the pill is the only place it is
 * shown in full (the tooltip is hover-only).
 */
export function StatusPill() {
  const { t } = useTranslation();
  const message = useStatusBarStore((s) => s.message);
  const type = useStatusBarStore((s) => s.type);
  const visible = useStatusBarStore((s) => s.visible);

  // Short-lived "Copied!" feedback. Reset whenever the underlying message
  // changes so a new warning arriving mid-feedback doesn't leave a stale
  // checkmark.
  const [copied, setCopied] = useState(false);
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(() => {
    setCopied(false);
    if (timerRef.current) {
      clearTimeout(timerRef.current);
      timerRef.current = null;
    }
  }, [message]);
  useEffect(() => {
    return () => {
      if (timerRef.current) clearTimeout(timerRef.current);
    };
  }, []);

  if (!visible || !message) return null;

  const handleCopy = () => {
    void navigator.clipboard.writeText(message)
      .then(() => {
        setCopied(true);
        if (timerRef.current) clearTimeout(timerRef.current);
        timerRef.current = setTimeout(() => setCopied(false), 1500);
      })
      .catch(() => {
        // Clipboard API can fail in some sandboxed contexts (e.g. a future
        // hardened WKWebView policy); fail silently — the hover tooltip
        // still surfaces the full message.
      });
  };

  return (
    <Tooltip content={message} variant="plain" position="bottom" delayMs={200} maxWidth="60vw">
      <button
        type="button"
        onClick={handleCopy}
        aria-label={t("common.ariaLabelCopyError")}
        title={t("common.copy")}
        className={cn(
          // `max-w-[min(60vw,32rem)]` caps the pill so a long error never
          // squeezes the title-bar toggles off screen. `truncate` (on the
          // inner span) only engages once the button has a finite width.
          "inline-flex h-6 max-w-[min(60vw,32rem)] items-center gap-1 rounded-md px-2 text-xs transition-colors",
          "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[var(--color-accent)]/40",
          type === "error" &&
            "text-danger-text bg-danger-surface-hover border-danger-border/70 hover:bg-danger-surface-hover/80",
          type === "warning" &&
            "text-warning-quiet bg-warning-surface-hover border-warning-border/70 hover:bg-warning-surface-hover/80",
          type === "info" &&
            "text-text-tertiary bg-surface/80 border border-border-outer/50 hover:bg-[var(--color-accent)]/10 dark:hover:bg-[var(--color-accent)]/15",
        )}
      >
        {copied ? (
          <>
            <Check className="h-3 w-3 shrink-0" aria-hidden="true" />
            <span className="truncate">{t("common.copied")}</span>
          </>
        ) : (
          <span className="truncate">{message}</span>
        )}
      </button>
    </Tooltip>
  );
}
