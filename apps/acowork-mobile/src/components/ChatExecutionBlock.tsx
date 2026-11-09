/**
 * ChatExecutionBlock — one execution SESSION's worth of rounds, drawn as one
 * collapsible block (ADR-106 D3/D4/D8/D9; design 27 §5.5). The mobile twin of
 * Desktop's `components/chat/ChatExecutionBlock`.
 *
 * The chat holds no copy of the work. Its transcript carries one bodyless
 * reference row per round, and the layout layer (`lib/chatTranscriptGroups`)
 * folds a session's rows into ONE block. Drawing a card per row turned a
 * long-lived run into a stack of near-identical boxes — on a 390pt screen that
 * is not just ugly, it buries the conversation.
 *
 * The visual language is this app's execution-surface `ExploreBlock`, class for
 * class: a quiet pill as the collapse header, a hairline list under it, one row
 * per item with a glyph and a caret, and the pair's content in stacked panels
 * under the open row. The only thing invented here is what a row pairs:
 * `user` / `assistant` instead of `tool_call` / `tool_result`, plus the
 * session's live state next to the count. Design 27 §6.5's mobile deltas:
 * - the newest round's answer shows inline — folding a reply behind a tap would
 *   make reading it cost more here than on the surface that owns the transcript;
 * - a decision card a round is parked on stays visible even when the block is
 *   folded, and its buttons live in a BOTTOM SHEET: on a thumb-sized screen the
 *   tap that approves a tool call must not be reachable by accident.
 *
 * Three rules this component owns:
 * - "in progress" is positive evidence only — a `pending` projection read (D9).
 *   The watched session's `session_state` refines the header copy; it never
 *   substitutes for that read.
 * - the card a round is parked on lives HERE, in the block (D8). Answering it
 *   posts to the execution session's own write entry, where the owner check
 *   already runs; no chat row is written for the decision.
 * - a hole is drawn as a hole (D5): a round that cannot be read says so, instead
 *   of the transcript inventing an outcome for it.
 */

import { memo, useEffect, useMemo, useRef, useState, type ReactNode } from 'react'
import { sendAnswer, sendApproval } from '../lib/api'
import { blockStatusText, isFailureStatus } from '../lib/chatBlocks'
import type { RunBlock } from '../lib/chatTranscriptGroups'
import { renderMarkdown } from '../lib/markdown'
import { useChatBlockStore, useChatBlocks } from '../stores/chatBlockStore'
import { isActiveStatus, useChatStore } from '../stores/chatStore'
import { useAgentStore } from '../stores/agentStore'
import { Sheet } from './ui'
import type { BlockCard, ChatBlockEntry, LiveStatus } from '../lib/types'

/** How many of a session's rounds stay visible when the block is opened. On a
 *  phone the transcript itself scrolls, so the freshest pair is what a reader
 *  came for; the rest are one tap away inside the block. */
const RECENT_ROUNDS = 2
/** How many more each reveal adds, so "show earlier" never dumps a long run on
 *  a 390pt screen at once. */
const REVEAL_STEP = 4

interface ChatExecutionBlockProps {
  /** The session's folded rounds, in order; the newest is the block's anchor. */
  block: RunBlock
  /** The Chat this block belongs to — the block read is participation-checked
   *  against it, so it is part of the address, not a filter (D10). */
  chatSessionId: string
  /** Avatar + name above the block, the same header a bubble wears. */
  showHeader?: boolean
  agentAvatar: ReactNode
  agentName: string
  /** Something follows this block in the transcript. This is `ExploreBlock`'s
   *  follow-up condition — the ONE that auto-collapses: while the run is the
   *  freshest thing on screen it stays open. */
  hasFollowUp?: boolean
  /** Drill into the execution session that owns these rounds. */
  onJumpToExec: (agentInstanceId: string, execSid: string) => void
  /** Address the composer at one round (D6 level 1) — what a `pause` card's
   *  "continue" rides. `quote` is the question the human is answering. */
  onReplyToRound: (rowId: string, quote: string) => void
  /** A viewer who is not a round's answerer (`answerable_by`): the question
   *  shows, the buttons do not (D10's read/write split). */
  canAnswer?: boolean
  /** ADR-106 revision 2026-11-04: the `@` sheet's "去续投" located this run —
   *  open the block and focus its in-place reply row on the rising edge. */
  focusInput?: boolean
  /** The block's in-place reply: continue the freshest round straight down the
   *  level-1 branch (`deliver(text, rowId)`). Absent = no in-place box (a
   *  surface that only drills into the execution view). */
  onReplyInPlace?: (rowId: string, text: string) => void
}

