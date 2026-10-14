import { describe, expect, it } from "vitest";

import { indexHealthBanner, type IndexHealthInput } from "./indexHealth";

const stats = (over: Partial<IndexHealthInput> = {}): IndexHealthInput => ({
  stored_dim: 1024,
  model_dim: 1024,
  total_nodes: 20,
  nodes_with_embedding: 20,
  vectors_of_other_dim: 0,
  ...over,
});

describe("indexHealthBanner", () => {
  it("says nothing when the store is consistent", () => {
    expect(indexHealthBanner(stats())).toBeNull();
    expect(indexHealthBanner(null)).toBeNull();
    expect(indexHealthBanner(undefined)).toBeNull();
  });

  it("does not treat a store that has never been indexed as broken", () => {
    expect(indexHealthBanner(stats({ stored_dim: 0, total_nodes: 0, nodes_with_embedding: 0 }))).toBeNull();
    expect(indexHealthBanner(stats({ model_dim: 0 }))).toBeNull();
  });

  it("reports a dimension mismatch when the model moved on", () => {
    const b = indexHealthBanner(stats({ stored_dim: 512, model_dim: 1024 }));
    expect(b?.titleKey).toBe("memoryPanel.dimMismatchTitle");
    expect(b?.vars.stored).toBe(512);
    expect(b?.vars.model).toBe(1024);
    expect(b?.extraKey).toBe("memoryPanel.dimMismatchIndexedDetail");
    // A model swap changes every vector's meaning - only a full re-embed is
    // honest, whatever the stale/missing counts happen to be.
    expect(b?.scope).toBe("all");
  });

  it("drops the indexed/total suffix on an empty store", () => {
    const b = indexHealthBanner(
      stats({ stored_dim: 512, model_dim: 1024, total_nodes: 0, nodes_with_embedding: 0 }),
    );
    expect(b?.extraKey).toBeUndefined();
  });

  // The case that motivated this file: the declared dimensions agree, every
  // node has a vector, and 294 of the store's vectors are still at the previous
  // width - invisible to search, invisible to the two checks above it.
  it("reports vectors left at a foreign width even though nothing is missing", () => {
    const b = indexHealthBanner(stats({ vectors_of_other_dim: 294 }));
    expect(b?.titleKey).toBe("memoryPanel.staleVectorsTitle");
    expect(b?.vars.stale).toBe(294);
    expect(b?.vars.stored).toBe(1024);
    // The defect this mapping exists to stop: a banner counting a handful of
    // invisible rows must not cost a full re-embed of the whole store.
    expect(b?.scope).toBe("repair");
  });

  it("reports nodes that lost their vectors", () => {
    const b = indexHealthBanner(stats({ nodes_with_embedding: 18, total_nodes: 20 }));
    expect(b?.titleKey).toBe("memoryPanel.missingEmbeddingsTitle");
    expect(b?.vars.indexed).toBe(18);
    expect(b?.vars.total).toBe(20);
    expect(b?.scope).toBe("repair");
  });

  // The panel and the runtime are built separately, so a live panel can be
  // talking to a runtime that does not send the field at all.
  it("treats a runtime that does not report the field as having nothing stale", () => {
    const withoutField = { ...stats(), vectors_of_other_dim: undefined };
    expect(indexHealthBanner(withoutField)).toBeNull();
  });

  it("prefers the mismatch over the stale count, since both mean rebuild", () => {
    const b = indexHealthBanner(stats({ stored_dim: 512, model_dim: 1024, vectors_of_other_dim: 7 }));
    expect(b?.titleKey).toBe("memoryPanel.dimMismatchTitle");
    // The scope follows the banner that fired, not the other defect waiting
    // behind it: after a model swap every row is suspect, stale count or not.
    expect(b?.scope).toBe("all");
  });

  // The live incident this rule prevents: a store with 5831 vectors, 2 of
  // them foreign-width, dimensions matching. The banner said "2 rows", the
  // rebuild button scanned all 5831.
  it("asks for a repair, not a full scan, when two vectors of thousands are foreign", () => {
    const b = indexHealthBanner(
      stats({ total_nodes: 5831, nodes_with_embedding: 5831, vectors_of_other_dim: 2 }),
    );
    expect(b?.titleKey).toBe("memoryPanel.staleVectorsTitle");
    expect(b?.scope).toBe("repair");
  });
});
