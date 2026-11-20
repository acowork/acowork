/**
 * filePanelAutoCollapse — the rule that trades right-panel width for
 * file-tab width.
 *
 * The reader's ask: a file tab appears → the right panel folds away so the
 * file pane gets the width; every file closes → the panel comes back.
 *
 * The rule is a pure reducer, so every transition can be asserted here
 * without a DOM: jsdom computes no layout, and AppLayout drags in Tauri /
 * MQTT / monaco stores. What is under test is the DECISION, not the pixels.
 */
import { describe, expect, it } from "vitest";

import {
  initFilePanelAutoState,
  reduceFilePanelAuto,
  type FilePanelAutoState,
} from "./filePanelAutoCollapse";

/** Fold a sequence of events, collecting the actions they produced. */
function run(
  initial: FilePanelAutoState,
  events: Parameters<typeof reduceFilePanelAuto>[1][],
): { state: FilePanelAutoState; actions: string[] } {
  let state = initial;
  const actions: string[] = [];
  for (const event of events) {
    const decision = reduceFilePanelAuto(state, event);
    state = decision.state;
    actions.push(decision.action.kind);
  }
  return { state, actions };
}

describe("a file tab appears", () => {
  it("folds an expanded panel away", () => {
    const { actions } = run(initFilePanelAutoState(true), [{ type: "files", hasFiles: true }]);
    expect(actions).toEqual(["collapse"]);
  });

  it("leaves an already-collapsed panel alone", () => {
    const { actions } = run(initFilePanelAutoState(false), [{ type: "files", hasFiles: true }]);
    expect(actions).toEqual(["none"]);
  });

  it("does nothing when a second file opens while the first is already open", () => {
    // The fold is edge-triggered, not level-triggered: opening a second tab
    // must not re-fire it.
    const { state, actions } = run(initFilePanelAutoState(true), [
      { type: "files", hasFiles: true },
    ]);
    expect(actions).toEqual(["collapse"]);
    expect(reduceFilePanelAuto(state, { type: "files", hasFiles: true }).action.kind).toBe("none");
  });
});

describe("every file closes", () => {
  it("brings the panel back when this rule folded it", () => {
    const { actions } = run(initFilePanelAutoState(true), [
      { type: "files", hasFiles: true },
      // The store echoes our own fold back at us.
      { type: "panel", expanded: false },
      { type: "files", hasFiles: false },
    ]);
    expect(actions).toEqual(["collapse", "none", "expand"]);
  });

  it("leaves a panel the reader had already put away alone", () => {
    // The panel was collapsed before any file opened: the file pane borrowed
    // nothing, so it hands nothing back.
    const { actions } = run(initFilePanelAutoState(false), [
      { type: "files", hasFiles: true },
      { type: "files", hasFiles: false },
    ]);
    expect(actions).toEqual(["none", "none"]);
  });

  it("does not re-fire the restore when a file closes while none is open", () => {
    const { actions } = run(initFilePanelAutoState(true), [
      { type: "files", hasFiles: false },
    ]);
    expect(actions).toEqual(["none"]);
  });
});

describe("the reader's own moves of the panel win", () => {
  it("keeps a panel the reader re-opened by hand", () => {
    // We folded the panel for the file pane; the reader opened it again while
    // the file was still open. Closing the last file must NOT fold it away
    // again — the reader's choice is the one that counts.
    const { state, actions } = run(initFilePanelAutoState(true), [
      { type: "files", hasFiles: true },
      { type: "panel", expanded: false },
      { type: "panel", expanded: true },
      { type: "files", hasFiles: false },
    ]);
    expect(actions).toEqual(["collapse", "none", "none", "none"]);
    expect(state.folded).toBe(false);
  });

  it("keeps a panel the reader folded by hand folded", () => {
    // We folded it; the reader expanded it; then the reader folded it again.
    // Their last word stands: the restore must not fire.
    const { actions } = run(initFilePanelAutoState(true), [
      { type: "files", hasFiles: true },
      { type: "panel", expanded: false },
      { type: "panel", expanded: true },
      { type: "panel", expanded: false },
      { type: "files", hasFiles: false },
    ]);
    expect(actions).toEqual(["collapse", "none", "none", "none", "none"]);
  });

  it("does not mistake the echo of our own fold for the reader's click", () => {
    // The store echoes every write back. Without the `pendingApply` marker the
    // echo of OUR collapse would read as "the reader folded it" and cancel the
    // restore — the panel would never come back.
    const { actions } = run(initFilePanelAutoState(true), [
      { type: "files", hasFiles: true },
      { type: "panel", expanded: false },
      { type: "files", hasFiles: false },
    ]);
    expect(actions).toEqual(["collapse", "none", "expand"]);
  });
});

describe("the selected tab is never touched", () => {
  it("keeps the panel's tab across a fold and a restore", () => {
    // The reducer carries no tab at all: folding the panel changes the
    // layout, not the selection, so the restored panel is the surface the
    // reader left.
    const { state } = run(initFilePanelAutoState(true), [
      { type: "files", hasFiles: true },
      { type: "panel", expanded: false },
      { type: "files", hasFiles: false },
    ]);
    expect(Object.keys(state)).not.toContain("tab");
  });
});

describe("StrictMode's double invocation", () => {
  it("folds only once when the files effect runs twice", () => {
    // React StrictMode double-invokes effects in dev. The reducer is fed the
    // SAME event twice; the second one must be a no-op, not a second fold.
    let state = initFilePanelAutoState(true);
    const first = reduceFilePanelAuto(state, { type: "files", hasFiles: true });
    state = first.state;
    const second = reduceFilePanelAuto(state, { type: "files", hasFiles: true });
    expect(first.action.kind).toBe("collapse");
    expect(second.action.kind).toBe("none");
  });

  it("restores only once when the files effect runs twice", () => {
    let state = initFilePanelAutoState(true);
    state = reduceFilePanelAuto(state, { type: "files", hasFiles: true }).state;
    state = reduceFilePanelAuto(state, { type: "panel", expanded: false }).state;
    const first = reduceFilePanelAuto(state, { type: "files", hasFiles: false });
    state = first.state;
    const second = reduceFilePanelAuto(state, { type: "files", hasFiles: false });
    expect(first.action.kind).toBe("expand");
    expect(second.action.kind).toBe("none");
  });
});