/** The session's live state, in as few words as a header line can carry. This is
 *  `session_state` talk, not the round's (D9 keeps liveness a `pending` read). */
function liveStatusText(s: LiveStatus): string {
  switch (s.status) {
    case 'llm_awaiting_first_chunk':
      return '等模型响应'
    case 'thinking':
      return '思考中'
    case 'llm_streaming':
      return '正在输出'
    case 'tool_executing':
      return '执行工具中'
    case 'waiting_approval':
      return '等你审批'
    case 'paused':
      return '已暂停'
    case 'errored':
      return '出错了'
    default:
      return '空闲'
  }
}

/** One-line summary of a question: whitespace folded, cut at what a row holds. */
function summarize(text: string, max = 40): string {
  const collapsed = text.replace(/\s+/g, ' ').trim()
  return collapsed.length > max ? `${collapsed.slice(0, max - 1)}…` : collapsed
}

export const ChatExecutionBlock = memo(function ChatExecutionBlock({
  block,
  chatSessionId,
  showHeader,
  agentAvatar,
  agentName,
  hasFollowUp,
  onJumpToExec,
  onReplyToRound,
  canAnswer = true,
  focusInput = false,
  onReplyInPlace,
}: ChatExecutionBlockProps) {
  const instanceId = block.instanceId
  const execSid = block.execSessionId
  const refs = useMemo(() => block.rounds.map((r) => r.ref), [block])
  const entries = useChatBlocks(chatSessionId, refs)
  const reload = useChatBlockStore((s) => s.reload)
  // The run's own name, read the way the session list reads it. Empty until the
  // session is titled — a fresh turn's block then wears its opening question.
  const runTitle = useAgentStore((s) =>
    s.agents[instanceId]?.sessions.find((x) => x.session_id === execSid)?.title,
  )
  // Mobile polls the state of ONE foreground session per agent (v1.1 §8), so a
  // live line is only honest while this block's session IS the watched one.
  const liveStatus = useChatStore((s) => {
    const view = s.agentStates[instanceId]
    return view && view.activeSessionId === execSid ? view.status : null
  })

  // D9: a `pending` read is the ONLY positive evidence a round is still going —
  // and the round that speaks for the block is its NEWEST one, the row the block
  // is anchored at. An older read that never resolves must not keep a finished
  // run looking live (t-1b7bdb21).
  const newest = block.rounds[block.rounds.length - 1]
  const newestEntry = newest ? entries[newest.ref] : undefined
  const newestPending = newestEntry?.status === 'pending'
  // A round parked on a card is not running: it is waiting for the human, and
  // "执行中" over an unanswered card tells the reader to wait when the block is
  // in fact waiting for them.
  const onCard = newestPending && !!newestEntry?.projection?.card
  const outcome = newestPending
    ? onCard
      ? 'waiting'
      : 'in_progress'
    : newestEntry?.status === 'ok'
      ? newestEntry.projection?.status ?? 'unknown'
      : // A hole or an outage says what it is: no read is not evidence of a
        // running round, and a missing word is not evidence of a finished one.
        'unknown'
  const inProgress = outcome === 'in_progress'
  // A round parked on a decision the human has not answered.
  const hasOpenCard = block.rounds.some((r) => {
    const entry = entries[r.ref]
    return !!entry?.projection?.card && entry.status === 'pending'
  })
  // The session's own activity outranks the block's settled rounds: a session
  // that IS busy is the freshest fact on this surface, and the round it is busy
  // with may simply not have reached this Chat as a reference row yet. What the
  // live state may NOT do is create liveness out of nothing — the live marker and
  // the auto-open below stay tied to this block's own pending read (D9).
  const shownState =
    (liveStatus && isActiveStatus(liveStatus) ? liveStatusText(liveStatus) : '') ||
    blockStatusText(outcome)

  // Rounds beyond the recent window stay in the block, one tap away — except the
  // round that owns this Chat's own question. Its `user_message` row was folded
  // into the block (one sentence, one place), so windowing that round away hides
  // the human's own words from the whole transcript and leaves the visible rows
  // looking like the agent answering questions nobody asked here. Rounds the
  // block produced on its own stay behind the reveal, which is what keeps a long
  // run from pushing the conversation off a 390pt screen.
  const [revealed, setRevealed] = useState(0)
  const pinned = new Set<number>()
  block.rounds.forEach((round, i) => {
    if (round.showUserLine && round.triggerRow) pinned.add(i)
  })
  const start = Math.max(0, block.rounds.length - RECENT_ROUNDS - revealed)
  const shownIndices = block.rounds.map((_, i) => i).filter((i) => i >= start || pinned.has(i))
  const shown = shownIndices.map((i) => block.rounds[i]!)
  const hiddenOlder = block.rounds.length - shownIndices.length

  // `ExploreBlock`'s state machine, on the same conditions: open while it is the
  // freshest thing on screen, fold once the conversation moves past it, and never
  // re-open over a reader who collapsed it by hand.
  const [open, setOpen] = useState(!hasFollowUp)
  const manuallyCollapsed = useRef(false)

  useEffect(() => {
    if ((inProgress || hasOpenCard) && !manuallyCollapsed.current) setOpen(true)
  }, [inProgress, hasOpenCard])

  useEffect(() => {
    if (hasFollowUp && !inProgress && !hasOpenCard) {
      setOpen(false)
      manuallyCollapsed.current = false
    }
  }, [hasFollowUp, inProgress, hasOpenCard])

  // A locate from the `@` sheet's "去续投" opens the block on its rising edge,
  // so the human arrives at the run they pointed at, not a collapsed header. The
  // in-place reply row is inside the body, so opening it is what makes the focus
  // land. This overrides a manual collapse on purpose — it was an explicit ask.
  const lastFocus = useRef(false)
  useEffect(() => {
    if (focusInput && !lastFocus.current) {
      setOpen(true)
      manuallyCollapsed.current = false
    }
    lastFocus.current = focusInput
  }, [focusInput])

  const question0 = block.rounds.find((r) => r.triggerRow)?.triggerRow?.body ?? ''
  // No Chat row opened this run (a PM dispatch, ADR-106 D12, or a turn composed
  // on the execution surface): the question the session actually received is the
  // only human text the run has, so it names the run.
  const asked0 =
    question0.trim() ||
    block.rounds
      .map((r) => {
        const user = entries[r.ref]?.projection?.user
        // A peer round's `user.body` is the machine envelope; the display
        // projection is the sentence that names the work (design 27 §5.4.5).
        return user?.display?.summary || user?.display?.body || user?.body || ''
      })
      .find((body) => body.trim() !== '') ||
    ''
  const name = runTitle?.trim() || summarize(asked0, 24) || '执行会话'
  // The task coordinate (ADR-106 D13): it rides the ROUND, and the header is
  // where a reader looks for "which piece of work is this".
  const taskId = block.rounds.find((r) => r.roundRow.meta?.task_id)?.roundRow.meta?.task_id

  // The rounds whose projection still carries a decision, whether or not the
  // body is open: §6.5's rule is that a parked round stays answerable. The card
  // comes from the read itself, so the transcript never judges whether the
  // question is still open — the execution side does.
  const parked = block.rounds.filter((r) => !!entries[r.ref]?.projection?.card)

  return (
    <div className="chat-run" data-testid="exec-block" data-session={`${instanceId}|${execSid}`}>
      {showHeader ? (
        <div className="msg-group msg-group-peer">
          <div className="msg-header">
            <div className="msg-avatar">{agentAvatar}</div>
            <div className="msg-sender">
              <span className="msg-sender-name">{agentName}</span>
            </div>
          </div>
        </div>
      ) : null}

      {/* The drill keeps its own chrome and sits ABOVE the block: it opens the
          execution session, a different object from the rounds listed inside it. */}
      <button
        type="button"
        className="chat-run-drill"
        title={name}
        onClick={() => onJumpToExec(instanceId, execSid)}
      >
        查看执行
      </button>

      <button
        type="button"
        className="explore-toggle"
        onClick={() => {
          const next = !open
          setOpen(next)
          manuallyCollapsed.current = next ? false : true
        }}
        aria-expanded={open}
      >
        <span
          className={`explore-toggle-icon${inProgress ? ' is-live' : ''}${
            isFailureStatus(newestEntry?.projection?.status ?? '') ? ' is-danger' : ''
          }`}
          aria-hidden
        >
          {inProgress ? '◌' : isFailureStatus(newestEntry?.projection?.status ?? '') ? '!' : '⚙'}
        </span>
        <span className="explore-toggle-label">{name}</span>
        <span className="explore-toggle-summary">
          {block.rounds.length} 轮{shownState ? ` · ${shownState}` : ''}
          {taskId ? ` · ${taskId}` : ''}
        </span>
        <span className="explore-toggle-caret" aria-hidden>
          {open ? '⌄' : '›'}
        </span>
      </button>

      {open ? (
        <div className="explore-body">
          {hiddenOlder > 0 ? (
            <button
              type="button"
              className="chat-run-older"
              onClick={() => setRevealed((n) => n + REVEAL_STEP)}
            >
              查看更早 {hiddenOlder} 轮
            </button>
          ) : null}
          {shown.map((round, i) => (
            <RoundItem
              key={round.ref}
              round={round}
              /** The label is the round's position in the SESSION, so a pinned
               *  round above the window keeps its own number. */
              index={shownIndices[i]!}
              /** The freshest round of an open block shows its pair; earlier ones
               *  fold to one line, the way a long run's older tool calls do. */
              defaultOpen={shownIndices[i] === block.rounds.length - 1}
              chatSessionId={chatSessionId}
              entry={entries[round.ref]}
              reload={reload}
            />
          ))}
          {/* ADR-106 revision 2026-11-04: continue the run in place. The row is
              present whenever the block is OPEN and the viewer may answer, and
              NEVER when the block is folded (§6.5 — a collapsed run shows no
              input). Its send rides `deliver(text, rowId)` — the level-1 branch —
              so the claim and the wait line are the same machinery a card answer
              already uses. The freshest round's reference row is what it answers. */}
          {canAnswer && onReplyInPlace ? (
            <BlockReplyRow
              rowId={newest?.roundRow.meta?.message_id ?? ''}
              focusSignal={focusInput}
              onSend={onReplyInPlace}
            />
          ) : null}
        </div>
      ) : null}

      {parked.map((round) => (
        <BlockCardSurface
          key={round.ref}
          card={entries[round.ref]!.projection!.card!}
          canAnswer={canAnswer}
          instanceId={instanceId}
          execSid={execSid}
          rowId={round.roundRow.meta?.message_id ?? ''}
          chatSessionId={chatSessionId}
          blockRef={round.ref}
          onReplyToRound={onReplyToRound}
        />
      ))}
    </div>
  )
})

