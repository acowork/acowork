# ADR-118: Desktop In-App Browser — Hosting Third-Party Pages in a Child Webview

**Status**: Decided and implemented (2026-11-18)
**Date**: 2026-11-18
**Decision owner**: 大鱼 (decision delegated to the Senior Engineer under the brief "most mainstream, permanent, no band-aids")
**Prerequisites**:
- [ADR-009: Gateway Workspace Isolation](./ADR-009-gateway-workspace-isolation.md) — the existing "who may read whose assets" boundary; this ADR's permission narrowing comes from the same source
- [ADR-112: Workspace Preview Capability Ticket](./ADR-112-workspace-preview-capability-ticket.md) — workspace `.html` rendered as a real HTTP document, the object migrated onto the new host here
- [ADR-116: Hosted Application Framework](./ADR-116-hosted-application-framework.md) — the existing precedent for "third-party documents running inside Desktop"

---

## 1. Decision summary

### 1.1 In one sentence

**Replace the in-app page preview's `<iframe>` with a same-window **child webview** (Tauri 2.12 `Webview::add_child`).** An `iframe` cannot show github.com — not because of permissions or configuration, but because the engine enforces `X-Frame-Options` / `CSP frame-ancestors` on the **embedded document**, and no frontend trick removes that. A child webview loads the target as a **top-level document**, where those headers do not apply — the same approach as Cursor / VS Code Simple Browser.

### 1.2 Key decisions

| # | Decision | Conclusion |
|---|---|---|
| D1 | Rendering host | **Same-window child webview** (`Window::add_child`). Not an `iframe`, not a separate `BrowserWindow`: the former is hard-blocked by XFO, the latter forks login state, focus and tray lifecycle away from the main window |
| D2 | Navigation policy | The policy is **parameterised by webview role** (`NavigationRole::{App, Preview}`) and `navigation_policy.rs` is its single implementation. `App` behaviour is unchanged (the old allow-list moved over line for line); `Preview` is the complement: `http/https` top-level navigation allowed, every other scheme refused |
| D3 | Session lifecycle | The Rust `PreviewRegistry` holds `label → {url,title,phase,visible,bounds}` with **at most 3 live sessions** and LRU eviction (`MAX_LIVE_SESSIONS`). The frontend binds a label to **tab identity** (`OpenFile.id`); switching tabs **parks** the session, closing the tab destroys it |
| D4 | Permission boundary | `capabilities/default.json` moves from `windows: ["main"]` to `webviews: ["main"]`. The `windows` key grants **every webview of that window** — including the previews hosting third-party sites — the IPC surface. `dev/ci.sh` gains `run_desktop_ipc_scope_redline` to pin this |
| D5 | Popups and downloads | In-page `window.open` / `target="_blank"`: Rust answers `NewWindowResponse::Deny` and emits `preview://new-window`; the frontend **opens a new preview tab** (falling back to the system browser without a workspace context). `on_download` refuses everything (no destination UI exists; refusing beats dropping files where the user cannot find them) |
| D6 | `unstable` feature | **Accepted.** `add_child` is only exported behind `tauri`'s `unstable` feature today. An exact `=2.12.3` pin plus a red line plus integration tests keep that dependency surface narrow — see §5 |
| D7 | Explicitly out of scope | No `iframe` fallback (two rendering paths mean two coordinate systems and two permission models — exactly what this ADR removes); no browser-in-a-separate-window (see §6); no `file://` / `data:` / `javascript:` previews (the `Preview` policy refuses them); no download manager |

### 1.3 Facts (all measured, none inferred)

1. `GET https://github.com/acowork/ACowork` answers `200` but carries `X-Frame-Options: deny` and `Content-Security-Policy: frame-ancestors 'none'` ⇒ any `iframe` render is blank. This is enforced by the **browser engine** when it loads a document into a frame; page JS, the `sandbox` attribute and relaxed CSP cannot change it.
2. Cursor / VS Code Simple Browser is not an `iframe` — it is a **top-level webview** (Electron `WebContentsView`) loading the same URL. XFO only constrains documents *inside a frame*, which is why they work.
3. The pre-change implementation also had two real defects, both killed by the same change:
   - Its 5-second "slow load" hint **never fired**: on an XFO refusal the `iframe` still dispatches `onLoad`, `loading` flips to `false` and the hint is cleared — the user saw "blank, unexplained".
   - "Open in browser" was a `target="_blank"` `<a>`, while Tauri v2 denies new windows by default and the repo had no `new_window_req_handler` ⇒ the click most likely did nothing.
