//! Account lifecycle HTTP API (ADR-076 §决策 3 / §决策 6).
//!
//! ```text
//! POST /api/auth/setup            {username, password, display_name?} → token pair
//! POST /api/auth/login            {username, password} → token pair
//! POST /api/auth/refresh          {refresh_token}      → token pair (rotated)
//! POST /api/auth/logout           {refresh_token}      → 204
//! POST /api/auth/change-password  {old_password, new_password} → 204
//! GET  /api/auth/me               → AccountView (redacted)
//! ```
//!
//! Always registered (ADR-114): the account system is unconditional, so
//! there is no deployment in which these routes are absent.
//!
//! `setup` is the first-boot exception: it needs no identity because none
//! can exist yet. It refuses to run once an administrator is loggable-in
//! (409), and the Gateway restricts it to a real loopback peer — see
//! `http/restricted_mode.rs` and `relay/remote_listener.rs`.
//!
//! Argon2id is deliberately slow (~100 ms), so every handler that hashes
//! or verifies runs on the blocking pool — never on an async worker.

use axum::{
    Extension, Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use serde::Deserialize;
use std::sync::Arc;

use acowork_core::account::AccountView;

use crate::auth::service::{AuthError, AuthService, TokenPair, PAIRING_TTL_SECS};
use crate::auth::issuer::now_unix;
use crate::auth::context::AuthContext;
use crate::error::ApiError;
use crate::state::AppState;

/// The account routes (always registered — ADR-114).
pub fn auth_routes() -> Router<AppState> {
    Router::new()
        .route("/api/auth/setup", post(setup_admin))
        .route("/api/auth/login", post(login))
        .route("/api/auth/refresh", post(refresh))
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/change-password", post(change_password))
        .route("/api/auth/first-login", post(first_login))
        .route("/api/auth/pair/create", post(pair_create))
        .route("/api/auth/pair/redeem", post(pair_redeem))
        .route("/api/auth/me", get(me))
}

/// ADR-114 first-boot setup: create the first login-capable administrator.
/// Public at both auth layers (no token can exist yet); the Gateway gates it
/// on the real peer address. `display_name` is optional and falls back to
/// the username on the server.
#[derive(Debug, Deserialize)]
pub struct SetupAdminRequest {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub struct RefreshRequest {
    pub refresh_token: String,
}

#[derive(Debug, Deserialize)]
pub struct ChangePasswordRequest {
    pub old_password: String,
    pub new_password: String,
}

/// ADR-076 §决策 6: consume a one-time `invite_token` and set the initial
/// password. Public (whitelisted) — the caller has no token pair yet.
#[derive(Debug, Deserialize)]
pub struct FirstLoginRequest {
    pub invite_token: String,
    pub new_password: String,
}

/// 方案③：移动端扫码兑换的入参。`pairing_token` 是桌面出码时回显的一次性
/// 短期随机串（不含账号/密码），兑换即得一套新 token。
#[derive(Debug, Deserialize)]
pub struct PairRedeemRequest {
    pub pairing_token: String,
}

/// The account service is always present (ADR-114).
fn service(state: &AppState) -> Result<Arc<AuthService>, ApiError> {
    // ADR-114: the account system is always present — there is no
    // deployment in which these routes are unreachable because it is off.
    Ok(state.auth_service.clone())
}

impl From<AuthError> for ApiError {
    fn from(e: AuthError) -> Self {
        match e {
            // Unknown user / wrong password / disabled / bad token / revoked
            // are all "you are not who you claim" from the caller's side.
            // Deliberately uniform: the response must not enumerate users.
            AuthError::InvalidCredentials => ApiError::unauthorized("invalid username or password"),
            AuthError::Token(t) => ApiError::unauthorized(&t.to_string()),
            AuthError::Revoked => ApiError::unauthorized("token revoked"),
            AuthError::Policy(m) => ApiError::unprocessable_entity(&m),
            AuthError::Conflict(m) => ApiError::conflict(&m),
            AuthError::Store(m) => ApiError::internal(&m),
        }
    }
}

/// Run a blocking auth operation off the async worker pool.
async fn blocking<T, F>(f: F) -> Result<T, ApiError>
where
    F: FnOnce() -> Result<T, AuthError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::internal(&format!("auth task failed: {e}")))?
        .map_err(ApiError::from)
}

