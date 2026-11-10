# ADR-112: A Capability Ticket for Workspace Static-Asset Preview

- **Number**: ADR-112
- **Status**: Implemented
- **Date**: 2026-11-10
- **Decision maker**: Nicholas (requirements and approach)
- **Related**: ADR-009 (Gateway boundary — the Runtime still owns the bytes, the Gateway only reverse-proxies), ADR-055 L2-7 (the `/workspace-files/*` static proxy and the remote-Node topology), ADR-087 (workspace file content lives at the `use` tier and must not leak at `view`), ADR-076 §Decision 3 (in `multi_user` mode the whole Gateway requires a Bearer token), ADR-108 (right-panel consolidation — preview lives in the workspace panel)
- **Chinese source of truth**: [docs/adr/zh/ADR-112-workspace-preview-capability-ticket.md](../zh/ADR-112-workspace-preview-capability-ticket.md) (maintained in the same commit)

---

## Summary

**In one sentence**: a workspace HTML file is no longer rendered from a Blob URL with an injected `<base href>`; the Gateway mints a short-lived **capability ticket bound to `(agent, workspace)`**, the page loads as a real `http://` document, and it renders in the same address-bar browser (`UrlPreviewView`) that a URL clicked in a chat message uses.

**In one sentence (why)**: a page's CSS / JS / fonts / images are **sub-resource requests made by the browser**, and a browser request cannot carry an `Authorization` header. The old path pointed those at the bearer-gated `/workspace-files/*` route, so the HTML skeleton painted (its text was local) while every stylesheet and script **401'd** — which is exactly the "it renders like a local file, layout is a total mess" the user reported.

---

## Background (evidence)

1. **The symptom and the cause do not match.** The user described it as "rendering like a local file", but the preview never used `file://`: the document was a `blob:` URL (`HtmlPreviewView`) and its sub-resources were `http://…/workspace-files/…`. The only difference is authentication — not the protocol, not the origin.

2. **The Gateway log proves the 401s.** `~/.acowork/acowork-gateway/data/logs/20261110_112918.log`, lines 21946–21969 (11:39:14, opening `ACoworkWebSite/index.html`): `styles.css?v=9`, `main.js`, `assets/logo.svg`, `assets/logo-gray.svg`, `assets/fonts/Inter-Variable.woff2`, `JetBrainsMono-Variable.woff2` — each `status=401 Unauthorized`, with `origin=<none>` (sub-resource requests from a blob document send no Origin).

3. **Reproduced live against the running Gateway.** Same route, no token:
   `GET /workspace-files/…/AGENTS.md` → `401 {"error":"unauthorized","message":"missing bearer token"}`.

4. **The mechanism is explicit in code.** `auth_middleware::is_public_path` does not allow `/workspace-files`, so `multi_user` gates the whole surface behind a Bearer token; `<link>` / `<script>` / `@font-face` cannot send one. This is the same defect `workspaceFileBlob.ts` documents for `<img>` — the HTML preview path had simply been missed.

5. **The working paths nearby prove the diagnosis.** Image / PDF / Office / Markdown previews all go through `useWorkspaceFileBlob` → authenticated `fetch` → Blob URL, and never break: they make **no sub-resource requests**. The bug only appears when a browser must fetch thirty files for one page by itself.

6. **A `?ticket=` design cannot work (key constraint).** Relative-URL resolution **keeps the path and discards the query**: a `?ticket=x` on the document URL is gone by the time `styles.css` resolves. The ticket therefore has to live in the **path** — the only form that covers sub-resources.

7. **Making `/workspace-files` public is not an option.** That would let any local process read every agent's workspace with no credential, and would break ADR-087 §option G (workspace file content must not leak at `view`; reading `workspace/file` is `use`).

---

## Decision

### D1 — The HTML preview becomes a real HTTP document

The `text/html` preview branch no longer fetches the text, builds a Blob and injects a `<base>`; it exchanges for a ticket and hands `http://<gateway>/workspace-preview/<ticket>/<agent>/<ws>/<path>` to `UrlPreviewView`.

Complexity deleted along the way: `HtmlPreviewView.tsx`, `htmlPreviewTransform.ts` (root-relative / `srcset` rewriting plus its self-test script), and the `check:html-preview` build step. The browser already resolves relative paths; that rewrite was client code approximating something it could not do.

### D2 — The ticket lives in the **path**, not the query

`GET /workspace-preview/{ticket}/{agent_id}/{workspace_id}/{*path}`. Relative sub-resources (`styles.css`, `assets/x.svg`, `url(...)` inside CSS) inherit the whole prefix automatically, so the document and all of its dependencies authenticate through one grant with no rewriting.

### D3 — One implementation, two authentication entry points

`workspaces::proxy_workspace_file` is the single byte path (per-segment encoding, `workspace_id` pass-through, Runtime-side `resolve_within` traversal guard). `/workspace-files/*` (Bearer) and `/workspace-preview/*` (ticket) are both entry points to it; the authentication difference is resolved at the entry, and the byte path is never written twice.

### D4 — Ticket semantics and guard-rails

Format `v1.<expires_at>.<mac_hex>` where `mac = HMAC-SHA256(secret, context|exp|len:agent|len:workspace)`, truncated to 128 bits. Agent / workspace are **not stored in the ticket** — verification recomputes them from the request path, so a ticket cannot be replayed against another workspace. Fields are length-prefixed so an id containing the separator cannot produce an ambiguous concatenation.

