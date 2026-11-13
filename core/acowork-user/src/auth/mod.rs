//! acowork-user authentication subsystem (ADR-084).
//!
//! - [`context`] — the trusted identity the Gateway injects (`X-Auth-*`).
//! - [`issuer`] — Ed25519 token issuance, re-exported from the shared
//!   contract in `acowork_core::auth`.
//! - [`revoked`] — refresh-token family revocation registry.
//! - [`service`] — the assembled account service (login / refresh / logout /
//!   change-password), always constructed (ADR-114).

pub mod context;
pub mod issuer;
pub mod revoked;
pub mod service;

pub use context::AuthContext;
pub use service::{AuthError, AuthPrincipal, AuthService, BootstrapAdmin, PasswordPolicy};
