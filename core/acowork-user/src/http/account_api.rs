//! Account management HTTP API (ADR-076 §决策 5 / §决策 6).
//!
//! ```text
//! GET    /api/users                        → { accounts, version }   (admin)
//! POST   /api/users                        → 201 { account, invite_token? }
//! GET    /api/users/{id}                   → AccountView (admin or self)
//! PUT    /api/users/{id}                   → AccountView (admin or self)
//! DELETE /api/users/{id}                   → 204 (self, or admin)
//! POST   /api/users/{id}/disable           → 204 (admin)
//! POST   /api/users/{id}/reset-password    → { invite_token } (admin)
//! ```
//!
//! Registered **only under `AUTH_MODE=multi_user`** (ADR-076 §决策 12),
//! *instead of* the local-mode presentation routes
//! ([`crate::http::profile_api`]) for the shared `/api/users` paths. Under
//! `local` this module never runs and `accounts.json` is never created.
//!
//! Two authorities are kept coherent here: `accounts.json` (the credential
//! authority, written by [`AuthService`]) and `user_profiles.json` (the
//! derived public view Runtime consumes as `last_user_profile`). Every
//! mutation writes the former and re-derives the latter.

use axum::{
    Extension, Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use acowork_core::account::{AccountView, Role};
use acowork_core::protocol::{UserProfile, UserProfileListFile};

use crate::auth::service::{AuthError, AuthService, ProfilePatch};
use crate::auth::issuer::now_unix;
use crate::auth::context::AuthContext;
use crate::error::ApiError;
use crate::state::AppState;

/// The account-management routes (merged only under `multi_user`).
pub fn account_routes() -> Router<AppState> {
    Router::new()
        // Static segment, so it wins over `/api/users/{user_id}` (matchit 0.8
        // prefers a literal over a parameter) — `/api/users/directory` is the
        // contact picker, not a lookup of an account named "directory".
        .route("/api/users/directory", get(list_directory))
        .route("/api/users", get(list_accounts).post(create_account))
        .route(
            "/api/users/{user_id}",
            get(get_account).put(update_account).delete(delete_account),
        )
        .route("/api/users/{user_id}/disable", post(disable_account))
        .route("/api/users/{user_id}/reset-password", post(reset_password))
}

// ── Request / response types ───────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateAccountRequest {
    pub username: String,
    #[serde(default)]
    pub display_name: String,
    /// Absent → the account is created inactive and an `invite_token` is
    /// returned for first-login (ADR-076 §决策 6).
    #[serde(default)]
    pub password: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct UpdateAccountRequest {
    /// Display fields (ADR-076 §决策 1). They live on `UserAccount`, so
    /// they are edited through the credential authority — writing them
    /// anywhere else would be clobbered by the next `sync_profiles`.
    #[serde(flatten)]
    pub profile: ProfilePatch,
    #[serde(default)]
    pub role: Option<Role>,
}

#[derive(Debug, Serialize)]
pub struct AccountListResponse {
    pub accounts: Vec<AccountView>,
    pub version: u64,
}

#[derive(Debug, Serialize)]
pub struct CreateAccountResponse {
    pub account: AccountView,
    /// Present only when no password was supplied — hand it to the owner.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invite_token: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ResetPasswordResponse {
    pub invite_token: String,
}

/// One row of the contact picker (ADR-076 §决策 8).
///
/// A deliberate projection, not `AccountView`: a non-admin gets what the
/// sidebar needs to *draw* a contact and nothing else — no role, no
/// timestamps, no profile fields.
///
/// The avatar pair is display state, not a secret, and is already part of
/// the deployment's public view: `user_profiles.json` (rebuilt from
/// `accounts.json`, consumed by Runtime as `last_user_profile`) carries it,
/// and `chat_api::ChatSummary` already returns `peer_avatar` /
/// `peer_builtin_avatar` to *any* authenticated caller (ADR-076 §决策 8).
/// Leaving them off here only forced the Desktop to draw a derived
/// placeholder in the sidebar while the inbox thread showed the real
/// avatar — same user, two different faces. Emptiness is a **filtered**
/// `Option`: an empty string is the wire contract for "user cleared this
/// field" (see `list_chats`), so it must not reach the client as `Some("")`.
#[derive(Debug, Serialize)]
pub struct DirectoryEntry {
    pub user_id: String,
    pub username: String,
    pub display_name: String,
    /// Login role (ADR-076 §决策 5) — the same value the bearer token
    /// carries, so the IM header can show "管理员" without a second lookup.
    pub role: Role,
    pub avatar: Option<String>,
    pub builtin_avatar: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct UserDirectoryResponse {
    pub users: Vec<DirectoryEntry>,
}

// ── Guards ─────────────────────────────────────────────────────────────

fn service(state: &AppState) -> Result<Arc<AuthService>, ApiError> {
    state.auth_service.clone().ok_or_else(|| {
        ApiError::service_unavailable("the account system is disabled (AUTH_MODE=local)")
    })
}

pub(crate) async fn blocking<T, F>(f: F) -> Result<T, ApiError>
where
    F: FnOnce() -> Result<T, AuthError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::internal(&format!("account task failed: {e}")))?
        .map_err(account_err)
}

