/**
 * Every scroll container in the app must be bound to the app's ONE scrollbar.
 *
 * Why this file exists:
 *   A scroll container carries two things that are easy to half-do — the
 *   `overflow-*` utility that makes it scroll, and the overlay binding that
 *   replaces the OS bar with the app's own thumb (`OVERLAY_SCROLL_VIEWPORT_CLASS`
 *   inside `<OverlayScrollbar>`, or paired with `OVERLAY_SCROLL_HOST_CLASS`
 *   when the element cannot be wrapped). Adding the first without the second is
 *   invisible to every other check: it type-checks, it renders, its behaviour
 *   tests pass — it just silently ships an OS scrollbar inside a themed
 *   surface. That is how ~57 sites drifted while the 45 hand-converted ones
 *   looked right.
 *
 *   A themed `::-webkit-scrollbar` rule in globals.css cannot cover for it:
 *   Chromium 121+ ignores that whole family the moment `scrollbar-width` /
 *   `scrollbar-color` are non-`auto` (globals.css carries the long version of
 *   that story). The floating thumb is the mechanism, so it has to be bound per
 *   scroll container — exactly the kind of invariant a test must hold.
 *
 * What it polices:
 *   An element whose attributes make it scroll vertically or in both directions
 *   (`overflow-y-auto`, `overflow-auto`, `overflow-y-scroll`, `overflow-scroll`,
 *   or the inline-style equivalents) must, within the same element's
 *   attributes, also name the overlay binding. Horizontal-only scrollers
 *   (`overflow-x-auto`) are exempt: their x-axis affordance is opt-in via
 *   `showXThumb`, and a long-value tab strip / `<pre>` that only pans sideways
 *   is not the complaint this gate encodes.
 *
 * What it deliberately does NOT police:
 *   - Elements that only `overflow-hidden` (clipping, not scrolling).
 *   - The overlay primitives themselves (`OverlayScrollbar.tsx`), which *are*
 *     the binding.
 *   - The `PENDING` list below: the conversion backlog, keyed by file so the
 *     entry survives line movement. Two assertions hold it honest — a new
 *     unbound scroller anywhere else fails immediately, and an entry whose file
 *     no longer has any unbound scroller fails as stale, so the list can only
 *     shrink.
 */
import { readFileSync, readdirSync, statSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join, relative, resolve } from "node:path";
import ts from "typescript";
import { describe, expect, it } from "vitest";

const __dirname = dirname(fileURLToPath(import.meta.url));
const APP_ROOT = resolve(__dirname, "..", "..", "..");
const SRC = resolve(APP_ROOT, "src");

/** Conversion backlog: files that still hold unbound scroll containers. Delete
 *  a line when its batch lands — the "no stale entries" test enforces it. */
