/**
 * The Users group must carry the same row hairline the Agents group does.
 *
 * The report: agent rows in the sidebar are separated by a hairline, the
 * user rows under them are not, so the two groups read as two different
 * lists stitched together. `UserRow` is the only sidebar row that never
 * got the divider — the agent row and the two other sidebar pickers
 * ([ProjectSidebar](../../views/pm/ProjectSidebar.tsx),
 * [ExtensionsView](../../views/ExtensionsView.tsx)) all draw it by hand.
 *
 * This pins the visual contract: every row but the last one in the group
 * gets the divider, and the last one does not (a trailing rule under the
 * final row would double up with the group's own bottom border).
 */
import { describe, it, expect, vi, beforeEach } from "vitest";
import { execSync } from "node:child_process";
import { readFileSync, existsSync } from "node:fs";
import { resolve } from "node:path";
import { render, screen, act } from "@testing-library/react";
import { UserList } from "./UserList";
import { useAuthStore } from "../../stores/authStore";
import { useUserChatStore } from "../../stores/userChatStore";
import type { Role, UserAccount } from "../../lib/types";

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(async () => ({})),
}));

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(() => Promise.resolve(() => {})),
}));

function account(role: Role, user_id: string, display_name: string): UserAccount {
  return {
    user_id,
    username: user_id,
    display_name,
    role,
    language: "en",
    timezone: "UTC",
    created_at: "2025-01-01T00:00:00Z",
    updated_at: "2025-01-01T00:00:00Z",
  };
}

const DIVIDER = "row-divider-b";

/**
 * A `::after` box only exists at all if `content` is set — without it the
 * pseudo-element is not generated and the hairline silently does not
 * render, no matter how correct `border-b` / `bottom-0` / the colour are.
 * Tailwind v4.3 ships NO `after:content-*` utility (verified against the
 * built bundle), so hand-rolled `after:border-b` rows cannot be fixed in
 * JSX at all: the divider is the shared `.row-divider-b` class in
 * globals.css. This guard keeps anyone from reintroducing the dead form.
 *
 * Repo-wide rather than per-component because the bug hit five sidebars
 * at once, and a className assertion cannot catch it (the class is
 * present either way) — only the stylesheet tells.
 */
it("no row draws a divider through a bare after:border-b utility", () => {
  const root = resolve(__dirname, "..", "..", "..");
  const files = execSync(`git ls-files --cached --others --exclude-standard "src/**/*.tsx"`, { encoding: "utf8", cwd: root })
    .split(String.fromCharCode(10))
    .filter(Boolean)
    // `git ls-files` without --cached also lists index entries whose file
    // is gone from the working tree; a deleted-but-unstaged component
    // would then blow up the scan with ENOENT instead of a clean verdict.
    .filter((f) => existsSync(resolve(root, f)))
    // This file legitimately mentions `after:border-b` in assertions.
    .filter((f) => !/\.test\.tsx?$/.test(f));
  const offenders: string[] = [];
  for (const f of files) {
    for (const line of readFileSync(resolve(root, f), "utf8").split(String.fromCharCode(10))) {
      if (/after:border-[bt]/.test(line)) offenders.push(`${f}: ${line.trim()}`);
    }
  }
  expect(offenders).toEqual([]);
});

describe("UserList rows carry the agent-list hairline", () => {
  beforeEach(() => {
    const self = account("admin", "u-self", "Alice");
    const peerA = account("user", "u-peer-a", "Bravo");
    const peerB = account("user", "u-peer-b", "Chandra");
    useAuthStore.setState({
      mode: "multi_user",
      status: "logged_in",
      account: self,
      accessToken: "token",
      viewAsUserId: null,
      registrationOpen: false,
    });
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => ({
        ok: true,
        status: 200,
        json: async () => ({ accounts: [self, peerA, peerB] }),
        text: async () => "{}",
      })),
    );
    useUserChatStore.setState({ activePeerId: null, activePeerLabel: null, chats: [], messages: [] });
  });

  it("divides every pair of rows but leaves the last row undecorated", async () => {
    await act(async () => {
      render(<UserList />);
    });

    const rows = [screen.getByTestId("user-row-u-self"), screen.getByTestId("user-row-u-peer-a")];

    // `u-peer-b` sorts last (admins first, then display name), so it is the
    // only row that must NOT get a rule.
    for (const row of rows) {
      expect(row.className).toContain(DIVIDER);
    }
    expect(screen.getByTestId("user-row-u-peer-b").className).not.toContain(DIVIDER);
  });

  it("still drops the rule on a single-row group", async () => {
    useAuthStore.setState({ account: account("admin", "u-self", "Alice") });
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => ({
        ok: true,
        status: 200,
        json: async () => ({ accounts: [useAuthStore.getState().account!] }),
        text: async () => "{}",
      })),
    );

    await act(async () => {
      render(<UserList />);
    });

    expect(screen.getByTestId("user-row-u-self").className).not.toContain(DIVIDER);
  });

  it("raises the group header on the same fill as the node header", async () => {
    // The Users band must match the Agents node band (NodeGroupHeader) and
    // the right-panel card title bars: one `panel-block` fill. Both themes
    // auto-flip, so a `dark:bg-*` pair here would drift from the token.
    await act(async () => {
      render(<UserList />);
    });

    const band = screen.getByTestId("user-group-header").parentElement!;
    expect(band.className).toContain("bg-panel-block");
    expect(band.className).not.toMatch(/dark:bg-/);
  });
});
