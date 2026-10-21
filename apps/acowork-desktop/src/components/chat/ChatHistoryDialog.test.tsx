/**
 * ChatHistoryDialog — sender naming + tab idiom.
 *
 * Two user-visible regressions pinned here:
 *
 * 1. The sender line printed the raw participant id — for an agent that is
 *    its INSTANCE id (a UUID), which names nobody. Rows must name the agent
 *    from the local registry and the human from the signed-in account, and
 *    only fall back to the raw id when neither resolves.
 * 2. The 文本 / 图片 / 文件 tabs were accent-tinted chips of their own
 *    invention; they now follow `GlobalSearchDialog`'s pill idiom so the two
 *    dialogs read as one family.
 *
 * The tab check scans the source for the class strings (jsdom resolves
 * neither the zinc utilities nor the arbitrary `--ui-*` tokens) — same
 * approach as `common/menuItemSelected.test.tsx`.
 */
import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, act } from "@testing-library/react";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(async () => ({})) }));
vi.mock("../../lib/config", () => ({ getGatewayUrl: () => "http://gw.test" }));

const searchChatHistory = vi.hoisted(() => vi.fn());
vi.mock("../../lib/chat-history-api", () => ({ searchChatHistory }));

import { ChatHistoryDialog } from "./ChatHistoryDialog";
import { useAuthStore } from "../../stores/authStore";
import { useAgentStore } from "../../stores/agentStore";
import type { AgentStorage } from "../../stores/agentStore";
import type { HistoryHit } from "../../lib/chat-history-api";

const ME = "u-me";
/** Deliberately UUID-shaped: an unreadable id is what the bug rendered. */
const AGENT_INSTANCE = "8b1f6f0e-4a4f-4f3a-9d8b-2c5f0a7e6d31";

const __dirname = dirname(fileURLToPath(import.meta.url));
const source = readFileSync(resolve(__dirname, "ChatHistoryDialog.tsx"), "utf8");

function hit(over: Partial<HistoryHit> & Pick<HistoryHit, "from" | "kind" | "body">): HistoryHit {
  return { seq: 1, line_pos: 1, ts: 1_700_000_000, attachments: [], ...over };
}

beforeEach(() => {
  searchChatHistory.mockReset();
  useAuthStore.setState({
    mode: "multi_user",
    status: "logged_in",
    accessToken: "token",
    account: {
      user_id: ME,
      username: "me",
      display_name: "Me Myself",
      role: "user",
      language: "en",
      timezone: "UTC",
      created_at: "",
      updated_at: "",
    },
  });
  useAgentStore.setState({
    agents: {
      [AGENT_INSTANCE]: {
        meta: {
          instance_id: AGENT_INSTANCE,
          agent_id: "com.acowork.senior-engineer",
          name: "senior-engineer",
          display_name: "Senior Engineer",
        },
      } as unknown as AgentStorage,
    },
  } as never);
});

describe("ChatHistoryDialog sender naming", () => {
  it("names the agent from the registry and the user from the account", async () => {
    searchChatHistory.mockResolvedValue({
      hits: [
        hit({ seq: 2, line_pos: 9, from: AGENT_INSTANCE, kind: "agent_message", body: "on it" }),
        hit({ seq: 1, line_pos: 4, from: ME, kind: "user_message", body: "please fix" }),
      ],
      total_lines: 9,
      next_before_line: null,
    });

    await act(async () => {
      render(<ChatHistoryDialog open chatSessionId="s-1" onClose={() => {}} />);
    });

    expect(screen.getByText("Senior Engineer")).toBeTruthy();
    expect(screen.getByText("Me Myself")).toBeTruthy();
    // The unreadable ids never reach the sender line.
    expect(screen.queryByText(AGENT_INSTANCE)).toBeNull();
    expect(screen.queryByText(ME)).toBeNull();

    // Every text row leads with a sender avatar (registry agent -> AgentAvatar,
    // human -> UserAvatar), so a row is scannable by face before the name.
    const agentRow = screen.getByText("on it").closest("button") as HTMLElement;
    const userRow = screen.getByText("please fix").closest("button") as HTMLElement;
    expect(agentRow.querySelector("img")).not.toBeNull();
    expect(userRow.querySelector("img")).not.toBeNull();
    expect(agentRow.firstElementChild?.tagName).toBe("IMG");
  });

  it("keeps a caller-supplied resolver (group surfaces know the directory)", async () => {
    searchChatHistory.mockResolvedValue({
      hits: [hit({ from: "u-bob", kind: "user_message", body: "hi" })],
      total_lines: 1,
      next_before_line: null,
    });

    await act(async () => {
      render(
        <ChatHistoryDialog
          open
          chatSessionId="s-1"
          onClose={() => {}}
          resolveName={() => "Bob"}
        />,
      );
    });

    expect(screen.getByText("Bob")).toBeTruthy();
  });
});

describe("ChatHistoryDialog footer", () => {
  it("keeps the only dismissal in a divider-separated footer", async () => {
    searchChatHistory.mockResolvedValue({ hits: [], total_lines: 0, next_before_line: null });

    await act(async () => {
      render(<ChatHistoryDialog open chatSessionId="s-1" onClose={() => {}} />);
    });

    const close = screen.getByRole("button", { name: "Close" });
    const footer = close.parentElement as HTMLElement;
    // The footer IS the divider-separated dialog zone (border-t + zone height),
    // matching GlobalSearchDialog.
    expect(footer.className).toContain("border-t");
    expect(footer.className).toContain("min-h-[var(--ui-dialog-zone-h)]");
    // One dismissal control only — the header's X is gone.
    expect(screen.getAllByRole("button", { name: "Close" })).toHaveLength(1);
    expect(source).not.toContain("<X ");
  });
});

describe("ChatHistoryDialog tab strip", () => {
  it("uses GlobalSearchDialog's pill tabs", () => {
    // Active pill: zinc wash + full-strength text (NOT accent-colored text on
    // an accent tint, which is what this dialog shipped with).
    expect(source).toContain("rounded-full px-3 py-1.5 text-xs font-medium transition-colors");
    expect(source).toContain('"bg-zinc-200 text-text dark:bg-zinc-700"');
    expect(source).toContain(
      '"text-text-tertiary hover:bg-zinc-100 dark:hover:bg-zinc-700/50"',
    );
    // The hand-rolled accent chip is gone.
    expect(source).not.toContain("bg-[var(--color-accent)]/12");
    // Zone height token, so the strip lines up with the search dialog's rows.
    expect(source).toContain("min-h-[var(--ui-dialog-zone-h)]");
  });
});
