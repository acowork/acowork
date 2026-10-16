import { describe, expect, it, vi } from "vitest";
import type { DirectoryUser } from "../lib/types";
import { createPmUserDirectoryStore } from "./pmUserDirectoryStore";

const ADMIN: DirectoryUser = {
  user_id: "admin-id",
  username: "admin",
  display_name: "Admin Name",
};
const NICHOLAS: DirectoryUser = {
  user_id: "nicholas-id",
  username: "nicholas",
  display_name: "Nicholas Name",
};

describe("pmUserDirectoryStore", () => {
  it("loads the directory once and caches it for the same signed-in user and token", async () => {
    const loadUsers = vi.fn(async () => [NICHOLAS]);
    const store = createPmUserDirectoryStore(loadUsers);

    await store.getState().loadDirectory("admin-id", "admin-token");
    await store.getState().loadDirectory("admin-id", "admin-token");

    expect(loadUsers).toHaveBeenCalledTimes(1);
    expect(store.getState()).toMatchObject({
      ownerUserId: "admin-id",
      users: [NICHOLAS],
      loaded: true,
      loadError: false,
    });
  });

  it("reuses the cached directory after token refresh for the same account", async () => {
    const loadUsers = vi.fn(async () => [NICHOLAS]);
    const store = createPmUserDirectoryStore(loadUsers);

    await store.getState().loadDirectory("admin-id", "admin-token");
    await store.getState().loadDirectory("admin-id", "rotated-admin-token");

    expect(loadUsers).toHaveBeenCalledTimes(1);
  });

  it("shares a pending directory request among PM member consumers", async () => {
    let finish!: (users: DirectoryUser[]) => void;
    const loadUsers = vi.fn(
      () => new Promise<DirectoryUser[]>((resolve) => { finish = resolve; }),
    );
    const store = createPmUserDirectoryStore(loadUsers);

    const first = store.getState().loadDirectory("admin-id", "admin-token");
    const second = store.getState().loadDirectory("admin-id", "admin-token");
    expect(loadUsers).toHaveBeenCalledTimes(1);
    finish([NICHOLAS]);
    await Promise.all([first, second]);
    expect(store.getState().users).toEqual([NICHOLAS]);
  });

  it("does not retain a previous account's directory after a user switch", async () => {
    const loadUsers = vi.fn(async (_url: string, token: string) =>
      token === "admin-token" ? [NICHOLAS] : [ADMIN],
    );
    const store = createPmUserDirectoryStore(loadUsers);

    await store.getState().loadDirectory("admin-id", "admin-token");
    await store.getState().loadDirectory("nicholas-id", "nicholas-token");

    expect(store.getState()).toMatchObject({
      ownerUserId: "nicholas-id",
      users: [ADMIN],
      loaded: true,
    });
  });

  it("clears directory data and records a recoverable load error on request failure", async () => {
    const store = createPmUserDirectoryStore(vi.fn().mockRejectedValue(new Error("offline")));

    await store.getState().loadDirectory("admin-id", "admin-token");

    expect(store.getState()).toMatchObject({
      ownerUserId: "admin-id",
      users: [],
      loading: false,
      loaded: false,
      loadError: true,
    });
  });

  it("ignores a stale request completing after reset", async () => {
    let finish!: (users: DirectoryUser[]) => void;
    const loadUsers = vi.fn(
      () => new Promise<DirectoryUser[]>((resolve) => { finish = resolve; }),
    );
    const store = createPmUserDirectoryStore(loadUsers);

    const pending = store.getState().loadDirectory("admin-id", "admin-token");
    store.getState().reset();
    finish([NICHOLAS]);
    await pending;

    expect(store.getState()).toMatchObject({
      ownerUserId: null,
      users: [],
      loaded: false,
    });
  });
});
