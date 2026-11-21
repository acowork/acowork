# Changelog

All notable changes to ACowork are documented here.

Release notes attached to a GitHub Release are **generated** at publish time by
[`dev/common/gen_changelog.mjs`](dev/common/gen_changelog.mjs) from Conventional
Commits, so this file is a human-readable summary rather than a second source of
truth. If a change needs a paragraph of explanation that a commit subject cannot
carry, add it here in the same commit as the change.

Versioning follows [Semantic Versioning](https://semver.org/). The single source
of truth for the current version is [`VERSION`](VERSION); see
[`RELEASING.md`](RELEASING.md).

Commit subjects are English, `<type>(<scope>): <summary>` — see
[`CONTRIBUTING.md`](CONTRIBUTING.md#commit-messages).

## [Unreleased]

No GitHub Release has been published yet. The current tree declares `0.1.0`;
the first released tag will be `v0.1.0`.

### The in-app page preview is now a real browser view (ADR-118)

The desktop workspace preview could not open github.com — or any site that answers
`X-Frame-Options: deny` / `CSP frame-ancestors 'none'` — because it rendered pages in an
`<iframe>`, and that restriction is enforced by the browser engine on the embedded
document. There is no frontend workaround; Cursor and VS Code instead load the page as a
top-level webview.

The preview is now a **same-window child webview** (`Webview::add_child`, Tauri's
`unstable` feature): the page loads as a top-level document, so those headers do not
apply. Alongside it:

- navigation policy is parameterised by webview role (`App` unchanged, `Preview` allows
  `http`/`https` only), so an external URL is no longer blocked by the app's own allow-list;
- preview sessions are registered, capped at 3 with LRU eviction, and parked (not
  destroyed) on tab switch, so switching tabs no longer reloads the page;
- `capabilities/default.json` scopes by `webviews` instead of `windows` — previously every
  child webview inherited the app's IPC surface, which included `shell:allow-open` and `fs`;
  `dev/ci.sh` gained `run_desktop_ipc_scope_redline` to keep it that way;
- native surfaces paint above HTML, so HTML overlays hide the webview while open, and the
  toolbar/error card live outside its rectangle.

Two defects fixed by the same change: the "page is taking longer" hint that never fired
(an iframe still dispatches `onLoad` when XFO refuses it) and the "open in browser" button
that did nothing (`target="_blank"` with no new-window handler).
