/**
 * GroupResponderCard — the group's default responder (ADR-105, design 27
 * §5.3.1 revision) as a settings card.
 *
 * Pins the wiring, not the pixels:
 *   - opening the card GETs `chat-sessions/{id}/responder` and mirrors the
 *     value plus the server's `editable` answer (the store owns the governance
 *     rule; the card must not re-derive it);
 *   - picking an agent PUTs that id, and clearing picks the empty option whose
 *     wire value is `null` — not `""`;
 *   - a refused write snaps the select back to the stored value instead of
 *     claiming a change that did not land;
 *   - `editable: false` disables the control (read is open to every member,
 *     write is owner-or-admin);
 *   - a stored id that no longer resolves to an agent member keeps its own
 *     option and says so, rather than rendering as "unset".
 */
import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/react";

vi.mock("../../lib/config", () => ({ getGatewayUrl: () => "http://gw.test" }));
// `t` echoes the key so the assertions read against the i18n vocabulary.
vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));
const showToast = vi.hoisted(() => vi.fn());
vi.mock("../common/ToastProvider", () => ({ showToast }));

import { GroupResponderCard } from "./GroupResponderCard";
import { useAuthStore } from "../../stores/authStore";

const CANDIDATES = [
  { id: "ag-1", name: "Architect" },
  { id: "ag-2", name: "Reviewer" },
];

function jsonResponse(body: unknown, ok = true, status = 200) {
  return { ok, status, json: async () => body } as unknown as Response;
}

const URL_S1 = "http://gw.test/api/chat-sessions/s1/responder";

beforeEach(() => {
  showToast.mockReset();
  useAuthStore.setState({ accessToken: "token" } as never);
});

describe("GroupResponderCard", () => {
  it("reads the current responder and writes a picked agent", async () => {
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce(jsonResponse({ responder: null, editable: true }))
      .mockResolvedValueOnce(jsonResponse({ responder: "ag-2", editable: true }));
    vi.stubGlobal("fetch", fetchMock);

    render(<GroupResponderCard sessionId="s1" candidates={CANDIDATES} />);

    const select = (await screen.findByTestId("group-responder-select")) as HTMLSelectElement;
    expect(fetchMock.mock.calls[0][0]).toBe(URL_S1);
    expect(fetchMock.mock.calls[0][1].headers.Authorization).toBe("Bearer token");
    await waitFor(() => expect(select.value).toBe(""));

    fireEvent.change(select, { target: { value: "ag-2" } });

    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(2));
    const [url, init] = fetchMock.mock.calls[1]!;
    expect(url).toBe(URL_S1);
    expect(init.method).toBe("PUT");
    expect(JSON.parse(init.body)).toEqual({ responder: "ag-2" });
    await waitFor(() => expect(select.value).toBe("ag-2"));
    expect(showToast).toHaveBeenCalledWith(
      expect.objectContaining({ type: "success" }),
    );
  });

  it("clears the role with an explicit null, not an empty string", async () => {
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce(jsonResponse({ responder: "ag-1", editable: true }))
      .mockResolvedValueOnce(jsonResponse({ responder: null, editable: true }));
    vi.stubGlobal("fetch", fetchMock);

    render(<GroupResponderCard sessionId="s1" candidates={CANDIDATES} />);
    const select = (await screen.findByTestId("group-responder-select")) as HTMLSelectElement;
    await waitFor(() => expect(select.value).toBe("ag-1"));

    fireEvent.change(select, { target: { value: "" } });

    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(2));
    expect(JSON.parse(fetchMock.mock.calls[1]![1].body)).toEqual({ responder: null });
    await waitFor(() => expect(select.value).toBe(""));
  });

  it("snaps back to the stored value when the write is refused", async () => {
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce(jsonResponse({ responder: "ag-1", editable: true }))
      // The PUT is refused (403): a plain member racing an owner change.
      .mockResolvedValueOnce(jsonResponse({}, false, 403))
      // The reload that follows re-reads the truth.
      .mockResolvedValueOnce(jsonResponse({ responder: "ag-1", editable: true }));
    vi.stubGlobal("fetch", fetchMock);

    render(<GroupResponderCard sessionId="s1" candidates={CANDIDATES} />);
    const select = (await screen.findByTestId("group-responder-select")) as HTMLSelectElement;
    await waitFor(() => expect(select.value).toBe("ag-1"));

    fireEvent.change(select, { target: { value: "ag-2" } });

    await waitFor(() =>
      expect(showToast).toHaveBeenCalledWith(expect.objectContaining({ type: "error" })),
    );
    await waitFor(() => expect(select.value).toBe("ag-1"));
  });

  it("disables the control for a member the server says cannot edit", async () => {
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce(jsonResponse({ responder: "ag-1", editable: false }));
    vi.stubGlobal("fetch", fetchMock);

    render(<GroupResponderCard sessionId="s1" candidates={CANDIDATES} />);
    const select = (await screen.findByTestId("group-responder-select")) as HTMLSelectElement;
    await waitFor(() => expect(select.value).toBe("ag-1"));
    expect(select.disabled).toBe(true);
    // Read-only members still see WHO answers — the value is rendered (in the
    // header badge as well as the option list), not hidden.
    expect(screen.getAllByText("Architect").length).toBeGreaterThan(0);
  });

  it("keeps a stored id that is no longer an agent member visible as such", async () => {
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce(jsonResponse({ responder: "ag-gone", editable: true }));
    vi.stubGlobal("fetch", fetchMock);

    render(<GroupResponderCard sessionId="s1" candidates={CANDIDATES} />);
    const select = (await screen.findByTestId("group-responder-select")) as HTMLSelectElement;
    await waitFor(() => expect(select.value).toBe("ag-gone"));
    expect(screen.getByText("groupResponder.missing")).toBeTruthy();
  });
});