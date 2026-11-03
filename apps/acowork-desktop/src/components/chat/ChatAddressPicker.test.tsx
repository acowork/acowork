// @vitest-environment jsdom
/**
 * The second level of the `@` (ADR-106 D7).
 *
 * The list is the session-history menu's list: the same `GET …/sessions` paging
 * path, narrowed to this chat by the `chat_session_id` / `surface` parameters,
 * newest first, with a search box and a pager. What these tests pin is
 * the shape of the offer, plus the pieces of its plumbing and look that had
 * drifted:
 * - the rows come from the chat-scoped listing slice, fetched with this chat's
 *   scope, so the picker cannot be shown somebody else's work or move the
 *   sidebar's page cursor;
 * - "new task" is the FIRST row, because D6 requires a question to be answerable
 *   in one click rather than after a scroll;
 * - declining to name a session is still an answer (the ladder's lower levels
 *   must stay reachable);
 * - the popup's chrome (one visible case + one source anchor below): the cursor
 *   row carries the app-wide accent tint, inset from the hairline, because a
 *   neutral full-bleed band made the two `@` levels look glued to their frame.
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { fireEvent, render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { ChatAddressPicker } from "./ChatAddressPicker";
import { chatListKey, useAgentStore } from "../../stores/agentStore";
import { useChatStore } from "../../stores/chatStore";
import type { SessionInfo } from "../../lib/types";

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({
    t: (key: string, vars?: Record<string, unknown>) =>
      vars ? `${key}:${Object.values(vars).join("/")}` : key,
  }),
}));

function session(id: string, title: string): SessionInfo {
  return {
    session_id: id,
    title,
    created_at: "2026-11-01T10:00:00Z",
    message_count: 1,
    status: "idle",
  } as SessionInfo;
}

const SESSIONS = [
  session("s-1", "Draft the release note"),
  session("s-2", "Review the schema"),
];

/** The chat the picker is opened in — every case below is one 1:1 pair. */
const CHAT = "chat-1";

function seed(
  sessions: SessionInfo[],
  { currentPage = 1, totalPages = 1, isLoading = false, error = false } = {},
) {
  const fetchChatSessions = vi.fn(async () => {});
  useAgentStore.setState({
    chatSessionLists: {
      [chatListKey("agent-a", "direct", CHAT)]: {
        sessions,
        pagination: {
          currentPage,
          totalPages,
          totalCount: sessions.length,
          pageSize: 10,
        },
        isLoading,
        error,
      },
    },
    fetchChatSessions,
  } as never);
  return fetchChatSessions;
}

function picker(onPick = vi.fn(), onClose = vi.fn()) {
  render(
    <ChatAddressPicker
      instanceId="agent-a"
      chatSessionId={CHAT}
      agentName="Architect"
      onPick={onPick}
      onClose={onClose}
    />,
  );
  return { onPick, onClose };
}