interface RoundItemProps {
  round: RunBlock['rounds'][number]
  /** Position in the session's round list (1-based) — the row's label. */
  index: number
  defaultOpen: boolean
  chatSessionId: string
  entry: ChatBlockEntry | undefined
  reload: (chatId: string, refs: string[]) => void
}

/** One round: a question line that opens into the question/answer pair, in
 *  `ExploreBlock`'s row-plus-panels chrome. The decision a round is parked on is
 *  NOT drawn here — it rides under the block, so it stays answerable while the
 *  list is folded (§6.5). */
function RoundItem({ round, index, defaultOpen, chatSessionId, entry, reload }: RoundItemProps) {
  const [open, setOpen] = useState(defaultOpen)
  const projection = entry?.projection
  // An opener round's sentence stands on the transcript as its own bubble —
  // the block's line leads with the reference row's one-line `round_name`,
  // never with a second copy of the raw text and never with the projection's
  // stored question (that one is the delivered prompt, reference parts and all).
  // The Runtime's `reference_name` owns what may be written there: a PM dispatch
  // is named by its own text, never by the wake-up playbook, and an agent-origin
  // round is named by its display projection.
  const question =
    (round.showUserLine ? round.triggerRow?.body : '') ||
    (round.questionOnTranscript
      ? (round.roundRow.meta?.round_name ??
          // Rows written before the rename carry the fact under the key it
          // borrowed from the envelope's PM playbook. Read, never written.
          round.roundRow.meta?.assignment_brief ??
          '')
      : '') ||
    // A peer round (design 27 §5.4.5): `user.body` is the machine envelope, and
    // the display projection beside it is the sentence a human reads.
    projection?.user?.display?.summary ||
    projection?.user?.display?.body ||
    projection?.user?.body ||
    ''
  const display = projection?.user?.display ?? null
  // The expanded panel shows the full text, not the row's one-liner. A display
  // PRESENT is a decision, not a hint: its empty `body` means the producer wrote
  // only a one-line `summary`, and falling through to `user.body` there painted
  // the machine envelope into the panel the display exists to replace.
  const questionBody = display
    ? display.body || display.summary
    : (round.showUserLine ? (round.triggerRow?.body ?? '') : '') ||
      projection?.user?.body ||
      ''
  // The machine format, reachable but never painted by default.
  const rawEnvelope = display ? (projection?.user?.body ?? '') : ''
  // The brief is the OPENER round's line and nothing else (ADR-106 revision
  // 2026-11-04). As a fallback for any round it published the PM wake-up
  // playbook as a round's question whenever the projection could not be read —
  // the reference row's name and the system slot are different things.
  const summary = summarize(question) || '（无提问内容）'
  const pending = entry?.status === 'pending'
  // A row parked on a card is waiting for the human, not executing — the same
  // rule the header follows, because a row that says "执行中" over the card it is
  // waiting on is the block contradicting its own buttons.
  const status = !pending
    ? entry?.status === 'ok'
      ? projection?.status ?? 'unknown'
      : 'unknown'
    : projection?.card
      ? 'waiting'
      : 'in_progress'
  const inProgress = status === 'in_progress'

  return (
    <div className="chat-run-round" data-seq={round.roundRow.seq}>
      <button
        type="button"
        className="chat-run-row"
        onClick={() => setOpen((v) => !v)}
        aria-expanded={open}
      >
        <span className="chat-run-row-name">
          第 {index + 1} 轮
          <span
            className={`chat-run-glyph${inProgress ? ' is-live' : ''}${
              isFailureStatus(status) ? ' is-danger' : ''
            }`}
            aria-hidden
          >
            {inProgress ? '◌' : isFailureStatus(status) ? '!' : '✓'}
          </span>
        </span>
        <span className="explore-item-args">{summary}</span>
        <span className="chat-run-row-caret" aria-hidden>
          {open ? '⌄' : '›'}
        </span>
      </button>

      {open ? (
        <div className="chat-run-detail">
          {round.showUserLine && question ? <div className="chat-run-question">{question}</div> : null}
          {/* An agent-origin round has no bubble of its own on the transcript
              (ADR-106 D12), so this is the only place its question appears — as
              the display projection the turn carried, with the machine envelope
              one tap away. Painting `projection.user.body` here is what made a
              block read `[intent:2f3c…:analyze] {"title":…}`. */}
          {!round.showUserLine && display && questionBody ? (
            <div className="chat-run-question">
              {renderMarkdown(questionBody)}
              {display.truncated ? (
                <span className="chat-run-note">正文已截断，完整内容见原始信封</span>
              ) : null}
              {rawEnvelope ? (
                <details className="chat-run-raw">
                  <summary>原始信封（机器格式）</summary>
                  <pre>{rawEnvelope}</pre>
                </details>
              ) : null}
            </div>
          ) : null}
          <RoundBody
            entry={entry}
            chatSessionId={chatSessionId}
            reload={reload}
            blockRef={round.ref}
            status={status}
          />
        </div>
      ) : null}
    </div>
  )
}