const PENDING: Record<string, string> = {
  "src/components/agent-list/AgentLibraryDialog.tsx": "sweep B – dialogs",
  "src/components/agent-list/CreateWizard.tsx": "sweep B – wizards",
  "src/components/agent-list/PermissionDialog.tsx": "sweep B – dialogs",
  "src/components/agent-list/PublishWizard.tsx": "sweep B – wizards",
  "src/components/chat/ChatPanel.tsx": "sweep A – chat panels (todo/queue lists, JSON blobs)",
  "src/components/chat/ChatUsageIcon.tsx": "sweep A – chat popovers",
  "src/components/chat/CompactionCard.tsx": "sweep A – chat cards",
  "src/components/chat/ContextUsageIcon.tsx": "sweep A – chat popovers",
  "src/components/chat/GroupMembersPanel.tsx": "sweep A – chat panels",
  "src/components/chat/MessageBubble.tsx": "sweep C – <pre> blobs",
  "src/components/chat/MessageRefBlock.tsx": "sweep A – chat cards",
  "src/components/chat/SessionPanel.tsx": "sweep A – chat panels",
  "src/components/chat/UserGroupMembersPanel.tsx": "sweep A – chat panels",
  "src/components/common/ErrorBoundary.tsx": "sweep C – <pre> blobs",
  "src/components/common/ErrorBox.tsx": "sweep C – <pre> blobs",
  "src/components/common/list/ListBox.tsx": "sweep A – shared list primitive",
  "src/components/debug/PromptsSkillsCard.tsx": "sweep A – debug panels",
  "src/components/doc/editor/DocRichEditor.tsx": "sweep C – editor hosts",
  "src/components/editor/CommitPicker.tsx": "sweep B – pickers",
  "src/components/editor/DocxViewer.tsx": "sweep C – viewer hosts",
  "src/components/editor/ImageViewer.tsx": "sweep C – viewer hosts",
  "src/components/editor/MarkdownPreviewView.tsx": "sweep C – viewer hosts",
  "src/components/editor/PdfPageColumn.tsx": "sweep C – viewer hosts",
  "src/components/editor/PptxViewer.tsx": "sweep C – viewer hosts",
  "src/components/editor/SheetViewer.tsx": "sweep C – viewer hosts",
  "src/components/editor/SymbolSearchPanel.tsx": "sweep C – editor hosts",
  "src/components/harness/AddProviderFlow.tsx": "sweep C – <pre> blobs",
  "src/components/harness/LspTab.tsx": "sweep C – <pre> blobs",
  "src/components/layout/GatewaySwitcherMenu.tsx": "sweep B – popovers",
  "src/components/onboarding/OnboardingFlow.tsx": "sweep B – flows",
  "src/components/right-panel/AgentSetupTab.tsx": "sweep B – popovers",
  "src/components/right-panel/ToolsCards.tsx": "sweep C – <pre> blobs",
  "src/components/search/AgentPickerChip.tsx": "sweep B – popovers",
  "src/components/search/GlobalSearchDialog.tsx": "sweep B – dialogs",
  "src/components/search/PreviewPane.tsx": "sweep C – <pre> blobs",
  "src/components/skills/SkillBrowser.tsx": "sweep B – panels",
  "src/components/skills/SkillDetail.tsx": "sweep B – panels",
  "src/components/workspace/RemoteFolderPicker.tsx": "sweep B – dialogs",
  "src/components/workspace/WorkspaceExplorer.tsx": "sweep B – popovers",
  "src/components/workspace/WorkspaceManager.tsx": "sweep B – dialogs",
  "src/components/workspace/WorkspaceSelector.tsx": "sweep B – popovers",
  "src/views/ExtensionsView.tsx": "sweep B – views",
  "src/views/doc/DocMarkdownView.tsx": "sweep C – viewer hosts",
  "src/views/doc/DocPermissionsCard.tsx": "sweep B – panels",
  "src/views/doc/DocTreeSidebar.tsx": "sweep B – panels",
  "src/views/doc/ReviewQueue.tsx": "sweep B – panels",
  "src/views/doc/TrashDialog.tsx": "sweep B – dialogs",
  "src/views/pm/TaskTreePicker.tsx": "sweep B – pickers",
};

