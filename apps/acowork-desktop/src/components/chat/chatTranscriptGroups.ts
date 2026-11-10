/**
 * Transcript layout: one run block per execution session (ADR-106 D3/D4,
 * presentation contract in design 27 §5.5).
 *
 * Storage is per round — a chat holds one bodyless `execution_round` row per
 * round of work, and that is what keeps the timeline honest (D3, and the
 * rejected alternatives in ADR-106). Rendering is per SESSION: the rounds of one
 * execution session belong together the same way one run's tool calls fold into
 * a single `ExploreBlock` on the execution surface. Drawing one block per row
 * turned a long-lived session into a stack of near-identical cards, which is
 * neither of the two grammars.
 *
 * So this module folds a session's reference rows into ONE item anchored at the
 * session's OLDEST row in the loaded window (a continuation adds a round without
 * moving the block), and takes with it the chat's own
 * `user_message` rows those rounds were built from: the block renders the
 * question as the round's own line, so the transcript must not hold the same
 * sentence twice (D6's re-delivery rule is the precedent).
 *
 * One exception, ADR-106's presentation-contract amendment: the round that
 * OPENED the session (`meta.opened_new`) keeps its trigger row as its own
 * bubble. A new session otherwise appears beside the words that created it with
 * no visible pairing at all — "a question and an answer, back and forth" is
 * what makes the block read as an exchange rather than an orphan. The block's
 * line for that round then leads with the reference row's `round_name`, which is
 * what the execution session actually received — not a second copy of the raw
 * sentence.
 *
 * Two shapes stay standalone:
 * - a reference row missing a coordinate addresses nothing, so it has no block
 *   to join and renders its own explicit hole (D3);
 * - a human row that opened a round in ANOTHER session (an `@A @B` fan-out) is
 *   claimed by the first group only, so it stays visible once instead of twice.
 */

import type { ChatMessageRow } from "../../stores/chatSessionStore";
import { isCardNotice, mayAnswerCard } from "../../lib/chatCardAnswer";
import type { CardAnswerHandler } from "../../lib/chatCardAnswer";

/** One round as the block sees it. */
export interface RoundItem {
  /** `instance|session|round` — the block read's key (D5). */
  ref: string;
  /** The durable reference row this round is folded from. */
  roundRow: ChatMessageRow;
  /** The chat's own row that opened the round (`in_reply_to`), when this surface
   *  owns it. Rendered as the round's question line, not as a bubble. */
  triggerRow?: ChatMessageRow;
  /** False when the same sentence is already standing on its own in the
   *  transcript (a fan-out trigger claimed by another session's block, or the
   *  opener round of a new session): then the block shows the answer only, so
   *  one message never appears twice. */
  showUserLine: boolean;
  /** The trigger row stays a bubble of its own: this round opened the session
   *  (`meta.opened_new`), so the transcript shows the human's words that
   *  created it and the block must not claim them. */
  questionOnTranscript?: boolean;
}

/** One execution session's worth of rounds, rendered as a single block. */
export interface RunBlock {
  kind: "run";
  instanceId: string;
  execSessionId: string;
  /** Chronological. The newest is what the block opens to. */
  rounds: RoundItem[];
  /** The block's position in the transcript: the session's OLDEST row here.
   *  Continuations add rounds without moving it (see `anchorOf`). */
  anchorRow: ChatMessageRow;
  /** The addressing question this session is waiting on, when the router asked
   *  and the human has not answered (ADR-106 D6 level 5).
   *
   *  Hung on the block instead of standing at its own transcript position. The
   *  card asks about THIS run ("continue in S?"), so drawing it anywhere else
   *  separates the question from the thing it is about: at the bottom of the
   *  transcript the reader answers a question about a block they cannot see, and
   *  with the block now anchored where the exchange started (rather than at its
   *  newest row) that distance only grows. Its seq still resolves to the block,
   *  so a locate on the card lands where the card is drawn. */
  pendingAsk?: ChatMessageRow;
}

/** Any other transcript row, rendered as itself. */
export interface PlainItem {
  kind: "row";
  row: ChatMessageRow;
}

export type TranscriptItem = RunBlock | PlainItem;

/**
 * The answer a surface may hand the card a transcript item carries, or
 * `undefined` when there is nothing this viewer may answer there.
 *
 * ONE gate for both chat surfaces (1:1 and group). The question can arrive in
 * two shapes — a standalone row, or hung at the tail of the run it asks about
 * (D6 level 5) — and the answer is the same re-delivery either way. Which shape
 * an item is, is a layout fact; whether the card is answerable at all is the
 * card's own vocabulary (`isCardNotice`) and `mayAnswerCard`'s (`to` names the
 * person it asked). Keeping those gates at the call sites is how the 1:1 panel
 * once ended up building a handler for rows only, which left every block-hosted
 * question with no button at all.
 */
export function cardAnswerFor(
  item: TranscriptItem,
  me: string,
  handler: CardAnswerHandler | undefined,
): CardAnswerHandler | undefined {
  if (!handler) return undefined;
  const card = item.kind === "run" ? item.pendingAsk : item.row;
  if (!card || !isCardNotice(card.meta?.notice_type)) return undefined;
  return mayAnswerCard(card, me) ? handler : undefined;
}

