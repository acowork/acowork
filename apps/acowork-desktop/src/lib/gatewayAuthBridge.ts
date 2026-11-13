/**
 * Tauri-side account-session bridge (ADR-076 §决策 3).
 *
 * `installAuthFetchInterceptor` covers the webview's own `fetch` calls. The
 * Rust command layer is a *second* HTTP client on the same session and needs
 * the same credentials — but it must not own the token pair: refresh rotation
 * has no grace window, so a second rotation source would trip the Gateway's
 * reuse detection and revoke the whole token family (ADR-076 §10.1 #3).
 *
 * So Rust only *mirrors* the access token:
 *
 *   1. every change of the session module's access token is pushed to Rust
 *      through the `set_gateway_access_token` command,
 *   2. a Rust request that answers 401 emits `gateway-auth-required`
 *      (`gateway_client.rs`), which lands here,
 *   3. this side reports the rejection to `lib/session/tokens.ts` — the very
 *      same call the fetch interceptor uses, so parallel 401s from both
 *      clients share one rotation — and step 1 mirrors the result, releasing
 *      the paused Rust request so it replays with the new token.
 *
 * A failed rotation mirrors `null`, which also releases the waiter (the
 * original 401 is surfaced instead of stalling for the full renew timeout).
 *
 * Installed once from `main.tsx`, before App renders.
 */

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  current,
  onTokenChange,
  reportRejected,
  restore,
} from "./session/tokens";
import { isRecoveryReload } from "./recoveryReload";
import { log } from "./logger";

/** Mirrors `AUTH_REQUIRED_EVENT` in `src-tauri/src/gateway_client.rs`. */
const AUTH_REQUIRED_EVENT = "gateway-auth-required";

let installed = false;

function pushAccessToken(token: string | null): void {
  void invoke("set_gateway_access_token", { accessToken: token }).catch((err) => {
    // Not fatal: the request that needed the token simply surfaces its 401.
    log.warn("[gatewayAuth] failed to mirror the access token into Rust:", err);
  });
}

export function installGatewayAuthBridge(): void {
  if (installed) return;
  installed = true;

  // Adopt the persisted pair before `authStore.init()` gets to run. The boot
  // commands (SplashScreen → `init_local_gateway` / `connect_mqtt`, agent
  // installs) execute before the App-level auth gate resolves, and under
  // the always-on account system (ADR-114) they must carry a token — otherwise every one of them
  // answers 401. `init()` later re-reads the same values, so this only moves
  // the adoption earlier.
  restore();

  let mirrored: string | null | undefined;
  /**
   * Mirror a token into Rust. When `fireConnect` is set, a
   * `null → non-null` transition (a fresh interactive login) also fires
   * `connect_mqtt`: relay mode's strict remote listener rejects CONNECT
   * without an access token, so the boot connect was skipped and nothing
   * else re-runs it in that window (the only other places that call it
   * are a Settings change / manual Connect).
   */
  const mirror = (token: string | null, fireConnect: boolean) => {
    if (token === mirrored) return;
    // Capture the previous value before we overwrite `mirrored`. A
    // `null → non-null` transition means an account session just became
    // available — the Rust-side MQTT client never created one during
    // boot (see `fireConnect` above). Fire-and-forget: errors are
    // non-fatal because the boot path (SplashScreen) still has its own
    // catch and the UI offers manual escapes.
    const wasNull = (mirrored ?? null) === null;
    mirrored = token;
    pushAccessToken(token);
    if (fireConnect && wasNull && token !== null) {
      void invoke("connect_mqtt").catch((err) => {
        log.warn("[gatewayAuth] connect_mqtt after token mirror failed:", err);
      });
    }
  };

  // Install-time mirror: suppressed on a normal boot — `SplashScreen`'s
  // `bootGateway` owns the boot connect and must not be raced (its
  // `set_gateway_config` hasn't run yet; Rust's default mode is `Local`,
  // so connecting here would first build a client against the wrong
  // endpoint). EXCEPTION: after a recovery reload the SplashScreen never
  // runs (see recoveryReload.ts — the boot is skipped by design), so
  // nothing else would ever connect; keep the install-time trigger there.
  mirror(current(), isRecoveryReload);
  onTokenChange((token) => mirror(token, true));

  // Rust stopped on a 401 and is waiting for a newer token (10s budget). It
  // reports the rejection like any other consumer; the session module owns
  // the decision of what to do about it.
  void listen(AUTH_REQUIRED_EVENT, () => {
    void (async () => {
      const rejected = current();
      const fresh = rejected ? await reportRejected(rejected) : null;
      // Release the waiter unless the module actually handed back a *different*
      // token. On a real rotation the subscription above mirrors it and wakes
      // Rust; but `null` (session gone) or the *same* token (no rotation
      // possible — transport not yet wired, nothing to rotate) must fall
      // through here, or the waiter stalls for the full 10s renew budget.
      // Mirrors the interceptor's `!fresh || fresh === token` guard.
      if (fresh && fresh !== rejected) return;
      // No replacement (session gone / no refresh token): release the waiter
      // explicitly. `mirror` would dedupe a `null` that is already mirrored,
      // and Rust only wakes on an epoch bump — so bypass it.
      mirrored = null;
      pushAccessToken(null);
    })();
  }).catch((err) => {
    log.warn("[gatewayAuth] failed to listen for gateway-auth-required:", err);
  });
}