const SCROLL_MARKER =
  /\boverflow-y-(?:auto|scroll)\b|\boverflow-auto\b|\boverflow-scroll\b|overflowY:\s*["'](?:auto|scroll)["']/;

/** Any way of saying "this element wears the app's overlay scrollbar". */
const OVERLAY_BINDING = /OVERLAY_SCROLL_VIEWPORT_CLASS|overlay-scrollbar(-viewport)?\b/;

/** Comments inside an opening tag are legal JSX (`{/* … *\/}`), and several rows
 *  explain themselves by naming `overflow-auto` in prose — matching the raw
 *  attribute text would report those rows as violations. */
function stripComments(attributes: string): string {
  return attributes.replace(/\/\*[\s\S]*?\*\//g, "").replace(/\/\/[^\n]*/g, "");
}

function sourceFiles(dir: string, out: string[] = []): string[] {
  for (const name of readdirSync(dir)) {
    const full = join(dir, name);
    if (statSync(full).isDirectory()) sourceFiles(full, out);
    else if (name.endsWith(".tsx") && !name.includes(".test.")) out.push(full);
  }
  return out;
}

export interface ScrollViolation {
  file: string;
  line: number;
  element: string;
}

/** Element-level scan: a violation is an element that scrolls without the
 *  binding, so `cn("… overflow-y-auto", OVERLAY_SCROLL_VIEWPORT_CLASS)` split
 *  across lines still counts as bound. */
export function findUnboundScrollContainers(file: string, source: string): ScrollViolation[] {
  const sf = ts.createSourceFile(file, source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  const violations: ScrollViolation[] = [];

  const visit = (node: ts.Node) => {
    if (ts.isJsxOpeningElement(node) || ts.isJsxSelfClosingElement(node)) {
      const attributes = stripComments(node.attributes.getText(sf));
      if (SCROLL_MARKER.test(attributes) && !OVERLAY_BINDING.test(attributes)) {
        const { line } = sf.getLineAndCharacterOfPosition(node.getStart(sf));
        violations.push({
          file: relative(APP_ROOT, file).replace(/\\/g, "/"),
          line: line + 1,
          element: node.tagName.getText(sf),
        });
      }
    }
    ts.forEachChild(node, visit);
  };

  visit(sf);
  return violations;
}

function scanApp(): ScrollViolation[] {
  return sourceFiles(SRC).flatMap((file) => findUnboundScrollContainers(file, readFileSync(file, "utf8")));
}

describe("overlay scrollbar coverage", () => {
  it("the scan is not vacuous", () => {
    expect(
      findUnboundScrollContainers("probe.tsx", `export const A = () => <div className="overflow-y-auto">x</div>;`),
    ).toHaveLength(1);
    expect(
      findUnboundScrollContainers(
        "probe.tsx",
        `export const B = () => <div className={cn("overflow-y-auto", OVERLAY_SCROLL_VIEWPORT_CLASS)}>x</div>;`,
      ),
    ).toHaveLength(0);
    expect(
      findUnboundScrollContainers(
        "probe.tsx",
        `export const C = () => <OverlayScrollbar className="py-1">x</OverlayScrollbar>;`,
      ),
    ).toHaveLength(0);
    // Horizontal-only scrollers stay out of scope.
    expect(
      findUnboundScrollContainers("probe.tsx", `export const D = () => <pre className="overflow-x-auto">x</pre>;`),
    ).toHaveLength(0);
    // A comment naming the utility is not a scroll container.
    expect(
      findUnboundScrollContainers(
        "probe.tsx",
        `export const E = () => <li className="min-w-max">{/* the scroller's overflow-auto */}</li>;`,
      ),
    ).toHaveLength(0);
  });

  it("no new unbound scroll containers outside the conversion backlog", () => {
    const leaks = scanApp()
      .filter((v) => !(v.file in PENDING))
      .map((v) => `${v.file}:${v.line} <${v.element}>`);
    expect(
      leaks,
      "These scroll containers would show an OS scrollbar. Bind them (see overlayScrollThumb.ts / OverlayScrollbar.tsx).",
    ).toEqual([]);
  });

  it("the conversion backlog has no stale entries", () => {
    const filesWithLeaks = new Set(scanApp().map((v) => v.file));
    const stale = Object.keys(PENDING).filter((file) => !filesWithLeaks.has(file));
    expect(
      stale,
      "These files are fully converted — delete their PENDING entry so the backlog keeps shrinking.",
    ).toEqual([]);
  });

  it("globals.css never disables the ::-webkit-scrollbar skin", () => {
    // The root cause of the whole leak: a non-`auto` standard scrollbar
    // property makes Chromium 121+ drop the entire `::-webkit-scrollbar` family,
    // so the themed skin silently stops applying and every unconverted scroller
    // falls back to the OS bar. `scrollbar-width: none` is the one allowed use
    // (it is how a bound viewport hides its native bar).
    const css = readFileSync(resolve(SRC, "styles", "globals.css"), "utf8").replace(/\/\*[\s\S]*?\*\//g, "");
    expect(css).not.toMatch(/scrollbar-color\s*:/);
    const widthValues = [...css.matchAll(/scrollbar-width\s*:\s*([^;}]+)/g)].map((m) => m[1].trim());
    expect(widthValues.every((value) => value === "none")).toBe(true);
  });
});