4. Tauri 2.12's `add_child` **deadlocks when called synchronously on Windows** (the WebView2 controller creation callback needs the UI thread's message pump, and waiting blocks it). Every creation path must therefore be an `async` command driven from a tokio worker thread.
5. `capability.windows` means "**every** webview of the matching window inherits this capability" (`tauri-utils` acl/capability docs); only `webviews` scopes by label.
6. A child webview is a **native layer**, always composited above the HTML: `z-index` does nothing and no DOM portal can cover it. Alongside D3/D4 this fact drives three UI constraints (§4.2).

---

## 2. Architecture

```
React                                     Rust (src-tauri)
─────────────────────────────────────     ────────────────────────────────────────
UrlPreviewView                            browser/host.rs   ── Tauri commands
  ├─ toolbar (back/forward/refresh/URL)    preview_open / set_bounds / set_visible
  └─ WebPreviewHost                        preview_navigate / reload / history
       ├─ useNativeOverlayCoverage         preview_close / close_all / list
       ├─ previewSessionStore (label↔tab)  browser/registry.rs ── PreviewRegistry
       └─ lib/previewHost.ts (typed IPC)     label/url/title/phase/visible/bounds
                                            LRU ≤ MAX_LIVE_SESSIONS
navigation_guard (plugin on_navigation)   ← navigation_policy::is_allowed(role, url)
                                                   ▲
                                     Window::add_child(WebviewBuilder("preview-N", External(url)))
                                                   │  native child webview (top-level doc)
                                                   ▼
                                           github.com / workspace .html ticket URL
```

### 2.1 Frontend layering (why three modules, not one)

| Module | Single responsibility | Never does |
|---|---|---|
| [lib/previewHost.ts](apps/acowork-desktop/src/lib/previewHost.ts) | Command/event contract, error-code parsing, URL normalisation | Touch the DOM, hold state |
| [stores/previewSessionStore.ts](apps/acowork-desktop/src/stores/previewSessionStore.ts) | `sessionKey → label → PreviewState` projection | Issue IPC |
| [components/editor/WebPreviewHost.tsx](apps/acowork-desktop/src/components/editor/WebPreviewHost.tsx) | Measure → create/reuse session → push geometry and visibility | Render visible UI (the native surface covers everything) |
| [components/editor/UrlPreviewView.tsx](apps/acowork-desktop/src/components/editor/UrlPreviewView.tsx) | Toolbar and error card (both must sit **outside** the webview rect) | Own session lifecycle |

### 2.2 Coordinate space

`PreviewBounds` is in **window-client logical pixels** — the same space as `getBoundingClientRect()`, so no scroll or DPI conversion is needed; the frontend only **rounds** (fractional coordinates render soft text in WebView2 and would push IPC every resize frame). `previewGeometry.ts` reduces a DOM rect to four numbers as pure functions so they are unit-testable.

---

## 3. Decision details

### D1 Same-window child webview

Three hard requirements had to hold at once: (a) pages refused by XFO must render; (b) login state and lifecycle stay shared with the main window (tray hide/show, single instance, DPI scaling); (c) panel geometry is decided by React layout, so the webview must follow a `ResizeObserver`.

`add_child` satisfies all three: it is a webview **of this window**, we supply its rectangle, and it shares the window's profile.

### D2 Role-parameterised navigation policy

`navigation_guard` is a **plugin-level** `on_navigation`, and Tauri chains it onto **every** webview (`tauri/src/manager/webview.rs`). With a single "app allow-list", a preview webview's external URL is blocked outright — the symptom is "webview created, permanently blank", with no error.

So `navigation_policy.rs` exposes `is_allowed(role, url)`:

| Role | Rule | Basis |
|---|---|---|
| `App` | The original allow-list (dev server / gateway / bundled pages), everything else refused | **Line-for-line equivalent** to the pre-change behaviour (`navigation_guard.rs` is now only a call site) |
| `Preview` | `http` / `https` allowed, every other scheme refused | Complement semantics: a preview is the door to the outside world and needs no knowledge of internal addresses |

The role comes from `webview.label()` (`PREVIEW_LABEL_PREFIX = "preview-"`), never from the frontend. `open_preview` validates the URL with the same policy **before creating anything**, so a bad scheme yields a `blocked_scheme:` error instead of a blank webview.

### D3 Session lifecycle and keys

| Event | Behaviour | Why |
|---|---|---|
| Tab opened | `preview_open` → label `preview-N` → frontend binds `OpenFile.id → label` | Create once, reuse afterwards |
| Tab switched away | `preview_set_visible(false)` (**park**) | The page must not reload: scroll position, forms and video survive |
| Tab switched back | `preview_set_bounds` + `set_visible(true)` | Only geometry and visibility change |
| Tab closed | `preview_close` (via `registerFileDisposer`) | A session must not outlive its tab |
| More than 3 live | `registry.overflow()` → destroy the least recently active | Every preview is its own renderer process (WebView2); no cap means no memory ceiling |
| Reused label was evicted | Backend returns `unknown_preview:` → frontend **drops the binding and rebuilds once** (at most twice, then reports an error) | The user sees "the page reloads", never a permanently blank tab |