/// Map an [`AuthError`] for the account-management surface.
///
/// `pub(crate)`: the mode-independent avatar routes in
/// [`crate::http::profile_api`] go through the same account store and reuse
/// this mapping (a missing account is a 404 there too).
pub(crate) fn account_err(e: AuthError) -> ApiError {
    match e {
        // A missing account is a 404 here (unlike login, where it must stay
        // indistinguishable from a bad password — see `auth_api`).
        AuthError::InvalidCredentials => ApiError::not_found("account not found"),
        AuthError::Token(t) => ApiError::unauthorized(&t.to_string()),
        AuthError::Revoked => ApiError::unauthorized("token revoked"),
        AuthError::Policy(m) => ApiError::unprocessable_entity(&m),
        AuthError::Conflict(m) => ApiError::conflict(&m),
        AuthError::Store(m) => ApiError::internal(&m),
    }
}

/// An admin may touch any account; everyone else only their own.
fn require_self_or_admin(ctx: &AuthContext, user_id: &str) -> Result<(), ApiError> {
    if ctx.is_admin() || ctx.user_id == user_id {
        Ok(())
    } else {
        Err(ApiError::forbidden(
            "administrator token required to manage another account",
        ))
    }
}

fn require_admin(ctx: &AuthContext) -> Result<(), ApiError> {
    if ctx.is_admin() {
        Ok(())
    } else {
        Err(ApiError::forbidden("administrator token required"))
    }
}

// ── Handlers ───────────────────────────────────────────────────────────

/// `GET /api/users` — list every account (admin only).
async fn list_accounts(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
) -> Result<Json<AccountListResponse>, ApiError> {
    require_admin(&ctx)?;
    let auth = service(&state)?;
    let list = auth.load_accounts().map_err(|e| ApiError::internal(&e))?;
    Ok(Json(AccountListResponse {
        accounts: list.accounts.iter().map(AccountView::from).collect(),
        version: list.version,
    }))
}

/// `GET /api/users/directory` — the contact picker's source, for **any**
/// authenticated caller (ADR-076 §决策 8).
///
/// This is the one account listing that is not admin-only, and the exception
/// is deliberate:
///
/// - Display fields are already the deployment's public view —
///   `user_profiles.json` is rebuilt from `accounts.json` and consumed by
///   Runtime as `last_user_profile` (ADR-076 §决策 1), so a name is not a
///   secret here.
/// - A chat feature with no way to name a recipient is unusable for everyone
///   who is not an admin, which is everyone it is for.
///
/// The exposure is bounded instead: enabled accounts only (a disabled account
/// cannot log in, so there is nobody to contact — its *history* still lists
/// it, which is `chat_api`'s job), never the caller themselves, and only
/// `user_id` / `username` / `display_name` plus the two avatar fields (see
/// [`DirectoryEntry`] for why those two are not a new exposure).
async fn list_directory(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
) -> Result<Json<UserDirectoryResponse>, ApiError> {
    let auth = service(&state)?;
    let list = auth.load_accounts().map_err(|e| ApiError::internal(&e))?;
    let users = list
        .accounts
        .iter()
        .filter(|a| a.disabled_at.is_none() && a.user_id != ctx.user_id)
        .map(|a| DirectoryEntry {
            user_id: a.user_id.clone(),
            username: a.username.clone(),
            display_name: a.display_name.clone(),
            role: a.role,
            avatar: a.avatar.as_deref().filter(|s| !s.is_empty()).map(str::to_string),
            builtin_avatar: a
                .builtin_avatar
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        })
        .collect();
    Ok(Json(UserDirectoryResponse { users }))
}

