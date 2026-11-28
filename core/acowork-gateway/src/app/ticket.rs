//! Capability tickets for app webview sessions (ADR-116).
//!
//! An app's front end is loaded in a sandboxed cross-origin iframe. The
//! Desktop's Bearer token lives only in the main window's memory and MUST NOT
//! be handed to the iframe, and an `<img>` / `<script>` sub-resource cannot
//! carry an `Authorization` header anyway. The Desktop therefore mints a
//! short-lived ticket over its authenticated session, passes it through the
//! iframe URL *fragment* (never a query string, never a log), and the app page
//! exchanges it once for an `HttpOnly` cookie scoped to exactly one app.
//!
//! The ticket is self-contained and stateless: a restart rotates the in-memory
//! secret and invalidates every outstanding session. Its MAC covers the app
//! id, so a ticket minted for one app cannot be replayed against another.

use std::sync::Arc;

use sha2::{Digest, Sha256};

pub const SESSION_COOKIE: &str = "acowork_app_session";
/// Header alternative to the cookie. An app page receives its ticket in the
/// iframe URL fragment (never a query string) and attaches it to its own
/// fetches, which avoids SameSite restrictions on cross-site iframes.
pub const SESSION_HEADER: &str = "x-app-session";
pub const SESSION_TTL_SECS: u64 = 8 * 60 * 60;

const TICKET_VERSION: &str = "v1";
const MAC_CONTEXT: &str = "acowork/app-session/v1";
const MAC_BYTES: usize = 16;
const SHA256_BLOCK: usize = 64;

/// In-memory HMAC key, generated once per Gateway process.
pub struct AppSessionSecret([u8; 32]);

impl AppSessionSecret {
    pub fn random() -> Self {
        use rand::RngExt;
        Self(rand::rng().random())
    }

    #[cfg(test)]
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl std::fmt::Debug for AppSessionSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AppSessionSecret(<redacted>)")
    }
}

pub type SharedAppSessionSecret = Arc<AppSessionSecret>;

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

fn mac_input(expires_at: u64, app_id: &str, user_id: &str) -> String {
    format!(
        "{MAC_CONTEXT}|{expires_at}|{}:{app_id}|{}:{user_id}",
        app_id.len(),
        user_id.len(),
    )
}

/// Mint a ticket bound to one app and one platform user.
pub fn mint_session_ticket(
    secret: &AppSessionSecret,
    app_id: &str,
    user_id: &str,
    expires_at: u64,
) -> String {
    let mac = hmac_sha256(&secret.0, mac_input(expires_at, app_id, user_id).as_bytes());
    format!(
        "{TICKET_VERSION}.{expires_at}.{}.{}",
        to_hex(user_id.as_bytes()),
        to_hex(&mac[..MAC_BYTES])
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionTicketError {
    Malformed,
    Mismatch,
    Expired,
}

/// Verify a ticket against the app id in the request path. MAC is checked
/// first so a caller without the key learns nothing about expiry or users.
pub fn verify_session_ticket(
    secret: &AppSessionSecret,
    ticket: &str,
    app_id: &str,
    now: u64,
) -> Result<(u64, String), SessionTicketError> {
    let mut parts = ticket.split('.');
    let (Some(version), Some(expires_raw), Some(user_hex), Some(mac_hex), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return Err(SessionTicketError::Malformed);
    };
    if version != TICKET_VERSION {
        return Err(SessionTicketError::Malformed);
    }
    let Ok(expires_at) = expires_raw.parse::<u64>() else {
        return Err(SessionTicketError::Malformed);
    };
    let (Some(user_bytes), Some(mac_bytes)) = (from_hex(user_hex), from_hex(mac_hex)) else {
        return Err(SessionTicketError::Malformed);
    };
    if mac_bytes.len() != MAC_BYTES {
        return Err(SessionTicketError::Malformed);
    }
    let Ok(user_id) = String::from_utf8(user_bytes) else {
        return Err(SessionTicketError::Malformed);
    };
    let expected = hmac_sha256(
        &secret.0,
        mac_input(expires_at, app_id, &user_id).as_bytes(),
    );
    if !crate::mqtt::enrollment::constant_time_eq(&expected[..MAC_BYTES], &mac_bytes) {
        return Err(SessionTicketError::Mismatch);
    }
    if now >= expires_at {
        return Err(SessionTicketError::Expired);
    }
    Ok((expires_at, user_id))
}

/// Build the `Set-Cookie` value. `Path` is scoped to this app so another app's
/// iframe can never present this cookie. `SameSite=Strict` plus `HttpOnly`
/// keep it off cross-site sub-requests and out of app JavaScript.
pub fn session_cookie(app_id: &str, ticket: &str, max_age: u64) -> String {
    format!(
        "{SESSION_COOKIE}={ticket}; Path=/api/apps/{app_id}/; Max-Age={max_age}; HttpOnly; SameSite=Strict"
    )
}

/// Extract one cookie value from a `Cookie` header.
pub fn read_session_cookie(headers: &axum::http::HeaderMap) -> Option<String> {
    let raw = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    for part in raw.split(';') {
        let (name, value) = part.trim().split_once('=')?;
        if name == SESSION_COOKIE {
            return Some(value.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticket_round_trip_and_app_binding() {
        let secret = AppSessionSecret::from_bytes([7u8; 32]);
        let ticket = mint_session_ticket(&secret, "pm", "u-alice", 10_000);
        let (expires, user) = verify_session_ticket(&secret, &ticket, "pm", 1).unwrap();
        assert_eq!(expires, 10_000);
        assert_eq!(user, "u-alice");
        assert_eq!(
            verify_session_ticket(&secret, &ticket, "doc", 1),
            Err(SessionTicketError::Mismatch)
        );
        assert_eq!(
            verify_session_ticket(&secret, &ticket, "pm", 10_000),
            Err(SessionTicketError::Expired)
        );
    }

    #[test]
    fn tampered_or_foreign_secret_is_rejected() {
        let secret = AppSessionSecret::from_bytes([3u8; 32]);
        let ticket = mint_session_ticket(&secret, "pm", "u-alice", 10_000);
        assert_eq!(
            verify_session_ticket(&AppSessionSecret::from_bytes([4u8; 32]), &ticket, "pm", 1),
            Err(SessionTicketError::Mismatch)
        );
        assert_eq!(
            verify_session_ticket(&secret, "v1.broken", "pm", 1),
            Err(SessionTicketError::Malformed)
        );
    }

    #[test]
    fn cookie_is_scoped_and_httponly() {
        let cookie = session_cookie("pm", "abc", 3600);
        assert!(cookie.contains("Path=/api/apps/pm/"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Strict"));
    }
}
