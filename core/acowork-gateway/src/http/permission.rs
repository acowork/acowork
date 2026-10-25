//! ADR-087 — HTTP control-plane authorization layer (Gateway reverse-proxy
//! entry, single enforcement point).
//!
//! Runs *inside* `auth_middleware` (so an [`AuthContext`] exists whenever
//! `AUTH_MODE=multi_user`) and gates every route that can change a Node's
//! machine or an Agent instance's configuration:
//!
//! - **Node-manage** — install/ensure/clone target, fs browse, rename,
//!   drain/remove: node owner ∨ guest ∨ admin.
//! - **Agent-manage** — lifecycle, config, workspaces (reads included —
//!   read and write are the same tier, ADR-087 D4/D9a), files, debug.
//! - **Agent-use** — the session control plane: manage ∨ (`Shared` agent).
//! - **Transfer** — guests list / visibility: owner ∨ admin (guests out).
//!
//! Design invariants (do not relax without updating the ADR):
//!
//! 1. **Default-deny.** Any `/api/agents/{id}/**` route not explicitly
//!    registered in [`classify`] is gated as `AgentManage`. A new route
//!    that forgets to register cannot leak — it locks non-owners out
//!    (fail-closed), which the route-registration invariant test
//!    ([`route_table_covers_every_registered_route`]) turns red in CI.
//! 2. **Local mode is a no-op** (ADR-087 D8): the middleware returns
//!    early, the ownership tables are still written but never evaluated.
//! 3. **Single truth.** The booleans rendered into
//!    `GET /api/agents|nodes` come from the same
//!    [`ownership::can_manage`] / [`ownership::can_use`] functions this
//!    middleware enforces — the 403 and the rendered `can_manage=false`
//!    cannot disagree.
//! 4. Machine actors (Node tokens, internal service tokens) carry no
//!    `AuthContext`; they pass through. The threat model is the
//!    *authenticated low-privilege user* (ADR-087 §2.2); machine callers
//!    are already restricted by their own credential checks per route.

use axum::{
    extract::{Request, State},
    http::{Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};
use serde_json::json;

use crate::gateway::ownership::{self, OwnerRecord, Visibility};
use crate::http::acl::{self, Op};
use crate::http::auth_middleware::AuthContext;
use crate::http::routes::AppState;

/// Does this node-scoped remainder address one of the LSP proxies?
///
/// `lsp/{*tail}` (relay passthrough) and `lsp-admin/{*tail}` (registry
/// writes) both carry their own gate; matched on the *first* segment so
/// a node id containing a hyphen can never be confused with them.
fn is_lsp_proxy_rest(rest: &str) -> bool {
    matches!(rest.split('/').next(), Some("lsp") | Some("lsp-admin"))
}

/// Query/segment extraction outcome for the resource a request targets.
#[derive(Debug, PartialEq, Eq)]
pub enum Target<'a> {
    /// No ownership-gated resource involved.
    None,
    Agent(&'a str),
    /// Node addressed by id (path segment or `?target=`). The Gateway's
    /// own machine is `Node("")` — ownerless by ADR-087 D2.4 → admin-only.
    Node(&'a str),
}

/// Which resource a request path addresses. Pure function of the path;
/// unit-tested exhaustively.
pub fn extract_target<'a>(method: &Method, path: &'a str) -> Target<'a> {
    let mut segs = path.trim_start_matches('/').splitn(4, '/').peekable();
    // Everything lives under /api/...
    if segs.next() != Some("api") {
        return Target::None;
    }
    match segs.next() {
        Some("agents") => match segs.next() {
            // POST /api/agents/install|ensure — target node comes from the
            // request BODY, invisible to a path-only classifier; those
            // handlers call [`check_node_manage`] after parsing the form.
            Some("install") | Some("ensure") => Target::None,
            // GET /api/agents — list, filtered server-side (handler).
            None => Target::None,
            Some(id) => {
                // The raw `{id}` path variable (instance or package id);
                // resolved to the canonical instance key by the middleware
                // via [`AppState`] before the ownership lookup.
                let rest = segs.next().unwrap_or("");
                // `classify` only ever yields agent-scoped tiers, so the
                // routing decision is binary: an agent path or not.
                match classify(method, rest).1 {
                    Tier::Agent | Tier::Attribution | Tier::Admin | Tier::Claim => Target::Agent(id),
                }
            }
        },
        Some("nodes") => match segs.next() {
            // GET /api/nodes — list (field-trimmed, handler).
            // POST /api/nodes/enrollment-tokens — login-only (ADR-087 D2).
            None | Some("enrollment-tokens") => Target::None,
            Some(id) => {
                let rest = segs.next().unwrap_or("");
                match (method.as_str(), rest) {
                    ("PATCH", "visibility" | "guests" | "owner") => Target::Node(id),
                    ("PATCH", "") => Target::Node(id), // rename
                    ("POST", "claim") => Target::Node(id),
                    // Permission dialog read side — manage tier (a guest
                    // may see the list; attribution writes are gated
                    // separately at owner ∨ admin inside the handler).
                    ("GET", "permissions") => Target::Node(id),
                    // ADR-089 D4 / ADR-091 §5.7: the LSP relay passthrough
                    // and the LSP registry admin proxy each evaluate the
                    // ADR-087 predicates *inline* (read → `can_view`, write
                    // → `can_manage`) because they must also decide the
                    // 404-vs-403 shape and the relay-readiness gate. A second
                    // generic gate here would disagree with the handler — for
                    // a guest on a private node this one would allow the read
                    // the handler deliberately tiers, and a manage-only arm
                    // would 403 a `public` node's reader. Handled = the
                    // proxy is the single enforcement point.
                    (_, rest) if is_lsp_proxy_rest(rest) => Target::None,
                    _ => Target::None,
                }
            }
        },
        Some("fs") if segs.next() == Some("browse") => Target::Node(""), // id from ?target=
        _ => Target::None,
    }
}