interface RoundBodyProps {
  entry: ChatBlockEntry | undefined
  chatSessionId: string
  reload: (chatId: string, refs: string[]) => void
  /** The ref this round is read under — the retry asks for exactly it. */
  blockRef: string
  status: string
}

/** The answer panel. Anything that is NOT an answer states what is missing,
 *  instead of painting a blank box (D5). */
function RoundBody({ entry, chatSessionId, reload, blockRef, status }: RoundBodyProps) {
  const body = entry?.projection?.answer?.body.trim() ?? ''

  if (!entry) {
    return <div className="chat-run-note">正在加载本轮执行…</div>
  }
  if (entry.status === 'unreachable') {
    return (
      <div className="chat-run-note is-danger">
        <span>执行所在的主机暂不可达</span>
        <button
          type="button"
          className="chat-block-hole-action"
          onClick={() => reload(chatSessionId, [blockRef])}
        >
          重试
        </button>
      </div>
    )
  }
  if (entry.status === 'not_found' || !entry.projection) {
    return <div className="chat-run-note">本轮执行已不可用</div>
  }
  if (body) {
    // §6.5: the answer is inline, at reading size — never folded behind a tap.
    return <div className="chat-block-answer">{renderMarkdown(body)}</div>
  }
  return <div className="chat-run-note">{blockStatusText(status)}</div>
}

