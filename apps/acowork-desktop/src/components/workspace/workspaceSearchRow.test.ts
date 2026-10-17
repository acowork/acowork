/**
 * The right workspace search dropdown must keep its trailing "add to chat"
 * button pinned to the END OF THE ROW at any pane width.
 *
 * Why this file exists:
 *   User-visible feature: hovering a search hit reveals a `+`
 *   (MessageSquarePlus) button that attaches the file to the active
 *   conversation. The row read: `[icon] name  dir   [+]`. The file name was
 *   `shrink-0`, so it could never shrink below its content width and a long
 *   name pushed the row wider than the pane. The `shrink-0` dir span then
 *   absorbed the squeeze down to zero width, leaving `ml-auto` on the button
 *   with no free space to consume — so the button rode out past the end of
 *   the name and only became visible once the pane was dragged wide enough.
 *   The affordance was effectively missing on a narrow right panel.
 *
 * What this pins (the flex shrink contract, by element):
 *   1. name  — carries `min-w-0` + `truncate` and a shrink factor > 1, so
 *              it yields under pressure, yields faster than the dir (name is
 *              the identifying token), and is never `shrink-0`.
 *   2. dir   — carries `min-w-0` + `truncate` and a shrink factor >= 1, so
 *              it stays the secondary shrink victim.
 *   3. button— keeps `ml-auto` + `shrink-0`: `ml-auto` is what pins it to
 *              the row's trailing edge, `shrink-0` is what keeps it from
 *              ever being squeezed out again.
 *   4. row   — exposes the full path via `title`, so a truncated name is
 *              still recoverable on hover.
 *
 * Why source scanning instead of rendered DOM:
 *   The bug is pure flex layout — `ml-auto` resolving to 0 because no free
 *   space remained. jsdom computes no layout (clientWidth is always 0), so a
 *   render assertion cannot observe it; the class strings ARE the spec here.
 *   Same approach as listHeaderHeight.test.ts and sidebarRowFontSize.test.ts.
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
function classListOf(tag: "span" | "button", needle: string): string {
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
  it("the file name yields instead of being shrink-0", () => {
    const cls = classListOf("span", "{f.name}");
    // `shrink-0` here is the regression: it let a long name push the row
    // wider than the pane and starve the button's `ml-auto`.
    expect(cls).not.toMatch(/(^|\s)shrink-0(\s|$)/);
    // `truncate` needs `min-w-0` to have anything to shrink against.
    expect(cls).toMatch(/(^|\s)min-w-0(\s|$)/);
    expect(cls).toMatch(/(^|\s)truncate(\s|$)/);
    // Shrink factor > the dir's, so the identifying name outlives the path.
    const factor = cls.match(/shrink-\[(\d+(?:\.\d+)?)\]/);
    expect(factor, "name span must carry an explicit shrink factor").not.toBeNull();
    expect(Number(factor![1])).toBeGreaterThan(1);
  });

  it("the dir span is the secondary shrink victim", () => {
    const cls = classListOf("span", "abbreviatePath(f.dir)");
    expect(cls).toMatch(/(^|\s)min-w-0(\s|$)/);
    expect(cls).toMatch(/(^|\s)truncate(\s|$)/);
    // Default `flex-shrink: 1` — must not be pinned with shrink-0 either.
    expect(cls).not.toMatch(/(^|\s)shrink-0(\s|$)/);
  });

  it("the add-to-chat button keeps ml-auto + shrink-0", () => {
    const cls = classListOf("button", "handleSearchAddToChat(f)");
    // The button is authored with `cn(...)`, so the classes live in string
    // literals inside the expression rather than in one flat list — assert
    // against the joined literals instead of the raw source text.
    const flat = cls.replace(/["'`]/g, " ");
    // `ml-auto` pins the button to the row's trailing edge;
    // `shrink-0` stops it being squeezed out again.
    expect(flat).toMatch(/(^|\s)ml-auto(\s|$)/);
    expect(flat).toMatch(/(^|\s)shrink-0(\s|$)/);
  });

  it("the row exposes the full path for truncated names", () => {
    expect(ROW).toMatch(/title=\{f\.relPath\}/);
  });
});
