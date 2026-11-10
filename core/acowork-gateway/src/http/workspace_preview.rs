//! ADR-112 — workspace static-asset preview tickets.
//!
//! The workspace HTML preview needs a **real HTTP document**. A page's CSS /
//! JS / fonts / images are *sub-resource* loads performed by the browser, and
//! a browser load cannot carry an `Authorization` header. Rendering the page
//! from a Blob URL with a `<base href>` pointed at the bearer-gated
//! `/workspace-files/…` route therefore 401s every sub-resource and paints an
//! unstyled skeleton — the bug this module fixes (ADR-112 §决策 1).
//!
//! The fix is a **capability URL**: an authenticated caller mints a short
//! ticket, and the ticket rides in the *path* of a read-only proxy route.
//! Path position is load-bearing — relative sub-resource URLs
//! (`styles.css`, `assets/x.svg`) preserve path segments but **drop query
//! strings**, so a `?ticket=…` design would authenticate the document and
//! nothing else.
//!
//! Ticket format: `v1.<expires_at_unix>.<mac_hex>`, where the MAC is
//! `HMAC-SHA256(secret, context | expires_at | agent_id | workspace_id)`,
//! truncated to 128 bits. Agent + workspace are *not* stored in the ticket:
//! verification recomputes the MAC from the request path, so a ticket minted
//! for one workspace cannot be replayed against another.
//!
//! Guard-rails (ADR-112 §决策 4 — do not relax without amending the ADR):
//!   - GET only, read-only bytes; a ticket never authorizes a write.
//!   - Bound to exactly one `(agent_id, workspace_id)` pair, TTL-bounded.
//!   - Minting sits at the **same authorization tier** as reading the file it
//!     previews (`GET /api/agents/{id}/workspaces/file` = `use`), so a ticket
//!     cannot widen a caller's access — it only re-expresses an existing read
//!     right in a form a browser can carry.
//!   - The secret is in-memory only: a Gateway restart invalidates every
//!     outstanding ticket and the Desktop re-mints (§决策 5).

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::http::routes::AppState;
use crate::http::workspaces::proxy_workspace_file;

/// Every ticket-authenticated request lives under this prefix. It is the one
/// string `auth_middleware::is_public_path` keys on, so this constant and the
/// route template in [`workspace_preview_routes`] must stay in lockstep.
pub const PREVIEW_PATH_PREFIX: &str = "/workspace-preview/";

/// Ticket lifetime.
///
/// Long enough that a preview tab left open across a working day keeps
/// reloading without a round-trip, short enough that a leaked URL goes stale
/// on its own. The Desktop treats `expires_at` as authoritative and re-mints
/// well before it (see `lib/workspacePreviewUrl.ts`), so this is a ceiling on
/// the blast radius of a leaked link, not a functional limit.
pub const TICKET_TTL_SECS: u64 = 8 * 60 * 60;

const TICKET_VERSION: &str = "v1";

/// Domain separator: a MAC minted by any other Gateway feature can never
/// verify here, and vice versa.
const MAC_CONTEXT: &str = "acowork/workspace-preview/v1";

/// 128-bit MAC truncation — far beyond guessing, and keeps the URL short
/// (32 hex chars instead of 64).
const MAC_BYTES: usize = 16;

/// SHA-256 block size, for the HMAC ipad/opad pads.
const SHA256_BLOCK: usize = 64;

/// In-memory HMAC key for preview tickets (ADR-112 §决策 5).
///
/// Generated per process (`AppState::new`); never persisted, never logged.
/// `Debug` is redacted so a `{:?}` of an `AppState` in a test failure cannot
/// leak the key.
pub struct PreviewTicketSecret([u8; 32]);

impl PreviewTicketSecret {
    /// A fresh random key — the only production constructor.
    pub fn random() -> Self {
        use rand::RngExt;
        Self(rand::rng().random())
    }