interface CardSurfaceProps {
  card: BlockCard
  canAnswer: boolean
  instanceId: string
  execSid: string
  /** The reference row's own id — the coordinate a continuation addresses (D6
   *  level 1). */
  rowId: string
  chatSessionId: string
  /** This round's ref, re-read once the answer lands so the answered card leaves
   *  the transcript on the decision, not on the next projection that happens to
   *  arrive. Named `blockRef` because `ref` is a reserved JSX attribute. */
  blockRef: string
  onReplyToRound: (rowId: string, quote: string) => void
}

/** The round's pending decision (D8), collapsed to one line with its buttons in
 *  a bottom sheet (§6.5). Rendered whether or not the block is folded. */
function BlockCardSurface({
  card,
  canAnswer,
  instanceId,
  execSid,
  rowId,
  chatSessionId,
  blockRef,
  onReplyToRound,
}: CardSurfaceProps) {
  if (canAnswer) {
    return (
      <RoundCard
        card={card}
        instanceId={instanceId}
        execSid={execSid}
        rowId={rowId}
        chatSessionId={chatSessionId}
        blockRef={blockRef}
        onReplyToRound={onReplyToRound}
      />
    )
  }
  return (
    <div className="chat-block-card">
      <div className="chat-block-card-static">
        <span className="chat-block-card-tag">{cardLabel(card)}</span>
        <span className="chat-block-card-text">{truncate(card.prompt)}</span>
      </div>
      <div className="chat-block-card-note">只有发起本轮的人可以回复</div>
    </div>
  )
}

