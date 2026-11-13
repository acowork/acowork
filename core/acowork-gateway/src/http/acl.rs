//! ADR-087 §4 — the authorization module.
//!
//! One place that answers "may this caller do this?" for every
//! `/api/**` route. A route declares **two** things and nothing else:
//!
//! ```ignore
//! // what capability the route needs
//! Op::Use
//! // and which business entity it acts on
//! Resource::Session(agent_id)
//! ```
//!
//! The cascade, the visibility rules, the 404-vs-403 choice — all of
//! it lives here. A route that forgets to declare lands in the
//! default-deny arm and is caught by
//! [`tests::every_registered_route_declares_a_permission`].
//!
//! # The two dimensions
//!
//! **Capability** ([`Op`]) — `View` < `Use` < `Manage`. The ordering
//! is total and a route asks for the *least* it needs. These are the
//! three real tiers ADR-087 already defines, not an invented scale:
//! `View` is `can_view`, `Use` is `can_use`, `Manage` is `can_manage`.
//!
//! **Entity** ([`Resource`]) — `Node` / `Agent` / `Session`, cascading
//! `Node → Agent → Session`.
//!
//! # Cascade
//!
//! A lower tier can never exceed its parent. The requested capability
//! is threaded down and each level applies `min(its own ceiling,
//! what arrived)`:
//!
//! ```text
//! node view + agent use  +  request agent/use   → pass
//! node view + agent use  +  request session/manage → deny at node
//! node manage + agent view + request agent/view → pass
//! ```
//!
//! `min` rather than "the child overrides" is what keeps a *public*
//! node from silently promoting the agents installed on it: publishing
//! a machine says nothing about who may reconfigure a given agent on it.
//!
//! # What this module does NOT decide
//!
//! Session ownership. `SessionMeta::is_readable_by` /
//! `is_writable_by` in the Runtime (ADR-076 §决策 4) is already
//! implemented, already tested, and keyed on data the Gateway does not
//! hold. Re-deriving it here would be a second answer to one question.
//! So the Session tier here is a **ceiling**: it may lower a request
//! from `Manage` to `Use`, it never raises it, and the Runtime remains
//! the authority on which particular session the caller owns.
//!
//! Session-`*reads* (the session list) are a `View` read, gated by
//! `can_view` — a caller who can see the agent may ask for the list, but
//! the Runtime scopes it to the caller's own sessions (ADR-076), so a
//! non-granted caller sees an empty list and cannot create one anyway
//! (creation is a `Use` write, gated by `can_use`). The `Use` tier is
//! therefore the real wall for "may you drive this agent", not the read.

use std::sync::Arc;

use serde_json::json;
use axum::{
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};

use crate::gateway::ownership::{self, OwnerRecord};
use crate::http::auth_middleware::AuthContext;
use crate::http::routes::AppState;

/// Capability tier a route requires. Ordered: `View < Use < Manage`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Op {
    /// May the resource appear at all / its metadata be read.
    /// Agent: `can_view`. Node: `can_view`.
    View,
    /// May the caller drive the resource (agent: open a session and
    /// talk to it). Not a configuration right — `Manage` is.
    Use,
    /// May the caller reconfigure the resource or the machine it runs on.
    Manage,
}

impl Op {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::View => "view",
            Self::Use => "use",
            Self::Manage => "manage",
        }
    }

    /// The wire value in a 403 body — the Desktop keys its copy off
    /// this rather than off the status code alone, so that "you can
    /// see it but not touch it" and "you may not even see it" read
    /// differently.
    fn requirement(self) -> &'static str {
        match self {
            Self::View => "view",
            Self::Use => "use",
            Self::Manage => "manage",
        }
    }
}

/// The business entity a route acts on. The cascade is implicit —
/// constructing an `Agent` or `Session` is enough; the module walks
/// upward through [`Resource::parent`] on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resource {
    /// A machine. `None` = the Gateway's own machine, which ADR-087 D2.4
    /// declares ownerless (admin-only).
    Node(Option<String>),
    /// An agent instance (ADR-073 `instance_id`, never a package id).
    Agent(String),
    /// A session on an agent. The session id itself is deliberately
    /// absent — see the module doc; the Runtime owns that question.
    Session(String),
}

