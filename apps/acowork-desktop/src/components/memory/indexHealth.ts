/**
 * The Memory panel's "rebuild the index" banner decision, kept out of the
 * component so the rule itself is testable.
 *
 * Three conditions, in the order a user can act on:
 *
 * 1. The store declares a dimension the active model does not produce. The
 *    whole index is one rebuild away from matching the model.
 * 2. Some vectors are stored at a width the store does not declare. These rows
 *    are not missing - vector search skips a foreign-width row silently and
 *    says nothing - so no count of *missing* embeddings can see them, and a
 *    comparison of declared dimensions cannot either. This is the state a
 *    half-applied model swap leaves behind, and until now the panel was
 *    silent about it while reporting the store as healthy.
 * 3. Some memory nodes have no vector at all.
 *
 * Each condition also carries its repair scope. Conditions 2 and 3 mean the
 * model *did not* change and only the rows retrieval cannot see need work -
 * re-embedding a 5000-row store to heal 2 rows is the bug this mapping
 * prevents. Condition 1 is a model swap: every vector changes meaning, so
 * only a full rebuild is honest.
 *
 * `nodes_with_embedding` and `total_nodes` must be counted over the same set of
 * nodes, or condition 3 can never fire: conversation messages carry vectors but
 * are not memory nodes, so counting them made the comparison false on every
 * store with chat history.
 */

import type { RebuildScope } from "../../lib/types";

export type { RebuildScope } from "../../lib/types";

export type IndexHealthInput = {
  stored_dim: number;
  model_dim: number;
  total_nodes: number;
  nodes_with_embedding: number;
  /** Absent on a runtime built before the field existed; reads as "nothing stale". */
  vectors_of_other_dim?: number;
};

export type IndexHealthBanner = {
  titleKey: string;
  detailKey: string;
  /**
   * Every number the panel has, passed to whichever keys interpolate. One bag
   * rather than per-branch params: i18next ignores params a string does not
   * use, and the suffix line needs a different pair from the line before it.
   */
  vars: Record<string, number>;
  /** Appended to the detail line, separated by " · ". */
  extraKey?: string;
  /**
   * Scope the rebuild button must request for this banner. Part of the
   * banner, not a separate decision: the scope is *why* the banner fired,
   * and splitting them lets the two drift.
   */
  scope: RebuildScope;
};

export function indexHealthBanner(
  stats: IndexHealthInput | null | undefined,
): IndexHealthBanner | null {
  if (!stats) return null;

  const vars = {
    stored: stats.stored_dim,
    model: stats.model_dim,
    indexed: stats.nodes_with_embedding,
    total: stats.total_nodes,
    stale: stats.vectors_of_other_dim ?? 0,
  };

  const dimMismatch =
    stats.stored_dim > 0 && stats.model_dim > 0 && stats.stored_dim !== stats.model_dim;
  if (dimMismatch) {
    return {
      titleKey: "memoryPanel.dimMismatchTitle",
      detailKey: "memoryPanel.dimMismatchDetail",
      vars,
      // The indexed/total line only means something once there are nodes.
      extraKey: stats.total_nodes > 0 ? "memoryPanel.dimMismatchIndexedDetail" : undefined,
      // A model swap changes every vector's meaning; only rows at the new
      // width are trustworthy, and after a swap none are.
      scope: "all",
    };
  }

  if (vars.stale > 0) {
    return {
      titleKey: "memoryPanel.staleVectorsTitle",
      detailKey: "memoryPanel.staleVectorsDetail",
      vars,
      // The store's declared width already matches the model; only the
      // foreign-width rows are invisible to retrieval.
      scope: "repair",
    };
  }

  const missingEmbeddings =
    stats.model_dim > 0 && stats.total_nodes > 0 && stats.nodes_with_embedding < stats.total_nodes;
  if (missingEmbeddings) {
    return {
      titleKey: "memoryPanel.missingEmbeddingsTitle",
      detailKey: "memoryPanel.missingEmbeddingsDetail",
      vars,
      // Same reasoning: unembedded rows are the rows missing from retrieval.
      scope: "repair",
    };
  }

  return null;
}
