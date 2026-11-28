/**
 * Derive the turn-end deliverables banner from the transcript's tool calls.
 *
 * ## One source of truth, one derivation
 *
 * Every deliverable is a file on disk with an address, and the producing tool
 * already records that address in the transcript:
 *   - workspace files / image artifacts → `tool_result` `metadata.produced`
 *     (the Runtime maps node-local paths to `(workspace_id, rel_path)` and keeps
 *     the `created` flag — the one fact the frontend cannot reconstruct);
 *   - doc-service documents → `tool_result` `metadata.doc` (the Runtime stamps
 *     the `doc_write` / `doc_move` fact so it survives the live content
 *     truncation; older sessions fall back to the result envelope —
 *     [deliverableDocs.ts](deliverableDocs.ts)).
 *
 * This module folds those facts into one banner per turn, so files, images and
 * online documents share a single derivation and a single renderer. The Runtime
 * supplies facts; everything about *what to show and where* lives here.
 *
 * ## Two row shapes, one derivation
 *
 * A turn reaches this module in one of two shapes, and the banner must not be
 * able to tell them apart:
 *   - **live / raw page** — one row per tool call; the facts sit on each
 *     `tool_result`'s `toolData`;
 *   - **projected page** (`GET /messages?groups=stub`, every history load) — a
 *     contiguous explore run collapses into ONE `explore_stub` row that
 *     forwards the same facts on `exploreStub.facts`.
 * Both are read through the same parsers in {@link rowFacts}. Before the stub
 * forwarded them, a reloaded session showed no deliverables row at all while
 * the live turn kept its own.
 *
 * ## Legacy compatibility
 *
 * Sessions recorded before this change carry a persisted `deliverables` system
 * row (ADR-119). Those rows are kept verbatim (their files already passed the
 * display policy); only documents — which that row never tracked — are derived
 * and attached. New sessions have no such row, so one is synthesized at turn end.
 *
 * ## Display policy (frontend-owned)
 *
 * Created files only (a rewrite is process, not output), deduped by coordinates,
 * capped at 8 images + 8 other files so the row stays glanceable. The backend
 * stamps every addressable fact; this is where "what counts" is decided.
 *
 * ## Identity stability
 *
 * The adapter recomputes its snapshot on every stream delta. A naive derivation
 * would allocate a fresh banner row each time, breaking `React.memo` on every
 * historical banner even though nothing changed. {@link DeliverablesMemo} keeps
 * a per-session cache: a turn whose real rows and deliverable facts are unchanged
 * reuses the *same* banner row object and the *same* emitted array, and the whole
 * output keeps its reference when element-wise identical. The cache is owned by
 * the caller (one per session adapter), so its lifetime is the session's — no
 * module-global state.
 */
import type { ChatMessage } from "./types";
import {
  DELIVERABLES_META_TYPE,
  isDeliverablesRow,
  isImageDeliverable,
  type DeliverableFile,
} from "./deliverableFiles";
import { dedupeDocs, type DeliverableDoc } from "./deliverableDocs";
import { turnArtifactFacts } from "./artifactFacts";

const MAX_IMAGES = 8;
const MAX_OTHER_FILES = 8;

/** Apply the frontend display policy to raw produced facts: created-only,
 *  deduped by coordinates, capped independently for images and other files. */
function applyFilePolicy(files: readonly DeliverableFile[]): DeliverableFile[] {
  const out: DeliverableFile[] = [];
  const seen = new Set<string>();
  let images = 0;
  let others = 0;
  for (const file of files) {
    // A rewrite (created === false) is the turn editing something that already
    // existed — process, not output. Legacy facts without the flag are treated
    // as created (they predate the flag and already passed the old policy).
    if (file.created === false) continue;
    const key = `${file.workspaceId}:${file.relPath}`;
    if (seen.has(key)) continue;
    seen.add(key);
    if (isImageDeliverable(file)) {
      if (images >= MAX_IMAGES) continue;
      images += 1;
    } else {
      if (others >= MAX_OTHER_FILES) continue;
      others += 1;
    }
    out.push(file);
  }
  return out;
}

/** Build a synthetic deliverables row for a turn that has no persisted one. */
function makeDerivedRow(
  turn: readonly ChatMessage[],
  files: DeliverableFile[],
  docs: DeliverableDoc[],
): ChatMessage {
  const anchor = turn[0];
  const last = turn[turn.length - 1];
  return {
    id: `derived-deliverables-${anchor.id}`,
    type: "system",
    content: "",
    // Sort just after the turn's last row so the banner sits under the answer.
    timestamp: (last?.timestamp ?? anchor.timestamp) + 1,
    metadata: {
      type: DELIVERABLES_META_TYPE,
      files,
      docs,
      // Marks the row as frontend-derived (not a persisted Runtime projection).
      derived: true,
    },
  };
}

/** A turn's resolved deliverable facts, reduced to the banner it produces. */
interface TurnBanner {
  /** The row to splice into the turn (synthetic, or the legacy row + docs). */
  row: ChatMessage | null;
  /** Index of the legacy row the banner replaces, or -1 for a synthetic append. */
  legacyIdx: number;
  /** Content signature — equal signatures mean an identical banner. */
  sig: string;
}

