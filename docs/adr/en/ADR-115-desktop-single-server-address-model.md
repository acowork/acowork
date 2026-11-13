# ADR-115: Desktop Single-Server-Address Model (local / remote Demoted to an Inference from the Address)

**Status**: Implemented
**Date**: 2026-11-13
**Deciders**: 大鱼

**Revises**:
- The "mode + address" two-knob connection settings Desktop inherited after [ADR-114](./ADR-114-account-system-decoupled-from-network-topology.md) § 5.1
- The wording in ADR-055 (Node topology) / ADR-087 (remote mode auto-resuming the local Node) that treats `local` / `remote` as a **user-selectable deployment mode**

**Predecessors**: ADR-114 (account system decoupled from network topology), ADR-033 (MQTT real-time events), ADR-059 § 5.1 (bootstrap-phase readiness)

---

## 1. Problem

ADR-114 pulled the *account system* out of network topology, but Desktop's **connection settings** still carried the same class of coupling: two independent, user-writable knobs that could contradict each other.

```
gatewayMode (persisted: local | remote | relay)  ← chosen by the user
gatewayUrl  (persisted: any string)              ← typed by the user
        ↓
Rust: whether to spawn a child / which MQTT transport / whether the remote ACL applies
        ↓
Frontend: login gate, onboarding wizard and settings card each branch on mode
```

Nothing kept those two values consistent, and the resulting failure chain was reproduced end to end:

1. The user connected to a remote Gateway on the login screen and typed a wrong username/password — login rejected.
2. The address was remembered, so every launch tried to log in there and failed; **the login screen had no address switcher**, so the only escape was reinstalling / deleting `.acowork`.
3. The user stopped the remote Gateway, restarted, picked `local` and clicked `Start local` — still failed, because `gatewayUrl` was still the remote address: `spawn_gateway` brought a child up on `127.0.0.1` while every probe went to the LAN host.
4. After resetting the remote password the login succeeded, and the settings page showed **mode = `local`, address = the remote address**.

Four independent root causes:

| # | Root cause | Effect |
|---|---|---|
| R1 | `gatewayMode` and `gatewayUrl` are two persisted knobs with no consistency constraint | The self-contradictory state in R3's aftermath can persist indefinitely |
| R2 | The login and first-boot screens have a form but no address control | A wrong address is a deadlock: cannot get in, cannot get out |
| R3 | The implication points the wrong way: `mode=local` does not check that the address is local | A child is spawned on `127.0.0.1` while probes target the LAN |
| R4 | SplashScreen's `gatewayReady` is `useState(false)` — once `true` it never goes back | The boot pipeline runs exactly once; there is no restart path after a failure |

On top of that, the `local` / `remote` distinction carries no information in a Desktop context: both are **direct Gateway connections**, and the only difference is whether the address is this machine. The only real choice is *which server address to connect to* — and `localhost` belongs in that list as an ordinary entry, not as a hidden mode.

## 2. Decision

### 2.1 There is exactly one user-facing connection setting: the server address

`gatewayUrl` is the only input. `gatewayMode` is an **inference**, not a choice:

```
gatewayUrl ──▶ isLoopbackGatewayUrl(url) ? "local" : "remote"
      relayActive ──▶ "relay" (the one explicit switch, set only by the dedicated entry)
```

The verdict lives in one pure module, `apps/acowork-desktop/src/lib/gatewayUrl.ts` (no store imports, so the boot path, the store, the address picker and the settings page all share it):

- `normalizeGatewayUrl(raw)`: trim + strip trailing `/`, so the store, Rust's `base_url` and the history LRU all hold the same string.
- `isLoopbackHost(host)`: `localhost`, `::1`, `0:0:0:0:0:0:0:1`, all of `127.0.0.0/8` (`127.0.0.2` is also this machine), and strictly IPv4-mapped `::ffff:127.0.0.0/8`. `::ffff:192.168.x.x` is a real LAN address and does not count.
- `isLoopbackGatewayUrl(url)`: falls back to manual host extraction when URL parsing fails, **failing closed to remote**.
- `deriveGatewayMode(url, relayActive)`: relay wins; otherwise the loopback verdict decides.
- `gatewayAddressOptions(current, history, lastGoodUrl)`: deduplicated, ordered `localhost → last connected → current → history`. **`localhost` is always present** and is an ordinary row (tagged "This machine"), not a hidden mode.

### 2.2 `gatewayMode` becomes a derived, read-only projection