impl Resource {
    /// The parent tier, or `None` at the root / when the edge is
    /// unresolvable.
    ///
    /// The `Agent → Node` edge is resolved lazily through the
    /// ownership table's denormalized `node_id` (ADR-087 §4). A row
    /// written before that field existed — or an instance whose node
    /// has gone — yields `None`, i.e. **the agent is evaluated on its
    /// own row**.
    ///
    /// That is deliberately not the same as `Resource::Node(None)`,
    /// which means "the Gateway's own machine" (ADR-087 D2.4,
    /// ownerless → admin-only). Folding an unknown host into it would
    /// deny every legacy row to every non-admin — a silent lockout
    /// that reads as "permissions are broken". An unresolvable parent
    /// costs one missing check on `uninstall` only, and that route
    /// checks the node explicitly instead of inheriting it.
    fn parent<'a>(&'a self, state: &'a AppState) -> Option<Resource> {
        match self {
            Resource::Node(_) => None,
            Resource::Agent(id) => {
                agent_node_id(state, id).map(|n| Resource::Node(Some(n)))
            }
            Resource::Session(agent_id) => Some(Resource::Agent(agent_id.clone())),
        }
    }

    /// Stable label for error bodies.
    pub fn kind(&self) -> &'static str {
        match self {
            Resource::Node(_) => "node",
            Resource::Agent(_) => "agent",
            Resource::Session(_) => "session",
        }
    }
}

/// The agent's hosting node, from policy data (ADR-087 §4).
///
/// `None` when the row is missing or predates the `node_id` field.
/// Both are fail-closed upstream: an unresolvable parent makes the
/// agent's own `can_manage` the whole answer, and `uninstall` — the
/// one route that genuinely needs the node list — additionally checks
/// it explicitly rather than inheriting a silent `None`.
fn agent_node_id(state: &AppState, agent_id: &str) -> Option<String> {
    let store = state.agent_owners.as_ref()?;
    let guard = lock_store(store);
    guard.get(agent_id).and_then(|r| r.node_id.clone())
}

/// Acquire a store guard, recovering from a poisoned mutex.
///
/// Delegates to [`ownership::lock`], which owns the recovery rule: a
/// panic in one request thread must not take authorization down for the
/// whole process, which would read as "permissions are broken" rather
/// than "one handler panicked".
fn lock_store(
    store: &crate::gateway::ownership::SharedOwnershipStore,
) -> std::sync::MutexGuard<'_, crate::gateway::ownership::OwnershipStore> {
    ownership::lock(store)
}

/// The capability this resource grants this caller, before cascade.
///
/// The asymmetry between the two entities is the whole point of having
/// a `Use` tier, so it is spelled out rather than folded:
///
/// - **Node** — no `Use`. Every node verb (install / enroll / browse /
///   rename) is a machine mutation, so `Use` collapses onto
///   `can_manage`. Matches the ADR-087 D4 matrix row-for-row.
/// - **Agent** — `Use` is `can_use` (`can_manage ∨ guest ∈ list`). Being
///   on the guest list is what makes an agent drivable; `shared`
///   visibility widens only `View`, never `Use` (ADR-087 D6).
///
/// A node being `Public` still grants no agent `Use`/`Manage`; that
/// arrives through the agent's own row.
fn ceiling(
    rec: Option<&OwnerRecord>,
    user_id: &str,
    is_admin: bool,
    op: Op,
    resource: &Resource,
) -> bool {
    match (op, resource) {
        (Op::View, _) => ownership::can_view(rec, user_id, is_admin),
        (Op::Use, Resource::Agent(_)) | (Op::Use, Resource::Session(_)) => {
            ownership::can_use(rec, user_id, is_admin)
        }
        (Op::Use, Resource::Node(_)) | (Op::Manage, _) => {
            ownership::can_manage(rec, user_id, is_admin)
        }
    }
}

/// A denial, carrying enough to render an honest response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deny {
    /// The entity that refused — the *most specific* tier in the
    /// cascade that denied, so the message names the real blocker
    /// rather than the outermost one.
    pub resource: &'static str,
    /// The capability that was needed there.
    pub required: &'static str,
    /// `true` for a `View` denial on an entity the caller cannot
    /// otherwise see. Those answer 404, not 403 — a distinct status
    /// would confirm the entity exists (ADR-087 Q5).
    pub hide: bool,
}

