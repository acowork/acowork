// @vitest-environment jsdom
/**
 * The second level of the `@` (ADR-106 D7).
 *
 * What these tests pin is the shape of the offer, not its look:
 * - the list is the Runtime's candidate set — the permission for THIS chat — so
 *   the picker must not filter it by residency or invent rows;
 * - "new task" is the FIRST row, because D6 requires a question to be answerable
 *   in one click rather than after a scroll;
 * - declining to name a session is still an answer (the ladder's lower levels
 *   must stay reachable), and so is a failed fetch — a list that could not load
 *   must not become a dead end.
 */
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { ChatAddressPicker } from "./ChatAddressPicker";
import { fetchChatCandidates, type ChatCandidate } from "../../lib/chatCandidates";

vi.mock("../../lib/chatCandidates", () => ({
  fetchChatCandidates: vi.fn(),
}));
vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({
    t: (key: string, vars?: Record<string, unknown>) =>
      vars && "name" in vars ? `${key}:${vars.name}` : key,
  }),
}));

const CANDIDATES: ChatCandidate[] = [
  {
    session_id: "s-1",
    label: "Draft the release note",
    status: "working",
    origin: "chat",
    last_active_secs: 100,
  },
  {
    session_id: "s-2",
    label: "Review the schema",
    status: "idle",
    origin: "manual",
    last_active_secs: 90,
  },
];

function picker(onPick = vi.fn(), onClose = vi.fn()) {
  render(
    <ChatAddressPicker
      instanceId="agent-a"
      chatSessionId="grp-1"
      surface="group"
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
    vi.mocked(fetchChatCandidates).mockResolvedValue(CANDIDATES);
  });

  it("asks the Runtime for this chat's candidates, named by surface", async () => {
    picker();
    await screen.findByText("Draft the release note");
    expect(fetchChatCandidates).toHaveBeenCalledWith(
      "agent-a",
      "grp-1",
      "group",
      expect.objectContaining({ signal: expect.any(AbortSignal) }),
    );
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
    fireEvent.click(screen.getByText("chatView.block.newTask"));
    expect(onPick).toHaveBeenLastCalledWith({ forceNew: true });
  });

  it("keeps 'no preference' as an answer when the list is not empty", async () => {
    // Refusing to name a session is what leaves the turn on the ladder's lower
    // levels, where the Router may answer it as chat — so the refusal has to be
    // a row, not just an Escape nobody will find.
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
    // Focus starts on the escape row; one step down is the first session.
    fireEvent.keyDown(list, { key: "ArrowDown" });
    fireEvent.keyDown(list, { key: "Enter" });
    expect(onPick).toHaveBeenCalledWith({
      sessionId: "s-1",
      label: "Draft the release note",
    });
  });

  it("says so when there is nothing to continue, without hiding the escape", async () => {
    vi.mocked(fetchChatCandidates).mockResolvedValue([]);
    const { onPick } = picker();
    await screen.findByText("chatView.address.empty");
    expect(screen.getByText("chatView.block.newTask")).toBeTruthy();
    fireEvent.click(screen.getByText("chatView.block.newTask"));
    expect(onPick).toHaveBeenCalledWith({ forceNew: true });
  });

  it("offers a retry when the read failed, instead of an empty dead end", async () => {
    vi.mocked(fetchChatCandidates).mockRejectedValue(new Error("gateway down"));
    picker();
    const retry = await screen.findByText("chatView.block.retry");
    vi.mocked(fetchChatCandidates).mockResolvedValue(CANDIDATES);
    fireEvent.click(retry);
    await waitFor(() => expect(screen.getByText("Draft the release note")).toBeTruthy());
    expect(vi.mocked(fetchChatCandidates)).toHaveBeenCalledTimes(2);
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
        chatSessionId="grp-1"
        surface="group"
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

  it("shows whose tasks these are on a multi-party surface", async () => {
    picker();
    expect(await screen.findByText("chatView.address.subject:Architect")).toBeTruthy();
  });
});