Guard-rails (do not relax without amending this ADR):
- **Read-only**: `GET` only, file bytes only, never a write.
- **Bound**: one ticket is valid for exactly one `(agent_id, workspace_id)` pair.
- **Expiring**: 8 hours (`TICKET_TTL_SECS`).
- **Non-escalating**: the mint route `POST /api/agents/{id}/workspaces/preview-ticket` is classified `use` in `permission::classify` — the **same tier** as `GET /workspaces/file`. The ticket re-expresses an existing read right in a browser-carryable form; it is not a new privilege. Classifying it `manage` would let a guest read the JSON but not render the page, producing the bizarre split of "HTML preview is owner-only while every other preview works".
- **Never persisted**: the key is a per-process random value (created in `AppState::new`), so a restart invalidates every outstanding ticket.
- **Never logged**: the ticket lives in the path, and the access-log middleware replaces it with `***` (`workspace_preview::redact_preview_ticket` — the same rule ADR-089 D2 applies to `?access_token=`). Otherwise a single page load would write an 8-hour credential into the log six-plus times, and the log is exactly what a user hands over when reporting a bug.

### D5 — A fresh ticket per navigation, no client-side cache

`lib/workspacePreviewUrl.ts` mints a ticket **immediately before every navigation**; concurrent callers for the same `(agent, workspace)` share one mint.

Not caching across navigations rests on a fact, not a preference: the signing key is per process, so a Gateway restart invalidates every outstanding ticket — and the Desktop **cannot observe that restart**. The HTTP layer carries no process identity and `/health` reports none (only `status` / `version` / `checks`). Any client cache would keep handing an unverifiable ticket to the iframe after a restart, and all the user would see is a 403 JSON page inside the preview — harder to diagnose than the 401 deluge it replaced. A mint is a pure HMAC computation on the Gateway (no Node / Runtime hop), so the cost of not caching is one sub-millisecond request.

(Two alternatives were considered and rejected: a process boot id on `/health`, which changes a public response the whole platform depends on; and a HEAD pre-flight of the ticket URL, which costs another round-trip and a CORS dependency. Neither is simpler than not caching.)

A failed mint does **not** fall back to the old blob path (that would silently resurrect the 401 bug with no way to tell it apart from a working preview) — it renders an error card with a retry, and the retry necessarily gets a fresh ticket because nothing was cached.

---

## Consequences

**Positive**
- Workspace HTML and external URLs share one rendering surface (address bar, back/forward/refresh, open-in-browser) and render like a real site: CSS, JS, fonts, images, `<iframe>`, `<video>` all work.
- Two modules and a build step are gone; `<html>` relative-path semantics are no longer emulated.
- The capability ticket is the presigned-URL pattern and can be reused for future static assets the browser must fetch directly.

**Costs / known limits**
- A new public prefix exists. What it buys an attacker is "GET-read one workspace's file subtree, unguessable URL, 8 hours" — the same ceiling as the caller reading files by hand. But whoever holds the URL (any local process, or the LAN depending on how the Gateway binds) can read that subtree while it is valid. This is the one surface this ADR widens, and future work must assume it.
- **Root-relative paths still do not work**: `<img src="/assets/x.png">` resolves against the Gateway root and 404s. Fixing that needs the preview to own an origin/port (e.g. a dedicated static server), which costs far more than it buys — and that form failed under the old design too (401), so nothing regresses. Static sites should use relative paths, which is the standing convention of every static host.
- The ticket is visible in the address bar. That is deliberate: it is the real, copyable address, and it expires on its own after 8 hours.
- **The preview renders the file on disk, not the unsaved editor buffer.** The old blob renderer drew `activeFile.content`, including unsaved edits; the new one lets the browser GET the bytes — the unavoidable price of a real HTTP document, since serving unsaved content over HTTP would need a write-to-disk or an in-memory endpoint, which is not worth it. Current behaviour: while a file is dirty the preview still shows the last saved content. (Follow-up: hint that in the preview area when dirty, or save automatically before switching to preview.)

---

## Implementation

- **Gateway**: `http/workspace_preview.rs` (mint/verify, log redaction, both routes, error mapping, 21 tests); `proxy_workspace_file` extracted in `http/workspaces.rs`; registration plus `AppState.workspace_preview_secret` in `http/routes.rs`; prefix allow-listing in `http/auth_middleware.rs::is_public_path`; `use` classification in `http/permission.rs`.
- **Desktop**: new `lib/workspacePreviewUrl.ts` and `components/editor/WorkspaceHtmlPreview.tsx`; the `text/html` branch of `FileEditorPanel` rewired; `HtmlPreviewView.tsx`, `htmlPreviewTransform.ts`, `scripts/check-html-preview.mts` and the `check:html-preview` build step deleted; `htmlPreviewTsModuleWarning` removed and `htmlPreviewFailed` / `htmlPreviewRetry` added across all five locale files.
- **Tests**: gateway unit tests cover expiry, MAC tampering, cross-workspace replay, field-splicing ambiguity, key rotation, and an RFC 4231 known-answer check for the inlined HMAC, plus "verification precedes proxying" (a forged ticket is 403, a genuine one reaches the proxy's 503). Desktop tests cover ticket caching / in-flight coalescing / pre-expiry renewal / a failure not poisoning the cache, and the component's contract that it hands over an **HTTP document rather than a blob**, its error card, its retry, and dropping a stale response when the file changes mid-mint.