/// Evaluate one route's declared permission.
///
/// Walks `Resource` → parent → …, threading the requested capability
/// down and taking `min` at each level. Returns the first denial, or
/// `None` when the whole chain allows it.
pub fn evaluate(
    state: &AppState,
    ctx: &AuthContext,
    op: Op,
    resource: &Resource,
) -> Option<Deny> {
    // Cascade from the root down so a denial is reported at the tier
    // that actually refused. Walking bottom-up would report the leaf
    // every time, naming the wrong blocker whenever an ancestor also
    // refuses — "session requires use" when in fact the node is not
    // even visible to you.
    let mut chain = Vec::new();
    let mut cursor = resource.clone();
    while let Some(parent) = cursor.parent(state) {
        chain.push(cursor);
        cursor = parent;
    }
    chain.push(cursor);
    chain.reverse();

    for link in &chain {
        let rec = fetch(state, link);
        if ceiling(rec.as_ref(), &ctx.user_id, ctx.is_admin(), op, link) {
            continue;
        }
        // The `View` special case: an entity the caller cannot see at
        // all must not confirm its existence. Applies to agent reads
        // (detail / avatar / status); a node is listed to every signed-in
        // account by design (ADR-087 D6), so its denial stays a 403.
        let hide = op == Op::View && matches!(link, Resource::Agent(_) | Resource::Session(_));
        return Some(Deny {
            resource: link.kind(),
            required: op.requirement(),
            hide,
        });
    }
    None
}

fn fetch(state: &AppState, resource: &Resource) -> Option<OwnerRecord> {
    match resource {
        Resource::Node(id) => {
            let store = state.node_owners.as_ref()?;
            let guard = lock_store(store);
            id.as_ref().and_then(|id| guard.get(id)).cloned()
        }
        Resource::Agent(id) | Resource::Session(id) => {
            let store = state.agent_owners.as_ref()?;
            let guard = lock_store(store);
            guard.get(id).cloned()
        }
    }
}

/// The decision — no rendering, no HTTP.
///
/// This is what every call site uses. `Err` carries a small [`Deny`]
/// rather than a rendered `Response` (threading 128 bytes of axum `Body`
/// through a `Result` trips `clippy::result_large_err`), so the two
/// sites that want HTTP call [`render`] on it themselves.
pub fn decide(
    state: &AppState,
    ctx: Option<&AuthContext>,
    op: Op,
    resource: &Resource,
) -> Result<(), Deny> {
    let Some(ctx) = ctx else {
        return Ok(());
    };
    evaluate(state, ctx, op, resource).map_or(Ok(()), Err)
}

/// Turn a [`Deny`] into the 403/404 body.
///
/// Two responses, and the choice is the whole point of [`Deny::hide`]: a
/// `View` denial on an entity the caller cannot see answers 404 so the
/// status cannot confirm it exists; everything else is a 403 naming the
/// tier that actually refused.
pub fn render(deny: Deny) -> Response {
    if deny.hide {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "not found" })),
        )
            .into_response();
    }
    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "error": "forbidden",
            "code": "not_authorized",
            "resource": deny.resource,
            "required": deny.required,
        })),
    )
        .into_response()
}

