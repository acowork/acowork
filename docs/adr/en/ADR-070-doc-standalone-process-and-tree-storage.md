# ADR-070: acowork-doc as a Standalone Process + Tree Storage Selection

> **Chinese source of truth**: [ADR-070](../zh/ADR-070-doc-standalone-process-and-tree-storage.md)
> **Terminology**: see [GLOSSARY.md](./GLOSSARY.md)

## Status

Decided (2026-09, settled when D0–D4 implementation completed)

## Decision Makers

Architecture review (user decision: the zero-business iron rule for Gateway + the
service layer modelled on the Runtime use cases)

## Related

- [ADR-064](./ADR-064-pm-standalone-process.md) — PM as a standalone process; the direct precedent
- [ADR-061-pm](./ADR-061-pm-storage-tree.md) — PM tree storage; doc reuses its "filesystem is the truth" philosophy
- [ADR-055](./ADR-055-remote-runtime-node-topology.md) — Gateway narrows to pure networking
- [docs/design/zh/20-doc-online-document.md](../../design/zh/20-doc-online-document.md) — doc design v1.0, whose §2.2 data directory deviation this ADR records

---

## Context

ADR-064 established that business logic MUST NOT enter the Gateway. Embedding the doc
domain (tree storage, optimistic concurrency, a PR-style review flow, search) would
repeat every problem PM had:

| Problem | Explanation |
|---------|-------------|
| Business logic in the Gateway | the doc domain compiles into the Gateway binary |
| Dependency weight | chrono/uuid/serde_json/tokio/axum on an already over-weight Gateway |
| Fault isolation | a doc panic (e.g. a corrupt `library.json` during reconcile) takes down the Gateway singleton |
| Storage coupling | doc data stuffed into `{gateway.data_dir}/acowork-doc` |

doc and pm differ on their invariants, so pm cannot be reused wholesale:

