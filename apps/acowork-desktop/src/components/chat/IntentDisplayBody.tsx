/**
 * An agent-origin turn as a human reads it (design 27 §5.4.5).
 *
 * The turn has two texts and they answer different questions: `content` is the
 * machine envelope (`[intent:from:action] {params}` + the coordinates a reply is
 * posted to) that the MODEL reads, and the display projection beside it is the
 * same turn written for a person. Three parts, always in this order:
 *
 * 1. the projection's Markdown — the default text;
 * 2. a note when the producer cut it (the envelope still holds everything);
 * 3. the envelope itself, folded — reachable because it is the only thing that
 *    carries the reply coordinates, never painted by default because it is what
 *    read as `[intent:2f3c…:analyze] {"title":…}` to a human.
 *
 * Two surfaces paint this (a session-message bubble and an expanded execution
 * round) and they must not state the order, the i18n keys or the fold differently.
 * The wrapper — fill, padding, font size — stays with each surface: that is
 * layout, and the two sit in different containers.
 */
import type { CSSProperties } from "react";
import type { IntentDisplay } from "../../lib/types";
import { useTranslation } from "../../i18n/useTranslation";
import { ChatMarkdown } from "./ChatMarkdown";

export function IntentDisplayBody({
  display,
  envelope,
  fontSize,
}: {
  /** The Runtime's projection for this turn. Required: with no display the
   *  caller renders the stored body and never reaches this component. */
  display: IntentDisplay;
  /** The machine text this display was derived from. Omitted when the surface
   *  has no access to it (a projection need not carry the raw row). */
  envelope?: string | null;
  /** Bubble font size, so the two surfaces scale together. */
  fontSize?: CSSProperties;
}) {
  const { t } = useTranslation();
  const raw = envelope?.trim() ? envelope : null;
  // `body` empty is a shape the producer may write (a payload with no words:
  // every field is `serde(default)`), and the summary is then the only human
  // text there is. Painting nothing would leave the panel blank while the
  // envelope sits folded beside it.
  const text = display.body || display.summary;
  return (
    <>
      <ChatMarkdown content={text} style={fontSize} />
      {display.truncated ? (
        <div className="mt-1 text-text-tertiary">
          {t("chatView.block.displayTruncated")}
        </div>
      ) : null}
      {raw ? (
        <details className="mt-1">
          <summary className="cursor-pointer text-text-tertiary">
            {t("chatView.block.rawEnvelope")}
          </summary>
          <pre className="mt-1 whitespace-pre-wrap break-words rounded bg-zinc-100 p-2 text-text-tertiary dark:bg-zinc-800">
            {raw}
          </pre>
        </details>
      ) : null}
    </>
  );
}
