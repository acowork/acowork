/**
 * Owns the boot gate for the connection pipeline (ADR-115).
 *
 * Previously `App.tsx` held `gatewayReady` in local state, so a completed
 * boot was terminal: a later address change from the login screen could not
 * re-run `bootGateway` (spawn a local Gateway, re-push config to Rust,
 * re-resolve the account state). That is exactly the "进不去、退也退不出"
 * deadlock — changing the address on a login form that cannot reach its
 * Gateway did nothing at all.
 *
 * `restart()` drops the gate and bumps `epoch`; `App.tsx` keys the
 * SplashScreen on `epoch` so a fresh boot pipeline runs against the new
 * address. `markReady()` is the only path into the app.
 */
import { create } from "zustand";
import { isRecoveryReload } from "../lib/recoveryReload";

interface ConnectionStore {
  /** True once the boot pipeline handed off to the application. */
  ready: boolean;
  /** Incremented on every restart so the SplashScreen remounts. */
  epoch: number;
  markReady: () => void;
  /** Re-run the whole connection pipeline (config push → spawn/probe →
   *  MQTT → auth init). Safe to call from a login/setup screen. */
  restart: () => void;
}

export const useConnectionStore = create<ConnectionStore>((set) => ({
  // Sleep-recovery reloads skip the splash: the Rust backend and Gateway
  // process survived the webview reload, so only the listeners need re-init.
  ready: isRecoveryReload,
  epoch: 0,
  markReady: () => set({ ready: true }),
  restart: () => set((s) => ({ ready: false, epoch: s.epoch + 1 })),
}));