/// The permission a route declares: a capability tier plus which
/// business entity it acts on. This is the *only* thing a route
/// specifies — the cascade, the 404-vs-403 choice and the whole
/// decision live in [`crate::http::acl`].
///
/// Default-deny: anything not explicitly `Use` / `View` / `None` is
/// `Manage` (ADR-087 §8 mitigation). A route that forgets to declare
/// lands locked, and
/// [`tests::every_registered_route_declares_a_permission`] turns red.
pub fn classify(method: &Method, rest: &str) -> (Op, Tier) {
    let head = rest.split('/').next().unwrap_or("");
    // Ownership transfer — admin only (ADR-087 D7: transfer is
    // an authorization change; the owner may share but never hand over
    // mastership without an audit trail).
    if method == Method::PATCH && head == "owner" {
        return (Op::Manage, Tier::Admin);
    }
    // Claim of an ownerless resource — handler-enforced, not a tier
    // gate (see [`Tier::Claim`]).
    if method == Method::POST && head == "claim" {
        return (Op::Manage, Tier::Claim);
    }
    // Attribution ops (owner ∨ admin only, guests excluded — ADR-087 D9
    // R1: manage grants do not carry attribution rights).
    if method == Method::PATCH && matches!(head, "visibility" | "guests") {
        return (Op::Manage, Tier::Attribution);
    }
    // Debug console — manage (a high-privilege surface; ADR-087).
    if head == "debug" {
        return (Op::Manage, Tier::Agent);
    }
    // Agent lifecycle. `stop` / `upgrade` / `clone` / `publish` mutate or
    // replace the running agent -> manage. `start` is deliberately `use`:
    // a guest who meets a stopped agent should wake it rather than hunt an
    // owner, and starting is non-destructive.
    if matches!(head, "stop" | "upgrade" | "clone" | "publish") {
        return (Op::Manage, Tier::Agent);
    }
    if head == "start" {
        return (Op::Use, Tier::Agent);
    }
    // Bare id: the detail read is visibility-filtered (ADR-087 D4: shared ->
    // every logged-in user); the only other verb registered there is
    // `DELETE` = uninstall -> manage. Private/ownerless reads answer 404 via
    // the ACL's Q5 fallback.
    if rest.is_empty() {
        return if matches!(method, &Method::GET | &Method::HEAD) {
            (Op::View, Tier::Agent)
        } else {
            (Op::Manage, Tier::Agent)
        };
    }
    // The permission roster is public (ADR-087 review: knowing *who* to ask
    // for access is not a secret). Only its writes -- visibility / guests /
    // owner, handled above -- are manage.
    if head == "permissions" {
        return if matches!(method, &Method::GET | &Method::HEAD) {
            (Op::View, Tier::Agent)
        } else {
            (Op::Manage, Tier::Attribution)
        };
    }
    // Memory: reading the graph / nodes / stats is retrieval -- `use`, not
    // `view`, because it surfaces what the agent remembers of *every* past
    // conversation and has no per-session private/public wall behind it.
    // Distill / rebuild-embeddings / node CRUD rewrite that knowledge ->
    // manage.
    if head == "memory" {
        return if matches!(method, &Method::GET | &Method::HEAD) {
            (Op::Use, Tier::Agent)
        } else {
            (Op::Manage, Tier::Agent)
        };
    }
    // Retrieval that runs a query over the agent's whole corpus -- `use`,
    // never plain `view`: someone who can only *see* the agent should not be
    // searching everything it knows. `rag/status` is a liveness read.
    if head == "search" {
        return (Op::Use, Tier::Agent);
    }
    // LSP relay endpoint resolution -- only meaningful while *driving* the
    // agent's code tools, so `use` (it also names a machine-local sidecar).
    if head == "lsp-endpoint" {
        return (Op::Use, Tier::Agent);
    }
    if head == "rag" {
        return if matches!(method, &Method::GET | &Method::HEAD) {
            (Op::View, Tier::Agent)
        } else {
            (Op::Use, Tier::Agent)
        };
    }
    // Session control plane (ADR-087 matrix row 174).
    //   reads  -> `view` -- whose content is then gated by the session's own
    //                      private/public flag, the *second* wall (Runtime
    //                      `is_readable_by`); a private session is absent to
    //                      a viewer.
    //   DELETE -> `manage` -- destroys the session and its files.
    //   every other write -> `use` -- the chat tier. `can_use` = owner vee
    //                      guest vee admin; `shared` visibility widens `view`
    //                      only, never `use`. Cross-session safety is the
    //                      Runtime's per-session `is_writable_by`, not this
    //                      gate: a guest may only drive their OWN session.
    if matches!(head, "sessions" | "latest-session" | "messages") {
        if matches!(method, &Method::GET | &Method::HEAD) {
            return (Op::View, Tier::Agent);
        }
        return if method == Method::DELETE {
            (Op::Manage, Tier::Agent)
        } else {
            (Op::Use, Tier::Agent)
        };
    }
    // Filesystem / git *work* — these read and mutate the owner's actual
    // working files, so BOTH directions sit at `use` (ADR-087 §7 方案 G:
    // exposing workspace file *content* at `view` would let any viewer
    // drag the owner's files off the machine). A guest edits files and
    // reverts as ordinary work; a pure viewer cannot.
    if matches!(head, "files" | "git") {
        return (Op::Use, Tier::Agent);
    }
    // Workspace *definition* is config (`manage` to create/delete/switch);
    // reading the tree / file content is working-data, so `use` (same 方案 G
    // rationale as files/git).
    if head == "workspaces" {
        return if matches!(method, &Method::GET | &Method::HEAD) {
            (Op::Use, Tier::Agent)
        } else {
            (Op::Manage, Tier::Agent)
        };
    }
    // `POST /interactions` stamps the caller's "last touched" marker and
    // fires from the chat-send path -- `use`, so a guest talking to the agent
    // is not silently denied the default-agent feature.
    if head == "interactions" {
        return (Op::Use, Tier::Agent);
    }
    // Liveness / presence reads: needed by a shared agent's chat UI, leak
    // nothing about the machine.
    if matches!(method, &Method::GET | &Method::HEAD) && matches!(head, "status" | "health") {
        return (Op::View, Tier::Agent);
    }
    // Agent-*defining* configuration -- prompts / skills / model / providers
    // / mcp / tools / risk rules / avatar / cron / workspace / manifest.
    // Changing these rewrites what the agent *is*, so writes are `manage`;
    // merely reading them is ordinary `view`.
    if matches!(
        head,
        "config" | "prompts" | "skills" | "model" | "providers" | "mcp-servers"
            | "mcp-ensure"
            | "mcp-tools" | "builtin-tools" | "tools" | "shell-risk-rules" | "avatar"
            | "avatar-config" | "avatar-file" | "cron" | "manifest"
    ) {
        return if matches!(method, &Method::GET | &Method::HEAD) {
            (Op::View, Tier::Agent)
        } else {
            (Op::Manage, Tier::Agent)
        };
    }
    // Per-model token statistics (proxied to Runtime's session metadata).
    // Confirmed default-deny per ADR-087 §8: aggregate usage can surface
    // provider/model configuration, so it stays in the manage bucket until
    // an owner decision widens it. Kept explicit so the red-line sync in
    // `dev/ci.sh` documents the decision rather than the fall-through.
    if head == "token-stats" {
        return (Op::Manage, Tier::Agent);
    }
    // Default -- fail-closed: an UNREGISTERED route (any method) lands in
    // `manage`, so a new endpoint that forgets to declare itself cannot
    // silently widen to `view`/`use`. Every real read route is matched by
    // an explicit branch above. `dev/ci.sh::run_permission_route_redline`
    // keeps the two in sync. (ADR-087 §8.)
    (Op::Manage, Tier::Agent)
}

