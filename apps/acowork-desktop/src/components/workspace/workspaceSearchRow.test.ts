/**
 * The right workspace search dropdown must keep its trailing "add to chat"
 * button pinned to the END OF THE ROW at any pane width, AND must not
 * crush the file name down to two or three letters when the path still
 * has room to give up.
 *
 * Why this file exists:
 *   User-visible feature: hovering a search hit reveals a `+`
 *   (MessageSquarePlus) button that attaches the file to the active
 *   conversation. The row reads: `[icon] name  dir   [+]`. Two
 *   regressions have hit this row before, both real:
 *
 *     (1) `shrink-0` on the name made it refuse to shrink below its
 *         content width, so a long name pushed the row wider than the
 *         pane. The dir span absorbed the overflow down to zero, the
 *         `ml-auto` on the button had no free space to consume, and
 *         the button rode out past the dropdown's right edge until the
 *         pane was dragged wider.
 *
 *     (2) The "fix" was to put `shrink-[3]` on the name. A higher
 *         flex-shrink factor shrinks the item FASTER, so this swapped
 *         which span got crushed. With that flag, a narrow pane turned
 *         "App.tsx" into "Ap..." while the directory stayed readable.
 *
 *   Both extremes hurt. The correct fix decouples the button from the
 *   text region entirely.
 *
 * What this pins (the row layout contract, by element):
 *   1. row  — a 3-column grid: `auto` (icon) | `minmax(0,1fr)` (text)
 *             | `auto` (button), plus `overflow-hidden`. The grid
 *             places the button in its own auto column, so it cannot be
 *             pushed off-screen by long names. The text column is the
 *             only thing that gets clipped.
 *   2. text wrapper — flex container with `min-w-0 overflow-hidden`
 *             so the name+dir region clips, not leaks.
 *   3. name  — `shrink-0 whitespace-nowrap`. The identifying token is
 *              pinned at its content width inside the text column and
 *              only gets clipped by the wrapper when the pane is so
 *              narrow that even the name can't fit. The full path
 *              stays reachable via `title`.
 *   4. dir   — `min-w-0 truncate`, no `shrink-0`. This is the yield
 *              victim: it absorbs the squeeze first so the name keeps
 *              its content width.
 *   5. button— `shrink-0` inside its auto column. The grid places it
 *              at the trailing edge; `ml-auto` is no longer needed and
 *              would be misleading (a leftover that hints the old flex
 *              row is still in play).
 *   6. row   — exposes the full path via `title={f.relPath}`.
 *
 * Why source scanning instead of rendered DOM:
 *   The bug is pure flex/grid layout — `ml-auto` resolving to 0 because
 *   no free space remained, asymmetric shrink factors starving the wrong
 *   span. jsdom computes no layout (clientWidth is always 0), so a
 *   render assertion cannot observe it; the class strings ARE the spec
 *   here. Same approach as listHeaderHeight.test.ts and
 *   sidebarRowFontSize.test.ts.
 *
 * Scoping: the assertions read the result-row block only (from the row's
 * `key={f.relPath}` to the closing `handleSearchAddToChat(f)` call) so the
 * unrelated header toolbar and the file tree below it cannot satisfy — or
 * break — these checks.
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { describe, expect, it } from "vitest";

const __dirname = dirname(fileURLToPath(import.meta.url));
const SRC = readFileSync(resolve(__dirname, "WorkspaceExplorer.tsx"), "utf8");

/** The `matchingFiles.map(...)` result-row block, isolated from the rest of
 *  the component. Ends past the button's own className (which is written
 *  after its onClick), so the action element is inside the window. */
function resultRowBlock(): string {
  const start = SRC.indexOf("matchingFiles.map(");
  expect(start, "result row block not found in WorkspaceExplorer.tsx").toBeGreaterThan(-1);
  const end = SRC.indexOf("</Tooltip>", start);
  expect(end, "result row block is not terminated").toBeGreaterThan(start);
  return SRC.slice(start, end);
}

const ROW = resultRowBlock();

/** The class list of the JSX element of type `tag` that contains `needle`.
 *
 *  Resolved by element boundary, not by attribute order: the needle may sit
 *  anywhere inside the element (a text child, an onClick body, an icon), so
 *  the opening tag is located first and its `className` read out of it. That
 *  way a neighbouring element's classes can neither satisfy nor break an
 *  assertion, and reordering attributes inside one element cannot silently
 *  repoint a check at the wrong node. */