    /// Fixed key, for tests that need a ticket they can precompute.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl std::fmt::Debug for PreviewTicketSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PreviewTicketSecret(<redacted>)")
    }
}

/// Shared handle stored on [`AppState`].
pub type SharedPreviewTicketSecret = Arc<PreviewTicketSecret>;

/// Why a ticket failed verification.
///
/// Deliberately coarse: the caller learns "invalid" vs "expired" (an expired
/// link is actionable — reopen the file), never which field disagreed. That
/// keeps a probing attacker from learning how the MAC input is encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TicketError {
    /// Not a `v1.<exp>.<mac>` triple, or the MAC hex is the wrong length.
    Malformed,
    /// Structurally fine, but the MAC does not cover *this* agent + workspace.
    Mismatch,
    /// MAC verified; the ticket is past its `expires_at`.
    Expired,
}

// ── Crypto primitives ────────────────────────────────────────────────────
//
// HMAC-SHA256 + constant-time compare are inlined here rather than pulled in
// as `hmac` / `subtle` dependencies: the Gateway's Cargo.toml already carries
// `sha2`, and this module's whole crypto surface is these two functions —
// both small enough to audit in place.

/// HMAC-SHA256 (RFC 2104) over `message` with `key`.
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut key_block = [0u8; SHA256_BLOCK];
    if key.len() > SHA256_BLOCK {
        let digest = Sha256::digest(key);
        key_block[..digest.len()].copy_from_slice(&digest);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }

    let mut inner_pad = [0x36u8; SHA256_BLOCK];
    let mut outer_pad = [0x5cu8; SHA256_BLOCK];
    for i in 0..SHA256_BLOCK {
        inner_pad[i] ^= key_block[i];
        outer_pad[i] ^= key_block[i];
    }

    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(message);
    let inner = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner);
    outer.finalize().into()
}

fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(text.len() / 2);
    for pair in bytes.chunks(2) {
        let high = (pair[0] as char).to_digit(16)?;
        let low = (pair[1] as char).to_digit(16)?;
        out.push(((high << 4) | low) as u8);
    }
    Some(out)
}

/// Seconds since the Unix epoch. Saturates at 0 for clocks before 1970 rather
/// than panicking — a skewed VM clock must not take the preview route down.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ── Mint / verify ────────────────────────────────────────────────────────

/// The MAC input. Length-prefixing each field keeps the encoding unambiguous
/// even though agent / workspace ids are attacker-influenced path segments
/// (a `|` inside one cannot forge a different field split).
fn ticket_mac(
    secret: &PreviewTicketSecret,
    expires_at: u64,
    agent_id: &str,
    workspace_id: &str,
) -> [u8; 32] {
    let message = format!(
        "{MAC_CONTEXT}|{expires_at}|{}:{agent_id}|{}:{workspace_id}",
        agent_id.len(),
        workspace_id.len(),
    );
    hmac_sha256(&secret.0, message.as_bytes())
}

/// Mint a ticket bound to `(agent_id, workspace_id)` and valid until
/// `expires_at` (Unix seconds).
pub fn mint_ticket(
    secret: &PreviewTicketSecret,
    agent_id: &str,
    workspace_id: &str,
    expires_at: u64,
) -> String {
    let mac = ticket_mac(secret, expires_at, agent_id, workspace_id);
    format!("{TICKET_VERSION}.{expires_at}.{}", to_hex(&mac[..MAC_BYTES]))
}