/// Which ownership relation the route needs on top of its [`Op`].
///
/// [`crate::http::acl::Op`] carries the capability; this carries the
/// one thing `can_view` / `can_use` / `can_manage` do not express on
/// their own — that a *guest* may manage but may never re-attribute,
/// and that only an admin may hand over ownership. Both are ADR-087
/// D9 R1 / D7.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// The ordinary capability tier: the route's [`Op`] (View/Use/Manage)
    /// decides the exact gate via `acl::ceiling` — View -> `can_view`,
    /// Use -> `can_use` (owner ∨ guest ∨ admin), Manage -> `can_manage`
    /// (owner ∨ admin, guest excluded). No attribution right.
    Agent,
    /// Owner ∨ admin — the guest list and the visibility switch.
    Attribution,
    /// Admin only — ownership transfer and claim.
    Admin,
    /// Claim of an ownerless resource (ADR-087 D7, revised): NOT a
    /// capability tier — `manage` cannot be evaluated against an
    /// ownerless row (it would be admin-only and lock out the local-node
    /// claim flow). The middleware passes the request through and the
    /// handler enforces: ownerless-only (409 otherwise) and
    /// admin ∨ caller-on-local-machine (403 otherwise).
    Claim,
}

/// The 403/404 body (ADR-087 D5.3). `required` names the tier, not the
/// relationship — a rejected guest is "not on the list", not "not the
/// owner".
fn forbidden(resource: &'static str, required: &'static str) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "error": "forbidden",
            "code": "not_authorized",
            "resource": resource,
            "required": required,
        })),
    )
        .into_response()
}

/// Middleware entry — assembled in `http/routes.rs` right after the auth
/// gate. No-op under `AUTH_MODE=local` (ADR-087 D8).
pub async fn permission_middleware(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    if !state.auth_mode.is_multi_user() {
        return next.run(req).await;
    }
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        // Machine actor (Node token / internal service token) or a path
        // the auth gate already answered. Ownership is a *user* concept;
        // pass through — per-route credential checks still apply.
        return next.run(req).await;
    };
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();

    match route_requirement(&state, &method, &path, &query).await {
        None => next.run(req).await,
        Some((op, tier, resource)) => {
            // ADR-087 D9 R1 / D7: the two attribution relations sit
            // *above* the capability tiers, because they are about who
            // the resource belongs to rather than what may be done to
            // it. A guest holds `manage` and still may not edit the
            // guest list.
            if tier == Tier::Admin {
                return if ctx.is_admin() {
                    next.run(req).await
                } else {
                    forbidden(resource.kind(), "admin")
                };
            }
            if tier == Tier::Attribution {
                let rec = fetch_record(&state, &resource);
                return if ownership::can_transfer(rec.as_ref(), &ctx.user_id, ctx.is_admin()) {
                    next.run(req).await
                } else {
                    forbidden(resource.kind(), "transfer")
                };
            }
            if tier == Tier::Claim {
                // Pass through: claim is enforced in the handler (it
                // needs the resource's live owner + hosting node, and
                // answers 409 — not 403 — for an already-owned row).
                return next.run(req).await;
            }
            match acl::decide(&state, Some(&ctx), op, &resource) {
                Ok(()) => next.run(req).await,
                Err(deny) => acl::render(deny),
            }
        }
    }
}

/// The ownership row for a resource, cloned out of the store guard so
/// no lock is ever held across an `await`.
fn fetch_record(state: &AppState, resource: &acl::Resource) -> Option<OwnerRecord> {
    let store = match resource {
        acl::Resource::Node(_) => state.node_owners.as_ref()?,
        acl::Resource::Agent(_) | acl::Resource::Session(_) => state.agent_owners.as_ref()?,
    };
    let id = match resource {
        acl::Resource::Node(None) => return None,
        acl::Resource::Node(Some(id)) => id.as_str(),
        acl::Resource::Agent(id) | acl::Resource::Session(id) => id.as_str(),
    };
    ownership::lock(store).get(id).cloned()
}

