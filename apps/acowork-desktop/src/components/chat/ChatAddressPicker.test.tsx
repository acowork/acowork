// @vitest-environment jsdom
/**
 * The second level of the `@` (ADR-106 D7, redrawn by the 2026-11-04
 * revision): a typed `@session` is MATERIAL, not a delivery.
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
 * - a reference TOGGLES and stays open across picks — the commit is the
 *   footer's "done", which hands `{refs, labels}` to the composer, and the
 *   fifth pick is the last the Gateway's 400 allows;
 * - "去续投" is a navigation (`onLocate`), never an address — it commits
 *   nothing to the message;
 * - the popup's chrome (one visible case + one source anchor below): the cursor
 *   row carries the app-wide accent tint, inset from the hairline, because a
 *   neutral full-bleed band made the two `@` levels look glued to their frame.
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { fireEvent, render, screen, within } from "@testing-library/react";
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

function picker(onPick = vi.fn(), onClose = vi.fn(), onLocate = vi.fn()) {
  render(
    <ChatAddressPicker
      instanceId="agent-a"
      chatSessionId={CHAT}
      agentName="Architect"
      onPick={onPick}
      onLocate={onLocate}
      onClose={onClose}
    />,
  );
  return { onPick, onClose, onLocate };
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

  it("attaches the picked session as material on commit; the escape names none", async () => {
    const { onPick } = picker();
    const done = await screen.findByTestId("chat-address-done");
    fireEvent.click(screen.getByText("Review the schema"));
    // A reference keeps the list open — multi-select means one click is not
    // yet the answer; "done" is what commits it.
    expect(onPick).not.toHaveBeenCalled();
    fireEvent.click(done);
    expect(onPick).toHaveBeenLastCalledWith({
      refs: [{ agentInstanceId: "agent-a", sessionId: "s-2" }],
      labels: { "agent-a|s-2": "Review the schema" },
    });
    fireEvent.click(screen.getByText("chatView.address.noPreference"));
    expect(onPick).toHaveBeenLastCalledWith({});
  });

  it("the escape row still means new work, immediately", async () => {
    const { onPick } = picker();
    fireEvent.click(await screen.findByText("chatView.block.newTask"));
    expect(onPick).toHaveBeenCalledWith({ forceNew: true });
  });

  it("『去续投』 is a navigation — it locates, it never addresses", async () => {
    const { onPick, onLocate } = picker();
    const row = (
      await screen.findByText("Review the schema")
    ).closest("[data-address-index]") as HTMLElement;
    fireEvent.click(within(row).getByText("chatView.address.goContinue"));
    expect(onLocate).toHaveBeenCalledWith("s-2", "Review the schema");
    expect(onPick).not.toHaveBeenCalled();
  });

  it("clamps the reference at the Gateway's five and keeps commit visible", async () => {
    seed([
      session("s-1", "one"),
      session("s-2", "two"),
      session("s-3", "three"),
      session("s-4", "four"),
      session("s-5", "five"),
      session("s-6", "six"),
    ]);
    picker();
    const boxes = await screen.findAllByRole("checkbox");
    expect(boxes).toHaveLength(6);
    for (const box of boxes.slice(0, 5)) fireEvent.click(box);
    // The sixth is stopped at the pick, not learned from a failed send.
    expect(boxes[5].hasAttribute("disabled")).toBe(true);
    expect(screen.getByTestId("chat-address-done")).toBeTruthy();
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

  it("walks the list with the arrows and takes effect on Enter in one key", async () => {
    const { onPick } = picker();
    const list = await screen.findByTestId("chat-address-picker");
    // The highlight starts on the escape row; one step down is the first session.
    fireEvent.keyDown(list, { key: "ArrowDown" });
    fireEvent.keyDown(list, { key: "Enter" });
    // Enter is COMMIT-NOW, like Enter on "new task": the highlighted run is
    // forced into the selection even though Space never checked it, and the
    // picker closes — no second trip to the footer.
    expect(onPick).toHaveBeenCalledWith({
      refs: [{ agentInstanceId: "agent-a", sessionId: "s-1" }],
      labels: { "agent-a|s-1": "Draft the release note" },
    });
  });

  it("Enter adds the highlighted run to what Space already picked, then commits", async () => {
    const { onPick } = picker();
    const list = await screen.findByTestId("chat-address-picker");
    fireEvent.keyDown(list, { key: "ArrowDown" });
    fireEvent.keyDown(list, { key: " " }); // s-1 picked, list stays open
    expect(onPick).not.toHaveBeenCalled();
    fireEvent.keyDown(list, { key: "ArrowDown" });
    fireEvent.keyDown(list, { key: "Enter" }); // s-2 forced in, both commit
    expect(onPick).toHaveBeenCalledWith({
      refs: [
        { agentInstanceId: "agent-a", sessionId: "s-1" },
        { agentInstanceId: "agent-a", sessionId: "s-2" },
      ],
      labels: {
        "agent-a|s-1": "Draft the release note",
        "agent-a|s-2": "Review the schema",
      },
    });
  });

  it("Enter on a focused checkbox selects that run and commits immediately", async () => {
    const { onPick } = picker();
    const list = await screen.findByTestId("chat-address-picker");
    const box = list.querySelectorAll('button[role="checkbox"]')[1];
    // The native click would toggle and keep the list open; Enter means
    // select + commit + close instead.
    fireEvent.keyDown(box, { key: "Enter" });
    expect(onPick).toHaveBeenCalledWith({
      refs: [{ agentInstanceId: "agent-a", sessionId: "s-2" }],
      labels: { "agent-a|s-2": "Review the schema" },
    });
  });

  it("keeps Enter on the escape row as the one-key new task", async () => {
    const { onPick } = picker();
    const list = await screen.findByTestId("chat-address-picker");
    fireEvent.keyDown(list, { key: "Enter" });
    expect(onPick).toHaveBeenCalledWith({ forceNew: true });
  });

  it("toggles the highlighted run's reference on Space, the checkbox's own key", async () => {
    const { onPick } = picker();
    const list = await screen.findByTestId("chat-address-picker");
    const box = () =>
      list.querySelectorAll('button[role="checkbox"]')[0] as HTMLButtonElement;
    fireEvent.keyDown(list, { key: "ArrowDown" });
    fireEvent.keyDown(list, { key: " " });
    expect(box().getAttribute("aria-checked")).toBe("true");
    // Space is a TOGGLE, not a one-way pick — un-highlighting matters as much
    // when the thumb, not the Enter key, is doing the work.
    fireEvent.keyDown(list, { key: " " });
    expect(box().getAttribute("aria-checked")).toBe("false");
    expect(onPick).not.toHaveBeenCalled();
  });

  it("leaves Space on the escape row as typing, not a pick", async () => {
    const { onPick, onClose } = picker();
    const input = await screen.findByLabelText("chatView.address.search");
    // The highlight starts on the "new task" row, which has no reference to
    // toggle; swallowing Space there would make titles with a space
    // unsearchable.
    fireEvent.keyDown(input, { key: " " });
    expect(onPick).not.toHaveBeenCalled();
    expect(onClose).not.toHaveBeenCalled();
  });

  it("does not double-handle Space inside a focused checkbox", async () => {
    picker();
    const list = await screen.findByTestId("chat-address-picker");
    const box = list.querySelectorAll('button[role="checkbox"]')[0];
    // A focused control owns its activation keys: the container must stay out
    // of the way so the browser's native button click toggles exactly once.
    fireEvent.keyDown(box, { key: " " });
    expect(box.getAttribute("aria-checked")).toBe("false");
  });

  it("leaves Tab to the browser so 'done' is reachable by keyboard", async () => {
    const { onPick } = picker();
    const list = await screen.findByTestId("chat-address-picker");
    // Tab used to be hijacked as an Enter alias, which sealed every control
    // behind the search box — including the footer's commit button.
    fireEvent.keyDown(list, { key: "ArrowDown" });
    fireEvent.keyDown(list, { key: "Tab" });
    expect(onPick).not.toHaveBeenCalled();
    const box = list.querySelectorAll('button[role="checkbox"]')[0];
    expect(box.getAttribute("aria-checked")).toBe("false");
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
    // The keys bubble from the input to the list's handler: Enter commits
    // straight away, no footer click needed.
    expect(onPick).toHaveBeenCalledWith({
      refs: [{ agentInstanceId: "agent-a", sessionId: "s-1" }],
      labels: { "agent-a|s-1": "Draft the release note" },
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
      "[data-address-index]",
    ) as HTMLElement;
    // Solid + breathing — the session-history list's own "still working" mark
    // (design 27 §6.1), so the two lists speak one cue.
    expect(row.querySelector("svg.animate-pulse")).toBeTruthy();
    expect(row.textContent).toContain("chatView.address.statusWorking");
  });

  it("marks a settled task with the outline brain, inventing no breath", async () => {
    picker();
    const row = (await screen.findByText("Review the schema")).closest(
      "[data-address-index]",
    ) as HTMLElement;
    // No entry in the session-state map = never reported busy: outline, static.
    expect(row.querySelector("svg")).toBeTruthy();
    expect(row.querySelector("svg.animate-pulse")).toBeNull();
    expect(row.querySelector("svg[fill='none']")).toBeTruthy();
    expect(row.textContent).toContain("chatView.address.statusIdle");
  });
});