interface CardActionsProps {
  card: BlockCard
  instanceId: string
  execSid: string
  rowId: string
  /** The Chat the read is participation-checked against — the retry/re-read
   *  rides it, because a block is addressed, not filtered (D10). */
  chatSessionId: string
  /** This round's ref: the answered round is the one re-read. */
  blockRef: string
  onReplyToRound: (rowId: string, quote: string) => void
}

/** The answerable form of the parked decision — the sheet's opener, or the
 *  composer address for a `pause`. */
function RoundCard({
  card,
  instanceId,
  execSid,
  rowId,
  chatSessionId,
  blockRef,
  onReplyToRound,
}: CardActionsProps) {
  const [sheet, setSheet] = useState(false)
  const [busy, setBusy] = useState(false)
  const [failed, setFailed] = useState(false)

  const post = async (run: () => Promise<void>) => {
    setBusy(true)
    setFailed(false)
    try {
      await run()
      // The execution view keeps its own copy of the pending request; clear it
      // only when it IS this request (see `dismissAnsweredCard`).
      useChatStore.getState().dismissAnsweredCard(instanceId, execSid, card.request_id)
      // Re-read this round, so the answered card leaves the transcript on the
      // decision rather than on the next projection that happens to arrive.
      useChatBlockStore.getState().reload(chatSessionId, [blockRef])
      setSheet(false)
    } catch {
      // The card stays open: the round is still parked on the question, and a
      // silent failure would strand the human with a button that did nothing.
      setFailed(true)
    } finally {
      setBusy(false)
    }
  }

  if (card.kind === 'pause') {
    return (
      <div className="chat-block-card">
        <div className="chat-block-card-static">
          <span className="chat-block-card-tag">步骤确认</span>
          <span className="chat-block-card-text">{truncate(card.prompt)}</span>
        </div>
        <button
          type="button"
          className="chat-block-continue"
          data-testid="block-reply"
          onClick={() => onReplyToRound(rowId, card.prompt ?? '')}
        >
          继续执行
        </button>
      </div>
    )
  }

  return (
    <div className="chat-block-card">
      <button
        type="button"
        className="chat-block-card-btn"
        data-testid="block-card-open"
        onClick={() => setSheet(true)}
      >
        <span className="chat-block-card-tag">{cardLabel(card)}</span>
        <span className="chat-block-card-text">{truncate(card.prompt)}</span>
        <span className="chat-block-card-go">去回复</span>
      </button>
      {sheet ? (
        <CardSheet
          card={card}
          busy={busy}
          failed={failed}
          onClose={() => setSheet(false)}
          onApprove={(approved) => void post(() => sendApproval(instanceId, execSid, card.request_id, approved))}
          onAnswer={(answer) => void post(() => sendAnswer(instanceId, execSid, card.request_id, answer))}
        />
      ) : null}
    </div>
  )
}

