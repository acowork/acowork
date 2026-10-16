import { act, renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useAuthStore } from "../../stores/authStore";
import { usePmUserDirectoryStore } from "../../stores/pmUserDirectoryStore";
import { useResolvedMembers } from "./pmMembers";
import type { PmProjectMember } from "../../lib/pm-types";

const ADMIN_ID = "21a727ee-d2ac-4d7a-9bb1-a846cb34cf3c";
const NICHOLAS_ID = "a5a57ab4-9a6c-4b53-909a-c7d6ca91ac05";
const addedAt = "2026-10-08T07:22:18Z";

function member(userId: string): PmProjectMember {
  return { instance_id: userId, kind: "user", added_at: addedAt };
}

function directoryResponse(user: { user_id: string; username: string; display_name: string }) {
  return new Response(JSON.stringify({ users: [user] }), {
    status: 200,
    headers: { "Content-Type": "application/json" },
  });
}

describe("useResolvedMembers account-directory integration", () => {
  beforeEach(() => {
    usePmUserDirectoryStore.getState().reset();
    useAuthStore.setState({
      mode: "multi_user",
      status: "logged_in",
      accessToken: "admin-token",
      account: {
        user_id: ADMIN_ID,
        username: "admin",
        display_name: "Admin Name",
        role: "admin",
        language: "zh-CN",
        timezone: "Asia/Shanghai",
        created_at: "2026-01-01T00:00:00Z",
        updated_at: "2026-01-01T00:00:00Z",
      },
    });
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    usePmUserDirectoryStore.getState().reset();
  });

  it("loads the authenticated directory and displays another user's name", async () => {
    let finish!: (response: Response) => void;
    const fetchMock = vi.fn(() => new Promise<Response>((resolve) => { finish = resolve; }));
    vi.stubGlobal("fetch", fetchMock);

    const { result } = await act(async () => renderHook(() => useResolvedMembers([member(NICHOLAS_ID)])));
    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(1));
    await act(async () => {
      finish(directoryResponse({
        user_id: NICHOLAS_ID,
        username: "nicholas",
        display_name: "Nicholas Name",
      }));
    });
    await waitFor(() => expect(result.current[0].displayName).toBe("Nicholas Name"));

    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(fetchMock.mock.calls[0][0]).toContain("/api/users/directory");
    expect(fetchMock.mock.calls[0][1]).toMatchObject({
      headers: { Authorization: "Bearer admin-token" },
    });
  });

  it("merges the caller account because the directory excludes its caller", async () => {
    const fetchMock = vi.fn().mockResolvedValue(new Response(JSON.stringify({ users: [] }), {
      status: 200,
      headers: { "Content-Type": "application/json" },
    }));
    vi.stubGlobal("fetch", fetchMock);

    const { result } = await act(async () => renderHook(() => useResolvedMembers([member(ADMIN_ID)])));
    await waitFor(() => expect(result.current[0].displayName).toBe("Admin Name"));
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it("resolves admin's name after switching the signed-in account to nicholas", async () => {
    useAuthStore.setState({
      account: {
        user_id: NICHOLAS_ID,
        username: "nicholas",
        display_name: "Nicholas Name",
        role: "user",
        language: "zh-CN",
        timezone: "Asia/Shanghai",
        created_at: "2026-01-01T00:00:00Z",
        updated_at: "2026-01-01T00:00:00Z",
      },
      accessToken: "nicholas-token",
    });
    let finish!: (response: Response) => void;
    const fetchMock = vi.fn(() => new Promise<Response>((resolve) => { finish = resolve; }));
    vi.stubGlobal("fetch", fetchMock);

    const { result } = await act(async () => renderHook(() => useResolvedMembers([member(ADMIN_ID)])));
    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(1));
    await act(async () => {
      finish(directoryResponse({
        user_id: ADMIN_ID,
        username: "admin",
        display_name: "Admin Name",
      }));
    });
    await waitFor(() => expect(result.current[0].displayName).toBe("Admin Name"));
    expect(fetchMock.mock.calls[0][1]).toMatchObject({
      headers: { Authorization: "Bearer nicholas-token" },
    });
  });

  it("keeps member display safe when the user directory request fails", async () => {
    let fail!: (error: Error) => void;
    const fetchMock = vi.fn(() => new Promise<Response>((_resolve, reject) => { fail = reject; }));
    vi.stubGlobal("fetch", fetchMock);

    const { result } = await act(async () => renderHook(() => useResolvedMembers([member(NICHOLAS_ID)])));
    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(1));
    await act(async () => { fail(new TypeError("offline")); });
    await waitFor(() => expect(usePmUserDirectoryStore.getState().loadError).toBe(true));
    expect(result.current[0].displayName).toBeNull();
    expect(result.current[0].unresolved).toBe(true);
  });
});