/// `POST /api/auth/setup` — ADR-114 first-boot administrator creation.
///
/// Returns a token pair so the Desktop's onboarding wizard can go straight
/// into the authenticated app without a second login round-trip. The
/// service itself refuses a second setup (409), independently of the
/// Gateway's health snapshot, which lags by one poll interval.
async fn setup_admin(
    State(state): State<AppState>,
    Json(req): Json<SetupAdminRequest>,
) -> Result<Json<TokenPair>, ApiError> {
    let auth = service(&state)?;
    let display_name = req.display_name.unwrap_or_default();
    let pair = blocking(move || {
        auth.setup_first_admin(&req.username, &display_name, &req.password, now_unix())
    })
    .await?;
    // The presentation cache is derived from accounts.json; the new admin
    // must be visible to the Runtime's `last_user_profile` immediately.
    crate::http::account_api::sync_profiles(&state).await;
    Ok(Json(pair))
}

async fn login(
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<TokenPair>, ApiError> {
    let auth = service(&state)?;
    let pair = blocking(move || auth.login(&req.username, &req.password, now_unix())).await?;
    Ok(Json(pair))
}

async fn refresh(
    State(state): State<AppState>,
    Json(req): Json<RefreshRequest>,
) -> Result<Json<TokenPair>, ApiError> {
    let auth = service(&state)?;
    let pair = blocking(move || auth.refresh(&req.refresh_token, now_unix())).await?;
    Ok(Json(pair))
}

async fn logout(
    State(state): State<AppState>,
    Json(req): Json<RefreshRequest>,
) -> Result<StatusCode, ApiError> {
    let auth = service(&state)?;
    blocking(move || auth.logout(&req.refresh_token, now_unix())).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Change the **caller's own** password (ADR-076 §决策 6).
///
/// Always the token's real identity, never `as_user` — `as_user` is a
/// read-only view (ADR-076 §决策 4), so an admin cannot change someone
/// else's password through this route.
async fn change_password(
    State(state): State<AppState>,
    Extension(auth_ctx): Extension<AuthContext>,
    Json(req): Json<ChangePasswordRequest>,
) -> Result<StatusCode, ApiError> {
    let auth = service(&state)?;
    let user_id = auth_ctx.user_id;
    blocking(move || {
        auth.change_password(&user_id, &req.old_password, &req.new_password, now_unix())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The caller's own redacted account record.
async fn me(
    State(state): State<AppState>,
    Extension(auth_ctx): Extension<AuthContext>,
) -> Result<Json<AccountView>, ApiError> {
    let auth = service(&state)?;
    let account = auth.account(&auth_ctx.user_id)?;
    Ok(Json(AccountView::from(&account)))
}

/// `POST /api/auth/first-login` — activate an invited account
/// (ADR-076 §决策 6).
///
/// Public (no bearer token): the whole point is that the account has no
/// password yet. The `invite_token` is single-use and 24h-bound.
async fn first_login(
    State(state): State<AppState>,
    Json(req): Json<FirstLoginRequest>,
) -> Result<Json<TokenPair>, ApiError> {
    let auth = service(&state)?;
    let pair = blocking(move || auth.first_login(&req.invite_token, &req.new_password, now_unix()))
        .await?;
    Ok(Json(pair))
}

/// `POST /api/auth/pair/create` — 桌面出码（方案③）。
///
/// 需已登录身份（非公开）：取 token 的真实 `user_id`，为其签发一张 60 秒
/// 有效、兑换即烧掉的一次性配对票据。票据只存哈希，明文仅本次回显供编码进
/// 二维码。纯内存操作，不走 blocking。
async fn pair_create(
    State(state): State<AppState>,
    Extension(auth_ctx): Extension<AuthContext>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let auth = service(&state)?;
    let token = auth.create_pairing(&auth_ctx.user_id, now_unix());
    Ok(Json(serde_json::json!({
        "pairing_token": token,
        "expires_in": PAIRING_TTL_SECS,
    })))
}

/// `POST /api/auth/pair/redeem` — 移动端扫码兑换（方案③）。
///
/// 公开端点（在 user context 与 gateway auth_middleware 两处 `is_public_path`
/// 均放行），调用方尚无 token pair。凭一次性配对票据换取一套全新 token（按
/// 出码用户当前角色现签）。票据过期/未知/已兑换均 401，不可区分。
async fn pair_redeem(
    State(state): State<AppState>,
    Json(req): Json<PairRedeemRequest>,
) -> Result<Json<TokenPair>, ApiError> {
    let auth = service(&state)?;
    let pair = blocking(move || auth.redeem_pairing(&req.pairing_token, now_unix())).await?;
    Ok(Json(pair))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::BootstrapAdmin;
    use crate::auth::issuer::TokenError;
    use crate::test_support::build_router;
    use acowork_core::account::Role;
    use argon2::Params;
    use axum::body::Body;
    use axum::http::Request;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use tower::ServiceExt;

    const PWD: &str = "s3cret123";

    fn temp_dir() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "acowork-test-authapi-{}-{}",
            std::process::id(),
            unique
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Seed `accounts.json` with one account whose hash uses weak Argon2
    /// params (verification reads the params back from the PHC string, so
    /// this is both fast and honest).
    fn seed_account(
        svc: &AuthService,
        username: &str,
        role: acowork_core::account::Role,
    ) -> String {
        use acowork_core::account::{AccountListFile, UserAccount};
        let user_id = format!("u-{username}");
        let account = UserAccount {
            user_id: user_id.clone(),
            username: username.into(),
            display_name: username.into(),
            role,
            password_hash: crate::account::password::hash_password_with(
                PWD,
                Params::new(8, 1, 1, Some(32)).unwrap(),
            )
            .unwrap(),
            password_changed_at: "t".into(),
            password_expires_at: None,
            language: "en".into(),
            timezone: "UTC".into(),
            city: None,
            country: None,
            occupation: None,
            avatar: None,
            builtin_avatar: None,
            communication_style: None,
            custom: Default::default(),
            created_at: "t".into(),
            updated_at: "t".into(),
            last_login_at: None,
            disabled_at: None,
            invite_token_hash: None,
            invite_expires_at: None,
        };
        svc.save_accounts(&AccountListFile {
            version: 1,
            accounts: vec![account],
        })
        .unwrap();
        user_id
    }

    fn state(dir: &Path) -> AppState {
        crate::test_support::multi_user_state(dir)
    }

    async fn body_json(resp: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn post(uri: &str, body: &str, token: Option<&str>) -> Request<Body> {
        let mut b = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        b.body(Body::from(body.to_string())).unwrap()
    }

    fn get(uri: &str, token: Option<&str>) -> Request<Body> {
        let mut b = Request::builder().method("GET").uri(uri);
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        b.body(Body::empty()).unwrap()
    }

    /// ADR-114: the account routes are unconditional. There is no
    /// deployment in which "the account system is off", so the route must
    /// exist even with an empty store — what changes is the *state*
    /// (`uninitialized` / `needs_setup` / `ready`), not the route table.
    #[tokio::test]
    async fn account_routes_exist_even_with_an_empty_store() {
        let dir = temp_dir();
        let router = build_router(state(&dir));

        let resp = router
            .oneshot(post(
                "/api/auth/login",
                r#"{"username":"a","password":"b"}"#,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// ADR-114: setup creates the first administrator from an empty store,
    /// hands back a usable token pair, and refuses to run a second time.
    #[tokio::test]
    async fn setup_creates_the_first_admin_and_is_one_shot() {
        let dir = temp_dir();
        let router = build_router(state(&dir));

        let resp = router
            .clone()
            .oneshot(post(
                "/api/auth/setup",
                r#"{"username":"Root","password":"rootpass1","display_name":"Boss"}"#,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let tokens = body_json(resp).await;
        let access = tokens["access_token"].as_str().unwrap().to_string();

        // The returned token works immediately; the username is normalised.
        let resp = router
            .clone()
            .oneshot(get("/api/auth/me", Some(&access)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let me = body_json(resp).await;
        assert_eq!(me["username"], "root");
        assert_eq!(me["display_name"], "Boss");
        assert_eq!(me["role"], "admin");

        // …and the chosen credentials log in through the normal path.
        let resp = router
            .clone()
            .oneshot(post(
                "/api/auth/login",
                r#"{"username":"root","password":"rootpass1"}"#,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // A second setup is a 409, never a silent password overwrite. This
        // holds even when the Gateway's health snapshot still says
        // "needs setup" — the service is the authority on its own store.
        let resp = router
            .oneshot(post(
                "/api/auth/setup",
                r#"{"username":"attacker","password":"hacked123"}"#,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    /// The normal boot seeds a passwordless `admin`; setup must activate
    /// *that* account in place rather than leaving a second administrator
    /// behind (or 409-ing on the reserved username).
    #[tokio::test]
    async fn setup_activates_the_seeded_admin_in_place() {
        let dir = temp_dir();
        let st = state(&dir);
        st.auth_service.ensure_bootstrap_admin().unwrap();
        let before = st.auth_service.load_accounts().unwrap();
        assert_eq!(before.accounts.len(), 1);
        let seeded_id = before.accounts[0].user_id.clone();

        let router = build_router(st.clone());
        let resp = router
            .oneshot(post(
                "/api/auth/setup",
                r#"{"username":"owner","password":"ownerpass1"}"#,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let after = st.auth_service.load_accounts().unwrap();
        assert_eq!(after.accounts.len(), 1, "setup must not add a second admin");
        assert_eq!(after.accounts[0].user_id, seeded_id);
        assert_eq!(after.accounts[0].username, "owner");
        assert_eq!(after.accounts[0].display_name, "owner");
        assert_ne!(
            after.accounts[0].password_hash,
            acowork_core::account::DISABLED_PASSWORD_HASH
        );
    }

    /// Policy failures are 422 and must not half-initialise the store.
    #[tokio::test]
    async fn setup_enforces_username_and_password_policy() {
        let dir = temp_dir();
        let st = state(&dir);
        let router = build_router(st.clone());

        for body in [
            r#"{"username":"root","password":"short"}"#,
            r#"{"username":"root","password":"nodigitshere"}"#,
            r#"{"username":"Bad Name","password":"longpass1"}"#,
        ] {
            let resp = router
                .clone()
                .oneshot(post("/api/auth/setup", body, None))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        }
        assert!(st.auth_service.load_accounts().unwrap().accounts.is_empty());
    }

    #[tokio::test]
    async fn account_api_gates_api_and_serves_login() {
        let dir = temp_dir();
        let st = state(&dir);
        let user_id = seed_account(st.auth_service.as_ref(), "alice", Role::User);
        let router = build_router(st);

        // No token → 401 with a WWW-Authenticate challenge.
        let resp = router
            .clone()
            .oneshot(get("/api/agents", None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // Health is reachable pre-login.
        let resp = router.clone().oneshot(get("/health", None)).await.unwrap();
        assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);

        // Login → a token pair.
        let resp = router
            .clone()
            .oneshot(post(
                "/api/auth/login",
                &format!(r#"{{"username":"alice","password":"{PWD}"}}"#),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let tokens = body_json(resp).await;
        let access = tokens["access_token"].as_str().unwrap().to_string();
        let refresh = tokens["refresh_token"].as_str().unwrap().to_string();

        // /me returns the caller's redacted record — never the hash.
        let resp = router
            .clone()
            .oneshot(get("/api/auth/me", Some(&access)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let me = body_json(resp).await;
        assert_eq!(me["user_id"], user_id.as_str());
        assert_eq!(me["username"], "alice");
        assert!(me.get("password_hash").is_none(), "leaked hash: {me}");

        // A non-admin presenting as_user is rejected outright.
        let resp = router
            .clone()
            .oneshot(get("/api/auth/me?as_user=u-someone", Some(&access)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Refresh rotates the pair.
        let resp = router
            .clone()
            .oneshot(post(
                "/api/auth/refresh",
                &format!(r#"{{"refresh_token":"{refresh}"}}"#),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let rotated = body_json(resp).await;
        assert_ne!(rotated["refresh_token"], refresh.as_str());

        // Change-password requires the old one, and kills the old session.
        let resp = router
            .clone()
            .oneshot(post(
                "/api/auth/change-password",
                r#"{"old_password":"wrong","new_password":"newpass123"}"#,
                Some(&access),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let resp = router
            .clone()
            .oneshot(post(
                "/api/auth/change-password",
                &format!(r#"{{"old_password":"{PWD}","new_password":"newpass123"}}"#),
                Some(&access),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let resp = router
            .clone()
            .oneshot(post(
                "/api/auth/refresh",
                &format!(
                    r#"{{"refresh_token":"{}"}}"#,
                    rotated["refresh_token"].as_str().unwrap()
                ),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn bootstrapped_admin_logs_in_through_the_router() {
        let dir = temp_dir();
        let svc = AuthService::new(
            &dir,
            Default::default(),
            Some(BootstrapAdmin {
                username: "root".into(),
                password: "rootpass1".into(),
                display_name: None,
            }),
        )
        .unwrap();
        svc.ensure_bootstrap_admin().unwrap();

        let mut st = state(&dir);
        st.auth_service = Arc::new(svc);
        let router = build_router(st);

        let resp = router
            .oneshot(post(
                "/api/auth/login",
                r#"{"username":"root","password":"rootpass1"}"#,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn error_mapping_is_uniform_for_bad_credentials() {
        // 401 for every "not you" case; 422 for policy; 500 for storage.
        for e in [
            AuthError::InvalidCredentials,
            AuthError::Revoked,
            AuthError::Token(TokenError::BadSignature),
            AuthError::Token(TokenError::Expired),
        ] {
            assert_eq!(ApiError::from(e).code, 401);
        }
        assert_eq!(
            ApiError::from(AuthError::Policy("too short".into())).code,
            422
        );
        assert_eq!(ApiError::from(AuthError::Store("disk".into())).code, 500);
    }
}