/// Resolve a request to the permission it declares, or `None` when the
/// path is not ownership-gated at all.
///
/// This is the only place that maps a URL to a permission — the
/// declaration itself is [`classify`], and the decision is
/// [`acl::evaluate`]. Nothing here evaluates policy, which is what
/// keeps "what does this route need" and "is it allowed" from
/// drifting into two implementations.
async fn route_requirement(
    state: &AppState,
    method: &Method,
    path: &str,
    query: &str,
) -> Option<(Op, Tier, acl::Resource)> {
    match extract_target(method, path) {
        Target::None => None,
        Target::Agent(raw_id) => {
            let (op, tier) = classify(method, agent_rest(path));
            // Resolve the route variable to the canonical instance key
            // (ADR-073). Unknown instance → `Resource::Agent` on the
            // raw id, which resolves to no ownership row and is
            // therefore ownerless (fail-closed); the handler produces
            // its own 404 with a useful message.
            let instance_id = {
                let gw = state.gateway_state.read().await;
                gw.resolve_installed_key(raw_id)
            };
            Some((
                op,
                tier,
                acl::Resource::Agent(instance_id.unwrap_or_else(|| raw_id.to_string())),
            ))
        }
        Target::Node(node_id) => {
            // `/api/fs/browse` addresses the node via `?target=`; empty /
            // "local" = the Gateway's own machine → ownerless (D2.4).
            let resolved = if node_id.is_empty() {
                match query_target(query) {
                    Some(t) if !t.is_empty() && t != "local" => Some(t.to_string()),
                    _ => None,
                }
            } else {
                Some(node_id.to_string())
            };
            let tier = if path.ends_with("/owner") {
                // ADR-087 D7: mastership transfer — admin only.
                Tier::Admin
            } else if path.ends_with("/claim") {
                // ADR-087 D7 (revised): ownerless claim — handler-enforced.
                Tier::Claim
            } else if path.ends_with("/visibility") || path.ends_with("/guests") {
                Tier::Attribution
            } else {
                Tier::Agent
            };
            // A node's permission roster is public, mirroring the agent
            // rule (knowing *who* to ask is not a secret). Every other
            // node verb is a machine mutation -> `Manage` (ADR-087 D4).
            let op = if matches!(method, &Method::GET | &Method::HEAD)
                && path.ends_with("/permissions")
            {
                Op::View
            } else {
                Op::Manage
            };
            Some((op, tier, acl::Resource::Node(resolved)))
        }
    }
}

/// The `{rest}` after `/api/agents/{id}/`.
fn agent_rest(path: &str) -> &str {
    let mut it = path.trim_start_matches('/').splitn(4, '/');
    let _ = it.next(); // "api" is guaranteed by extract_target callers
    let _ = it.next();
    let _ = it.next();
    it.next().unwrap_or("")
}

/// Parse `target=` out of a raw query string (no dep needed — the only
/// consumer is `/api/fs/browse`).
pub(crate) fn query_target(query: &str) -> Option<&str> {
    query.split('&').find_map(|kv| kv.strip_prefix("target="))
}

// ── Handler-side helpers (body-addressed resources) ──────────────────

/// ADR-087 D4: installing onto / uninstalling from / cloning-on a node
/// requires the **node** manage list. The target node id lives in the
/// request body (multipart form), invisible to the path classifier, so
/// `install` / `ensure` / `clone` / `uninstall` handlers call this after
/// parsing. `ctx = None` (Local mode / machine actor) → no-op.
pub fn check_node_manage(
    state: &AppState,
    ctx: Option<&AuthContext>,
    node_id: &str,
) -> Result<(), crate::http::routes::ApiError> {
    use crate::http::routes::ApiError;
    // Delegate rather than re-deriving `can_manage` here: a second copy of
    // the node rule is a second answer to one question, and it is the copy
    // that drifts. Same [`acl::decide`] the middleware uses — only the
    // rendering differs, because this returns through `Result<_, ApiError>`
    // rather than as a middleware `Response`.
    match acl::decide(
        state,
        ctx,
        Op::Manage,
        &acl::Resource::Node(Some(node_id.to_string())),
    ) {
        Ok(()) => Ok(()),
        Err(deny) => Err(ApiError::forbidden(&format!(
            "not authorized: {} requires {}",
            deny.resource, deny.required
        ))),
    }
}

// ADR-087 D4: `uninstall` is Node-manage ∧ Agent-manage. The agent half
// is enforced by the middleware (`DELETE /api/agents/{id}` →
// `classify(_,"")` → AgentManage); handlers add the node half with
// `check_node_manage`.

/// Record the installer as owner (ADR-087 D3). Called after a successful
/// install/ensure/clone dispatch. Local mode (no `AuthContext`) writes
/// `owner = None` — the table still records existence for a future
/// multi_user switch (D8), and ownerless is admin-only (fail-closed).
/// The default agent pre-installed by onboarding lands as `Shared`
/// (ADR-087 D3 exception); every other install is `Private`.
/// Build the owner row a fresh instance gets (ADR-087 D2/D3): caller as
/// owner, plus the default-shared exception for the onboarding agent.
///
/// `node_id` is denormalized onto the row so the ACL cascade can walk
/// `agent → node` without joining the MQTT retained inventory
/// (ADR-087 §4). Every caller — install / ensure / clone — already
/// holds the target node when it dispatches, so this is free at write
/// time and unavailable everywhere else.
fn build_owner_record(
    ctx: Option<&AuthContext>,
    package_id: &str,
    node_id: Option<&str>,
) -> OwnerRecord {
    let mut rec = OwnerRecord::new(ctx.map(|c| c.user_id.clone()))
        .with_node(node_id.filter(|n| !n.is_empty()).map(str::to_string));
    if package_id == DEFAULT_SHARED_AGENT_PACKAGE {
        rec.visibility = Visibility::Shared;
    }
    rec
}

/// Synchronously commit an owner row for an instance that already exists
/// (clone path — the node answered with the landed instance id).
/// ensure/clone idempotency: never overwrite an existing owner (D3).
pub(crate) fn record_agent_owner(
    state: &AppState,
    ctx: Option<&AuthContext>,
    instance_id: &str,
    package_id: &str,
    node_id: Option<&str>,
) {
    let Some(store) = state.agent_owners.as_ref() else {
        return;
    };
    let rec = build_owner_record(ctx, package_id, node_id);
    let mut guard = store.lock().unwrap_or_else(|e| e.into_inner());
    guard.put_if_absent(instance_id, rec);
}

