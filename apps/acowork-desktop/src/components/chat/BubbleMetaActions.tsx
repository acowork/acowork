/**
 * BubbleMetaActions — the hover-revealed controls that sit left of a user
 * bubble's timestamp.
 *
 * Why one component: the affordance is wanted on four render paths — the two
 * execution-surface user bubbles (`MessageBubble`, `UserWithAttachmentsBubble`)
 * and the two chat-session ones (`AgentChatView`, `GroupChatPanel`) — and the
 * rule that has to stay identical across them is not the icon set, it is the
 * INERT state. A control that is merely `opacity-0` still takes clicks and
 * still reserves width, which is how a button came to be stranded mid-bubble
 * once before (see the agent meta row in `AgentChatView`). So:
 *
 *  - collapsed to zero width and unclickable until the pointer is on the
 *    bubble, then unfurled — the cluster grows leftward from the timestamp,
 *    which never moves;
 *  - unfurled for the whole copy confirmation even if the pointer leaves.
 *    A checkmark swallowed by a mouse-move reports nothing, and the click
 *    exists to give that feedback.
 *
 * The cluster is right-styled with the transcript's own action row
 * (`RunBlockActionRow`): a round tinted-on-hover icon on the global accent,
 * no new color values.
 */
import { useCallback, useState } from "react";
import { RotateCcw } from "lucide-react";
import { CopyButton } from "../common/CopyButton";
import { useTranslation } from "../../i18n/useTranslation";
import { useActiveSessionReadOnly } from "../../lib/session-write-access";
import { cn } from "../../lib/utils";

export interface BubbleMetaActionsProps {
    /** Body the copy button puts on the clipboard. Empty or absent renders
     *  no copy button — there is nothing to confirm. */
    copyText?: string;
    /** Re-send this turn's text into the session. Absent on the chat
     *  surfaces, where a row is a chat-log entry and not a turn in an agent
     *  session; present on the execution surface, where it is the same call
     *  the right-click menu makes. */
    onResend?: () => void;
    className?: string;
}

/** The meta row is `text-10` chrome; a 24px button beside it would be the
 *  loudest thing on the line. */
const META_BUTTON =
    "inline-flex h-5 w-5 shrink-0 items-center justify-center rounded-full text-text-tertiary " +
    "transition-colors hover:bg-[var(--color-accent)]/12 hover:text-[var(--color-accent)] " +
    "focus-visible:bg-[var(--color-accent)]/12 focus-visible:text-[var(--color-accent)]";

export function BubbleMetaActions({ copyText, onResend, className }: BubbleMetaActionsProps) {
    const { t } = useTranslation();
    const [copied, setCopied] = useState(false);
    const handleCopiedChange = useCallback((value: boolean) => setCopied(value), []);
    // A re-send is a session write, and `session-write-access.ts` is explicit
    // about who owns that gate: the CONTROL renders it, because threading a
    // `readOnly` prop into each child is how three write controls came to be
    // left enabled on a read-only session. So the icon asks for itself
    // rather than waiting to be told — a fifth surface that passes
    // `onResend` gets the gate without knowing to ask for one.
    // Copy is not a write and stays available.
    const readOnly = useActiveSessionReadOnly();
    const canResend = !!onResend && !readOnly;

    if (!copyText && !canResend) return null;

    return (
        <div
            className={cn(
                "flex items-center gap-1 overflow-hidden transition-all",
                // Unfurled by the pointer, by the keyboard reaching a button
                // inside it, or by the copy confirmation — the three ways
                // this row can legitimately be in use.
                "w-0 shrink opacity-0 group-hover:w-auto group-hover:shrink group-hover:opacity-100",
                "has-[:focus-visible]:w-auto has-[:focus-visible]:shrink has-[:focus-visible]:opacity-100",
                copied && "w-auto shrink opacity-100",
                className,
            )}
        >
            {canResend && (
                <button
                    type="button"
                    onClick={onResend}
                    aria-label={t("chatPanel.resend")}
                    title={t("chatPanel.resend")}
                    className={META_BUTTON}
                >
                    <RotateCcw className="h-3 w-3" aria-hidden="true" />
                </button>
            )}
            {copyText && (
                <CopyButton
                    text={copyText}
                    iconOnly
                    onCopiedChange={handleCopiedChange}
                    ariaLabel={t("chatPanel.copy")}
                    className="h-5 w-5"
                />
            )}
        </div>
    );
}