export interface TranscriptLayout {
  items: TranscriptItem[];
  /** Every row's render key, folded ones included — a search hit must still
   *  scroll to the block that now holds the row (ADR-097 locate). */
  keyOfSeq: Map<number, string>;
}

/** The block identity: one execution session, as the reference row names it. A
 *  row missing any of the three coordinates addresses nothing (D3), so it has no
 *  session and no round to name. */
function sessionOf(row: ChatMessageRow): {
  instanceId: string;
  execSessionId: string;
  roundId: string;
} | null {
  const instanceId = row.meta?.agent_instance_id?.trim() ?? "";
  const execSessionId = row.meta?.agent_session_id?.trim() ?? "";
  const roundId = row.meta?.round_id?.trim() ?? "";
  if (!instanceId || !execSessionId || !roundId) return null;
  return { instanceId, execSessionId, roundId };
}

export function buildTranscriptItems(
  rows: ChatMessageRow[],
  keyOf: (row: ChatMessageRow) => string,
): TranscriptLayout {
  // The chat's own rows, addressable by the id a reference row answers.
  const byMessageId = new Map<string, ChatMessageRow>();
  for (const r of rows) {
    if (r.kind === "user_message" && r.meta?.message_id) {
      byMessageId.set(r.meta.message_id, r);
    }
  }

  interface Group {
    instanceId: string;
    execSessionId: string;
    rounds: RoundItem[];
  }
  const groups = new Map<string, Group>();
  // A trigger row is folded into exactly one block; the second claimant shows
  // the answer only, because the question is already on the transcript.
  const claimedTriggers = new Set<string>();

  for (const r of rows) {
    if (r.kind !== "execution_round") continue;
    const coord = sessionOf(r);
    if (!coord) continue;
    const key = `${coord.instanceId}|${coord.execSessionId}`;
    const group =
      groups.get(key) ??
      {
        instanceId: coord.instanceId,
        execSessionId: coord.execSessionId,
        rounds: [] as RoundItem[],
      };
    if (!groups.has(key)) groups.set(key, group);

    const triggerId = r.meta?.in_reply_to?.trim() ?? "";
    const trigger = triggerId ? byMessageId.get(triggerId) : undefined;
    // The round that opened a NEW session is the paired half of an exchange:
    // its question stays on the transcript as its own bubble, and the block
    // line leads with the brief instead of repeating the sentence.
    const opener = r.meta?.opened_new === true && !!trigger;
    let showUserLine = true;
    if (trigger && !opener) {
      if (claimedTriggers.has(trigger.meta?.message_id ?? "")) {
        showUserLine = false;
      } else {
        claimedTriggers.add(trigger.meta?.message_id ?? "");
      }
    } else if (opener) {
      showUserLine = false;
    }
    group.rounds.push({
      ref: `${key}|${coord.roundId}`,
      roundRow: r,
      ...(trigger ? { triggerRow: trigger } : {}),
      showUserLine,
      ...(opener ? { questionOnTranscript: true } : {}),
    });
  }

  // A group's anchor is its OLDEST row: the block sits where the session first
  // appeared, and it stays there for the session's whole life.
  //
  // The newest-row anchor this used to use was the reported defect. A session
  // outlives the turn that opened it: every continuation writes one more
  // reference row further down the transcript, and anchoring at the newest one
  // dragged the block down past every row in between — after a few exchanges
  // the chat's questions were all at the top and the block that answered them
  // was at the bottom, the back-and-forth gone. Position is a property of the
  // exchange, not of its latest round, so it is fixed at the exchange's start.
  //
  // Nothing is lost by it. The block reads `RECENT_ROUNDS` newest rounds and
  // scrolls, `keyOfSeq` still maps every folded seq to this block's key (so a
  // locate on a new round lands on the block), and the virtual list keyed on
  // the anchor row now keeps ONE identity for the block's lifetime instead of
  // rebuilding it — and dropping its height cache — on every round.
  const anchorOf = new Map<string, ChatMessageRow>();
  for (const [key, group] of groups) {
    anchorOf.set(key, group.rounds[0]!.roundRow);
  }

  // The addressing question a block answers for (ADR-106 D6 level 5): a
  // `confirm_continue` card proposes ONE session, named by its only option, so
  // the card belongs to that session's block rather than to a transcript
  // position of its own. Skipped — and therefore left standing where it is —
  // when the question is already answered, when it offers anything other than a
  // single target (a multi-candidate `session_choice` has no one block to belong
  // to), or when this window holds no single block for that session: a card
  // hung on a guess is worse than a card in the flow.
  const askAnchorOf = new Map<number, ChatMessageRow>();
  const blocksBySession = new Map<string, string[]>();
  for (const [key, group] of groups) {
    const keys = blocksBySession.get(group.execSessionId) ?? [];
    keys.push(key);
    blocksBySession.set(group.execSessionId, keys);
  }
  const askByKey = new Map<string, ChatMessageRow>();
  for (const r of rows) {
    if (r.kind !== "system_notice" || r.meta?.notice_type !== "confirm_continue") {
      continue;
    }
    const question = r.delivery?.question;
    // A settled question is not folded (the block it would hang on already tells
    // the story), and neither is a dead one: `answerable === false` names a row
    // that is not in the conversation, so nothing can ever take that answer. Both
    // leave the transcript as they are and are painted as nothing (ADR-106 D14).
    if (!question || question.answered || question.answerable === false) continue;
    const options = question.options ?? [];
    if (options.length !== 1) continue;
    const sessionId = options[0]?.session_id?.trim() ?? "";
    if (!sessionId) continue;
    const keys = blocksBySession.get(sessionId) ?? [];
    if (keys.length !== 1) continue;
    // One card per block. The router can propose twice about the same session
    // while the human answers neither; the newest is then the live question, and
    // the older one stays a row of its own. Folding it into the map's overwritten
    // entry would remove it from the transcript without drawing it anywhere — a
    // question the human can no longer answer.
    askByKey.set(keys[0]!, r);
  }
  for (const [key, card] of askByKey) {
    askAnchorOf.set(card.seq, anchorOf.get(key)!);
  }

  const items: TranscriptItem[] = [];
  const keyOfSeq = new Map<number, string>();

  // Which seqs a block draws, computed BEFORE the transcript walk. The previous
  // version resolved this inside the walk, as it met each block, which made the
  // mapping depend on whether a block was emitted before or after the rows it
  // folded: a folded `user_message` written after the block overwrote the
  // block's own mapping with the row's key, and a locate then scrolled to a row
  // that is not in the DOM. It only worked because the block used to sit at the
  // newest row — i.e. always last. Anchor order is a layout choice; this mapping
  // must not inherit it.
  const foldedInto = new Map<number, ChatMessageRow>();
  for (const [key, group] of groups) {
    const anchor = anchorOf.get(key)!;
    for (const round of group.rounds) {
      if (round.roundRow !== anchor) foldedInto.set(round.roundRow.seq, anchor);
      // Only the block that OWNS the question row folds it: a fan-out trigger
      // claimed by another session's block is not rendered by this one, and
      // mapping its seq here would point a locate at a block that does not show
      // the sentence. The opener's trigger is on the transcript as a bubble of
      // its own — `showUserLine` is false for it, so it is not folded.
      if (round.triggerRow && round.showUserLine) {
        foldedInto.set(round.triggerRow.seq, anchor);
      }
    }
  }

  for (const r of rows) {
    const coord = r.kind === "execution_round" ? sessionOf(r) : null;
    const key = coord ? `${coord.instanceId}|${coord.execSessionId}` : null;
    if (key) {
      const group = groups.get(key)!;
      const anchor = anchorOf.get(key)!;
      // Folded rows still resolve to the block's key, so a locate lands on the
      // block that holds them.
      if (r !== anchor) {
        keyOfSeq.set(r.seq, keyOf(foldedInto.get(r.seq) ?? anchor));
        continue;
      }
      keyOfSeq.set(r.seq, keyOf(anchor));
      const pendingAsk = askByKey.get(key);
      items.push({
        kind: "run",
        instanceId: group.instanceId,
        execSessionId: group.execSessionId,
        rounds: group.rounds,
        anchorRow: anchor,
        ...(pendingAsk ? { pendingAsk } : {}),
      });
      continue;
    }
    // The addressing question a block was handed: it is drawn at the block's
    // tail, so its seq resolves there (a locate on the card scrolls to it).
    const askAnchor = askAnchorOf.get(r.seq);
    if (askAnchor) {
      keyOfSeq.set(r.seq, keyOf(askAnchor));
      continue;
    }
    // A question row a block folded: it renders inside that block, so its seq
    // must resolve there — a locate on it scrolls to the block that shows it.
    const folded = foldedInto.get(r.seq);
    if (folded) {
      keyOfSeq.set(r.seq, keyOf(folded));
      continue;
    }
    items.push({ kind: "row", row: r });
    keyOfSeq.set(r.seq, keyOf(r));
  }

  return { items, keyOfSeq };
}

/** The rows a folded block renders as its own question lines — the transcript
 *  must not paint them twice, but the locate/highlight path still needs their
 *  seq to find the block. An opener round's trigger is NOT one of them: it
 *  renders as its own bubble, and its seq belongs to that row. */
export function blockSeqs(block: RunBlock): number[] {
  const seqs: number[] = [block.anchorRow.seq];
  // The addressing question is drawn here too, so this block owns its seq —
  // a highlight on that row belongs on this item, not on nothing.
  if (block.pendingAsk) seqs.push(block.pendingAsk.seq);
  for (const round of block.rounds) {
    seqs.push(round.roundRow.seq);
    if (round.triggerRow && !round.questionOnTranscript) {
      seqs.push(round.triggerRow.seq);
    }
  }
  return [...new Set(seqs)];
}