/// `POST /api/users` — create an account.
///
/// Admin-only unless `[multi_user].registration_open` is set, in which case
/// any authenticated caller may create an ordinary `User` account — never
/// an admin. (Anonymous signup would need `/api/users` on the middleware
/// whitelist; that is the ADR's `allow_public_signup` "demo-only" mode,
/// which has no config field yet and is deliberately not wired.)
async fn create_account(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Json(req): Json<CreateAccountRequest>,
) -> Result<(StatusCode, Json<CreateAccountResponse>), ApiError> {
    let auth = service(&state)?;
    if !ctx.is_admin() && !registration_open(&state).await {
        return Err(ApiError::forbidden(
            "only an administrator may create accounts (registration is closed)",
        ));
    }

    let username = req.username.clone();
    let display_name = req.display_name.clone();
    let password = req.password.clone();
    let (account, invite) = blocking(move || {
        auth.create_account(
            &username,
            &display_name,
            password.as_deref(),
            Role::User,
            now_unix(),
        )
    })
    .await?;

    sync_profiles(&state).await;

    Ok((
        StatusCode::CREATED,
        Json(CreateAccountResponse {
            account: AccountView::from(&account),
            invite_token: invite,
        }),
    ))
}

/// `GET /api/users/{id}` — one account's redacted record.
async fn get_account(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(user_id): Path<String>,
) -> Result<Json<AccountView>, ApiError> {
    require_self_or_admin(&ctx, &user_id)?;
    let auth = service(&state)?;
    let account = auth.account(&user_id).map_err(account_err)?;
    Ok(Json(AccountView::from(&account)))
}

/// `PUT /api/users/{id}` — edit display fields and (admin only) role.
async fn update_account(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(user_id): Path<String>,
    Json(req): Json<UpdateAccountRequest>,
) -> Result<Json<AccountView>, ApiError> {
    require_self_or_admin(&ctx, &user_id)?;
    // Role changes are a privilege boundary — admin only, even on self
    // (a user must not promote themselves).
    if req.role.is_some() {
        require_admin(&ctx)?;
    }
    let auth = service(&state)?;

    let uid = user_id.clone();
    let patch = req.profile;
    let auth2 = auth.clone();
    let mut account = blocking(move || auth.update_account(&uid, patch, now_unix())).await?;
    if let Some(role) = req.role {
        let uid = user_id.clone();
        account = blocking(move || auth2.set_role(&uid, role, now_unix())).await?;
    }

    sync_profiles(&state).await;
    Ok(Json(AccountView::from(&account)))
}