/// Verify `ticket` against the `(agent_id, workspace_id)` it is being used
/// for. Returns the expiry it was minted with.
///
/// Checks run MAC-first: an attacker learns nothing about `expires_at` (or
/// anything else) from a ticket they cannot sign.
pub fn verify_ticket(
    secret: &PreviewTicketSecret,
    ticket: &str,
    agent_id: &str,
    workspace_id: &str,
    now: u64,
) -> Result<u64, TicketError> {
    let mut parts = ticket.split('.');
    let (Some(version), Some(expires_raw), Some(mac_hex), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(TicketError::Malformed);
    };
    if version != TICKET_VERSION {
        return Err(TicketError::Malformed);
    }
    let expires_at: u64 = expires_raw.parse().map_err(|_| TicketError::Malformed)?;
    let presented = from_hex(mac_hex).ok_or(TicketError::Malformed)?;
    if presented.len() != MAC_BYTES {
        return Err(TicketError::Malformed);
    }

    let expected = ticket_mac(secret, expires_at, agent_id, workspace_id);
    // Reuse the Gateway's single constant-time compare rather than a private
    // copy: verification time must not depend on how many leading MAC bytes an
    // attacker guessed right.
    if !crate::mqtt::enrollment::constant_time_eq(&presented, &expected[..MAC_BYTES]) {
        return Err(TicketError::Mismatch);
    }
    if now >= expires_at {
        return Err(TicketError::Expired);
    }
    Ok(expires_at)
}

/// Redact the capability ticket from a request URI before it is logged.
///
/// The ticket rides in the **path**, so without this the access-log middleware
/// would write a live capability into the Gateway log on every request a
/// previewed page makes — the document plus each CSS / JS / font. That is the
/// same hazard ADR-089 D2 handles for `?access_token=`: a credential a browser
/// can only carry in the URL must not be recorded in a file the user is asked
/// to hand over when reporting a bug.
///
/// Returns the URI unchanged when it is not a preview path — cheap enough to
/// call on every request.
pub fn redact_preview_ticket(uri: axum::http::Uri) -> axum::http::Uri {
    let raw = uri.to_string();
    let Some(rest) = raw.strip_prefix(PREVIEW_PATH_PREFIX) else {
        return uri;
    };
    let Some((_, tail)) = rest.split_once('/') else {
        return uri;
    };
    // `***` rather than `<redacted>`: angle brackets are not legal in a URI
    // path, so a placeholder containing them would fail to re-parse and
    // silently fall back to the *unredacted* original.
    format!("{PREVIEW_PATH_PREFIX}***/{tail}")
        .parse()
        .unwrap_or(uri)
}

// ── HTTP surface ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct MintTicketRequest {
    /// Workspace the preview is scoped to (`ws-…`, or `__agent_home__`).
    pub workspace_id: String,
}

#[derive(Debug, Serialize)]
pub struct MintTicketResponse {
    /// Opaque ticket — put it in the `{ticket}` path segment, nothing else.
    pub ticket: String,
    /// Unix seconds. The Desktop re-mints before this (ADR-112 §决策 5).
    pub expires_at: u64,
    pub ttl_secs: u64,
}

/// `POST /api/agents/{id}/workspaces/preview-ticket`
///
/// Mints a read-only capability for one workspace's static files. Exists so
/// the HTML preview can be served as a real HTTP document: the address-bar
/// browser cannot attach a bearer token to a navigation, but it can carry an
/// opaque path segment (ADR-112 §决策 2).
///
/// The caller has already been authorized by `permission_middleware` at the
/// same tier as `GET /workspaces/file` (`use`), so this handler does no
/// permission work of its own — it only refuses requests that name no
/// workspace, which would mint a ticket nothing can resolve.
pub async fn mint_preview_ticket(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(body): Json<MintTicketRequest>,
) -> Result<Json<MintTicketResponse>, (StatusCode, Json<serde_json::Value>)> {
    if body.workspace_id.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "invalid_request",
                "message": "workspace_id must not be empty",
            })),
        ));
    }
    let expires_at = now_unix().saturating_add(TICKET_TTL_SECS);
    let ticket = mint_ticket(
        &state.workspace_preview_secret,
        &agent_id,
        &body.workspace_id,
        expires_at,
    );
    Ok(Json(MintTicketResponse {
        ticket,
        expires_at,
        ttl_secs: TICKET_TTL_SECS,
    }))
}

