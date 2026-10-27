/**
 * GroupChatPromptCard — the group `chat.md` as a unified settings card
 * (ADR-099 extended), edited through the shared Monaco file tab.
 *
 * Pins the wiring, not the pixels:
 *   - opening the card GETs the session's `chat-md` (the server seeds it and
 *     strips the non-trimmable `# Safety` tail, so the body lands whole);
 *   - the edit action does NOT inline a textarea — it hands the loaded body to
 *     `fileEditorStore.openGroupChatMd`, the same Monaco tab the agent uses for
 *     its own prompt files;
 *   - when the server reports `editable: false`, no edit / restore affordance
 *     is rendered at all — a plain member is never handed a control the backend
 *     would refuse with a 403;
 *   - `editable: true` + a custom override shows the restore shortcut, which
 *     DELETEs the override and re-reads so the badge flips back in place.
 */
import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, act, fireEvent, waitFor } from "@testing-library/react";

vi.mock("../../lib/config", () => ({ getGatewayUrl: () => "http://gw.test" }));
vi.mock("../common/ToastProvider", () => ({ showToast: vi.fn() }));

const openGroupChatMd = vi.hoisted(() => vi.fn());
vi.mock("../../stores/fileEditorStore", () => ({
  useFileEditorStore: (selector: (s: { openGroupChatMd: typeof openGroupChatMd }) => unknown) =>
    selector({ openGroupChatMd }),
}));

import { GroupChatPromptCard } from "./GroupChatPromptCard";
import { useAuthStore } from "../../stores/authStore";

const BODY = "You are the conversation front desk of {{agent_name}}.";

function jsonResponse(body: unknown) {
  return { ok: true, status: 200, json: async () => body } as unknown as Response;
}

/** 204 has no body — the restore path is a bare ok-check, not a JSON read. */
function noContentResponse() {
  return { ok: true, status: 204 } as unknown as Response;
}

beforeEach(() => {
  openGroupChatMd.mockReset();
  useAuthStore.setState({ accessToken: "token" } as never);
});

describe("GroupChatPromptCard", () => {
  it("loads the seeded body and opens the Monaco tab on edit", async () => {
    const fetchMock = vi.fn().mockResolvedValueOnce(
      jsonResponse({ content: BODY, is_default: true, editable: true }),
    );
    vi.stubGlobal("fetch", fetchMock);

    render(<GroupChatPromptCard sessionId="s1" />);

    const editBtn = await screen.findByTestId("group-chat-prompt-edit");
    // Body-only read over the chat-prompt endpoint, authenticated with Bearer.
    expect(fetchMock.mock.calls[0][0]).toBe("http://gw.test/api/chat-sessions/s1/chat-md");
    expect(fetchMock.mock.calls[0][1].headers.Authorization).toBe("Bearer token");

    // Edit routes through the shared file-editor store, not an inline textarea.
    await act(async () => {
      fireEvent.click(editBtn);
    });
    expect(openGroupChatMd).toHaveBeenCalledWith("s1", BODY);
    // The card never writes on its own — only the one GET happened.
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it("renders no edit / restore affordance when the server says editable is false", async () => {
    const fetchMock = vi.fn().mockResolvedValueOnce(
      jsonResponse({ content: BODY, is_default: true, editable: false }),
    );
    vi.stubGlobal("fetch", fetchMock);

    render(<GroupChatPromptCard sessionId="s1" />);

    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(1));
    expect(screen.queryByTestId("group-chat-prompt-edit")).toBeNull();
    expect(screen.queryByTestId("group-chat-prompt-restore")).toBeNull();
  });

  it("restore DELETEs the override and re-reads so the badge flips back", async () => {
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce(
        jsonResponse({ content: "Custom kernel.", is_default: false, editable: true }),
      )
      .mockResolvedValueOnce(noContentResponse())
      .mockResolvedValueOnce(
        jsonResponse({ content: BODY, is_default: true, editable: true }),
      );
    vi.stubGlobal("fetch", fetchMock);

    render(<GroupChatPromptCard sessionId="s1" />);

    const restoreBtn = await screen.findByTestId("group-chat-prompt-restore");
    await act(async () => {
      fireEvent.click(restoreBtn);
    });

    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(3));
    const [url, init] = fetchMock.mock.calls[1];
    expect(url).toBe("http://gw.test/api/chat-sessions/s1/chat-md");
    expect(init.method).toBe("DELETE");
    // The follow-up re-read is the third call (GET), which flips the badge.
    expect(fetchMock.mock.calls[2][1].method).toBeUndefined();
  });
});