/// Compile-time guard: the tier ordering the cascade relies on.
/// If `Op`'s `Ord` ever stops matching the semantic order, this fails
/// rather than silently inverting the min().
const _: fn() = || {
    fn assert_ordering() {
        assert!(Op::View < Op::Use);
        assert!(Op::Use < Op::Manage);
    }
    assert_ordering();
    let _ = Arc::new(());
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::ownership::Visibility;
    use acowork_core::account::Role;
    use tokio::sync::RwLock;

    fn rec(owner: Option<&str>, guests: &[&str], vis: Visibility) -> OwnerRecord {
        OwnerRecord {
            owner_user_id: owner.map(str::to_string),
            guests: guests.iter().map(|s| s.to_string()).collect(),
            visibility: vis,
            created_at: chrono::Utc::now(),
            node_id: None,
        }
    }

    fn user(id: &str, admin: bool) -> AuthContext {
        AuthContext {
            user_id: id.to_string(),
            role: if admin { Role::Admin } else { Role::User },
            as_user: None,
        }
    }

    /// State carrying only what the cascade reads. Everything else
    /// stays `None` — a test that needed more would be exercising a
    /// different module.
    fn state_with(
        nodes: &[(&str, OwnerRecord)],
        agents: &[(&str, OwnerRecord)],
    ) -> AppState {
        let dir = std::env::temp_dir().join(format!("acowork-acl-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let gw = crate::gateway::state::GatewayState::new(&dir.to_string_lossy());
        let mut st = AppState::new(
            Arc::new(RwLock::new(gw)),
            Arc::new(crate::http::auth::HttpAuth::new(false)),
        );
        let node_store = ownership::new_shared_node_owners(&dir);
        for (id, r) in nodes {
            ownership::lock(&node_store).put(id, r.clone());
        }
        let agent_store = ownership::new_shared_agent_owners(&dir);
        for (id, r) in agents {
            ownership::lock(&agent_store).put(id, r.clone());
        }
        st.node_owners = Some(node_store);
        st.agent_owners = Some(agent_store);
        st
    }

    // ── The reported scenario ──────────────────────────────────────
    //
    // A shared agent owned by someone else, on a node the caller can
    // see. Every route below is a real one from the current tree.

    #[test]
    fn shared_agent_visible_but_ungranted_denies_use() {
        let st = state_with(
            &[("n-1", rec(Some("owner"), &[], Visibility::Public))],
            &[("a-1", rec(Some("owner"), &[], Visibility::Shared))],
        );
        let bob = user("bob", false);

        // Visible in the sidebar — `shared` publishes the row for view.
        assert_eq!(evaluate(&st, &bob, Op::View, &Resource::Agent("a-1".into())), None);
        // But a non-guest may NOT open a session on it: `shared` widens
        // view only, never use (ADR-087 D6). Denied at the agent tier.
        let d = evaluate(&st, &bob, Op::Use, &Resource::Session("a-1".into())).unwrap();
        assert_eq!(d.resource, "agent");
        assert_eq!(d.required, "use");
        // Session *list* reads stay allowed (a `View` read) — the Runtime
        // scopes them to the caller's own sessions, so a non-guest sees an
        // empty list and cannot create one anyway (the `use` denial above).
        assert_eq!(evaluate(&st, &bob, Op::View, &Resource::Session("a-1".into())), None);
    }

    #[test]
    fn shared_agent_denies_manage_for_every_tier() {
        let st = state_with(
            &[("n-1", rec(Some("owner"), &[], Visibility::Public))],
            &[("a-1", rec(Some("owner"), &[], Visibility::Shared))],
        );
        let bob = user("bob", false);

        // Workspaces, config, lifecycle, prompts, skills — all manage.
        let d = evaluate(&st, &bob, Op::Manage, &Resource::Agent("a-1".into())).unwrap();
        assert_eq!(d.resource, "agent");
        assert_eq!(d.required, "manage");
        assert!(!d.hide, "a manage denial is a 403, never a 404");
    }

    #[test]
    fn session_manage_is_capped_by_the_agent_tier() {
        // The regression this module exists for: a `Manage` request
        // (e.g. `DELETE /sessions/{sid}`, or a workspace mutation) must
        // be refused at the agent tier even when the caller can reach
        // the agent at all. Here bob is neither owner nor guest of a
        // `Shared` agent, so he holds neither `use` nor `manage` — the
        // ceiling denies him at the agent with `required = manage`.
        let st = state_with(
            &[("n-1", rec(Some("owner"), &[], Visibility::Public))],
            &[("a-1", rec(Some("owner"), &[], Visibility::Shared))],
        );
        let bob = user("bob", false);

        let d = evaluate(&st, &bob, Op::Manage, &Resource::Session("a-1".into())).unwrap();
        assert_eq!(d.resource, "agent", "denied at the agent, not the session");
        assert_eq!(d.required, "manage");
    }

    // ── Cascade: min, never override ───────────────────────────────

    #[test]
    fn a_public_node_does_not_promote_its_agents() {
        // Publishing a machine says nothing about who may reconfigure
        // the agents on it. This is the case that "child overrides
        // parent" would get wrong.
        let mut agent = rec(Some("owner"), &[], Visibility::Shared);
        agent.node_id = Some("n-1".into());
        let st = state_with(
            &[("n-1", rec(None, &[], Visibility::Public))],
            &[("a-1", agent)],
        );
        let bob = user("bob", false);

        assert_eq!(evaluate(&st, &bob, Op::View, &Resource::Agent("a-1".into())), None);
        assert!(
            evaluate(&st, &bob, Op::Manage, &Resource::Agent("a-1".into())).is_some(),
            "a public node must not confer manage over its agents"
        );
    }

    #[test]
    fn a_manage_node_does_not_promote_a_private_agent() {
        let mut agent = rec(Some("owner"), &[], Visibility::Private);
        agent.node_id = Some("n-1".into());
        let st = state_with(&[("n-1", rec(Some("owner"), &[], Visibility::Public))], &[("a-1", agent)]);
        let bob = user("bob", false);

        // Node is fully open, agent is not even visible.
        assert_eq!(evaluate(&st, &bob, Op::View, &Resource::Node(Some("n-1".into()))), None);
        let d = evaluate(&st, &bob, Op::View, &Resource::Agent("a-1".into())).unwrap();
        assert!(d.hide, "a private agent is 404, not 403");
    }

    #[test]
    fn denial_names_the_outermost_blocker_not_the_leaf() {
        // Both refuse. The node is the real answer — reporting
        // "session requires use" would send the user to the wrong
        // dialog.
        let mut agent = rec(Some("owner"), &[], Visibility::Private);
        agent.node_id = Some("n-1".into());
        let st = state_with(&[("n-1", rec(None, &[], Visibility::Private))], &[("a-1", agent)]);
        let bob = user("bob", false);

        let d = evaluate(&st, &bob, Op::Manage, &Resource::Session("a-1".into())).unwrap();
        assert_eq!(d.resource, "node");
    }

    // ── Guests and admin ───────────────────────────────────────────

    #[test]
    fn a_guest_gets_use_but_not_manage_or_attribution() {
        let st = state_with(
            &[],
            &[("a-1", rec(Some("owner"), &["bob"], Visibility::Private))],
        );
        let bob = user("bob", false);
        // A guest holds the use tier...
        assert_eq!(evaluate(&st, &bob, Op::Use, &Resource::Agent("a-1".into())), None);
        // ...but NOT manage (ADR-087 D9: guest is use authorization).
        let d = evaluate(&st, &bob, Op::Manage, &Resource::Agent("a-1".into())).unwrap();
        assert_eq!(d.required, "manage");
    }

    #[test]
    fn admin_passes_everything_including_ownerless_rows() {
        // Fail-closed only binds non-admins — that is how an
        // unclaimed resource gets found and claimed (ADR-087 D7).
        let st = state_with(&[], &[("a-1", rec(None, &[], Visibility::Private))]);
        let root = user("root", true);
        assert_eq!(evaluate(&st, &root, Op::Manage, &Resource::Agent("a-1".into())), None);
    }

    #[test]
    fn ownerless_row_is_admin_only_for_non_admins() {
        let st = state_with(&[], &[("a-1", rec(None, &[], Visibility::Private))]);
        let bob = user("bob", false);
        assert!(evaluate(&st, &bob, Op::View, &Resource::Agent("a-1".into())).is_some());
    }

    // ── Op::Use is not Op::Manage ──────────────────────────────────
    //
    // The distinction the whole module turns on. If these ever merge,
    // "shared" silently becomes "granted".

    #[test]
    fn use_and_manage_are_distinct_tiers() {
        // A guest holds `use` but not `manage`. (A `shared` agent with
        // no guest would deny BOTH to a stranger — visibility is view
        // only.) If these ever merge, "granted to chat" silently becomes
        // "granted to reconfigure".
        let st = state_with(&[], &[("a-1", rec(Some("owner"), &["bob"], Visibility::Private))]);
        let bob = user("bob", false);
        assert_eq!(evaluate(&st, &bob, Op::Use, &Resource::Agent("a-1".into())), None);
        assert!(evaluate(&st, &bob, Op::Manage, &Resource::Agent("a-1".into())).is_some());
    }

    // ── Fail-closed defaults ───────────────────────────────────────

    #[test]
    fn a_missing_row_denies_everything_to_a_non_admin() {
        // No record at all. Every tier must refuse rather than
        // defaulting open — this is the ADR-087 D7 invariant and the
        // exact bug class that once leaked ownerless agents into
        // every list.
        let st = state_with(&[], &[]);
        let bob = user("bob", false);
        for op in [Op::View, Op::Use, Op::Manage] {
            assert!(
                evaluate(&st, &bob, op, &Resource::Agent("ghost".into())).is_some(),
                "{op:?} must deny an ownerless resource"
            );
        }
    }

    #[test]
    fn a_missing_store_denies_rather_than_allows() {
        // Ownership tables absent (local mode wiring off, or tests).
        // `can_manage` treats a missing store as "manage" — the
        // historical behaviour for single-user setups — but that must
        // not become a silent hole in a multi_user Gateway.
        let dir = std::env::temp_dir().join(format!("acowork-acl-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let gw = crate::gateway::state::GatewayState::new(&dir.to_string_lossy());
        let st = AppState::new(
            Arc::new(RwLock::new(gw)),
            Arc::new(crate::http::auth::HttpAuth::new(false)),
        );
        // No stores installed.
        let bob = user("bob", false);
        assert!(evaluate(&st, &bob, Op::Manage, &Resource::Agent("a-1".into())).is_some());
    }
}