describe("ChatAddressPicker", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    // The run-state mark is read from the chat store; start every case clean so
    // one case's `thinking` cannot breathe in another's row.
    useChatStore.setState({ agentStates: {} });
    seed(SESSIONS);
  });

  it("offers the cheap escape FIRST, above every suggestion", async () => {
    picker();
    const list = await screen.findByTestId("chat-address-picker");
    expect(list.querySelector('button[role="option"]')?.textContent).toContain(
      "chatView.block.newTask",
    );
  });

  it("names the session it is given, and the escape names none", async () => {
    const { onPick } = picker();
    fireEvent.click(await screen.findByText("Review the schema"));
    expect(onPick).toHaveBeenLastCalledWith({
      sessionId: "s-2",
      label: "Review the schema",
    });
    fireEvent.click(screen.getByText("chatView.address.noPreference"));
    expect(onPick).toHaveBeenLastCalledWith({});
  });

  it("keeps 'no preference' as an answer when the list is not empty", async () => {
    const { onPick } = picker();
    fireEvent.click(await screen.findByText("chatView.address.noPreference"));
    expect(onPick).toHaveBeenLastCalledWith({});
  });

  it("closes on Escape without choosing", async () => {
    const { onPick, onClose } = picker();
    const list = await screen.findByTestId("chat-address-picker");
    fireEvent.keyDown(list, { key: "Escape" });
    expect(onClose).toHaveBeenCalled();
    expect(onPick).not.toHaveBeenCalled();
  });

  it("walks the list with the arrows and picks the highlighted row on Enter", async () => {
    const { onPick } = picker();
    const list = await screen.findByTestId("chat-address-picker");
    // The highlight starts on the escape row; one step down is the first session.
    fireEvent.keyDown(list, { key: "ArrowDown" });
    fireEvent.keyDown(list, { key: "Enter" });
    expect(onPick).toHaveBeenCalledWith({
      sessionId: "s-1",
      label: "Draft the release note",
    });
  });

  it("opens with the caret in the search box, where typing a filter starts", async () => {
    picker();
    const input = await screen.findByLabelText("chatView.address.search");
    // What this pins: focus parked on the listbox — or stolen by a host that
    // cancels `mousedown` across the whole popup — left the search box
    // unclickable. jsdom never applies mousedown's default focus, so the check
    // is on where the picker puts the caret itself.
    expect(document.activeElement).toBe(input);
  });

  it("keeps the list's keyboard reachable from inside the search box", async () => {
    const { onPick } = picker();
    const input = await screen.findByLabelText("chatView.address.search");
    fireEvent.change(input, { target: { value: "draft" } });
    fireEvent.keyDown(input, { key: "ArrowDown" });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(onPick).toHaveBeenCalledWith({
      sessionId: "s-1",
      label: "Draft the release note",
    });
  });

  it("narrows the list as the human types in the search box", async () => {
    picker();
    await screen.findByText("Draft the release note");
    fireEvent.change(screen.getByLabelText("chatView.address.search"), {
      target: { value: "schema" },
    });
    expect(screen.queryByText("Draft the release note")).toBeNull();
    expect(screen.getByText("Review the schema")).toBeTruthy();
  });

  it("asks the chat-scoped listing for the next page", async () => {
    const fetchChatSessions = seed(SESSIONS, { currentPage: 1, totalPages: 3 });
    picker();
    fireEvent.click(await screen.findByLabelText("chatView.address.nextPage"));
    expect(fetchChatSessions).toHaveBeenCalledWith("agent-a", CHAT, "direct", 2);
  });

  it("asks with THIS chat's scope, not the agent's whole history", async () => {
    // The permission narrowing lives on the server and is driven by these two
    // parameters: a picker that fetched the unscoped listing would offer runs
    // this chat may not address, and the router would then be handed an
    // address the human had no right to name. A chat-scoped list is also never
    // the sidebar's list — sharing it would move the sidebar under the human.
    const fetchChatSessions = vi.fn(async () => {});
    useAgentStore.setState({ chatSessionLists: {}, fetchChatSessions } as never);
    picker();
    await screen.findByTestId("chat-address-picker");
    expect(fetchChatSessions).toHaveBeenCalledWith("agent-a", CHAT, "direct", 1);
  });

  it("keeps a group's page separate from the same agent's 1:1 page", async () => {
    // Same agent, same chat id space, different rules — so a group's `@` must
    // not be served the page the 1:1 composer loaded, nor repaint it.
    const fetchChatSessions = seed(SESSIONS);
    render(
      <ChatAddressPicker
        instanceId="agent-a"
        chatSessionId={CHAT}
        surface="group"
        onPick={vi.fn()}
        onClose={vi.fn()}
      />,
    );
    await screen.findByTestId("chat-address-picker");
    expect(fetchChatSessions).toHaveBeenCalledWith("agent-a", CHAT, "group", 1);
  });

  it("says so when there is nothing to continue, without hiding the escape", async () => {
    seed([]);
    picker();
    await screen.findByText("chatView.address.empty");
    expect(screen.getByText("chatView.block.newTask")).toBeTruthy();
    expect(screen.queryByText("chatView.address.noPreference")).toBeNull();
  });

  it("says the read failed instead of claiming the chat is empty", async () => {
    // A failed first read used to paint the empty state, which claims this chat
    // has no tasks — and the listing is fetched once per chat key, so the claim
    // would survive reopening the picker.
    const fetchChatSessions = seed([], { error: true });
    picker();
    await screen.findByText("chatView.address.failed");
    expect(screen.queryByText("chatView.address.empty")).toBeNull();
    fireEvent.click(screen.getByText("common.retry"));
    expect(fetchChatSessions).toHaveBeenCalledWith("agent-a", CHAT, "direct", 1);
  });

  it("closes when the pointer goes down outside it", async () => {
    const { onClose } = picker();
    const list = await screen.findByTestId("chat-address-picker");
    fireEvent.mouseDown(list);
    expect(onClose).not.toHaveBeenCalled();
    fireEvent.mouseDown(document.body);
    expect(onClose).toHaveBeenCalled();
  });

  it("leaves the opening control alone, so its own toggle still closes it", async () => {
    const onClose = vi.fn();
    const anchor = document.createElement("button");
    document.body.appendChild(anchor);
    render(
      <ChatAddressPicker
        instanceId="agent-a"
        chatSessionId={CHAT}
        anchorRef={{ current: anchor }}
        onPick={vi.fn()}
        onClose={onClose}
      />,
    );
    await screen.findByTestId("chat-address-picker");
    fireEvent.mouseDown(anchor);
    expect(onClose).not.toHaveBeenCalled();
    anchor.remove();
  });

  it("shows whose sessions these are on a multi-party surface", async () => {
    picker();
    expect(
      await screen.findByText("chatView.address.subject:Architect"),
    ).toBeTruthy();
  });

  it("tints the cursor row with the accent and floats it inside the hairline", async () => {
    picker();
    const list = await screen.findByTestId("chat-address-picker");
    const row = list.querySelector('[data-address-index="0"]') as HTMLElement;
    // `listRowSelected` — the same accent family the session-history list uses.
    // `--color-nav-item-hover` is documented as an icon-control fill and must
    // not come back here as a list row's cursor.
    expect(row.className).toContain("--color-accent");
    expect(row.className).not.toContain("bg-nav-item-hover");
    // Rounded and inset by the list region, so the wash reads as a row inside
    // the popup instead of a band glued to both edges.
    expect(row.className).toContain("rounded");
    expect(row.parentElement?.className).toContain("px-1");
    // `role="none"` keeps the options owned by the `listbox` above: an
    // intervening generic element would otherwise break the required relation.
    expect(row.parentElement?.getAttribute("role")).toBe("none");
  });

  it("gives the first `@` level the same cursor tint", () => {
    // Source anchor, not rendered DOM: the group's first level lives deep in
    // `GroupChatPanel`, whose render needs the whole store graph (the same
    // trade-off `chatCardChrome.test.tsx` makes). The two levels open from one
    // gesture, so they must not learn two visual languages.
    const src = readFileSync(
      resolve(dirname(fileURLToPath(import.meta.url)), "GroupChatPanel.tsx"),
      "utf8",
    );
    expect(src).toContain("listRowSelected");
    expect(src).not.toContain("bg-nav-item-hover text-text");
  });

  it("breathes the brain mark on a task that is still working", async () => {
    useChatStore.getState().updateSessionStatus("agent-a", "s-1", {
      status: "thinking",
    });
    picker();
    const row = (await screen.findByText("Draft the release note")).closest(
      "button[data-address-index]",
    ) as HTMLElement;
    // Solid + breathing — the session-history list's own "still working" mark
    // (design 27 §6.1), so the two lists speak one cue.
    expect(row.querySelector("svg.animate-pulse")).toBeTruthy();
    expect(row.textContent).toContain("chatView.address.statusWorking");
  });

  it("marks a settled task with the outline brain, inventing no breath", async () => {
    picker();
    const row = (await screen.findByText("Review the schema")).closest(
      "button[data-address-index]",
    ) as HTMLElement;
    // No entry in the session-state map = never reported busy: outline, static.
    expect(row.querySelector("svg")).toBeTruthy();
    expect(row.querySelector("svg.animate-pulse")).toBeNull();
    expect(row.querySelector("svg[fill='none']")).toBeTruthy();
    expect(row.textContent).toContain("chatView.address.statusIdle");
  });
});