/** The label a parked decision wears on its collapsed line. */
function cardLabel(card: BlockCard): string {
  return card.kind === 'approval' ? '需要审批' : card.kind === 'pause' ? '步骤确认' : 'Agent 询问'
}

/** Bound a card's question to one line on the collapsed row — the sheet is where
 *  the whole question is read. */
function truncate(prompt?: string): string {
  const collapsed = (prompt ?? '').replace(/\s+/g, ' ').trim()
  return collapsed.length > 34 ? `${collapsed.slice(0, 34)}…` : collapsed
}

/** The sheet's answer surface (§6.5). Approval offers exactly two decisions; an
 *  `ask_question` offers its labels plus free text — the same content the desktop
 *  block's inline buttons carry, just not one tap from the thumb. */
function CardSheet({
  card,
  busy,
  failed,
  onClose,
  onApprove,
  onAnswer,
}: {
  card: BlockCard
  busy: boolean
  failed: boolean
  onClose: () => void
  onApprove: (approved: boolean) => void
  onAnswer: (answer: string) => void
}) {
  const [free, setFree] = useState('')
  const options = card.options ?? []
  return (
    <Sheet open onClose={onClose} title={card.kind === 'approval' ? '允许执行？' : '需要你的回答'}>
      {card.prompt ? <div className="chat-sheet-question">{renderMarkdown(card.prompt)}</div> : null}
      {card.kind === 'approval' ? (
        <div className="chat-sheet-row">
          <button type="button" className="chat-sheet-option is-primary" disabled={busy} onClick={() => onApprove(true)}>
            批准
          </button>
          <button type="button" className="chat-sheet-option" disabled={busy} onClick={() => onApprove(false)}>
            拒绝
          </button>
        </div>
      ) : (
        <>
          {options.map((opt) => (
            <button key={opt} type="button" className="chat-sheet-option" disabled={busy} onClick={() => onAnswer(opt)}>
              {opt}
            </button>
          ))}
          <div className="chat-sheet-free">
            <input
              className="chat-sheet-input"
              placeholder="自由回答"
              aria-label="自由回答"
              value={free}
              onChange={(e) => setFree(e.target.value)}
            />
            <button
              type="button"
              className="chat-sheet-send"
              disabled={busy || !free.trim()}
              onClick={() => {
                onAnswer(free.trim())
                setFree('')
              }}
            >
              发送
            </button>
          </div>
        </>
      )}
      {failed ? <div className="chat-sheet-failed">回复未能发送，请重试</div> : null}
    </Sheet>
  )
}

