/**
 * Where a transcript row's **artifact facts** live — the client counterpart of
 * the Runtime's `agent/artifact_facts.rs`.
 *
 * ## Why this is its own module
 *
 * Two different questions get answered in the deliverables path, and they change
 * for different reasons:
 *
 *   - *where are the facts on this row?* — a property of the **transcript
 *     wire shape**, which the Runtime decides (this module);
 *   - *which of them do we show, how many, in what order?* — a property of the
 *     **product**, decided here in the Desktop and nowhere else
 *     (`deriveDeliverables.ts`, ADR-119).
 *
 * Keeping the shape adapter separate is what makes the policy module shape-blind:
 * it consumes `ArtifactFacts` and never learns that a projected history page
 * exists. When the projection gained a field, only this file changed — the policy
 * did not, and it must stay that way (a transcript projection is a transport
 * concern; a banner is not).
 *
 * ## The two shapes
 *
 *   - **raw `tool_result`** — live rows (MQTT `record_complete`) and a raw page:
 *     the facts sit on the row's own `toolData`, because the Runtime stamps them
 *     on the row that produced the artifact.
 *   - **`explore_stub`** — every history page (`groups=stub`): a contiguous
 *     thought / tool_call / tool_result run folds into one row, which forwards
 *     its members' facts on `exploreStub.facts` (same objects, same keys).
 *
 * Both are read through the SAME parsers below, which is what makes one turn
 * reconstruct identically whether it was streamed or reloaded. Before the stub
 * forwarded them, a reloaded session silently lost every banner.
 *
 * Tolerance: a malformed fact degrades to "this row produced nothing". A stub
 * with no `facts` (a Runtime older than the forwarding) yields `null` — absence
 * means "not known", never "none", so the banner is simply absent.
 */
import type { ChatMessage } from "./types";
import { parseProducedFiles, type DeliverableFile } from "./deliverableFiles";
import { docFromMetadata, parseDocToolResult, type DeliverableDoc } from "./deliverableDocs";

/** What one row contributes to its turn's deliverables. */
export interface ArtifactFacts {
  files: DeliverableFile[];
  docs: DeliverableDoc[];
}

/** The artifact facts one row carries, or `null` when it carries none.
 *
 *  `null` and "an empty set" are the same outcome for every caller, so they are
 *  the same value here: one contract ("this row contributes nothing") instead of
 *  two spellings a caller could accidentally treat differently. */
export function rowArtifactFacts(msg: ChatMessage): ArtifactFacts | null {
  if (msg.type === "tool_result") {
    const doc = parseDocToolResult(msg);
    return bundle(parseProducedFiles(msg.toolData), doc ? [doc] : []);
  }
  if (msg.type === "explore_stub") {
    const facts = msg.exploreStub?.facts;
    if (!facts || facts.length === 0) return null;
    const files: DeliverableFile[] = [];
    const docs: DeliverableDoc[] = [];
    for (const fact of facts) {
      files.push(...parseProducedFiles(fact));
      const doc = docFromMetadata(fact);
      if (doc) docs.push(doc);
    }
    return bundle(files, docs);
  }
  return null;
}

/** Two non-empty-implies-a-set rule, in one place. */
function bundle(files: DeliverableFile[], docs: DeliverableDoc[]): ArtifactFacts | null {
  if (files.length === 0 && docs.length === 0) return null;
  return { files, docs };
}

/**
 * Fold a turn's rows into one fact set, in row order.
 *
 * Order is the contract: the policy layer dedupes "last wins" for documents and
 * "first wins" for files, and both depend on the turn's log order — which is why
 * this does not dedupe or reorder anything itself.
 */
export function turnArtifactFacts(rows: readonly ChatMessage[]): ArtifactFacts {
  const out: ArtifactFacts = { files: [], docs: [] };
  for (const row of rows) {
    const facts = rowArtifactFacts(row);
    if (!facts) continue;
    out.files.push(...facts.files);
    out.docs.push(...facts.docs);
  }
  return out;
}