/// `GET /workspace-preview/{ticket}/{agent_id}/{workspace_id}/{*path}`
///
/// The browser-facing half of the capability: same bytes, same reverse proxy
/// as [`crate::http::workspaces::serve_workspace_ws_file`], but authenticated
/// by the ticket instead of a bearer header — because a sub-resource load
/// cannot send one.
pub async fn serve_preview_file(
    State(state): State<AppState>,
    Path((ticket, agent_id, workspace_id, file_rel_path)): Path<(String, String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let now = now_unix();
    match verify_ticket(
        &state.workspace_preview_secret,
        &ticket,
        &agent_id,
        &workspace_id,
        now,
    ) {
        Ok(_) => {
            proxy_workspace_file(&state, &agent_id, &workspace_id, &file_rel_path, &headers).await
        }
        Err(err) => ticket_error_response(err),
    }
}

/// 403 for every ticket rejection.
///
/// 403 rather than 401 is deliberate: 401 is the "you are not authenticated,
/// get a token" signal, and the preview surface has no token to get. It also
/// keeps the Desktop's auth interceptor (which treats 401 as a rotation
/// trigger) out of a failure the interceptor cannot fix.
fn ticket_error_response(err: TicketError) -> Response {
    let (code, message) = match err {
        TicketError::Expired => (
            "preview_ticket_expired",
            "Preview link expired — reopen the file to mint a new one.",
        ),
        TicketError::Mismatch | TicketError::Malformed => (
            "preview_ticket_invalid",
            "Preview link is not valid for this agent and workspace.",
        ),
    };
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({ "error": code, "message": message })),
    )
        .into_response()
}

