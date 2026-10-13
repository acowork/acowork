/**
 * The in-thread "N new messages" pill in [InboxPanel](./InboxPanel.tsx) must
 * carry the same red as the other two "you have unread" surfaces — the NavBar
 * tile dot ([NavButton](../components/common/NavButton.tsx)) and the UserList
 * row bubble ([UserList](../components/user-list/UserList.tsx)).
 *
 * It used to be `bg-[var(--color-accent)]`, which reads as "you are here"
 * (this app's selected colour) on a signal that means the opposite:
 * something landed while you were looking elsewhere. A user called this out
 * after seeing the WeChat convention — a red dot — and the accent pill
 * immediately looked wrong.
 *
 * Pinned as a source scan rather than a render assertion because InboxPanel
 * pulls in `useStickToBottom`, `MessageRow`, the auth store, the chat store,
 * and the i18n hook. A class-name regex does not go stale as long as the
 * `data-testid` marker is preserved — and a future refactor that moves the
 * pill to a child component still trips this test until the marker (or the
 * colour) is updated deliberately.
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { describe, expect, it } from "vitest";

const SRC = resolve(
  dirname(fileURLToPath(import.meta.url)),
  "InboxPanel.tsx",
);

/** Pull out the className string literal on the pill's <button>. */
function pillClassName(): string {
  const code = readFileSync(SRC, "utf8");
  // The pill is the only element in this file carrying the `inbox-new-messages-pill`
  // test id. Slice from that marker forward to its `className="..."` — a small
  // window so a className change elsewhere cannot accidentally satisfy this.
  const start = code.indexOf('data-testid="inbox-new-messages-pill"');
  if (start === -1) {
    throw new Error(
      "InboxPanel.tsx no longer has the `inbox-new-messages-pill` test id — " +
        "either the pill was removed (revisit this test) or the marker moved " +
        "(update this scan).",
    );
  }
  const window = code.slice(start, start + 800);
  const m = window.match(/className="([^"]+)"/);
  if (!m) {
    throw new Error(
      "pill <button> has no className string literal — scan pattern out of date.",
    );
  }
  return m[1];
}

describe("InboxPanel 'N new messages' pill colour", () => {
  it("fills with the destructive red token", () => {
    expect(pillClassName()).toContain("bg-[var(--color-destructive)]");
  });

  it("does not fall back to the global accent", () => {
    // The specific regression: accent is the app's "selected" colour and
    // reads as "you are here" — the opposite of what this pill means.
    expect(pillClassName()).not.toContain("bg-[var(--color-accent)]");
  });
});
