/**
 * Regression: opening a document from the left tree must NOT mark it dirty.
 *
 * Reported bug (2026-11): clicking a doc in the tree opened it already
 * "unsaved", so switching to any other document popped the discard dialog.
 *
 * The dirty flag has exactly one writer (`editorStore.setContent`), so a
 * document that is dirty right after `openDoc` means an editor engine echoed
 * its own initial render back as a user edit. This test drives the real
 * store + the real editor component (both engines) and asserts the flag.
 */
import { describe, expect, it, vi, beforeEach } from "vitest";
import { act, render, waitFor } from "@testing-library/react";

const mocks = vi.hoisted(() => ({
  docs: new Map<string, { meta: Record<string, unknown>; content: string; path: string }>(),
}));

vi.mock("../../lib/doc-api", () => ({
  getDoc: vi.fn(async (docId: string) => {
    const d = mocks.docs.get(docId);
    if (!d) throw new Error(`not found: ${docId}`);
    return { meta: d.meta, content: d.content, path: d.path };
  }),
}));

// Monaco is a global side effect; the rich engine must not depend on it.
vi.mock("../../lib/monacoBootstrap", () => ({
  initMonaco: () => Promise.resolve(),
}));

vi.mock("../../components/common/ConfirmDialog", () => ({
  ConfirmDialog: () => null,
}));

import { DocEditor } from "./DocEditor";
import { useDocEditorStore } from "../../stores/doc/editorStore";
import { useDocHealthStore } from "../../stores/doc/healthStore";

function seedDoc(docId: string, content: string) {
  mocks.docs.set(docId, {
    meta: {
      doc_id: docId,
      name: `${docId}.md`,
      version: 1,
      created_at: "2026-01-01T00:00:00Z",
      updated_at: "2026-01-01T00:00:00Z",
      deleted: false,
      creator: "u-1",
      import: null,
    },
    content,
    path: `${docId}.md`,
  });
}

/**
 * A body carrying the constructs the markdown ⇄ Tiptap round-trip *reshapes*
 * (`*` bullets → `-`, two-space hard break → backslash). This matters: a body
 * the round-trip reproduces verbatim hides the bug, because the engine echoing
 * its own initial render back produces text identical to the source and the
 * dirty guard swallows it. Only a body the parser normalises makes the echo
 * distinguishable from a user edit — which is exactly the reported symptom
 * ("点开就是未保存").
 */
const TRICKY = [
  "# 设计纪要",
  "",
  "第一段，含 `inline code` 与 [链接](https://example.com)。",
  "",
  "* 列表项 A",
  "* 列表项 B",
  "",
  "第一行  ",
  "第二行（两空格硬换行）",
  "",
  "1. 有序 A",
  "2. 有序 B",
  "",
  "> 引用",
  "",
  "| 列 1 | 列 2 |",
  "| --- | --- |",
  "| a | b |",
  "",
  "```js",
  "console.log(1)",
  "```",
  "",
].join("\n");

beforeEach(() => {
  mocks.docs.clear();
  useDocHealthStore.setState({ healthy: true });
  useDocEditorStore.setState({
    doc: null,
    content: "",
    dirty: false,
    loading: false,
    mode: "edit",
    engine: "rich",
    conflict: false,
    saveError: null,
    pendingOpenDocId: null,
  });
});

describe("DocEditor — opening a document does not dirty it", () => {
  it("rich engine: a freshly opened doc is not dirty and switching does not prompt", async () => {
    seedDoc("doc-a", TRICKY);
    seedDoc("doc-b", "# 另一篇\n\n正文\n");
    render(<DocEditor />);

    // Wait for the lazily-imported rich editor to mount, then open a doc.
    await waitFor(() =>
      expect(useDocEditorStore.getState().doc).toBeNull(),
    );
    await act(async () => {
      await useDocEditorStore.getState().openDoc("doc-a");
    });
    // Wait for the lazily imported rich editor to actually mount and
    // round-trip the body — a spinner proves nothing.
    await waitFor(() => {
      const el = document.querySelector(".doc-rich-content");
      expect(el?.textContent).toContain("设计纪要");
    });
    await act(async () => {
      await new Promise((r) => setTimeout(r, 50));
    });
    expect(useDocEditorStore.getState().dirty).toBe(false);

    // The switch must open, not block on a phantom unsaved change.
    let outcome: string | undefined;
    await act(async () => {
      outcome = await useDocEditorStore.getState().requestOpen("doc-b");
    });
    expect(outcome).toBe("opened");
    expect(useDocEditorStore.getState().doc?.meta.doc_id).toBe("doc-b");
    expect(useDocEditorStore.getState().dirty).toBe(false);
  });

  it("source engine: a freshly opened doc is not dirty", async () => {
    seedDoc("doc-a", TRICKY);
    useDocEditorStore.setState({ engine: "source" });
    render(<DocEditor />);
    await act(async () => {
      await useDocEditorStore.getState().openDoc("doc-a");
    });
    await act(async () => {
      await new Promise((r) => setTimeout(r, 50));
    });
    expect(useDocEditorStore.getState().dirty).toBe(false);
  });
});
