import { create } from "zustand";
import { fetchDirectory } from "../lib/auth-api";
import { getGatewayUrl } from "../lib/config";
import type { DirectoryUser } from "../lib/types";

export type DirectoryLoader = (
  gatewayUrl: string,
  accessToken: string,
) => Promise<DirectoryUser[]>;

interface PmUserDirectoryState {
  ownerUserId: string | null;
  loadedKey: string | null;
  users: DirectoryUser[];
  loading: boolean;
  loaded: boolean;
  loadError: boolean;
  loadDirectory: (userId: string, accessToken: string) => Promise<void>;
  reset: () => void;
}

/**
 * Shared account directory for PM member-name resolution. The directory API
 * excludes its caller, so the signed-in account is resolved separately by
 * pmMembers; this store contains only the other enabled accounts.
 */
export function createPmUserDirectoryStore(
  loadUsers: DirectoryLoader = fetchDirectory,
) {
  let generation = 0;
  let inFlight: { key: string; promise: Promise<void> } | null = null;

  return create<PmUserDirectoryState>((set, get) => ({
    ownerUserId: null,
    loadedKey: null,
    users: [],
    loading: false,
    loaded: false,
    loadError: false,

    loadDirectory: (userId, accessToken) => {
      const gatewayUrl = getGatewayUrl();
      const contextKey = `${gatewayUrl}\u0000${userId}`;
      const requestKey = `${contextKey}\u0000${accessToken}`;
      const current = get();
      if (current.loadedKey === contextKey && current.loaded) return Promise.resolve();
      if (inFlight?.key === requestKey) return inFlight.promise;

      const requestGeneration = ++generation;
      set({
        ownerUserId: userId,
        loadedKey: null,
        users: [],
        loading: true,
        loaded: false,
        loadError: false,
      });

      const promise = loadUsers(gatewayUrl, accessToken)
        .then((users) => {
          if (generation !== requestGeneration) return;
          set({ ownerUserId: userId, loadedKey: contextKey, users, loading: false, loaded: true, loadError: false });
        })
        .catch(() => {
          if (generation !== requestGeneration) return;
          set({ ownerUserId: userId, users: [], loading: false, loaded: false, loadError: true });
        })
        .finally(() => {
          if (inFlight?.key === requestKey) inFlight = null;
        });
      inFlight = { key: requestKey, promise };
      return promise;
    },

    reset: () => {
      generation += 1;
      inFlight = null;
      set({ ownerUserId: null, loadedKey: null, users: [], loading: false, loaded: false, loadError: false });
    },
  }));
}

export const usePmUserDirectoryStore = createPmUserDirectoryStore();
