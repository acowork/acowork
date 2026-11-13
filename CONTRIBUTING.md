# Contributing to ACowork

Thanks for your interest in improving ACowork. This document is the contract for
contributors: what we expect in a pull request, how the change gets reviewed, and
which project rules a reviewer will hold you to.

If you only want to report a bug or propose a design change, read
[Reporting issues](#reporting-issues) — you do not need to install anything.

ACowork is a **decentralized AI agent runtime platform**. Every agent is a declarative
`.agent` package (manifest + prompts + skills, no binary), loaded by a universal Runtime
binary and managed by a Gateway. Before you write code, read
[`AGENTS.md`](AGENTS.md) — it is the source of truth for architecture and conventions,
and this document assumes it.

---

## Table of contents

- [Quick start for a first contribution](#quick-start-for-a-first-contribution)
- [Reporting issues](#reporting-issues)
- [Setting up the development environment](#setting-up-the-development-environment)
- [The build and test gates you must pass](#the-build-and-test-gates-you-must-pass)
- [Project rules a reviewer will check](#project-rules-a-reviewer-will-check)
- [Writing code](#writing-code)
- [Commit messages](#commit-messages)
- [Documentation rules](#documentation-rules)
- [Adding or changing an agent package](#adding-or-changing-an-agent-package)
- [Review process](#review-process)
- [License and contribution terms](#license-and-contribution-terms)

---

## Quick start for a first contribution

The lowest-friction useful contribution is a bug fix with a failing test that
demonstrates it. The path is:

```bash
# 1. Fork the repository, then clone your fork
git clone https://github.com/<your-handle>/ACowork.git
cd ACowork

# 2. Branch off master
git checkout -b fix/<short-slug>

# 3. Build the backend (also sets up ONNX Runtime on first run)
./dev/common/build_core.sh         # Windows: .\dev\common\build_core.ps1

# 4. Make the change, then run the full gate
./dev/ci.sh all
```

CI lives on **GitHub** (`github.com/acowork/ACowork`, `.github/workflows/ci.yml`) and it is
the review home for this repository; the Gitee remote is a mirror. `dev/ci.sh` is the single
entry point for both sides — CI calls the same modes you do, so a gate you can run locally
has no hidden CI-only behaviour. `./dev/ci.sh all` is stricter than CI in exactly one way:
it also runs the frontend smoke suite, which boots a real Gateway and is not a per-push job.

**Good first contributions** in this repository:

| Area | What helps | Where to look |
|---|---|---|
| Rust core | Bug fixes, error handling, new API surface | `core/acowork-*/src/` |
| Desktop / Mobile UI | React components, i18n fixes, perf work | `apps/acowork-desktop/src/`, `apps/acowork-mobile/src/` |
| i18n | Adding a locale, fixing a missing key | `apps/acowork-desktop/src/i18n/locales/` |
| Agent packages | New example agents, better skill docs | `agents/` |
| Docs | ADR translations, protocol reference corrections | `docs/` |

---

## Reporting issues

Open an issue on the tracker with:

- **What you did** and **what you expected** to happen.
- **What actually happened**, including the exact error text.
- **How to reproduce** — a minimal `.agent` package, a curl command, or a log excerpt.
- **Environment** — OS, ACowork version/commit, and whether the Gateway runs in `local` or
  `multi_user` auth mode.

For **security** problems, do **not** open a public issue. Follow
[`SECURITY.md`](SECURITY.md) instead.

For **design** feedback, an issue is the right venue — but read the ADR corpus first
([`docs/adr/zh/`](docs/adr/zh/)). A large fraction of "this looks wrong" reports are
answered by an ADR that already decided the question, including the reason.

---

## Setting up the development environment

### Prerequisites

| Tool | Version | Needed for |
|---|---|---|
| Rust | stable ≥ 1.95 (edition 2024) | everything under `core/` |
| Node.js | ≥ 18 | `apps/acowork-desktop`, `apps/acowork-mobile` |
| Python 3 + `pillow` | 3.x | frontend smoke tests, store logos |
| PowerShell 7 | 7.x | the `.ps1` scripts on Windows |

`Cargo.lock` is **not** committed (see [`.gitignore`](.gitignore)), so the crate floor
in `core/Cargo.toml` is what carries dependency constraints. If you bump a dependency,
bump the floor in the same commit.

ONNX Runtime is **not** a manual prerequisite — `dev/common/build_core.*` probes `.ort/` and
offers to run `setup_ort.*` in place when it is missing, and `dev/ort_env.js` makes Tauri
auto-detect it on every platform.

### Building

```bash
# Core (Rust workspace) — all members
cd core
cargo build --release

# Or via the cross-platform scripts (recommended; they handle ORT discovery)
./dev/common/build_core.sh          # Linux / macOS
.\dev\common\build_core.ps1         # Windows
```

Desktop and Mobile both consume the same core binaries:

```bash
cd apps/acowork-desktop
npm install
npm run core:build:debug      # build the core binaries the app shells out to
npm run tauri dev             # native window, HMR frontend

npm run dev                   # browser-only dev server (Vite)
```

### Repository layout

```
core/                  Rust workspace — core/Cargo.toml [workspace] is the source of truth
  acowork-core/        Shared types, errors, config, MQTT proto
  acowork-gateway/     HTTP API, embedded MQTT broker, reverse proxy, lifecycle, package mgr
  acowork-runtime/     Agent runtime (main loop, tools, providers, sessions)
  acowork-node/        Node Agent — per-machine daemon hosting Runtime processes
  acowork-memory/      Memory service — MCP + panel REST
  acowork-user/        User domain service — accounts, credentials, roles, profiles
  acowork-pm/          Project & task management service
  acowork-doc/         Online document library service
  acowork-sign/        Package signing & verification
  acowork-vault/       Encrypted key/value store
  acowork-sqlite/      Runtime-side SQLite sidecars
  acowork-tool-sdk/    WASM custom tool SDK (Wasmtime host)
  acowork-embed/       ONNX-Runtime embedding model runner
  acowork-lsp-relay/   LSP protocol relay
  acowork-relay/       Cloud thin relay — BYO self-hosted tier
  acowork-eval/        Retrieval-eval kernel — dev-dependency only, never shipped

apps/
  acowork-desktop/     Tauri v2 desktop app (React/TS frontend + thin Rust backend)
  acowork-mobile/      Tauri v2 mobile app

docs/                  Architecture docs — design, ADR, PRD, protocols, runbooks
agents/                Agent packages shipped with the app
dev/                   Build, packaging, CI, deployment scripts
```

---

## The build and test gates you must pass

Everything below is what CI runs. Run it **before** you open a PR.

| Job | Command | Catches |
|---|---|---|
| `redlines` | `./dev/ci.sh redlines` | the 14 architecture red lines, in seconds, before anything compiles |
| `core` | `./dev/ci.sh check` → `clippy` → `test` → `integration`, plus a strict `cargo test -p acowork-embed --features download-ort` | compile, lints, unit + cross-crate e2e |
| `packages` | `cargo test -p acowork-sign --test example_packages`, `cargo test -p acowork-core --test hub_interop -- --ignored`, `sync-official.py --check --report-only` | a shipped `.agent` that does not match its source; a hub mirror that drifted |
| `web` | `npm ci && npm run test && npm run build` (desktop + mobile) | 350 desktop + 38 mobile test files, i18n coverage, the scroll-container backlog |

`dev/ci.sh test` downgrades a missing ONNX Runtime to a warning, which is right on a laptop
and wrong on a gate — so the `core` job runs the embed tests again, strictly.

### Core (Rust)

```bash
cd core
cargo check --all
cargo clippy --all-targets -- -D warnings      # warnings are errors
cargo test --workspace --exclude acowork-embed
```

> **`CARGO_TARGET_DIR` must be rooted at `core/`.** End-to-end test executables are
> built with `CARGO_TARGET_DIR=target-e2e` — set it from the `core/` directory so the
> artifacts land in `core/target-e2e` and never in a running process's default
> `target/`. Run it from a sub-crate (or its `src/`) and a second `target-e2e` tree
> silently grows inside that crate's source tree (it has already cost us a 2 GB
> `core/acowork-pm/src/target-e2e`).

### The red-line checks

`dev/ci.sh check` runs a set of **architecture red lines** — grep-based checks that
enforce the boundaries the ADRs froze. These are not style preferences; they encode
bug classes that have actually shipped. A PR that violates one will be closed, not
discussed.

| Red line | Enforces | Source |
|---|---|---|
| `run_node_redline` | `acowork-node` must not depend on `acowork-gateway` | ADR-055 §6.20 |
| `run_mqtt_redline` | `ErrorKind::MqttState` may not appear outside `acowork-mqtt-session` | ADR-065 §7 #1 |
| `run_gateway_fs_redline` | Gateway may not read agent-private files directly | ADR-009 §5 |
| `run_user_boundary_redline` | Gateway may not touch user-domain data paths | ADR-084 决策 1 |
| `run_meta_layout_redline` | Session meta files have exactly one owner | ADR-024 |
| `run_session_full_read_redline` | No full session-log read on a request path | ADR-096 |
| `run_gateway_auth_scope_redline` | Gateway may not assert a user identity from a client claim | ADR-076 §决策 4 |
| `run_user_auth_mode_redline` | Auth mode handling | ADR-076 |
| `run_user_chat_path_redline` | User chat path ownership | ADR-084 |
| `run_permission_route_redline` | Every agent/node route must live under the permission gate | ADR-087 D5 |
| `run_lsp_registry_redline` | LSP registry ownership | ADR-091 |
| `run_ort_api_redline` | `ort` API level must not exceed the oldest supported ONNX Runtime | — |
| `run_chat_projection_redline` | the chat projection keeps its registry fallback, write contract and row shapes | Design 31 §6 |
| `run_agent_skill_tool_redline` | an agent may not reach for a raw file tool where a skill owns the write | docs/plan §8 |

Every one of them is a grep over **production** code: `scan_production` skips everything from
a file's first `#[cfg(test)]` marker onward. A test fixture that hand-rolls a path or echoes
an identity header is simulating a peer, not crossing a boundary, and that code is not in the
shipped binary — so counting it would train everyone to add allowlist entries for test files,
which blinds the guard to the production code in the same file. A violation cannot hide after
the marker: past it the file compiles only for tests.

The two red lines with **frozen ceilings** are worth understanding before you touch the
relevant files:

- **Gateway filesystem ceiling** (ADR-009) — the Gateway is *communication + resource
  management + reverse proxy*. Agent-private data (skills, prompts, conversations,
  per-agent embedding, `{install_path}/assets`) is read and written **only through
  Runtime HTTP**. `install_path` is node-local, so a Gateway-side filesystem read works
  on a single machine and breaks across hosts.
- **Session meta layout ceiling** (ADR-024) — `conversations/meta/{session_id}.json` has
  exactly one owner: `core/acowork-runtime/src/conversation.rs`. A second writer silently
  desyncs the in-memory session listing.

> **If you hit a ceiling**, the correct move is to lower the number in the same commit
> that removes the call site — never to raise it, and never to add a new allowlisted
> file.
>
> A *writer allowlist* (as opposed to a numeric ceiling) is different, because a new
> legitimate owner does appear from time to time: `intent_api.rs` joined the
> identity-injection list when the intent API started forwarding a scoped read to the
> Runtime. That is an ADR amendment in all but name, so the entry must say in the guard
> why the site is a trusted **writer** deriving identity from the verified token, and not
> a second **authority** deciding it from a client claim. If you cannot write that
> sentence, the code is wrong, not the guard.

### Integration and smoke

```bash
# Node control-plane e2e, gateway settings API, node wire-protocol goldens,
# dependency red-line tests, eval shipping red-line
./dev/ci.sh integration

# Boots a real Gateway + Node against temp homes and exercises the HTTP/MQTT
# surface the desktop app talks to. Requires debug binaries.
./dev/ci.sh smoke
```

### Frontend (Desktop / Mobile)

```bash
cd apps/acowork-desktop
npm run build        # check:i18n + check:html-preview + tsc + vite build
npm run tauri dev

# Desktop unit tests (vitest, jsdom)
npx vitest run

cd ../acowork-mobile
npm run typecheck
npm test             # vitest run
```

### i18n is a compile-time gate

`npm run check:i18n` is not cosmetic — it enforces that every `t("a.b.c")` call site
resolves in **both** `en.json` and `zh-CN.json`, and that placeholders use the i18next
double-brace form `{{name}}`. Single-brace `{name}` is an error: i18next silently
fails to interpolate it and paints the literal `{name}` into the UI.

### The one command that runs everything

```bash
./dev/ci.sh all      # check + clippy + test + integration + smoke
```

---

## Project rules a reviewer will check

These come from [`AGENTS.md`](AGENTS.md) and the ADR corpus. They are the rules that
generate the most review comments.

### The Gateway boundary

> **Gateway = communication + resource management + reverse proxy.** Nothing else.

If your change makes the Gateway read a file, write a file, or open a database that
belongs to an agent or a user, it belongs in the Runtime or in `acowork-user` instead.
The Gateway reaches those through `http/proxy.rs` and `http/user_proxy.rs`.

### Memory is the service's, not the Runtime's

Per ADR-101, memory business logic lives in `acowork-memory`. The Gateway MUST NOT open
`instances/*/memory.db`. The single exception is the Node-side migration pass
(node-local fs owner → the service's `/admin/import`).

### The Desktop app is presentation only

No business logic, no state persistence. The React/TS layer talks to the Gateway over
HTTP and holds no authoritative state.

### Chat transcript rendering

Two rules that are easy to violate by accident:

- **Windowed and memoized.** Surfaces render through
  `apps/acowork-desktop/src/components/common/VirtualRowList.tsx`; rows stay
  `React.memo` with stable prop identities; prepend anchoring stays at the
  scroll-container level. **Never locate a transcript row by DOM selector (`[data-seq]`)** —
  off-viewport rows are not in the DOM.
- **Identity-preserving store updates.** A store write path whose new content equals what
  subscribers already hold must not produce a new object identity.

### Rust comments are English

Rust code comments (`//`, `//!`, `///`) MUST be English. This is not a stylistic
preference — mixed-language comments in `core/` are treated as a defect.

### ADR bilingual pairing

The **Chinese ADR is the source of truth**. Every Chinese ADR has an English
counterpart in [`docs/adr/en/`](docs/adr/en/) that **must be updated in the same
commit**. A stale English ADR is worse than a missing one: it reads as authoritative and
is not.

The pairing rules are in [`docs/adr/en/README.md`](docs/adr/en/README.md) — section
numbering stays stable across languages, identifiers are never translated, decision
numbers are preserved, supersession notices are preserved verbatim.

> If your change touches a Chinese ADR, the same commit must touch its English
> counterpart. Reviewers will check.

### Other conventions

- Design docs are bilingual: `docs/design/{zh,en}/`, `docs/module-design/{zh,en}/`.
- Paths in Markdown are **relative links**, never quoted: `[src/lib/foo.ts](src/lib/foo.ts)`.
- Do **not** kill the Gateway or Runtime process when testing — you are running inside it.
- Do **not** ship the `acowork-eval` crate in a release binary. It is a dev-dependency
  only; `acowork-eval/tests/shipping_redline.rs` enforces this.

---

## Writing code

### Rust

- **Edition 2024**, MSRV **1.95**.
- `cargo clippy --all-targets -- -D warnings` must be clean. Warnings are errors.
- Format with the repo config: `cargo fmt` (see `core/rustfmt.toml`,
  `core/clippy.toml`).
- Errors use `thiserror` for library error enums; `anyhow` for application-level
  context.
- Logging via `tracing`, not `println!`/`eprintln!`.
- Async traits use `#[async_trait]` with the crate floor ≥ 0.1.92 (see the comment in
  `core/Cargo.toml` — earlier releases trip clippy's `double_must_use`).
- **Doc comments on public items are the API contract.** If you change a public
  function's behaviour, update its `///` docs in the same commit. The comments in
  `core/` carry the rationale for the design, not just a restatement of the signature.

### TypeScript / React (Desktop & Mobile)

- React 19, TypeScript ≥ 5, Vite 6, Tailwind 4.
- `npm run build` runs `check:i18n` → `check:html-preview` → `tsc` → `vite build`.
  All four must pass.
- i18n: every user-visible string goes through `t()` and every key must exist in
  `en.json` **and** `zh-CN.json`.
- Component-level tests live beside the component as `*.test.ts(x)` and run under vitest
  (jsdom for the desktop app).

---

## Commit messages

**Conventional Commits, one line, English only:**

```
<type>(<scope>): <summary>
```

| Field | Rule |
|---|---|
| `type` | `feat`, `fix`, `refactor`, `docs`, `test`, `chore`, `style`, `perf` |
| `scope` | The crate or app you touched — see below |
| `summary` | Imperative mood, no trailing period, lowercase after the colon |

**A bug fix is `fix(...)`, not `bugfix(...)`.**

**Scopes in use** (measured from the commit history, plus the crate/app names):

`desktop`, `runtime`, `chat`, `gateway`, `adr`, `user`, `pm`, `mobile`, `memory`, `mcp`,
`providers`, `node`, `core`, `session`, `doc`, `relay`, `sign`, `protocol`, `runbook`,
`plan`, `design`, `dev`, `agents`, `assets`, `ui`

> `AGENTS.md` lists a narrower scope set. The history shows the wider set above is the
> de-facto convention — `chat`, `adr`, `mcp` and `relay` all appear regularly. Use the
> wider set; the point of the scope is to name the area you touched.

**Subject length.** The convention in `AGENTS.md` is ≤ 72 characters. The history runs
longer than that (the median subject over the last 200 commits is 67 characters, and the
longest is 105). Read the subject as a summary, not a sentence — write the *why* in the
body:

```bash
git commit -m "fix(runtime): expire inlined PDF payload at the round boundary" \
           -m "The payload stayed resident past the round boundary, so the next
round re-read a stale copy and rendered the previous document."
```

**Body.** Explain *why*, not *what* — the diff already says what. Reference the ADR or
design doc you are implementing when one exists:

```bash
git commit -m "fix(gateway): proxy session meta reads through the runtime" \
           -m "The Gateway read conversations/meta directly, which desynced the
session listing on multi-node deployments. See ADR-024 §5."
```

---

## Documentation rules

The repository documents its own decisions, and the docs are treated as part of the
change — not as a follow-up.

| Change | Doc that must move with it |
|---|---|
| An architectural decision | A new ADR in `docs/adr/zh/` **and** `docs/adr/en/` |
| A crate's interface or responsibility | `docs/module-design/{zh,en}/` |
| A protocol change | `docs/protocols/{zh,en}/` |
| A user-facing behaviour change | `docs/prd/{zh,en}/` |
| An operator-facing change (ports, deploy, troubleshooting) | `docs/runbooks/` |
| An implementation plan | `docs/plan/zh/` |

> **ADR authoring.** A new ADR number is a new ADR number — do not reuse a retired one.
> Numbering notes: `037` is unused; `061` and `062` each have two files (a decision and
> its companion report). If your ADR revises or supersedes an earlier one, the
> supersession notice must appear **in the same place** in both language versions.
> Reference an ADR by number and section: `ADR-024 §5`, `ADR-009 §5.4`.

---

## Adding or changing an agent package

Agent packages live in `agents/` and are declarative: `manifest.toml` + `prompts/` +
`skills/` + `assets/`. They contain **no executable code**.

```bash
# Build and sign an agent package
./dev/common/build-agent.sh        # Linux / macOS
.\dev\common\build-agent.ps1       # Windows
```

The script packages, generates a developer signing key if needed, signs the package, and
verifies the signature.

### Signing rules

- **Never commit a private key.** See [`SECURITY.md`](SECURITY.md). The signing keys used
  by `dev/common/build-agent.*` land in `agents/.signing-keys/` — that directory is
  git-ignored and generated per machine, so regenerate it with
  `cargo run --bin acowork-keygen -- --type developer --output-dir agents/.signing-keys`
  rather than committing it.
- Signing keys are generated per machine by `acowork-keygen`. The developer key type
  signs for local development; the platform key type is reserved for official builds.
- The signature covers the whole ZIP content (local files + central directory + end of
  directory), so there is no unsigned region.
- Agent upgrade safety: the new package's signature certificate fingerprint must match
  the installed version, otherwise the upgrade is rejected.

### Manifest rules

- `manifest.toml` is the package contract. See
  [`docs/design/zh/02-agent-package.md`](docs/design/zh/02-agent-package.md) for the full
  field list.
- A package must declare **all** the permissions it needs. The runtime enforces the
  declaration, and the user can refuse a permission at install time.
- Network access must be declared explicitly per domain (e.g.
  `network:https://api.openai.com`). Default is deny.
- Do **not** obfuscate, do not embed binary blobs, and do not fetch and execute remote
  code at runtime. Declare a tool under `[[tools]]` instead.

---

## Review process

1. **Open an issue first** for anything larger than a small fix. This is not
   bureaucracy — the design corpus is large, and an issue is the cheapest way to find out
   that the change you are about to write was already decided, in [`docs/adr/zh/`](docs/adr/zh/).
2. **Branch off `master`** with a descriptive branch name (`fix/<slug>`,
   `feat/<slug>`, `docs/<slug>`).
3. **Make the change** and run the full gate (`./dev/ci.sh all`).
4. **Open the PR.** In the description, state:
   - what problem the change solves,
   - how you verified it (which test, which gate, which manual step),
   - which ADR or design doc it implements or touches,
   - the ADR bilingual pairing status (Chinese updated **and** English updated).
5. **Review.** A maintainer checks, in this order:
   - **Does it pass the gate?** Red lines, clippy, tests.
   - **Does it respect the boundaries?** Gateway / Runtime / Node / user-domain split;
     memory ownership; Desktop-as-presentation.
   - **Is the metadata honest?** Does the commit message say what the change actually
     does? Do the declared permissions match what the code does?
   - **Is it documented?** Does the ADR / design doc move with the change?
   - **Is it useful?** Does it do something the existing behaviour does not already do?
6. **Merge.** Maintainers merge. Squash or rebase is fine — keep the commit subject
   readable after squashing.

### What gets declined

- A change that relaxes a red line without an ADR that explains why the boundary moved.
- A change that adds a Gateway-side filesystem read or a user-domain path reference.
- A change that mixes business logic into the Desktop app or the React layer.
- A change that ships `acowork-eval` in a release path.
- A Chinese ADR change without the English counterpart.
- A commit that does not pass `./dev/ci.sh all` locally.

---

## License and contribution terms

ACowork is licensed under **Apache-2.0** — see [`LICENSE`](LICENSE).

By contributing, you agree that your contributions are licensed under the same
Apache-2.0 terms, and you confirm you have the right to submit them.

Contributions that are not your own original work must come with a clear attribution and
a license that is compatible with Apache-2.0. If you are contributing on behalf of an
employer, make sure you have permission to do so.

Third-party dependencies keep their own licenses. The workspace pins them in
`core/Cargo.toml`; `cargo deny` / `cargo-about` output, when regenerated, belongs under
`core/` and is updated with the dependency change, not separately.

---

## Related documents

| Document | What it covers |
|---|---|
| [`AGENTS.md`](AGENTS.md) | Architecture, conventions, and the rules that generate review comments |
| [`SECURITY.md`](SECURITY.md) | Reporting a vulnerability, and the security model |
| [`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md) | Expected behaviour in project spaces |
| [`docs/adr/zh/`](docs/adr/zh/) | Architecture decision records (Chinese — source of truth) |
| [`docs/adr/en/`](docs/adr/en/) | The English ADR counterparts |
| [`docs/design/zh/`](docs/design/zh/) | Design docs |
| [`docs/runbooks/`](docs/runbooks/) | Operator runbooks |
| [`dev/README.md`](dev/README.md) | Build, packaging, CI and deployment scripts |