/// ADR-087 D7 / review M4: an async install (ensure / install dispatch)
/// does NOT get its row at dispatch time — the row is staged as pending
/// and committed by the MQTT dispatch when the instance's retained
/// inventory first lands, or dropped when the Node reports the operation
/// failed. A failed install therefore leaves no orphan row.
pub(crate) fn stage_agent_owner(
    state: &AppState,
    ctx: Option<&AuthContext>,
    instance_id: &str,
    package_id: &str,
    node_id: Option<&str>,
) {
    let Some(store) = state.pending_installs.as_ref() else {
        // Feature wiring absent (tests / ownership disabled) — fall back
        // to the synchronous write so behavior stays fail-safe.
        record_agent_owner(state, ctx, instance_id, package_id, node_id);
        return;
    };
    let rec = build_owner_record(ctx, package_id, node_id);
    ownership::register_pending(store, instance_id, rec);
}

/// ADR-087 D3: the onboarding default agent is the one package that is
/// shared by default — it is the platform's first assistant and every
/// user must be able to chat with it.
pub const DEFAULT_SHARED_AGENT_PACKAGE: &str = "com.acowork.senior-engineer";

/// Handler-side `can_manage` for a list/detail row: `ctx = None`
/// (Local mode / machine actor) → true (ADR-087 D8: single-user data is
/// fully manageable without auth).
pub(crate) fn caller_can_manage(ctx: Option<&AuthContext>, rec: Option<&OwnerRecord>) -> bool {
    match ctx {
        Some(c) => ownership::can_manage(rec, &c.user_id, c.is_admin()),
        None => true,
    }
}

/// Handler-side `can_use` — same Local-mode rule.
pub(crate) fn caller_can_use(ctx: Option<&AuthContext>, rec: Option<&OwnerRecord>) -> bool {
    match ctx {
        Some(c) => ownership::can_use(rec, &c.user_id, c.is_admin()),
        None => true,
    }
}

