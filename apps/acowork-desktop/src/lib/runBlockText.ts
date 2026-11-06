//! The plain-text export of one execution-session block (design 27 §5.5.5).
//!
//! A Chat's execution block is ONE session's worth of rounds, and the block's
//! action row offers "copy" over that whole thing — the human wants the run's
//! content in another window, not a per-round excerpt. The rendering component
//! owns *what each round says* (the question's precedence between the routing
//! brief and the round's own projection, the settled outcome when there is no
//! answer body); this module owns only the *shape* of the export, so the layout
//! rules are testable without mounting a transcript.
//!
//! Three rules the shape keeps:
//! - **the newest round is last, and every round is exported** — including the
//!   rounds the block keeps behind "show earlier". The button says "copy this
//!   run", and a run that silently dropped its first five rounds would hand the
//!   human a document that reads as if the session started at round six.
//! - **a hole is exported as a hole.** A round whose content could not be read
//!   (`not_found` / `unreachable`) is stated in one line; the export never
//!   invents an answer to keep the numbering tidy (ADR-106 D3/D5).
//! - **Markdown, not a chat log.** The answer bodies are Markdown already, so
//!   the export is Markdown too: a heading per round and the question prefixed,
//!   the answer pasted verbatim so its own fences/lists/tables survive a paste
//!   into an editor or another model.
//!
//! Labels arrive from the caller's `t()` rather than being hard-coded: the copy
//! is read by a human, so it speaks the UI's language. Keeping them out of here
//! is what makes this a pure function.

/** One round, already resolved by the component that holds the entries. */
export interface RunBlockTextRound {
  /** 1-based position in the SESSION's round list — the block's own numbering,
   *  so a round pinned above the visible window keeps its number. */
  index: number;
  /** The round's question (may be empty: a run that iterated on its own). */
  question: string;
  /** The round's answer body, or the settled outcome's copy when there is no
   *  body to show. Empty only for a round that never produced either. */
  answer: string;
  /** The round's content could not be read at all. */
  unreadable?: boolean;
}

/** Localized words the export needs. `t` is inlined by the caller so this file
 *  has no dependency on the i18n runtime. */
export interface RunBlockTextLabels {
  /** `Round {{n}}` in the UI's language. */
  round: (n: number) => string;
  /** `Q:` / `问：` — the question line's prefix. */
  questionPrefix: string;
  /** What a round with an unreadable body says instead of a body. */
  unreadable: string;
}

export interface RunBlockTextInput {
  /** The session's display name (the block header's own label). */
  name: string;
  rounds: RunBlockTextRound[];
  labels: RunBlockTextLabels;
}

/** Trim without eating a Markdown body's intentional trailing blank line —
 *  only the outer whitespace goes. */
function block(text: string): string {
  return text.replace(/^\s+|\s+$/g, "");
}

/**
 * Render one block as Markdown. Total and deterministic: the same rounds always
 * produce the same string, empty rounds produce a heading with nothing under it
 * (never a fabricated sentence), and a block with no rounds at all still names
 * the session so the paste is not an empty clipboard.
 */
export function runBlockToText({
  name,
  rounds,
  labels,
}: RunBlockTextInput): string {
  const title = block(name);
  // No name, no heading: an empty `#` line is worse than saying nothing, and
  // every caller resolves a name before it asks for the export.
  const parts: string[] = title ? [`# ${title}`] : [];

  for (const round of rounds) {
    const question = block(round.question);
    const answer = block(round.answer);
    const body = round.unreadable ? labels.unreadable : answer;
    parts.push(`## ${labels.round(round.index)}`);
    // A question the run never received (a round the session started itself)
    // is an absence, not an empty quote: skip the line instead of printing a
    // dangling prefix.
    if (question) parts.push(`${labels.questionPrefix}${question}`);
    if (body) parts.push(body);
  }

  return `${parts.join("\n\n")}\n`;
}