interface BlockReplyRowProps {
  /** The freshest round's reference row id — replying to it continues THAT run
   *  (D6 level 1). Empty means no round to answer yet, so the box does nothing. */
  rowId: string
  /** A rising edge (the `@` sheet's "去续投") focuses the row once it is shown. */
  focusSignal?: boolean
  onSend: (rowId: string, text: string) => void
}

/** The block's in-place reply row (ADR-106 revision 2026-11-04). Unlike the
 *  desktop box it is not hover-gated — a phone has no hover — it simply lives at
 *  the bottom of the OPEN block (the parent renders it only while `open`), so
 *  continuing a run the human can already see is a tap, not a hunt. An empty
 *  send is refused here and there: the composer's own gate is the trim guard. */
function BlockReplyRow({ rowId, focusSignal, onSend }: BlockReplyRowProps) {
  const [draft, setDraft] = useState('')
  const boxRef = useRef<HTMLTextAreaElement>(null)
  // Focus on the picker's rising edge only — a held signal must not yank focus
  // back to the box on every re-render while the human is typing elsewhere.
  const lastSignal = useRef(false)
  useEffect(() => {
    if (focusSignal && !lastSignal.current) boxRef.current?.focus()
    lastSignal.current = focusSignal ?? false
  }, [focusSignal])

  const send = () => {
    const text = draft.trim()
    if (!text || !rowId) return
    // Clear first, then hand off: an in-place continuation stays put for the
    // next one, so the box never disappears out from under the reader.
    setDraft('')
    onSend(rowId, text)
  }

  return (
    <div className="chat-block-reply">
      <textarea
        ref={boxRef}
        className="chat-block-reply-input"
        rows={1}
        placeholder="回复以继续此任务…"
        aria-label="继续此任务"
        value={draft}
        disabled={!rowId}
        onChange={(e) => setDraft(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === 'Enter' && !e.shiftKey) {
            e.preventDefault()
            send()
          }
        }}
      />
      <button
        type="button"
        className="chat-block-reply-send"
        data-testid="block-reply-send"
        disabled={!draft.trim() || !rowId}
        onClick={send}
        aria-label="发送"
      >
        发送
      </button>
    </div>
  )
}
