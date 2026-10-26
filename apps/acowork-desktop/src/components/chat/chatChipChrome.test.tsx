/**
 * Chat chips and badges — regression tests for the "no highlight color on a
 * chip outline or a chip label" rule.
 *
 * Why this file exists:
 *   Same root cause and same rule as chatCardChrome.test.tsx, one tier down:
 *   the inline chips in the chat surfaces had drifted to an accent OUTLINE
 *   plus accent LABEL text (the task chip, the active-skill badge, the
 *   "replying to" badge, the uploading document chip, the pending "from chat"
 *   tag). The global language keeps `--color-accent` on button backgrounds —
 *   including the hover/selected background washes in `listRowHover` — never
 *   on a border or a label.
 *
 * What this pins:
 *   1. DocumentChip's uploading state (rendered): a neutral hairline, no
 *      accent border. The `animate-pulse` carries the progress signal.
 *   2. TaskChip (rendered through GroupChatPanel in its own test file) and
 *      the two composer badges + the pending tag (source-anchored): the
 *      accent survives only as a background wash, never as border or text.
 *   3. The negative: no chat chip file paints an accent BORDER any more —
 *      that is the exact regression this rule is about, so it is pinned as a
 *      whole-file assertion rather than line by line.
 *
 * Why some of these scan source instead of rendering:
 *   ChatPanel and AgentChatView need the whole chat store harness to mount
 *   (MQTT, Tauri, monaco, session stores), which makes a render assertion on
 *   one badge brittle and unrepresentative. The class string IS the spec —
 *   same trade-off as capsule.test.tsx.
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { render } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { DocumentChip } from "./DocumentChip";

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));

const __dirname = dirname(fileURLToPath(import.meta.url));

function readSource(file: string): string {
  return readFileSync(resolve(__dirname, file), "utf8");
}

/** Return the single source line that contains `anchor`, so a rule can be
 *  pinned against one specific element in a large file. Throws if the anchor
 *  is missing or ambiguous — a moved/renamed badge turns the test red instead
 *  of silently skipping. */
function lineOf(file: string, anchor: string): string {
  const lines = readSource(file).split("\n").filter((l) => l.includes(anchor));
  if (lines.length !== 1) {
    throw new Error(`expected exactly one line containing "${anchor}" in ${file}, got ${lines.length}`);
  }
  return lines[0];
}

/** Same idea, but a slice of the source around a unique anchor — for elements
 *  whose class string and content live on different lines (a JSX badge spans
 *  an opening tag, a class attribute, and its children). */
function blockAround(file: string, anchor: string, before: number, after: number): string {
  const lines = readSource(file).split("\n");
  const hits = lines.map((l, i) => (l.includes(anchor) ? i : -1)).filter((i) => i >= 0);
  if (hits.length !== 1) {
    throw new Error(`expected exactly one line containing "${anchor}" in ${file}, got ${hits.length}`);
  }
  return lines.slice(hits[0] - before, hits[0] + after + 1).join("\n");
}

describe("DocumentChip uploading state", () => {
  it("wears a neutral hairline, not the highlight color", () => {
    const { container } = render(<DocumentChip filename="spec.pdf" format="pdf" status="uploading" />);
    const chip = container.querySelector("div[class*='rounded-md border']") as HTMLElement;
    expect(chip.className).toContain("border-border-divider");
    expect(chip.className).not.toContain("--color-accent");
    // The pulse is what replaced the accent as the progress cue.
    expect(chip.className).toContain("animate-pulse");
  });

  it("keeps the resting state on the outer hairline", () => {
    const { container } = render(<DocumentChip filename="spec.pdf" format="pdf" status="success" />);
    const chip = container.querySelector("div[class*='rounded-md border']") as HTMLElement;
    expect(chip.className).toContain("border-border-outer");
  });
});

describe("composer badges and inline tags", () => {
  // The two badges are copy-paste twins on two different chat surfaces; both
  // must land on the same neutral recipe. The anchor is the part of the class
  // string that survives the fix (the wash), so the test stays meaningful on
  // both sides of it.
  for (const file of ["ChatPanel.tsx", "AgentChatView.tsx"] as const) {
    it(`${file}: the badge outline and label stay neutral`, () => {
      const line = lineOf(file, "bg-[var(--color-accent)]/10 px-1.5 py-0.5 text-xs font-medium");
      expect(line).toContain("border-border-outer");
      // Tertiary, not secondary: the inline style this replaced resolved to
      // --color-text-tertiary, so the label tier must survive the accent fix
      // unchanged. Pinning the tier here stops a future pass from "helpfully"
      // darkening the badge while touching its border.
      expect(line).toContain("text-text-tertiary");
      expect(line).not.toContain("border-[var(--color-accent)]");
      // The label color used to ride an inline style, which no class-based
      // assertion would have caught.
      expect(line).not.toContain('style={{ color: "var(--color-accent)" }}');
      // Highlight survives only as the background wash.
      expect(line).toContain("bg-[var(--color-accent)]/10");
    });
  }

  it("ChatPanel: the pending 'from chat' tag is a label pill, not a highlight", () => {
    const block = blockAround("ChatPanel.tsx", "chatPanel.pendingFromChat", 1, 0);
    expect(block).toContain("bg-chat-badge");
    expect(block).toContain("text-text-secondary");
    expect(block).not.toContain("--color-accent");
  });

  it("GroupChatPanel: the task chip keeps the wash but drops the accent outline", () => {
    const block = blockAround("GroupChatPanel.tsx", "groupChat.taskChipCopy", 2, 5);
    expect(block).toContain("border-border-outer");
    expect(block).toContain("text-text-secondary");
    expect(block).not.toContain("hover:border-[var(--color-accent)]");
    expect(block).toContain("bg-[var(--color-accent)]/10");
  });

  it("no chat chip file paints an accent border any more", () => {
    for (const file of ["ChatPanel.tsx", "AgentChatView.tsx", "GroupChatPanel.tsx", "DocumentChip.tsx"]) {
      expect(
        readSource(file),
        `${file}: an accent border on a chip is the exact drift this rule forbids`,
      ).not.toMatch(/border-\[var\(--color-accent\)\]/);
    }
  });
});