function classListOf(tag: "span" | "button" | "div", needle: string): string {
  const at = ROW.indexOf(needle);
  expect(at, `"${needle}" not found in the result row`).toBeGreaterThan(-1);

  const openAt = ROW.lastIndexOf(`<${tag}`, at);
  expect(openAt, `no <${tag}> wraps "${needle}"`).toBeGreaterThan(-1);

  // End of the opening tag: a `>` at brace-depth 0 and outside quotes, so
  // `=>` inside onClick handlers does not terminate it early.
  const tagText = ROW.slice(openAt, scanOpeningTag(ROW, openAt));
  const attr = /className[=\s]/.test(tagText)
    ? tagText.slice(/className[=\s]/.exec(tagText)!.index + /className[=\s]/.exec(tagText)![0].length)
    : tagText;

  const quoted = attr.match(/^"([^"]*)"/);
  if (quoted) return quoted[1];
  if (attr.startsWith("{")) {
    // depth starts at 1: attr[0] is the opening brace we are scanning past.
    let depth = 1;
    for (let i = 1; i < attr.length; i++) {
      if (attr[i] === "{") depth++;
      else if (attr[i] === "}") {
        depth--;
        if (depth === 0) return attr.slice(1, i);
      }
    }
  }
  return attr;
}

/** Index just past the `>` that closes the opening tag starting at `from`. */
function scanOpeningTag(src: string, from: number): number {
  let depth = 0;
  let quote: string | null = null;
  for (let i = from; i < src.length; i++) {
    const c = src[i];
    if (quote) {
      if (c === quote) quote = null;
      continue;
    }
    if (c === '"' || c === "'") quote = c;
    else if (c === "{") depth++;
    else if (c === "}") depth--;
    else if (c === ">" && depth === 0) return i + 1;
  }
  return src.length;
}

describe("workspace search result row — trailing action stays at the row edge", () => {
  it("the row uses a 3-column grid so the button can't be pushed off-screen", () => {
    // icon (auto) | name+dir (minmax(0,1fr), clips on overflow) |
    // button (auto). The previous flat flex row let a long `shrink-0`
    // name push the row wider than the dropdown and ride the
    // `ml-auto` button out past the right edge.
    expect(ROW).toMatch(/grid-cols-\[auto_minmax\(0,1fr\)_auto\]/);
    // Belt-and-braces: the row also clips via `overflow-hidden` so any
    // future child that does overflow the grid is still contained.
    expect(ROW).toMatch(/(^|\s)overflow-hidden(\s|$)/);
  });

  it("the text wrapper clips its own overflow instead of pushing siblings", () => {
    // The inner flex around name + dir: without these two classes, the
    // name could reflow inside the wrapper and force the grid column
    // to demand more space than `minmax(0, 1fr)` is willing to give.
    const wrap = classListOf("div", "abbreviatePath(f.dir)");
    expect(wrap).toMatch(/(^|\s)min-w-0(\s|$)/);
    expect(wrap).toMatch(/(^|\s)overflow-hidden(\s|$)/);
  });

  it("the file name is pinned at its content width", () => {
    const cls = classListOf("span", "{f.name}");
    // `shrink-0` is by design here. The button no longer depends on the
    // name shrinking for `ml-auto` to work — the grid column pins it.
    // `whitespace-nowrap` keeps the name on one line so it renders at
    // its full content width until the wrapper clips it.
    expect(cls).toMatch(/(^|\s)shrink-0(\s|$)/);
    expect(cls).toMatch(/(^|\s)whitespace-nowrap(\s|$)/);
  });

  it("the dir span is the only thing that yields under pressure", () => {
    const cls = classListOf("span", "abbreviatePath(f.dir)");
    expect(cls).toMatch(/(^|\s)min-w-0(\s|$)/);
    expect(cls).toMatch(/(^|\s)truncate(\s|$)/);
    // The dir must NOT be pinned at shrink-0 — it has to absorb the
    // squeeze so the name keeps its content width.
    expect(cls).not.toMatch(/(^|\s)shrink-0(\s|$)/);
  });

  it("the add-to-chat button keeps shrink-0 and no longer relies on ml-auto", () => {
    const cls = classListOf("button", "handleSearchAddToChat(f)");
    const flat = cls.replace(/["'`]/g, " ");
    // `shrink-0` keeps the button from being squeezed.
    expect(flat).toMatch(/(^|\s)shrink-0(\s|$)/);
    // `ml-auto` is the leftover from the old flex row. The grid column
    // places the button at the trailing edge on its own; `ml-auto`
    // would be a misleading hint that the regression could come back.
    expect(flat).not.toMatch(/(^|\s)ml-auto(\s|$)/);
  });

  it("the row exposes the full path for clipped names", () => {
    expect(ROW).toMatch(/title=\{f\.relPath\}/);
  });
});