/// Drop the ownership row when the instance disappears (ADR-087 D7 —
/// uninstall / retained-inventory clear).
pub(crate) fn remove_agent_owner(state: &AppState, instance_id: &str) {
    let Some(store) = state.agent_owners.as_ref() else {
        return;
    };
    let mut guard = store.lock().unwrap_or_else(|e| e.into_inner());
    guard.remove(instance_id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Method;

    #[test]
    fn classify_default_denies_unknown_writes() {
        // A new route that forgets to register lands in AgentManage —
        // fail-closed (ADR-087 §8).
        for (method, rest) in [
            (Method::POST, "brand-new-widget"),
            (Method::PUT, "brand-new-widget"),
            (Method::GET, "brand-new-widget"),
            (Method::DELETE, ""),
        ] {
            assert_eq!(
                classify(&method, rest),
                (Op::Manage, Tier::Agent),
                "{method} {rest} must default-deny"
            );
        }
    }

    /// Shorthand for the common `(capability, ordinary-manage)` pair.
    const fn agent(op: Op) -> (Op, Tier) {
        (op, Tier::Agent)
    }

    #[test]
    fn classify_tiers() {
        // ADR-087 D9 R1 / D7: the two attribution relations sit above the
        // capability tiers — a guest holds manage and still may not
        // re-attribute, and only an admin hands over mastership.
        assert_eq!(
            classify(&Method::PATCH, "visibility"),
            (Op::Manage, Tier::Attribution)
        );
        assert_eq!(
            classify(&Method::PATCH, "guests"),
            (Op::Manage, Tier::Attribution)
        );
        assert_eq!(classify(&Method::PATCH, "owner"), (Op::Manage, Tier::Admin));

        // Read-only surfaces: view. Readable by anyone the agent is
        // visible to.
        assert_eq!(classify(&Method::GET, "status"), agent(Op::View));
        assert_eq!(classify(&Method::GET, "avatar"), agent(Op::View));
        // Regression: `avatar-file` is the route the Desktop fetches bytes
        // from, and it is a *sibling* of `avatar` — listing only the
        // latter left it in the default-deny branch, so a shared agent's
        // packaged avatar 403'd for every non-owner. Paired with the
        // mutating twin staying manage below.
        assert_eq!(classify(&Method::GET, "avatar-file"), agent(Op::View));
        assert_eq!(classify(&Method::DELETE, "avatar-file"), agent(Op::Manage));
        assert_eq!(classify(&Method::PUT, "avatar-config"), agent(Op::Manage));
        assert_eq!(classify(&Method::GET, ""), agent(Op::View));

        // Session control plane: reads view, DELETE manage, **every other
        // write use**. `shared` means "anyone logged in may open their own
        // session and chat" (ADR-087 D6/Q1), so create/messages/config/
        // workspace must be reachable at `use` — cross-session safety is
        // the Runtime's per-session `is_writable_by`, the second wall.
        // Only `DELETE` (destroys the session + its files) demands manage.
        for (method, rest) in [
            (Method::GET, "sessions"),
            (Method::GET, "latest-session"),
            (Method::GET, "messages"),
        ] {
            assert_eq!(classify(&method, rest), agent(Op::View), "GET {rest}");
        }
        for (method, rest) in [
            (Method::POST, "sessions"),
            (Method::POST, "sessions/s1/messages"),
            (Method::PUT, "sessions/s1/config"),
            (Method::PUT, "sessions/s1/workspace"),
            (Method::POST, "sessions/s1/open"),
            (Method::POST, "sessions/s1/compress"),
        ] {
            assert_eq!(classify(&method, rest), agent(Op::Use), "{method} {rest}");
        }
        assert_eq!(classify(&Method::DELETE, "sessions/s1"), agent(Op::Manage));
        // Agent-wide activity marker fires from the chat-send path, so it
        // is `use` — a guest talking to the agent must be able to stamp it.
        assert_eq!(classify(&Method::POST, "interactions"), agent(Op::Use));

        // Memory retrieval is `use` (surfaces cross-conversation content,
        // so not plain `view`); memory mutation is `manage`.
        assert_eq!(classify(&Method::GET, "memory/nodes"), agent(Op::Use));
        assert_eq!(classify(&Method::POST, "memory/distill"), agent(Op::Manage));
        // Global search runs a query over the corpus — `use`.
        assert_eq!(classify(&Method::GET, "search"), agent(Op::Use));
        // The permission roster is public — `view`.
        assert_eq!(classify(&Method::GET, "permissions"), agent(Op::View));

        // Agent-defining config: reads are `view`, writes are `manage`.
        assert_eq!(classify(&Method::GET, "config"), agent(Op::View));
        assert_eq!(classify(&Method::PUT, "config"), agent(Op::Manage));
        assert_eq!(classify(&Method::GET, "model"), agent(Op::View));
        assert_eq!(classify(&Method::PUT, "model"), agent(Op::Manage));
        assert_eq!(classify(&Method::GET, "prompts"), agent(Op::View));
        assert_eq!(classify(&Method::PUT, "prompts"), agent(Op::Manage));

        // Filesystem / git *work* reads and writes both sit at `use` —
        // exposing workspace file content at `view` is the 方案 G theft
        // vector, so a pure viewer cannot read the owner's files.
        assert_eq!(classify(&Method::GET, "files"), agent(Op::Use));
        assert_eq!(classify(&Method::POST, "files"), agent(Op::Use));
        assert_eq!(classify(&Method::POST, "git/revert"), agent(Op::Use));
        assert_eq!(classify(&Method::GET, "git/diff"), agent(Op::Use));
        // Workspace *definition* write is `manage`; reading the tree is
        // working-data -> `use`.
        assert_eq!(classify(&Method::GET, "workspaces/file"), agent(Op::Use));
        assert_eq!(classify(&Method::PUT, "workspaces/current"), agent(Op::Manage));

        // Lifecycle: `start` is `use` (wake a stopped agent), the rest
        // that mutate/replace the agent are `manage`.
        assert_eq!(classify(&Method::POST, "start"), agent(Op::Use));
        assert_eq!(classify(&Method::POST, "stop"), agent(Op::Manage));
        assert_eq!(classify(&Method::POST, "clone"), agent(Op::Manage));
        assert_eq!(classify(&Method::POST, "debug/enable"), agent(Op::Manage));
    }

    #[test]
    fn extract_target_paths() {
        assert_eq!(extract_target(&Method::GET, "/api/agents"), Target::None);
        assert_eq!(
            extract_target(&Method::POST, "/api/agents/install"),
            Target::None
        );
        assert_eq!(
            extract_target(&Method::POST, "/api/agents/ensure"),
            Target::None
        );
        assert_eq!(
            extract_target(&Method::POST, "/api/agents/i-1/workspaces"),
            Target::Agent("i-1")
        );
        assert_eq!(
            extract_target(&Method::POST, "/api/agents/i-1/sessions"),
            Target::Agent("i-1")
        );
        assert_eq!(
            extract_target(&Method::DELETE, "/api/agents/i-1"),
            Target::Agent("i-1")
        );
        assert_eq!(
            extract_target(&Method::GET, "/api/nodes"),
            Target::None
        );
        assert_eq!(
            extract_target(&Method::POST, "/api/nodes/enrollment-tokens"),
            Target::None
        );
        assert_eq!(
            extract_target(&Method::PATCH, "/api/nodes/n-1"),
            Target::Node("n-1")
        );
        assert_eq!(
            extract_target(&Method::PATCH, "/api/nodes/n-1/guests"),
            Target::Node("n-1")
        );
        // The LSP proxies gate themselves — the middleware must stay out of
        // the way, on reads *and* writes (ADR-089 D4, ADR-091 §5.7).
        assert_eq!(
            extract_target(&Method::GET, "/api/nodes/n-1/lsp/api/lsp/status"),
            Target::None
        );
        assert_eq!(
            extract_target(&Method::GET, "/api/nodes/n-1/lsp-admin/servers"),
            Target::None
        );
        assert_eq!(
            extract_target(&Method::DELETE, "/api/nodes/n-1/lsp-admin/servers/rust"),
            Target::None
        );
        // A node route whose first segment merely starts with those letters
        // stays on the normal (currently ungated) node path.
        assert!(!is_lsp_proxy_rest("lsps-state"));
        assert!(!is_lsp_proxy_rest(""));
        assert!(is_lsp_proxy_rest("lsp"));
        assert!(is_lsp_proxy_rest("lsp/api/lsp/status"));
        assert!(is_lsp_proxy_rest("lsp-admin/servers"));
        // fs browse carries the node id in the query, not the path.
        assert_eq!(
            extract_target(&Method::GET, "/api/fs/browse"),
            Target::Node("")
        );
        assert_eq!(extract_target(&Method::GET, "/health"), Target::None);
        assert_eq!(
            extract_target(&Method::GET, "/api/vault/keys"),
            Target::None
        );
    }

    #[test]
    fn query_target_parses() {
        assert_eq!(query_target("path=C%3A%5C&target=node-uuid"), Some("node-uuid"));
        assert_eq!(query_target("path=x"), None);
        assert_eq!(query_target(""), None);
    }

    #[test]
    fn agent_rest_extraction() {
        assert_eq!(agent_rest("/api/agents/i-1/workspaces/file"), "workspaces/file");
        assert_eq!(agent_rest("/api/agents/i-1"), "");
    }

    // ── Middleware-level integration (ADR-087 §9.2–§9.5) ─────────────
    //
    // Router-level tests: the real `permission_middleware` in front of a
    // stub handler, with a test-only layer that injects the AuthContext
    // the auth gate would have produced. This exercises the full
    // classify → resolve → tier-decide path, not just the pure functions.


    mod middleware {
        use super::*;
        use acowork_core::account::Role;
        use axum::body::Body;
        use axum::http::Request;
        use axum::middleware::{from_fn, from_fn_with_state, Next};
        use axum::response::IntoResponse;
        use axum::routing::{any, patch};
        use axum::Router;
        use std::sync::Arc;
        use tokio::sync::RwLock;
        use tower::ServiceExt;

        /// Stub downstream: 200 "reached" — the middleware either lets
        /// the request through or answers before this runs.
        async fn reached() -> impl IntoResponse {
            (StatusCode::OK, "reached")
        }

        fn base_state(auth_mode: crate::auth::AuthMode) -> AppState {
            let dir = std::env::temp_dir().join(format!(
                "acowork-perm-e2e-{}",
                uuid::Uuid::new_v4()
            ));
            let _ = std::fs::create_dir_all(&dir);
            let gw = crate::gateway::state::GatewayState::new(&dir.to_string_lossy());
            let mut state = AppState::new(
                Arc::new(RwLock::new(gw)),
                Arc::new(crate::http::auth::HttpAuth::new(false)),
            );
            state.auth_mode = auth_mode;
            state.agent_owners = Some(ownership::new_shared_agent_owners(&dir));
            state.node_owners = Some(ownership::new_shared_node_owners(&dir));
            state
        }

        /// Register `instance` in the install table (the gate resolves
        /// route ids through it, ADR-073).
        async fn seed_installed(state: &AppState, instance: &str) {
            use crate::gateway::state::AgentInfo;
            let manifest = acowork_core::AgentManifest {
                agent_id: "com.example.x".to_string(),
                version: "1.0.0".to_string(),
                name: "X".to_string(),
                display_name: None,
                role: None,
                avatar: None,
                builtin_avatar: None,
                description: String::new(),
                author: "t".to_string(),
                runtime_version: "0.1.0".to_string(),
                permissions: vec![],
                triggers: vec![],
                llm: Default::default(),
                memory: Default::default(),
                identity_deps: vec![],
                tools: vec![],
                capabilities: Default::default(),
                resources: Default::default(),
                sandbox: Default::default(),
                dev: false,
                skills: Default::default(),
            };
            let mut gw = state.gateway_state.write().await;
            gw.installed_agents.insert(
                instance.to_string(),
                AgentInfo {
                    instance_id: instance.to_string(),
                    agent_id: "com.example.x".to_string(),
                    version: "1.0.0".to_string(),
                    name: "X".to_string(),
                    install_path: "/tmp/x".to_string(),
                    manifest,
                    node_id: "local".to_string(),
                },
            );
        }

        fn user(id: &str, admin: bool) -> AuthContext {
            AuthContext {
                user_id: id.to_string(),
                role: if admin { Role::Admin } else { Role::User },
                as_user: None,
            }
        }

        fn owned(owner: &str, vis: Visibility) -> OwnerRecord {
            let mut r = OwnerRecord::new(Some(owner.to_string()));
            r.visibility = vis;
            r
        }

        /// Full stack: caller injection (outermost — stands in for the
        /// auth gate), the REAL permission middleware, stub routes
        /// mirroring the production surface.
        async fn app(caller: Option<AuthContext>, instance: &str, rec: Option<OwnerRecord>) -> Router {
            let state = base_state(crate::auth::AuthMode::MultiUser);
            seed_installed(&state, instance).await;
            if let Some(r) = rec {
                let store = state.agent_owners.clone().unwrap();
                ownership::lock(&store).put(instance, r);
            }
            let router = Router::new()
                .route("/api/agents/{id}", any(reached))
                .route("/api/agents/{id}/{*rest}", any(reached))
                .route("/api/nodes/{id}/owner", patch(reached))
                .route("/api/fs/browse", any(reached))
                .layer(from_fn_with_state(state.clone(), permission_middleware));
            let inject = move |req: Request<Body>, next: Next| {
                let caller = caller.clone();
                async move {
                    let mut req = req;
                    if let Some(ctx) = caller {
                        req.extensions_mut().insert(ctx);
                    }
                    next.run(req).await
                }
            };
            router.layer(from_fn(inject)).with_state(state)
        }

        async fn get(app: Router, method: &str, uri: &str) -> StatusCode {
            app.oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
        }

        // §9.2 — claim full flow: admin may PATCH owner on a resource
        // with no record (D7 claim); a plain user may not.
        #[tokio::test]
        async fn claim_owner_is_admin_only() {
            assert_eq!(
                get(app(Some(user("admin", true)), "i-1", None).await, "PATCH", "/api/agents/i-1/owner").await,
                StatusCode::OK,
                "admin claim passes the gate"
            );
            assert_eq!(
                get(app(Some(user("mallory", false)), "i-1", None).await, "PATCH", "/api/agents/i-1/owner").await,
                StatusCode::FORBIDDEN,
                "non-admin claim denied"
            );
        }

        // §9.3 — shared agent: a non-owner logged-in user may only VIEW
        // it — detail, status, avatar, and the (user-scoped) session list
        // read. `shared` publishes visibility, nothing more (ADR-087 D6).
        // Using it — opening a session, sending messages, setting that
        // session's config / workspace — requires being a guest (the use
        // tier). Managing it — deleting a session, reading the agent-wide
        // workspace / file tree — requires owner ∨ admin.
        #[tokio::test]
        async fn shared_agent_non_guest_may_view_but_not_use() {
            let rec = Some(owned("alice", Visibility::Shared));
            assert_eq!(
                get(app(Some(user("bob", false)), "i-1", rec.clone()).await, "GET", "/api/agents/i-1").await,
                StatusCode::OK,
                "shared detail read"
            );
            // View: reads pass — agent metadata / config *definition* reads.
            for (method, uri) in [
                ("GET", "/api/agents/i-1/sessions"),
                ("GET", "/api/agents/i-1/status"),
                ("GET", "/api/agents/i-1/avatar-file"),
                ("GET", "/api/agents/i-1/config"),
                ("GET", "/api/agents/i-1/model"),
                ("GET", "/api/agents/i-1/permissions"),
            ] {
                assert_eq!(
                    get(app(Some(user("bob", false)), "i-1", rec.clone()).await, method, uri).await,
                    StatusCode::OK,
                    "shared agent is visible to a logged-in non-guest: {method} {uri}"
                );
            }
            // Use: a non-guest may NOT drive the agent (shared ≠ use) —
            // includes workspace/file content reads (方案 G: not theftable
            // by a viewer) and memory/search retrieval.
            for (method, uri) in [
                ("POST", "/api/agents/i-1/sessions"),
                ("POST", "/api/agents/i-1/sessions/s1/messages"),
                ("PUT", "/api/agents/i-1/sessions/s1/config"),
                ("PUT", "/api/agents/i-1/sessions/s1/workspace"),
                ("GET", "/api/agents/i-1/memory/nodes"),
                ("GET", "/api/agents/i-1/search"),
                ("GET", "/api/agents/i-1/workspaces"),
                ("GET", "/api/agents/i-1/files"),
            ] {
                assert_eq!(
                    get(app(Some(user("bob", false)), "i-1", rec.clone()).await, method, uri).await,
                    StatusCode::FORBIDDEN,
                    "shared does not grant use to a non-guest: {method} {uri}"
                );
            }
            // Manage: session delete + config write + lifecycle stay denied.
            for (method, uri) in [
                ("DELETE", "/api/agents/i-1/sessions/s1"),
                ("PUT", "/api/agents/i-1/config"),
                ("POST", "/api/agents/i-1/stop"),
            ] {
                assert_eq!(
                    get(app(Some(user("bob", false)), "i-1", rec.clone()).await, method, uri).await,
                    StatusCode::FORBIDDEN,
                    "shared does not grant manage: {method} {uri}"
                );
            }
        }

        // A *guest* holds the use tier: they may open a session, send
        // messages, set that session's config / workspace, start the agent,
        // and retrieve memory / search. But guest is NOT manage — deleting
        // a session (destroys its files) and writing agent config stay
        // denied.
        #[tokio::test]
        async fn shared_agent_guest_may_use_but_not_manage() {
            let mut rec = owned("alice", Visibility::Shared);
            rec.guests.push("bob".to_string());
            for (method, uri) in [
                ("POST", "/api/agents/i-1/sessions"),
                ("POST", "/api/agents/i-1/sessions/s1/messages"),
                ("PUT", "/api/agents/i-1/sessions/s1/config"),
                ("PUT", "/api/agents/i-1/sessions/s1/workspace"),
                ("POST", "/api/agents/i-1/start"),
                ("GET", "/api/agents/i-1/memory/nodes"),
                ("GET", "/api/agents/i-1/search"),
            ] {
                assert_eq!(
                    get(app(Some(user("bob", false)), "i-1", Some(rec.clone())).await, method, uri).await,
                    StatusCode::OK,
                    "guest may {method} {uri}"
                );
            }
            // Manage-tier routes stay closed to a guest.
            for (method, uri) in [
                ("DELETE", "/api/agents/i-1/sessions/s1"),
                ("PUT", "/api/agents/i-1/config"),
                ("POST", "/api/agents/i-1/stop"),
            ] {
                assert_eq!(
                    get(app(Some(user("bob", false)), "i-1", Some(rec.clone())).await, method, uri).await,
                    StatusCode::FORBIDDEN,
                    "guest is use-tier, not manage: {method} {uri}"
                );
            }
        }

        // §9.4 — private agent: non-owner detail read answers 404 (Q5,
        // no existence leak); a manage write answers 403.
        #[tokio::test]
        async fn private_agent_read_404_write_403() {
            let rec = Some(owned("alice", Visibility::Private));
            assert_eq!(
                get(app(Some(user("bob", false)), "i-1", rec.clone()).await, "GET", "/api/agents/i-1").await,
                StatusCode::NOT_FOUND,
                "private detail does not leak existence"
            );
            assert_eq!(
                get(app(Some(user("bob", false)), "i-1", rec).await, "POST", "/api/agents/i-1/workspaces").await,
                StatusCode::FORBIDDEN,
                "private manage write denied"
            );
        }

        // §9.5 — Local mode is a no-op: the same private-agent request
        // that 404s in MultiUser passes untouched in Local.
        #[tokio::test]
        async fn local_mode_noop() {
            let state = base_state(crate::auth::AuthMode::Local);
            seed_installed(&state, "i-1").await;
            let store = state.agent_owners.clone().unwrap();
            ownership::lock(&store).put("i-1", owned("alice", Visibility::Private));
            let router = Router::new()
                .route("/api/agents/{id}", any(reached))
                .route("/api/agents/{id}/{*rest}", any(reached))
                .layer(from_fn_with_state(state.clone(), permission_middleware))
                .with_state(state);
            assert_eq!(
                get(router, "GET", "/api/agents/i-1").await,
                StatusCode::OK,
                "Local mode: middleware is no-op"
            );
        }

        // Node side of the claim gate: PATCH /api/nodes/{id}/owner is
        // admin-only (I4) — plain user 403, admin passes.
        #[tokio::test]
        async fn node_owner_patch_admin_only() {
            assert_eq!(
                get(app(Some(user("mallory", false)), "i-1", None).await, "PATCH", "/api/nodes/n-1/owner").await,
                StatusCode::FORBIDDEN,
                "plain user cannot touch node owner"
            );
            assert_eq!(
                get(app(Some(user("admin", true)), "i-1", None).await, "PATCH", "/api/nodes/n-1/owner").await,
                StatusCode::OK,
                "admin may claim node owner"
            );
        }

        // fs browse against a node the caller does not manage → 403;
        // the node owner passes.
        #[tokio::test]
        async fn fs_browse_gated_by_node_manage() {
            assert_eq!(
                get(app(Some(user("mallory", false)), "i-1", None).await, "GET", "/api/fs/browse?target=n-1").await,
                StatusCode::FORBIDDEN,
                "no manage relation -> 403"
            );
            let state = base_state(crate::auth::AuthMode::MultiUser);
            seed_installed(&state, "i-1").await;
            let store = state.node_owners.clone().unwrap();
            ownership::lock(&store).put("n-1", owned("alice", Visibility::Private));
            let router = Router::new()
                .route("/api/fs/browse", any(reached))
                .layer(from_fn_with_state(state.clone(), permission_middleware));
            let inject = |req: Request<Body>, next: Next| async move {
                let mut req = req;
                req.extensions_mut().insert(user("alice", false));
                next.run(req).await
            };
            let router = router.layer(from_fn(inject)).with_state(state);
            assert_eq!(
                get(router, "GET", "/api/fs/browse?target=n-1").await,
                StatusCode::OK,
                "node owner may browse"
            );
        }
    }
}