/** Resolve one turn's banner from its rows (pure; no caching). */
function resolveTurnBanner(turn: readonly ChatMessage[]): TurnBanner {
  // One fold over the turn's rows, whatever shape they arrive in: live
  // `tool_result` rows and projected `explore_stub` rows both contribute their
  // artifact facts here (the row-shape adapter is `artifactFacts.ts`), so the
  // banner is a function of the turn's content and of nothing else. The legacy
  // persisted row (pre-ADR-119-revision sessions) carries files only; docs are
  // still derived and attached to it.
  const facts = turnArtifactFacts(turn);
  const docs = dedupeDocs(facts.docs);
  const legacyIdx = turn.findIndex((m) => isDeliverablesRow(m));
  const last = turn[turn.length - 1];

  if (legacyIdx >= 0) {
    const legacy = turn[legacyIdx];
    if (docs.length === 0) {
      return { row: null, legacyIdx: -1, sig: "legacy-unchanged" };
    }
    const row: ChatMessage = {
      ...legacy,
      metadata: { ...(legacy.metadata ?? {}), docs },
    };
    return { row, legacyIdx, sig: `legacy:${legacy.id}:${JSON.stringify(docs)}` };
  }

  const files = applyFilePolicy(facts.files);
  if (files.length === 0 && docs.length === 0) {
    return { row: null, legacyIdx: -1, sig: "none" };
  }
  const row = makeDerivedRow(turn, files, docs);
  // The signature covers everything that shapes the banner row: its facts and
  // its sort anchor (the turn's last row), so a growing active turn re-sorts.
  const sig = `derived:${last?.id}:${last?.timestamp}:${JSON.stringify(files)}:${JSON.stringify(docs)}`;
  return { row, legacyIdx: -1, sig };
}

/** Splice a resolved banner into a turn's rows. */
function emitTurn(turn: readonly ChatMessage[], banner: TurnBanner): readonly ChatMessage[] {
  if (banner.row === null) return turn;
  if (banner.legacyIdx >= 0) {
    return turn.map((m, i) => (i === banner.legacyIdx ? (banner.row as ChatMessage) : m));
  }
  return [...turn, banner.row];
}

interface CachedTurn {
  /** The turn's real input rows (reference snapshot, for identity comparison). */
  input: readonly ChatMessage[];
  sig: string;
  rows: readonly ChatMessage[];
}

/**
 * Per-session memoized derivation. One instance per session adapter; reuse the
 * same instance across recomputes so unchanged banners keep their object identity
 * (and the output array keeps its reference). Drop it when the session is released.
 */
export class DeliverablesMemo {
  private turns = new Map<string, CachedTurn>();
  private lastOutput: ChatMessage[] | null = null;

  resolve(messages: readonly ChatMessage[]): ChatMessage[] {
    const out: ChatMessage[] = [];
    let turn: ChatMessage[] = [];
    let changed = false;

    const flush = () => {
      const emitted = this.flushTurn(turn);
      if (emitted !== turn) changed = true;
      for (const m of emitted) out.push(m);
      turn = [];
    };

    for (const msg of messages) {
      if (msg.type === "user" && turn.length > 0) flush();
      turn.push(msg);
    }
    flush();

    // No turn gained a banner → hand back the original array untouched.
    if (!changed) {
      this.lastOutput = null;
      this.turns.clear();
      return messages as ChatMessage[];
    }
    // Element-wise identity: if every emitted row is the same reference as last
    // time, return the previous array so subscribers do not re-render.
    const prev = this.lastOutput;
    if (prev && prev.length === out.length && prev.every((m, i) => m === out[i])) {
      return prev;
    }
    this.lastOutput = out;
    return out;
  }

  private flushTurn(turn: readonly ChatMessage[]): readonly ChatMessage[] {
    if (turn.length === 0) return turn;
    const anchorId = turn[0].id;
    const banner = resolveTurnBanner(turn);
    const cached = this.turns.get(anchorId);
    if (
      cached &&
      cached.sig === banner.sig &&
      cached.input.length === turn.length &&
      cached.input.every((m, i) => m === turn[i])
    ) {
      return cached.rows;
    }
    const rows = emitTurn(turn, banner);
    // Only cache turns that actually carry a banner; a bare turn needs no entry.
    if (banner.row !== null) {
      this.turns.set(anchorId, { input: turn, sig: banner.sig, rows });
    } else {
      this.turns.delete(anchorId);
    }
    return rows;
  }
}

/**
 * Stateless derivation (a fresh memo per call). Prefer a shared {@link DeliverablesMemo}
 * on a hot recompute path (the session adapter) so unchanged banners keep their
 * identity; this wrapper is for one-shot callers and tests.
 *
 * A turn runs from one `user` message up to (not including) the next. When no turn
 * gains a banner the original array reference is returned.
 */
export function withDerivedDeliverables(messages: readonly ChatMessage[]): ChatMessage[] {
  return new DeliverablesMemo().resolve(messages);
}