| Dimension | pm | doc |
|-----------|----|-----|
| Root entity | a project (a task is always a directory) | a document = an `.md` file, the file is authoritative |
| Directory semantics | `children/` isolated subdirectories | **a directory is a folder** |
| Content carrier | `task.json` + attachment directory | the Markdown is the content; metadata lives in `library.json` |
| Concurrency | version number | version number (`base_version` validated) |
| Modification path | humans edit; agents claim → submit | humans PUT; agents `doc_write` (policy returns `pending_review` for another owner's document → PR review) |

Design §2.2/Q8 originally put the data at `{data}/acowork-doc/` nested in the Gateway data
directory. Implementation aligned it with pm at `$HOME/.acowork/acowork-doc/`,
decoupling the storage lifecycle from the Gateway (ADR-064 Goal 3).

## Goals

1. **doc is a standalone process** — its own binary, its own port (default 18081,
   auto-incrementing on conflict up to +20), supervisor-managed lifecycle
2. **doc storage is independent** — data directory `$HOME/.acowork/acowork-doc/`, a peer
   of gateway/node/pm
3. **The filesystem is the truth** — a directory is a folder, a document is an `.md` file,
   and each directory keeps a rebuildable `library.json` accelerator index
4. **The Gateway only keeps** — spawn/monitor/restart (`doc_supervisor`), reverse proxy for
   `/api/doc/*`, and MCP catalog injection of `doc_mcp_url`
5. **External contracts** — Desktop `{gw}/api/doc/*`; agents reach
   `http://{advertise_host}:{gw_http_port}/api/doc/mcp`

## Alternatives

**A — standalone process (recommended, implemented).** The `acowork-doc` crate ships its
own `main.rs` serving the full router. The Gateway spawns it, polls `/health`, and
restarts with exponential backoff; a startup failure MUST NOT block the Gateway (503 +
Retry-After). `doc_proxy` proxies `/api/doc/{rest}` to `127.0.0.1:{doc_port}/{rest}`.

**B — embedded in the Gateway (rejected).** Duplicated business logic in the gateway,
dependency bloat, fault propagation.

**C — embedded in the pm process (rejected).** Merging two domains widens the coupling
surface and forces a full restart for either change.

## Decision

### Decision 1: doc is a standalone process (Option A)

- `core/acowork-doc` produces `acowork-doc.exe/bin`; `[doc]` config
  (enabled/port/data_dir/request_ttl_hours/auto_inject_mcp/mcp_http_path) is passed as CLI
  arguments by the Gateway config.
- The port defaults to 18081, auto-increments while probing on conflict (at most +20), and
  the chosen port is written to `doc.port` for `doc_proxy`.
- Failure is **non-fatal**: `/api/doc/*` returns 503 + `Retry-After` and the Desktop shows an
  offline panel rather than a blank screen.

### Decision 2: the filesystem is the truth; directory = folder, document = `.md` file

- The physical layout is authoritative: a directory maps to a folder, a document to an
  `.md` file, and the filename minus extension is the title.
- Each `library.json` is **only an accelerator index** (doc_id to physical name, import
  source, soft-delete flag). `reconcile` rebuilds it in three passes at startup (repair
  renames, mark orphans, fill missing), so a corrupt index never loses content.
- `.trash/` (soft delete plus sidecar restore info) and `.requests/` (PR JSON) stay out of
  the tree.
- All writes are atomic replace (temp file plus rename); there is no intermediate state.

### Decision 3: data directory is `$HOME/.acowork/acowork-doc/` (corrects design §2.2)

A peer of `acowork-pm/`, **not nested inside the Gateway data directory**; overridable
via `[doc].data_dir`.

### Decision 4: version number is `u64` with optimistic concurrency

Both library writes and review merges carry a `base_version` check; a mismatch returns
409 `version_conflict` (e2e semantics: the push was rejected). This also corrects an early
D0 draft that typed the field as `i64` — versions increase monotonically and never go
negative.

### Decision 5: identity is injected uniformly by the Gateway reverse proxy

- REST `/api/doc/*`: `doc_proxy` discards any client-declared `X-Actor` and injects a trusted
  `human`, preventing forgery as `agent:xxx`.
- MCP `/api/doc/mcp`: `X-MCP-Actor` is validated against `installed_agents` and forwarded
  when trusted; otherwise stripped and the caller is treated as anonymous (read-only tools
  only: list/read/search/pull/check_request).
- The doc server MUST NOT keep its own agent allowlist — the trust decision lives at the
  single Gateway authentication point.

### Decision 6: governance is policy (not a verb), size has two ceilings, replace parks in the trash

- **Write permission is policy, not a verb** (`service/policy.rs`, design §5.1): one
  `doc_write` covers create/overwrite/append. Editing your own document (`creator == actor`)
  or acting as an admin applies directly; editing someone else's document, or a document
  with no recorded creator, returns `pending_review` + `request_id`. The old verb triple
  (`doc_add`/`doc_submit_update`/`doc_pull`) encoded governance into the name, making
  "create, but reviewed" and "overwrite, without review" inexpressible.
- **Structural operations are ownership-gated** (`move` / `remove`): a review request models
  a *content proposal* and cannot express delete/rename, so they do not enter the review
  flow. A document with no recorded creator is deliberately **not** refused on ownership —
  it falls through to the node ACL, because with no review channel an ownership refusal
  would make the document permanently unreachable (a deadlock, not protection).
- **Two size ceilings** (`limit.rs`): `transport_ceiling` (wire JSON, worst-case escape
  factor) and `ensure_document_size` (decoded body, including the **resulting** size of an
  `append`). Checking only the former rejects a legal document whose escaping inflates it;
  checking only the latter allows unbounded memory. Every 413 answers with the
  `payload_too_large` JSON envelope (a custom `DocJson` extractor, so axum's plain-text
  rejection never leaks).
- **Replace parks the old body in the trash**: the overwrite branch of `move_doc` records the
  displaced document in `.trash/` and keeps its body for rollback. Deleting the destination
  first (the old MCP implementation) left a failed move in the state "destination deleted,
  source unmoved".

## Consequences

**Upside**

- The Gateway stays business-free; the doc domain and binary are fully isolated.
- Stable contracts for Desktop and agents, unchanged as internals shift.
- Storage is plain files, so backup is a directory copy and crash recovery is restart plus `reconcile`.
- The identity-forgery surface narrows to the `doc_proxy` injection point (13 unit tests).

**Downside**

- One more resident process (~10 MB); doc has a first reconcile delay linear in library size.
- REST and MCP must keep their semantics consistent (e2e D3-5/D4-3 keep verifying this).
- Each `library.json` must be refreshed after a structural change, else `reconcile` is the
  fallback (acceptable: the service has a single writer).

**Rollback**

- `[doc].enabled=false` disables doc entirely (503, other services unaffected).
- Change `[doc].data_dir` and restart to switch; the original directory is kept and can be
  copied back.