- Deleted: `DEFAULT_GATEWAY_MODE`, `setGatewayMode()`, `requestGatewayModeChange()`, `applyGatewayUrl()`, and the `acowork-gateway-mode` localStorage key.
- The store keeps a `gatewayMode` field, but it is a **projection computed on write** with a single write path: `commitGatewayUrl(raw, relayActive, notifyAuth)`. Every address write goes through it, recomputing the mode and pushing `set_gateway_config`.
- New actions: `switchGateway(url)` (drops the session and restarts the boot gate), `setRelayGateway(url)`, `adoptBootUrl(url)` (adopts Rust's real address after boot without restarting), `resetConnectionSettings()`, `recordGatewayUrl(url)`; new state `lastGoodUrl` / `relayActive`.

### 2.3 Relay stays a separate, explicit entry point

Relay is not "another address" but **another transport** (WSS MQTT + access-token CONNECT + remote ACL + TLS device domain). It therefore does not appear in the address list; it has a dedicated field in the settings page, and that is the **only** path that can set `relay`.

The invariant lives in the store rather than in each caller:

```ts
// Relay only exists over TLS device domains. Any write of a plain address
// silently drops it, so the store can never hold the "relay + http://" pair
// Rust rejects forever (MQTT dead while /health still answers — the classic
// false "connected").
const relay = relayActive && isHttpsUrl(url);
```

`relay_mqtt_wss_url` (`commands/chat_mqtt.rs`) accepts `https://` only; `relay + http://` leaves HTTP health answering while MQTT is permanently refused. This invariant closes the recurrence path "picked an http candidate while in relay mode".

### 2.4 The boot gate must be restartable

`connectionStore` (`ready` + `epoch` + `restart()`) replaces the `useState(false)` in `App.tsx`. `App.tsx` keys `SplashScreen` on `bootEpoch`, and `restart()` clears `ready` while bumping `epoch`, so the whole boot pipeline (config push → spawn/adopt → listeners → MQTT) can run again.

This is the fix for R4 and the direct answer to "cannot get out": a wrong address is no longer terminal, only a startup that can be retried.

### 2.5 The address control is mounted everywhere a user can get stuck

`GatewayAddressPicker` — the single address control (`UrlComboBox` + reachability dot + "This machine" / "Last connected" tags + optional reset) — is mounted in five places:

| Surface | Why it must be there |
|---|---|
| `LoginView` | The original failure site: a failed login has to be able to say "switch server", not just "wrong password" |
| `SetupRequiredView` | First-boot setup is refused with 403 `setup_local_only` by a remote Gateway (ADR-114 § 2.3), so the user must be able to switch to localhost |
| `SplashScreen` (in-flight candidates / timeout recovery) | The only escape hatch when nothing connects |
| `SettingsPage` | Changing servers after connecting |
| `OnboardingFlow` step 2 | The wizard must not offer a different way to configure the connection than every other entry point |

### 2.6 Record an address only at the moment it actually connects

`recordGatewayUrl()` fires only on the **rising edge into `connected`** (top-level subscription in `App.tsx`). An address that was typed but never connected does not enter history. **A failed login (wrong password) must not remove the address from history** — a wrong password is not a bad address, and deleting it would mislead the user about which half is broken. The address is also recorded as `lastGoodUrl`, ranked right after localhost, as the default way back to a working state.

### 2.7 localhost selected but no local Gateway running → start one automatically

This reuses `spawn_gateway`'s probe-then-spawn (adopt if reachable, otherwise spawn a child and wait for bootstrap READY). The Rust-side constraints:

- **Bind and advertise** (2026-11-13 revision): the child listens on `0.0.0.0:{GATEWAY_HTTP_PORT}` / `0.0.0.0:{GATEWAY_MQTT_PORT}`, and `ACOWORK_GATEWAY_ADVERTISE_HOST` is pinned to the **default-route interface's** IPv4 (`netinfo::default_route_ipv4`, the UDP connect trick; falls back to 127.0.0.1 with no route). The original loopback pin was an accounts-optional-era posture; since ADR-114 accounts are always on and first-boot setup is loopback-peer-only, so the LAN exposure is auth-gated — while loopback-only left the pairing QR (which carries the LAN IP) permanently unreachable: Desktop worked on localhost and every other device was locked out, exactly the shape this ADR exists to prevent. **Interface enumeration order must never pick the advertise address** (virtual adapters sort first); use the default route.
- **Startup grace** (2026-11-13 revision): one failed /health probe does not mean the port holder is stale. A standalone Gateway (debug build) needs seconds to answer /health; the original code `taskkill`ed the port holder right after a failed probe, killing a starting remote-mode Gateway and replacing it with a loopback child (observed incident). Now a held-but-unreachable port gets ~8 s of probe retries before it is declared stale.
- If the user configured a loopback **alias** (`http://localhost:19876`, `http://[::1]:19876`) or a different port, the child cannot serve that address, so probes and every later request fall into a black hole — yet another "cannot get in, cannot get out". `adopt_spawned_base_url()` (`commands/gateway.rs`) therefore re-points `base_url` at `defaults::GATEWAY_HTTP_URL` (the canonical loopback — always valid under the 0.0.0.0 bind, and immune to future network changes) with a warning when `ownership == Owned`, and the frontend mirrors it via `adoptBootUrl(boot.base_url)` so the persisted address, the probe address and the running process agree.

### 2.8 Candidate probing no longer short-circuits on loopback

`probeKnownCandidates()` used to contain `if (gatewayMode === "local") return 0;`, which made localhost a one-way door: with the local Gateway dead there was no way back to the LAN Gateway the user had been using. Loopback addresses now draw candidates from history like any other address.

## 3. Interface changes

| Old | New |
|---|---|
| `DEFAULT_GATEWAY_MODE` | Deleted |
| localStorage `acowork-gateway-mode` | Deleted |
| `setGatewayMode()` / `requestGatewayModeChange()` / `applyGatewayUrl()` | Deleted (no compatibility branches) |
| `gatewayMode` user-writable | Derived read-only, written only by `commitGatewayUrl` |
| — | `switchGateway(url)` / `setRelayGateway(url)` / `adoptBootUrl(url)` / `resetConnectionSettings()` / `recordGatewayUrl(url)` |
| — | `lastGoodUrl`, `relayActive`; localStorage `acowork-gateway-url-history` / `acowork-gateway-last-good-url` / `acowork-gateway-relay-active` |
| — | `lib/gatewayUrl.ts` (`normalizeGatewayUrl` / `isLoopbackGatewayUrl` / `deriveGatewayMode` / `isHttpsUrl` / `gatewayAddressOptions`) |
| — | `components/connection/GatewayAddressPicker.tsx`, i18n `connection.*` (8 keys × 5 locales) |
| SettingsPage mode radios + conditional URL block | One address control + separate relay entry + always-visible diagnostics card |
| OnboardingFlow step 2 mode radios | `GatewayAddressPicker` + status row + test-connection |
| GatewaySwitcherMenu local/remote/relay tabs | One address list (localhost always clickable) |
| SplashScreen mode dropdown + conditional address field | `GatewayAddressPicker` |

**Rust's `GatewayMode` enum stays.** It still decides the MQTT transport, the publish SavePlan, the relay ACL and owned-process cleanup. What changes is that the user no longer writes it and it is no longer persisted independently — the frontend derives it from the address and pushes it.

## 4. Data and migration

The project is not shipped, so there are no compatibility branches. The old `acowork-gateway-mode` key becomes an orphan and is never read; `acowork-gateway-url` is reused, so **an upgrade reconnects to the last address**. If that address no longer resolves, the splash timeout view offers the address list — that is a designed path, not a migration case.

An empty or unparseable `gatewayUrl` falls back to `DEFAULT_GATEWAY_URL` (`http://127.0.0.1:19876`), so the store can never be in a "no address" state.

## 5. Verification

- `cargo check --lib` (`apps/acowork-desktop/src-tauri`): passes (the type-path and move errors introduced by `adopt_spawned_base_url` are fixed).
- `npx tsc --noEmit`: no errors introduced by this change; the remaining 9 are pre-existing missing environment dependencies (`docx-preview` / `pdfjs-dist` / `jszip` / `xlsx`, all in the untouched `src/components/editor/`).
- `node scripts/check-i18n.mjs`: OK, 1940 call sites (down from 1962 after deleting the old `settings.local` / `settings.remote` / `settings.relay` / `settings.localModeKeepsUrl` / `settings.relayUrlSavedPlainHttp` mode copy).
- Full vitest: 343 files / 3385 tests — **3380 passed, 5 failed**; all 5 are pre-existing and unrelated (the mention-token rendering in `GroupChatPanel.blockRef.test.tsx`, plus 4 `src/components/editor/*` files with missing environment dependencies).
- Contract tests rewritten or added this round:
  - `settingsStore.modeSwitch.test.ts` (8): mode derived from the address, loopback aliases (`localhost` / `127.0.0.2` / `[::1]`) all derive `local`, switching to a plain address never keeps relay, `switchGateway` clears the session and restarts boot, same-address reconnect, trailing-slash normalization, `resetConnectionSettings` returns to localhost, `recordGatewayUrl` promotes last-good without changing the current address.
  - `settingsStore.switchGateway.test.ts` (6): LAN → `remote` push, loopback → `local` push, relay only ever produced by `setRelayGateway`, switching to a plain address after relay never yields relay+http, empty addresses ignored, exactly one restart per switch.
  - `GatewaySwitcherMenu.test.tsx` (19): localhost first, no `combobox`, row clicks go through `switchGateway`, only probe-rejected remote rows are disabled, localhost stays clickable even when the probe fails, probing never pollutes `gatewayStore.candidates`.
  - `SplashScreen.test.tsx` (4) / `SplashScreen.candidate.test.tsx` (11): the timeout view carries the address picker, localhost is an ordinary row, clicking localhost goes through `switchGateway` (the real escape hatch), a candidate click never rolls the store back, picking an http candidate under relay drops relay and lands on `(remote, url)`.
  - `gatewayConnectivity.test.ts`: localhost is an ordinary candidate, offered even from a remote address.
  - `lib/gatewayUrl.test.ts` (28 cases, new): the verdict module's own tests — normalize, the loopback positive/negative table (both IPv4-mapped spellings), `deriveGatewayMode`, scheme checks, and address-list ordering / dedupe / filtering.
  - `gatewayAutoStartLabel.test.tsx` / `gatewayGwId.test.tsx`: now driven by the address rather than by a mode flag; the relay entry's TLS validation is asserted as an inline `role="alert"` plus a disabled submit.

### 5.1 Issues found and fixed during implementation

| Severity | Problem | Fix |
|---|---|---|
| P0 | Picking an http candidate under relay: `setGatewayUrl` kept `relayActive`, so the derived mode stayed `relay` and `(relay, http://…)` was pushed — MQTT permanently refused while `/health` still answered (the false "connected" again) | Invariant moved into `commitGatewayUrl`: `relay = relayActive && isHttpsUrl(url)`; `isHttpsUrl()` exported and a regression test added |
| P1 | Initial state skipped the same invariant: a stale `acowork-gateway-relay-active=true` from an older build next to an http address resurrected relay+http on boot | `initialRelayActive = getPersistedRelayActive() && isHttpsUrl(initialGatewayUrl)` |
| P1 | `setRelayGateway` silently downgraded a non-https address (relay dropped, actually connected as remote) while the caller believed relay applied | Non-https is refused outright (no push, no reboot), matching the settings page's existing `role="alert"` + disabled submit |
| P1 | `probeKnownCandidates()` short-circuited on local, making localhost a one-way door | Short-circuit removed; localhost is an ordinary candidate; the test now asserts history candidates are offered even from a loopback address |
| P1 | A spawned Gateway only listens on the fixed port, so a configured loopback alias / other port was unreachable | `adopt_spawned_base_url()` in Rust + `adoptBootUrl()` in the frontend align the real address in both directions |
| P2 | `SettingsPage` re-implemented `isHttpsUrl` locally, duplicating `lib/gatewayUrl.ts` | Local copy deleted, single import |
| P2 | The relay input had no `label`/`htmlFor` association (unreachable for screen readers and for tests) | Added `id="gateway-relay-input"` + `htmlFor` |
| P2 | The relay submit button said "Apply" while the address control says "Connect", although both switch the connection | Unified on `connection.connect` |

### 5.2 Post-delivery file-by-file review

The whole diff was re-read file by file (including whether each new module has its own tests). **Confirmed and fixed**:

| Severity | Problem | Fix |
|---|---|---|
| P1 | `isLoopbackHost()` missed IPv4-mapped addresses: `new URL("http://[::ffff:127.0.0.1]:19876").hostname` is **normalized to hex** as `[::ffff:7f00:1]`, and the regex only matched the dotted `::ffff:127.x` form — so this machine was classified as remote: no local spawn, `isGatewayLocal()` false, out of step with the Gateway's `is_loopback_peer()` | Added the `::ffff:7f00:–7fff:ffff` hex-mapped range; routable mappings such as `::ffff:c0a8:10a` (192.168.1.10) stay remote; both spellings are now in the unit test |
| P1 | `lib/gatewayUrl.ts` is ADR-115's single source of truth yet had **no tests of its own** — the loopback table, ordering and dedupe were only covered indirectly through components. That is exactly how the bug above survived | Added `src/lib/gatewayUrl.test.ts` (28 cases): normalize, the loopback positive/negative table, `deriveGatewayMode`, scheme checks, and `gatewayAddressOptions` ordering / dedupe / filtering |
| P1 | A scheme-less value (`abc`, `192.168.1.5:19876`) was accepted by `commitGatewayUrl` and pushed to Rust as `base_url` — permanently unresolvable, and the next launch prefers it | `hasHttpScheme()` added to `gatewayUrl.ts`; `commitGatewayUrl` (the final line of defence) refuses it with a warning; `switchGateway` / `setRelayGateway` use the return value to decide whether to reboot; the picker disables Connect and shows an inline `role="alert"` |
| P2 | `GatewayAddressPicker` did not mark the current address, unlike `GatewaySwitcherMenu`'s `current` handling | Current row gets `aria-current`, the `listRowSelected` tint and a `connection.current` tag |
| P2 | The `SettingsPage` diagnostics card was titled `settings.status` while its first row was also labelled `settings.status` ("Status" card whose first line says "Status") | Retitled to the new `settings.connectionDiagnostics`; the stale `gatewayModeOpen` variable became `gatewayConnectionOpen`, and the "shared between modes" comment was corrected |
| P2 | ~40 lines of `GatewaySwitcherMenu`'s header comment still described the deleted mode listbox, `applyGatewayUrl`, `setGatewayMode` and "returns 0 in local mode" — dead documentation that misleads the next maintainer | Rewritten against the current implementation (one address list, probe whitelist, `switchGateway`), and it now states explicitly that there is no mode control and why |
| P2 | After dropping the empty-value guard, `switchGateway("   ")` fell through to `commitGatewayUrl`'s `|| DEFAULT_GATEWAY_URL` fallback and silently switched to localhost | The guard is back in `switchGateway` (caught immediately by the new test case) |
| P2 | The boot status line was hard-coded English (`"Starting local Gateway..."` / `"Connecting to Gateway..."`): it named no address (yet the address IS the only setting) and put the retired `local` concept back in front of the user; the rest of that screen (`Waiting for services...`, `Preparing:`, the timeout copy) was hard-coded too, so a Chinese UI mixed languages | The line became a `(key, params)` spec rendered through `t()` at paint time (correct across a language change), with `Starting Gateway: {{url}}` / `Connecting to Gateway: {{url}}`; added `waitingForServices` / `preparing` / `retryingConnection` / `timeoutNoResponse` / `bootFailed`; the title-bar chip's `bannerLocalStarting` / `bannerLocalDown` became `bannerStarting` / `bannerNotRunning` without `local`; 7 orphan keys left by the deleted mode UI (`modeLabel` / `modeLocal` / `modeRemote` / `modeRelay` / `startLocal` / `gatewayAddress` / `candidateDetected`) were removed, and `timeoutHint` no longer says "switch to a local Gateway" |
| P2 | The status line renders a DYNAMIC key (`t(status.key)`), which `scripts/check-i18n.mjs` cannot see (it only walks `t("literal")` call sites) — a typo would surface as a raw key name on the exact screen a broken install lands on | Keys collapsed into the `SPLASH_STATUS_KEYS` const union (a typo now fails `tsc`), backed by `SplashScreen.statusCopy.test.ts` (4 cases) asserting every locale resolves them, placeholders agree, and no locale reintroduces local vocabulary |

## 6. Consequences

**Benefits**: connection settings go from "two knobs that can contradict each other" to one string; `mode=local + remote address` becomes impossible to represent; all three failure modes (wrong address, wrong password, no local Gateway) have a defined way out (address list / back to login / automatic spawn); the splash boot pipeline is restartable, so "cannot get out" no longer exists.

**Costs / cautions**:
- `gatewayMode` in the store is a cached projection; bypassing the single `commitGatewayUrl` write path reintroduces inconsistency. Tests may `setState({ gatewayMode })` to build a scenario, but production code must never write that field on its own.
- History is recorded only on the `connected` rising edge, so **an address that has never connected does not appear in history**. That is deliberate (a typo must not become a shortcut), but it also means connecting to a new machine requires one manual entry.
- `lastGoodUrl` and "current address" can differ: the former is the most recent address that actually connected, the latter is the current selection. Both may appear in the list with their own tags; do not merge them.
- Relay and plain addresses switch through different paths (`setRelayGateway` / `switchGateway`). Switching to a plain address always clears `relayActive` — the other half of the § 2.3 invariant. Not clearing it recreates relay+http.
- Mobile is out of scope: its connection surface is "relay or not", with no multi-address scenario, and it keeps working under ADR-114's `account_state` semantics.