/// `DELETE /api/users/{id}` — soft-delete (self, or admin on anyone).
async fn delete_account(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(user_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_self_or_admin(&ctx, &user_id)?;
    let auth = service(&state)?;
    let uid = user_id.clone();
    blocking(move || auth.disable_account(&uid, now_unix())).await?;
    sync_profiles(&state).await;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/users/{id}/disable` — admin soft-delete of another account.
async fn disable_account(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(user_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_admin(&ctx)?;
    let auth = service(&state)?;
    let uid = user_id.clone();
    blocking(move || auth.disable_account(&uid, now_unix())).await?;
    sync_profiles(&state).await;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/users/{id}/reset-password` — mint a one-time invite (admin).
async fn reset_password(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(user_id): Path<String>,
) -> Result<Json<ResetPasswordResponse>, ApiError> {
    require_admin(&ctx)?;
    let auth = service(&state)?;
    let uid = user_id.clone();
    let invite_token = blocking(move || auth.reset_password(&uid, now_unix())).await?;
    sync_profiles(&state).await;
    Ok(Json(ResetPasswordResponse { invite_token }))
}

// ── Derived-view sync ──────────────────────────────────────────────────

/// Whether self-registration is enabled (`[multi_user].registration_open`).
async fn registration_open(state: &AppState) -> bool {
    state.shared.read().await.registration_open
}

/// Re-derive `user_profiles.json` from `accounts.json` (ADR-076 §决策 2:
/// the public view is *derived*). Runtime consumes the active profile as
/// `last_user_profile`, so an account mutation must not leave it stale.
///
/// `is_active` picks the most recently logged-in account (the natural
/// multi-user equivalent of "the active user"), falling back to the first
/// enabled admin so a freshly bootstrapped Gateway still pushes a profile.
///
/// ponytail: a single global `is_active` is a local-mode holdover — under
/// `multi_user` every session carries its own owner via `x-user-id`, so the
/// global active profile only feeds the legacy `last_user_profile` topic.
/// When Runtime learns per-request profiles this whole sync goes away.
/// Re-derive `user_profiles.json` from `accounts.json` (the authority).
///
/// `pub(crate)` because the mode-independent avatar routes in
/// [`crate::http::profile_api`] write through `accounts.json` too and must
/// refresh the same derived view.
pub(crate) async fn sync_profiles(state: &AppState) {
    let Some(auth) = state.auth_service.clone() else {
        return;
    };
    let Ok(accounts) = auth.load_accounts() else {
        return;
    };

    let active_id = accounts
        .accounts
        .iter()
        .filter(|a| a.disabled_at.is_none())
        .max_by(|a, b| a.last_login_at.cmp(&b.last_login_at))
        .or_else(|| {
            accounts
                .accounts
                .iter()
                .find(|a| a.role == Role::Admin && a.disabled_at.is_none())
        })
        .map(|a| a.user_id.clone());

    let users: Vec<UserProfile> = accounts
        .accounts
        .iter()
        .filter(|a| a.disabled_at.is_none())
        .map(|a| a.to_public_profile(active_id.as_deref() == Some(a.user_id.as_str())))
        .collect();

    {
        let mut gw = state.shared.write().await;
        gw.resource_cache.user_profile_list = UserProfileListFile {
            version: accounts.version,
            users,
        };
        crate::profiles::rebuild_and_save_user_profile_cache(&mut gw);
    }

    // ADR-084 M3: profile-change signal to the Gateway goes here.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::build_router;
    use axum::body::Body;
    use axum::http::Request;
    use std::path::{Path, PathBuf};
    use tower::ServiceExt;

    const PWD: &str = "s3cret123";

    fn temp_dir() -> PathBuf {
        crate::test_support::temp_dir("accountapi")
    }

    /// A `multi_user` state whose store holds one admin (`root`).
    fn admin_state(dir: &Path) -> AppState {
        crate::test_support::admin_state(dir)
    }

    fn req(method: &str, uri: &str, body: Option<&str>, token: Option<&str>) -> Request<Body> {
        let mut b = Request::builder().method(method).uri(uri);
        if body.is_some() {
            b = b.header("content-type", "application/json");
        }
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        b.body(match body {
            Some(s) => Body::from(s.to_string()),
            None => Body::empty(),
        })
        .unwrap()
    }

    async fn json(resp: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    }

    async fn login(router: &axum::Router, username: &str, password: &str) -> String {
        let resp = router
            .clone()
            .oneshot(req(
                "POST",
                "/api/auth/login",
                Some(&format!(
                    r#"{{"username":"{username}","password":"{password}"}}"#
                )),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "login as {username}");
        json(resp).await["access_token"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// Login and return `(access_token, refresh_token)`.
    async fn login_pair(router: &axum::Router, username: &str, password: &str) -> (String, String) {
        let resp = router
            .clone()
            .oneshot(req(
                "POST",
                "/api/auth/login",
                Some(&format!(
                    r#"{{"username":"{username}","password":"{password}"}}"#
                )),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "login as {username}");
        let v = json(resp).await;
        (
            v["access_token"].as_str().unwrap().to_string(),
            v["refresh_token"].as_str().unwrap().to_string(),
        )
    }

    /// The whole invite lifecycle: admin creates a passwordless account →
    /// gets an invite → first-login sets a password and logs in → the invite
    /// is burned → a second use fails. This is the Phase E contract.
    #[tokio::test]
    async fn invite_lifecycle_is_single_use() {
        let dir = temp_dir();
        let router = build_router(admin_state(&dir));
        let admin = login(&router, "root", PWD).await;

        // Create a passwordless account → 201 with an invite token.
        let resp = router
            .clone()
            .oneshot(req(
                "POST",
                "/api/users",
                Some(r#"{"username":"alice","display_name":"Alice"}"#),
                Some(&admin),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let created = json(resp).await;
        assert_eq!(created["account"]["username"], "alice");
        let invite = created["invite_token"].as_str().unwrap().to_string();

        // The invited account cannot log in yet (no password).
        let resp = router
            .clone()
            .oneshot(req(
                "POST",
                "/api/auth/login",
                Some(r#"{"username":"alice","password":"whatever1"}"#),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // First login activates it and returns a token pair.
        let resp = router
            .clone()
            .oneshot(req(
                "POST",
                "/api/auth/first-login",
                Some(&format!(
                    r#"{{"invite_token":"{invite}","new_password":"alicepass1"}}"#
                )),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(json(resp).await["access_token"].is_string());

        // The invite is single-use: replaying it fails.
        let resp = router
            .clone()
            .oneshot(req(
                "POST",
                "/api/auth/first-login",
                Some(&format!(
                    r#"{{"invite_token":"{invite}","new_password":"alicepass2"}}"#
                )),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // And the new password now works through the normal login path.
        let alice = login(&router, "alice", "alicepass1").await;
        assert!(!alice.is_empty());
    }

    /// Non-admins cannot list / create / disable / reset accounts; they can
    /// read and delete their own record.
    #[tokio::test]
    async fn non_admin_is_confined_to_self() {
        let dir = temp_dir();
        let router = build_router(admin_state(&dir));
        let admin = login(&router, "root", PWD).await;

        let created = json(
            router
                .clone()
                .oneshot(req(
                    "POST",
                    "/api/users",
                    Some(r#"{"username":"bob","password":"bobpass12"}"#),
                    Some(&admin),
                ))
                .await
                .unwrap(),
        )
        .await;
        let bob_id = created["account"]["user_id"].as_str().unwrap().to_string();
        let (bob, bob_refresh) = login_pair(&router, "bob", "bobpass12").await;

        // List is admin-only.
        let resp = router
            .clone()
            .oneshot(req("GET", "/api/users", None, Some(&bob)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Disable / reset are admin-only.
        for path in [
            format!("/api/users/{bob_id}/disable"),
            format!("/api/users/{bob_id}/reset-password"),
        ] {
            let resp = router
                .clone()
                .oneshot(req("POST", &path, None, Some(&bob)))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{path}");
        }

        // Reading *another* account is forbidden; self is fine.
        let resp = router
            .clone()
            .oneshot(req("GET", "/api/users/root", None, Some(&bob)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let resp = router
            .clone()
            .oneshot(req(
                "GET",
                &format!("/api/users/{bob_id}"),
                None,
                Some(&bob),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Self-delete (注销) succeeds, then login is refused.
        let resp = router
            .clone()
            .oneshot(req(
                "DELETE",
                &format!("/api/users/{bob_id}"),
                None,
                Some(&bob),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let resp = router
            .clone()
            .oneshot(req(
                "POST",
                "/api/auth/login",
                Some(r#"{"username":"bob","password":"bobpass12"}"#),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // And the pre-deletion refresh token is revoked, not merely
        // refused at login (ADR-076 §7.4 checklist).
        let resp = router
            .clone()
            .oneshot(req(
                "POST",
                "/api/auth/refresh",
                Some(&format!(r#"{{"refresh_token":"{bob_refresh}"}}"#)),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// Duplicate usernames are a 409, and the last admin cannot be demoted
    /// or disabled (a whole-store invariant).
    #[tokio::test]
    async fn conflicts_are_refused() {
        let dir = temp_dir();
        let router = build_router(admin_state(&dir));
        let admin = login(&router, "root", PWD).await;

        let resp = router
            .clone()
            .oneshot(req(
                "POST",
                "/api/users",
                Some(r#"{"username":"dup","password":"duppass12"}"#),
                Some(&admin),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let resp = router
            .clone()
            .oneshot(req(
                "POST",
                "/api/users",
                Some(r#"{"username":"DUP","password":"duppass12"}"#),
                Some(&admin),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);

        // The only admin cannot disable itself.
        let me = json(
            router
                .clone()
                .oneshot(req("GET", "/api/auth/me", None, Some(&admin)))
                .await
                .unwrap(),
        )
        .await;
        let root_id = me["user_id"].as_str().unwrap().to_string();
        let resp = router
            .clone()
            .oneshot(req(
                "POST",
                &format!("/api/users/{root_id}/disable"),
                None,
                Some(&admin),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    /// ADR-076 §决策 6: `[multi_user].registration_open` gates whether a
    /// non-admin may create accounts. Off (the default) → 403; on → the
    /// caller may create an ordinary `User`, never an admin.
    #[tokio::test]
    async fn registration_open_gates_non_admin_creation() {
        let dir = temp_dir();
        let state = admin_state(&dir);
        // Closed (the default): a non-admin's create is refused.
        let router = build_router(state.clone());
        let admin = login(&router, "root", PWD).await;
        let created = json(
            router
                .clone()
                .oneshot(req(
                    "POST",
                    "/api/users",
                    Some(r#"{"username":"alice","password":"alicepass1"}"#),
                    Some(&admin),
                ))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(created["account"]["role"], "user");
        let alice = login(&router, "alice", "alicepass1").await;
        let resp = router
            .clone()
            .oneshot(req(
                "POST",
                "/api/users",
                Some(r#"{"username":"carol","password":"carolpass1"}"#),
                Some(&alice),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Open: the same call now succeeds, and still cannot mint an admin
        // (role is hard-coded to `User` on this path).
        state.shared.write().await.registration_open = true;
        let resp = router
            .clone()
            .oneshot(req(
                "POST",
                "/api/users",
                Some(r#"{"username":"dave","password":"davepass1"}"#),
                Some(&alice),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let dave = json(resp).await;
        assert_eq!(dave["account"]["username"], "dave");
        assert_eq!(
            dave["account"]["role"], "user",
            "self-signup never mints admin"
        );
    }

    /// Under `local` mode `/api/users` is the presentation router — no
    /// invite semantics, and the account routes do not exist.
    #[tokio::test]
    async fn local_mode_keeps_presentation_routes() {
        let dir = temp_dir();
        let st = crate::test_support::local_state(&dir);
        let router = build_router(st);
        // The presentation GET works (no auth service → no middleware gate).
        let resp = router
            .clone()
            .oneshot(req("GET", "/api/users", None, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // The account-only reset route does not exist.
        let resp = router
            .oneshot(req("POST", "/api/users/x/reset-password", None, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// ADR-076 §决策 1: `PUT /api/users/{id}` carries the display fields
    /// (language / timezone / avatar / …) and persists them into
    /// `accounts.json` — the authority the derived view is rebuilt from.
    #[tokio::test]
    async fn update_account_persists_display_fields() {
        let dir = temp_dir();
        let router = build_router(admin_state(&dir));
        let admin = login(&router, "root", PWD).await;
        let created = json(
            router
                .clone()
                .oneshot(req(
                    "POST",
                    "/api/users",
                    Some(r#"{"username":"bob","password":"bobpass12"}"#),
                    Some(&admin),
                ))
                .await
                .unwrap(),
        )
        .await;
        let bob_id = created["account"]["user_id"].as_str().unwrap().to_string();
        let bob = login(&router, "bob", "bobpass12").await;

        let body = r#"{"display_name":"Bob B","language":"zh-CN","timezone":"Asia/Shanghai",
                       "city":"上海","avatar":"assets/avatar-01.png","custom":{"theme":"dark"}}"#;
        let resp = router
            .clone()
            .oneshot(req(
                "PUT",
                &format!("/api/users/{bob_id}"),
                Some(body),
                Some(&bob),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let view = json(resp).await;
        assert_eq!(view["language"], "zh-CN");
        assert_eq!(view["timezone"], "Asia/Shanghai");
        assert_eq!(view["avatar"], "assets/avatar-01.png");
        assert_eq!(view["custom"]["theme"], "dark");

        // The authority round-trips: a fresh read sees the same fields.
        let resp = router
            .clone()
            .oneshot(req(
                "GET",
                &format!("/api/users/{bob_id}"),
                None,
                Some(&bob),
            ))
            .await
            .unwrap();
        let view = json(resp).await;
        assert_eq!(view["language"], "zh-CN");
        assert_eq!(view["avatar"], "assets/avatar-01.png");
    }

    /// ADR-076 §5.5 regression: under `multi_user` the avatar routes write
    /// through `accounts.json` for the authenticated caller, so a later
    /// account mutation — which rebuilds `user_profiles.json` from the
    /// authority — can no longer wipe the avatar.
    #[tokio::test]
    async fn avatar_config_writes_through_accounts_under_multi_user() {
        let dir = temp_dir();
        let router = build_router(admin_state(&dir));
        let admin = login(&router, "root", PWD).await;
        let created = json(
            router
                .clone()
                .oneshot(req(
                    "POST",
                    "/api/users",
                    Some(r#"{"username":"bob","password":"bobpass12"}"#),
                    Some(&admin),
                ))
                .await
                .unwrap(),
        )
        .await;
        let bob_id = created["account"]["user_id"].as_str().unwrap().to_string();
        let bob = login(&router, "bob", "bobpass12").await;

        // Set the avatar through the mode-independent avatar route.
        let resp = router
            .clone()
            .oneshot(req(
                "PUT",
                "/api/user/avatar-config",
                Some(r#"{"avatar":"assets/avatar-01.png"}"#),
                Some(&bob),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(json(resp).await["avatar"], "assets/avatar-01.png");

        // Any other account mutation rebuilds the derived view…
        let resp = router
            .clone()
            .oneshot(req(
                "PUT",
                &format!("/api/users/{bob_id}"),
                Some(r#"{"display_name":"Bob B"}"#),
                Some(&bob),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // …and the avatar survives, because it lives in `accounts.json`.
        let resp = router
            .clone()
            .oneshot(req("GET", "/api/user/avatar-config", None, Some(&bob)))
            .await
            .unwrap();
        assert_eq!(json(resp).await["avatar"], "assets/avatar-01.png");
        let resp = router
            .oneshot(req(
                "GET",
                &format!("/api/users/{bob_id}"),
                None,
                Some(&bob),
            ))
            .await
            .unwrap();
        assert_eq!(json(resp).await["avatar"], "assets/avatar-01.png");
    }

    /// ADR-076 §5.5: avatar files are per-user under `multi_user` — nobody
    /// (admin included) may unlink a file inside another account's
    /// namespace, while the owner can and the own-field clear still runs
    /// through the authority.
    #[tokio::test]
    async fn avatar_file_deletes_are_confined_to_the_owner_namespace() {
        let dir = temp_dir();
        let router = build_router(admin_state(&dir));
        let admin = login(&router, "root", PWD).await;
        let created = json(
            router
                .clone()
                .oneshot(req(
                    "POST",
                    "/api/users",
                    Some(r#"{"username":"bob","password":"bobpass12"}"#),
                    Some(&admin),
                ))
                .await
                .unwrap(),
        )
        .await;
        let bob_id = created["account"]["user_id"].as_str().unwrap().to_string();
        let bob = login(&router, "bob", "bobpass12").await;

        // Seed one file inside bob's namespace.
        let file_rel = format!("assets/avatars/{bob_id}/avatar-01.png");
        let file_path = dir.join(&file_rel);
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        std::fs::write(&file_path, b"png").unwrap();

        // Another authenticated user (the admin here — there is no admin
        // bypass by design) cannot unlink a file in bob's namespace.
        let resp = router
            .clone()
            .oneshot(req(
                "DELETE",
                &format!("/api/user/avatar-file?path={file_rel}"),
                None,
                Some(&admin),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert!(
            file_path.exists(),
            "a refused delete must not destroy the file"
        );

        // The owner can — and the own-field clear runs through the authority.
        let resp = router
            .clone()
            .oneshot(req(
                "PUT",
                "/api/user/avatar-config",
                Some(&format!(r#"{{"avatar":"{file_rel}"}}"#)),
                Some(&bob),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = router
            .clone()
            .oneshot(req(
                "DELETE",
                &format!("/api/user/avatar-file?path={file_rel}"),
                None,
                Some(&bob),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(!file_path.exists());
        let resp = router
            .oneshot(req("GET", "/api/user/avatar-config", None, Some(&bob)))
            .await
            .unwrap();
        assert!(json(resp).await["avatar"].is_null());
    }

    /// `GET /api/users/directory` — the one account listing a non-admin may
    /// read (ADR-076 §决策 8). Pin the things that keep it a *bounded*
    /// exception: it is authenticated, it reaches a non-admin, it is a
    /// minimal projection, and it does not lose the route to
    /// `/api/users/{user_id}`.
    #[tokio::test]
    async fn user_directory_is_readable_by_any_account_but_bounded() {
        let dir = temp_dir();
        let router = build_router(admin_state(&dir));
        let admin = login(&router, "root", PWD).await;

        let mut carol_id = String::new();
        for name in ["alice", "carol"] {
            let resp = router
                .clone()
                .oneshot(req(
                    "POST",
                    "/api/users",
                    Some(&format!(
                        r#"{{"username":"{name}","display_name":"{name}","password":"{PWD}"}}"#
                    )),
                    Some(&admin),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::CREATED);
            let id = json(resp).await["account"]["user_id"]
                .as_str()
                .unwrap()
                .to_string();
            if name == "carol" {
                carol_id = id;
            }
        }
        let alice = login(&router, "alice", PWD).await;

        // Still gated: the middleware owns that, not this handler.
        let resp = router
            .clone()
            .oneshot(req("GET", "/api/users/directory", None, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // A non-admin reaches it — 200 (not the 403 they would get from
        // `GET /api/users/{user_id}`, which is the precedence check).
        let resp = router
            .clone()
            .oneshot(req("GET", "/api/users/directory", None, Some(&alice)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json(resp).await;
        let entries = body["users"].as_array().unwrap();
        let usernames: Vec<&str> = entries
            .iter()
            .map(|u| u["username"].as_str().unwrap())
            .collect();
        assert!(usernames.contains(&"root"), "admin is contactable: {usernames:?}");
        assert!(usernames.contains(&"carol"));
        assert!(!usernames.contains(&"alice"), "caller is not their own contact");

        // Minimal projection — nothing beyond the five named fields. The
        // avatar pair rides along because the sidebar has to be able to
        // draw a contact (see `DirectoryEntry`); the rest of the profile
        // stays server-side.
        let entry = entries[0].as_object().unwrap();
        let mut fields: Vec<&str> = entry.keys().map(String::as_str).collect();
        fields.sort_unstable();
        assert_eq!(
            fields,
            [
                "avatar",
                "builtin_avatar",
                "display_name",
                "role",
                "user_id",
                "username"
            ]
        );

        // The avatar pair is the reason the sidebar can draw the same face
        // the inbox thread shows. Regression: it used to be dropped here,
        // so a non-admin's contact picker fell back to a derived
        // placeholder icon for every peer while the chat panel — reading
        // `chat_api`'s `peer_avatar` — showed the real one. Set both
        // flavours on carol and read them back as alice.
        let carol = login(&router, "carol", PWD).await;
        for (field, value) in [
            ("avatar", "assets/avatars/carol/avatar-01.png"),
            ("builtin_avatar", "icon-18"),
        ] {
            let resp = router
                .clone()
                .oneshot(req(
                    "PUT",
                    "/api/user/avatar-config",
                    Some(&format!(r#"{{"{field}":"{value}"}}"#)),
                    Some(&carol),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }
        let resp = router
            .clone()
            .oneshot(req("GET", "/api/users/directory", None, Some(&alice)))
            .await
            .unwrap();
        let entries = json(resp).await;
        let carol_entry = entries["users"]
            .as_array()
            .unwrap()
            .iter()
            .find(|u| u["username"] == "carol")
            .expect("carol is a contact");
        assert_eq!(carol_entry["avatar"], "assets/avatars/carol/avatar-01.png");
        assert_eq!(carol_entry["builtin_avatar"], "icon-18");

        // …and a cleared field is `null`, not `""` — an empty string is
        // the wire contract for "cleared" on the write side, and handing it
        // through would make the client try to fetch an avatar at "".
        let resp = router
            .clone()
            .oneshot(req(
                "PUT",
                "/api/user/avatar-config",
                Some(r#"{"avatar":""}"#),
                Some(&carol),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = router
            .clone()
            .oneshot(req("GET", "/api/users/directory", None, Some(&alice)))
            .await
            .unwrap();
        let entries = json(resp).await;
        let carol_entry = entries["users"]
            .as_array()
            .unwrap()
            .iter()
            .find(|u| u["username"] == "carol")
            .expect("carol is a contact");
        assert!(carol_entry["avatar"].is_null(), "got {}", carol_entry["avatar"]);
        assert_eq!(carol_entry["builtin_avatar"], "icon-18");

        // Disabling removes an account: nobody can log in as them, so there
        // is nobody to contact.
        let resp = router
            .clone()
            .oneshot(req(
                "POST",
                &format!("/api/users/{carol_id}/disable"),
                None,
                Some(&admin),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let resp = router
            .oneshot(req("GET", "/api/users/directory", None, Some(&alice)))
            .await
            .unwrap();
        let usernames: Vec<String> = json(resp).await["users"]
            .as_array()
            .unwrap()
            .iter()
            .map(|u| u["username"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(usernames, vec!["root".to_string()]);
    }
}