`sourceUrls[sessionKey]` records "the last URL that came from props": on remount, navigation only happens when the prop **actually changed**, otherwise the page the user navigated to inside the preview would be yanked back.

### D4 Permission narrowing (security red line)

`windows: ["main"]` → `webviews: ["main"]`. The former makes an `add_child` preview inherit `shell:allow-open`, `fs`, `dialog`, … — i.e. **any rendered third-party site could call the app's IPC surface**. `run_desktop_ipc_scope_redline` guards it with three greps:

1. no `"windows": ["…"]` in `capabilities/*.json`;
2. `default.json` must contain `"webviews"`;
3. no capability may name a `preview` label (previews hold zero permissions).

### D5 Popups, downloads, and actually getting out

- `window.open`: `Deny` + a `preview://new-window` event → the frontend calls `openUrl(agentId, url)` to open another preview tab (with a workspace context) or `openExternal(url)` without one. Rationale: no unreclaimable native windows, and it matches the browser expectation of "new tab".
- Downloads: `on_download` refuses. Without a download panel there is no trustworthy destination; silently dropping files into `%TEMP%` would be worse.
- "Open in browser": now goes through `openExternal` (`plugin-shell`'s `open`, with `shell:allow-open` granted to the main webview only) instead of a `target="_blank"` `<a>` — that path is denied by default in Tauri v2 with no handler installed.

### D6 Accepting `unstable`, and containing it

`add_child` is only exported behind `tauri`'s `unstable` feature. This is not a taste question about beta APIs: without D1 there is no in-app browser, only "iframe (cannot open the page)" plus "system browser (leaves the app)".

Containment:
- `tauri = { version = "=2.12.3", features = [..., "unstable"] }` — an exact pin so a patch upgrade cannot silently change `add_child` semantics;
- `browser/` is the **only** directory touching `add_child` / `WebviewBuilder`; command signatures and DTOs live there, so the upgrade blast radius is known;
- integration tests cover the registry and the policy (GUI-free, regression-safe); the upgrade checklist is in §5.2.

---

## 4. Consequences

### 4.1 Positive

1. **Every page refused by XFO/CSP now previews** — github.com, google.com, bank sites — matching Cursor. This was the original requirement.
2. Three historical defects disappear together: the fake "slow load" hint (no more `onLoad` heuristic), the dead "open in browser" button, and unexplained blank panels.
3. The navigation policy becomes a **parameterised pure function** with an 8-case test of its own, and the permission leak is pinned by a red line forever.
4. Workspace `.html` previews (ADR-112) and external-URL previews **share one host**: one coordinate system, one lifecycle, one permission model — no more "two previewers behaving differently".

### 4.2 Negative / constraints we accept

1. **The native layer is always above the HTML.** Three non-negotiable UI rules follow:
   - toolbar, loading indicator and error card **must sit outside** the webview rect; nothing may be drawn on top of the preview area;
   - whenever any HTML overlay opens (modal, dropdown, context menu, lightbox) the webview **must be hidden**, or the dialog is invisible and its clicks fall through to the page — driven by `useNativeOverlayCoverage` using `document.elementFromPoint` hit testing (instrumenting the ~15 portals would silently regress with every new overlay);
   - known blind spot: overlays using `pointer-events: none` are invisible to `elementFromPoint`. No **input-blocking** overlay in this app uses that combination; new ones must call `preview_set_visible(false)` explicitly.
2. **Memory**: each live preview is a separate renderer process. The cap of 3 is a deliberate trade-off, not a tunable.
3. **Each live session is a real browser**: it runs third-party JS inside the main window's profile. The security boundary is D4 (zero IPC permissions), not sandbox isolation — previews and the app share one WebView2 profile.
4. **`unstable` dependency**: contained as described in D6.
5. **Platform coverage**: Windows is implemented and verified first; macOS/Linux use the same `add_child` path but were **not exercised this round** (§5.2 lists them in the checklist).

### 4.3 Not covered by this round (honest list)

- `dev/ci.sh`'s ADR-076 identity-injection red line already fails on HEAD (`core/acowork-gateway/src/app/mcp_proxy.rs:146`); unrelated to this change and needs its own handling.
- The desktop Rust integration tests (`capability_scope` / `preview_registry` / `navigation_policy`) are **not** run by `dev/ci.sh`'s `core` mode: they belong to `apps/acowork-desktop/src-tauri`, separate from the `core/` workspace. They run via local `cargo test`; whether Desktop gets its own CI job is a separate decision.
- GUI smoke testing (real `add_child` creation, DPI drag, overlay occlusion) needs a human. On this machine the lib unittest binary cannot start on Windows (`0xc0000139 STATUS_ENTRYPOINT_NOT_FOUND`: the test target gets no application manifest, so `comctl32` binds the System32 v5.82 copy and misses `TaskDialogIndirect`, which `rfd` imports statically) — a pre-existing environment issue; `[[test]]` integration targets are unaffected (18 passed this round).

---

## 5. Verification

### 5.1 Measured this round

| Item | Result |
|---|---|
| `cargo check --all-targets` (src-tauri) | Passes (full recompile, `unstable` feature included) |
| `cargo test --test navigation_policy` | 8 passed |
| `cargo test --test preview_registry` | 8 passed |
| `cargo test --test capability_scope` | 2 passed |
| `npx tsc --noEmit` (frontend) | No new errors (5 pre-existing missing third-party modules) |
| `npm run check:i18n` | 1999 call sites resolve; all 5 locale files valid |
| vitest (5 new files) | 31 passed: `previewGeometry` 7 / `previewSessionStore` 7 / `WebPreviewHost` 7 / `UrlPreviewView` 7 / `useNativeOverlayCoverage` 3 |
| `dev/ci.sh redlines` — `run_desktop_ipc_scope_redline` | OK |

### 5.2 Upgrade / platform-extension checklist

1. Confirm `tauri` still exports `WebviewWindow::add_child` and `NewWindowResponse` (`unstable`).
2. Confirm `add_child` must still be asynchronous on Windows (otherwise it deadlocks).
3. Repeat §5.1's Rust tests plus manual GUI smoke on macOS / Linux (geometry, occlusion, login state).
4. If Tauri ships a stable multi-webview API, drop the `unstable` feature and re-run everything in this section.

---

## 6. Alternatives considered and rejected

| Alternative | Verdict | Why rejected |
|---|---|---|
| Keep the `iframe`, relax `sandbox` / work around `X-Frame-Options` | Rejected | Physically impossible: the headers come from the target site and the engine enforces them on the embedded side. A textbook band-aid (fake hints, dead buttons, "open externally" fallbacks never fix a blank page) |
| Separate `BrowserWindow` / embedded system browser | Rejected | The value of an in-app browser is previewing **inside the panel**; a popup window breaks panel-level layout semantics and forces extra work on tray, single-instance and login state |
| Migrate to Electron (what Cursor uses) | Rejected | Rewriting the entire desktop shell for one feature; Tauri already exposes the equivalent primitive |
| Strip `X-Frame-Options` at a local reverse proxy | Rejected | Turns "render a page" into "proxy and rewrite third-party traffic": login, cookies, CSRF and TLS would all pass through us, and it only covers sites we can proxy — far less coverage than a child webview |
| Only offer "open in system browser" | Rejected | That removes the feature, and the user explicitly asked for Cursor parity |
| Have each portal component register "I am open" | Rejected (kept only as a supporting signal) | ~15 portals today and growing; one miss is an unrecoverable "dialog hidden behind the page" bug. Hit testing is the only criterion that does not regress with new overlays |

---

## 7. References

- Code: [browser/mod.rs](apps/acowork-desktop/src-tauri/src/browser/mod.rs), [browser/host.rs](apps/acowork-desktop/src-tauri/src/browser/host.rs), [browser/registry.rs](apps/acowork-desktop/src-tauri/src/browser/registry.rs), [navigation_policy.rs](apps/acowork-desktop/src-tauri/src/navigation_policy.rs), [navigation_guard.rs](apps/acowork-desktop/src-tauri/src/navigation_guard.rs)
- Frontend: [WebPreviewHost.tsx](apps/acowork-desktop/src/components/editor/WebPreviewHost.tsx), [UrlPreviewView.tsx](apps/acowork-desktop/src/components/editor/UrlPreviewView.tsx), [previewHost.ts](apps/acowork-desktop/src/lib/previewHost.ts), [previewSessionStore.ts](apps/acowork-desktop/src/stores/previewSessionStore.ts), [useNativeOverlayCoverage.ts](apps/acowork-desktop/src/hooks/useNativeOverlayCoverage.ts), [usePreviewBridge.ts](apps/acowork-desktop/src/hooks/usePreviewBridge.ts)
- Permissions and red line: [capabilities/default.json](apps/acowork-desktop/src-tauri/capabilities/default.json), [dev/ci.sh](dev/ci.sh) (`run_desktop_ipc_scope_redline`)
- Design: [14-desktop-app.md](../design/zh/14-desktop-app.md) §7.5
- Follow-on plan (browser automation / self-hosted browser MCP survey and design): [browser-automation-plan.md](../../plan/zh/browser-automation-plan.md)
- Related ADRs: [ADR-112](./ADR-112-workspace-preview-capability-ticket.md) (workspace HTML ticket URLs — this ADR's host), [ADR-116](./ADR-116-hosted-application-framework.md) (hosted apps also render third-party documents)