/// Preview routes (ADR-112).
///
/// Two entries with deliberately different authentication:
///   - the mint route is a normal `/api/*` route (bearer + permission gate);
///   - the serve route is public at the middleware level
///     ([`crate::http::auth_middleware::is_public_path`]) and authenticates
///     per-request via the ticket, which is the only credential a sub-resource
///     load can carry.
pub fn workspace_preview_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/agents/{id}/workspaces/preview-ticket",
            post(mint_preview_ticket),
        )
        .route(
            "/workspace-preview/{ticket}/{agent_id}/{workspace_id}/{*path}",
            get(serve_preview_file),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: [u8; 32] = [7u8; 32];

    fn secret() -> PreviewTicketSecret {
        PreviewTicketSecret::from_bytes(SECRET)
    }

    #[test]
    fn mint_then_verify_round_trips() {
        let ticket = mint_ticket(&secret(), "agent-1", "ws-1", 1_000);
        assert_eq!(
            verify_ticket(&secret(), &ticket, "agent-1", "ws-1", 999),
            Ok(1_000)
        );
    }

    #[test]
    fn the_ticket_never_reaches_the_access_log() {
        let uri: axum::http::Uri = "/workspace-preview/v1.1000.deadbeef/agent-1/ws-1/index.html"
            .parse()
            .unwrap();
        assert_eq!(
            redact_preview_ticket(uri).to_string(),
            "/workspace-preview/***/agent-1/ws-1/index.html"
        );
        // A query on a sub-resource survives the redaction.
        let with_query: axum::http::Uri =
            "/workspace-preview/v1.1000.deadbeef/agent-1/ws-1/app.js?v=2".parse().unwrap();
        assert_eq!(
            redact_preview_ticket(with_query).to_string(),
            "/workspace-preview/***/agent-1/ws-1/app.js?v=2"
        );
        // Everything else is untouched — including the bearer-gated sibling
        // route, whose path carries no credential of its own.
        for untouched in [
            "/workspace-files/agent-1/ws-1/index.html",
            "/api/status",
            "/workspace-preview/",
        ] {
            let parsed: axum::http::Uri = untouched.parse().unwrap();
            assert_eq!(redact_preview_ticket(parsed).to_string(), untouched);
        }
    }

    #[test]
    fn ticket_is_url_safe_and_short() {
        // Path-segment safety: no `/`, no `?`, no `%` — the ticket must
        // survive relative sub-resource resolution without escaping.
        let ticket = mint_ticket(&secret(), "a", "w", 1_000);
        assert!(ticket.starts_with("v1."));
        assert!(
            ticket
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.'),
            "unexpected character in {ticket}"
        );
        assert!(
            ticket.len() < 64,
            "ticket should stay short: {} chars",
            ticket.len()
        );
    }

    #[test]
    fn expiry_is_exclusive() {
        let ticket = mint_ticket(&secret(), "agent-1", "ws-1", 1_000);
        assert_eq!(
            verify_ticket(&secret(), &ticket, "agent-1", "ws-1", 1_000),
            Err(TicketError::Expired),
            "`now == expires_at` must not verify"
        );
        assert_eq!(
            verify_ticket(&secret(), &ticket, "agent-1", "ws-1", 5_000),
            Err(TicketError::Expired)
        );
    }

    #[test]
    fn ticket_cannot_be_replayed_on_another_workspace_or_agent() {
        let ticket = mint_ticket(&secret(), "agent-1", "ws-1", 9_999);
        assert_eq!(
            verify_ticket(&secret(), &ticket, "agent-1", "ws-2", 1),
            Err(TicketError::Mismatch),
            "a ticket for ws-1 must not read ws-2"
        );
        assert_eq!(
            verify_ticket(&secret(), &ticket, "agent-2", "ws-1", 1),
            Err(TicketError::Mismatch),
            "a ticket for agent-1 must not read agent-2"
        );
    }

    #[test]
    fn field_boundaries_cannot_be_shifted() {
        // Without length prefixes, ("a|1:b", "c") and ("a", "1:b|c") would
        // sign the same message. They must not.
        let ticket = mint_ticket(&secret(), "a|1:b", "c", 9_999);
        assert_eq!(
            verify_ticket(&secret(), &ticket, "a", "1:b|c", 1),
            Err(TicketError::Mismatch)
        );
        assert_eq!(
            verify_ticket(&secret(), &ticket, "a|1:b", "c", 1),
            Ok(9_999)
        );
    }

    #[test]
    fn another_secret_cannot_verify() {
        let ticket = mint_ticket(&PreviewTicketSecret::from_bytes([1u8; 32]), "a", "w", 9_999);
        assert_eq!(
            verify_ticket(&secret(), &ticket, "a", "w", 1),
            Err(TicketError::Mismatch),
            "a Gateway restart (new key) must invalidate outstanding links"
        );
    }

    #[test]
    fn tampered_mac_is_rejected() {
        let ticket = mint_ticket(&secret(), "a", "w", 9_999);
        let mut characters: Vec<char> = ticket.chars().collect();
        let last = characters.len() - 1;
        characters[last] = if characters[last] == '0' { '1' } else { '0' };
        let tampered: String = characters.into_iter().collect();
        assert_eq!(
            verify_ticket(&secret(), &tampered, "a", "w", 1),
            Err(TicketError::Mismatch)
        );
    }

    #[test]
    fn pushing_the_expiry_out_is_rejected() {
        // `expires_at` is readable in the URL, so extending it is the obvious
        // attack; the MAC covers it.
        let ticket = mint_ticket(&secret(), "a", "w", 1_000);
        let mac = ticket.rsplit('.').next().expect("mac segment");
        let forged = format!("v1.99999999999.{mac}");
        assert_eq!(
            verify_ticket(&secret(), &forged, "a", "w", 1),
            Err(TicketError::Mismatch)
        );
    }

    #[test]
    fn malformed_inputs_are_rejected() {
        for bad in [
            "",
            "v1",
            "v1.1000",
            "v1.1000.00",
            "v1.notanumber.00112233445566778899aabbccddeeff",
            "v2.1000.00112233445566778899aabbccddeeff",
            "v1.1000.00112233445566778899aabbccddeeff.extra",
            "v1.1000.zz112233445566778899aabbccddeeff",
        ] {
            assert!(
                matches!(
                    verify_ticket(&secret(), bad, "a", "w", 1),
                    Err(TicketError::Malformed)
                ),
                "{bad:?} should be malformed"
            );
        }
    }

    #[test]
    fn hmac_matches_rfc4231_case_2() {
        // Known-answer check for the inlined HMAC implementation, so a mistake
        // in the pad construction cannot pass as "just a different MAC".
        assert_eq!(
            to_hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn secret_is_redacted_in_debug_output() {
        assert_eq!(
            format!("{:?}", PreviewTicketSecret::from_bytes([9u8; 32])),
            "PreviewTicketSecret(<redacted>)"
        );
    }

    // ── Handler-level: verification must run before any proxying ─────────

    /// `AppState` carrying nothing but the preview secret. Any handler that
    /// tried to reach the Runtime with this state hits the
    /// "registry not initialized" 503 — which is exactly what lets a test
    /// distinguish "rejected at the ticket gate" (403) from "verified, then
    /// proxied" (503).
    fn bare_state() -> AppState {
        use tokio::sync::RwLock;

        let dir = std::env::temp_dir().join(format!("acowork-preview-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let gw = crate::gateway::state::GatewayState::new(&dir.to_string_lossy());
        AppState::new(
            Arc::new(RwLock::new(gw)),
            Arc::new(crate::http::auth::HttpAuth::new(false)),
        )
    }

    fn path_of(ticket: &str) -> Path<(String, String, String, String)> {
        Path((
            ticket.to_string(),
            "agent-1".to_string(),
            "ws-1".to_string(),
            "index.html".to_string(),
        ))
    }

    #[tokio::test]
    async fn forged_ticket_is_rejected_before_the_proxy_runs() {
        let state = bare_state();
        let response = serve_preview_file(
            State(state),
            path_of("v1.99999999999.00112233445566778899aabbccddeeff"),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "an unverifiable ticket must not reach the Runtime proxy"
        );
    }

    #[tokio::test]
    async fn expired_ticket_is_403_even_though_it_is_genuine() {
        let state = bare_state();
        let stale = mint_ticket(&state.workspace_preview_secret, "agent-1", "ws-1", 1);
        let response = serve_preview_file(State(state), path_of(&stale), HeaderMap::new()).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn valid_ticket_passes_the_gate_and_reaches_the_proxy() {
        // The state has no runtime registry, so a *verified* request must fall
        // through to the proxy's 503. Asserting 503 (not 403) is what proves
        // the gate opened — the byte path itself is the pre-existing, already
        // tested `/workspace-files` implementation (`proxy_workspace_file`).
        let state = bare_state();
        let fresh = mint_ticket(
            &state.workspace_preview_secret,
            "agent-1",
            "ws-1",
            now_unix() + 60,
        );
        let response = serve_preview_file(State(state), path_of(&fresh), HeaderMap::new()).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn mint_returns_a_ticket_bound_to_the_path_agent_and_workspace() {
        let state = bare_state();
        let minted = mint_preview_ticket(
            State(state.clone()),
            Path("agent-1".to_string()),
            Json(MintTicketRequest {
                workspace_id: "ws-1".to_string(),
            }),
        )
        .await
        .expect("mint should succeed");

        assert_eq!(minted.0.ttl_secs, TICKET_TTL_SECS);
        assert_eq!(
            verify_ticket(
                &state.workspace_preview_secret,
                &minted.0.ticket,
                "agent-1",
                "ws-1",
                now_unix()
            ),
            Ok(minted.0.expires_at)
        );
        // Same ticket, different workspace → refused.
        assert_eq!(
            verify_ticket(
                &state.workspace_preview_secret,
                &minted.0.ticket,
                "agent-1",
                "ws-2",
                now_unix()
            ),
            Err(TicketError::Mismatch)
        );
    }

    #[tokio::test]
    async fn mint_refuses_an_empty_workspace() {
        let state = bare_state();
        let refused = mint_preview_ticket(
            State(state),
            Path("agent-1".to_string()),
            Json(MintTicketRequest {
                workspace_id: "  ".to_string(),
            }),
        )
        .await;
        assert!(matches!(refused, Err((StatusCode::BAD_REQUEST, _))));
    }

    // ── Router-level: the reported bug, end to end ───────────────────────

    /// Build the merged router so these tests also catch a route-template typo
    /// (`{*path}` vs `{path}`) and a route conflict — both of which only show up
    /// as a silent 404 or a boot-time panic in production.
    fn merged_router(state: AppState) -> axum::Router {
        crate::http::routes::build_router(state)
    }

    async fn get(router: axum::Router, uri: &str) -> axum::http::Response<axum::body::Body> {
        use tower::ServiceExt;

        router
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri(uri)
                    .body(axum::body::Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("router responds")
    }

    /// The regression test for the reported bug.
    ///
    /// Under `multi_user` the whole Gateway requires a bearer token — but the
    /// browser sends none for a page's CSS / JS / font loads, which is why the
    /// old blob + `/workspace-files` preview painted an unstyled skeleton. A
    /// valid ticket must therefore pass **without** an `Authorization` header.
    /// Reaching the proxy (503 — no Runtime registry in this state) instead of
    /// the auth gate (401) is exactly the property that was missing.
    #[tokio::test]
    async fn a_ticketed_subresource_load_needs_no_authorization_header() {
        let state = multi_user_state();
        let fresh = mint_ticket(
            &state.workspace_preview_secret,
            "agent-1",
            "ws-1",
            now_unix() + 60,
        );

        let response = get(
            merged_router(state.clone()),
            &format!("/workspace-preview/{fresh}/agent-1/ws-1/styles.css"),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "a ticketed sub-resource load must pass the auth gate and reach the \
             proxy — a 401 here is the exact bug ADR-112 fixes"
        );

        // …and the same request through the old, bearer-gated route still 401s.
        // The fix opens a narrower second door; it does not unlock the first.
        let legacy = get(
            merged_router(state),
            "/workspace-files/agent-1/ws-1/styles.css",
        )
        .await;
        assert_eq!(
            legacy.status(),
            StatusCode::UNAUTHORIZED,
            "the bearer gate on /workspace-files must be untouched"
        );
    }

    /// A `multi_user` state whose token gate actually works: the user-service
    /// verifier is installed from the test fixture key pair, so an anonymous
    /// request gets the real 401 instead of the "user service not ready" 503.
    fn multi_user_state() -> AppState {
        let mut state = bare_state();
        state.auth_mode = crate::auth::AuthMode::MultiUser;
        // `bare_state` builds the Gateway state synchronously; the verifier is
        // normally installed by the user service supervisor.
        {
            let mut gw = state
                .gateway_state
                .try_write()
                .expect("fresh GatewayState lock must be free");
            gw.user_verifier = Some(crate::http::test_support::verifier());
        }
        state
    }

    #[tokio::test]
    async fn a_forged_ticket_is_403_on_the_merged_router() {
        let state = multi_user_state();
        let response = get(
            merged_router(state),
            "/workspace-preview/v1.99999999999.00112233445566778899aabbccddeeff/agent-1/ws-1/index.html",
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn the_mint_route_is_registered_under_the_path_the_desktop_calls() {
        use tower::ServiceExt;

        // `Local` mode: the bearer gate is a no-op, so this exercises routing +
        // handler only. The desktop builds exactly this path
        // (`lib/workspacePreviewUrl.ts`) — a rename on either side must fail
        // here rather than silently 404ing in the UI.
        let state = bare_state();
        let response = merged_router(state)
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/agents/agent-1/workspaces/preview-ticket")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(r#"{"workspace_id":"ws-1"}"#))
                    .expect("build request"),
            )
            .await
            .expect("router responds");

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert!(json["ticket"].as_str().is_some_and(|t| t.starts_with("v1.")));
        assert!(json["expires_at"].as_u64().is_some());
    }
}
