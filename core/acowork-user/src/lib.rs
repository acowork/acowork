//! acowork-user — the account / user-domain service (ADR-084).
//!
//! Accounts, credentials, roles, profiles / avatars and user-to-user chat,
//! extracted from the Gateway into a standalone process so the Gateway keeps
//! only its pure-network duties (ADR-055): MQTT broker host, HTTP entry
//! point, global resource authority.
//!
//! ## What lives here
//!
//! | Domain | Module |
//! |--------|--------|
//! | Account store + Argon2id credentials | [`account`] |
//! | Login / refresh / logout / change-password, token issuance | [`auth`] |
//! | User-to-user chat persistence | [`chat`] |
//! | REST surface (`/api/auth/*`, `/api/users/*`, `/api/user/avatar-*`) | [`http`] |
//! | `user_profiles.json` (the derived public view) | [`profiles`] |
//!
//! ## What deliberately does **not**
//!
//! - **Token verification.** The Gateway's `auth_middleware` verifies the
//!   Ed25519 signature locally on every request (ADR-084 §决策 2). This
//!   service never sees a request before that gate, and holds no verifying
//!   key on the request path.
//! - **Identity.** This service trusts the `X-Auth-*` headers the Gateway
//!   injects (see [`auth::context`]); it must therefore bind loopback only.
//! - **`last_user_profile` publication.** Runtime-facing global resources
//!   stay a Gateway concern; this service only writes the file the Gateway
//!   reads (ADR-084 §决策 4b).
//!
//! ## Process model
//!
//! Standalone binary, spawned and supervised by the Gateway (ADR-084 §决策 5,
//! same pattern as `acowork-pm` / `acowork-doc`). Data directory
//! `$HOME/.acowork/acowork-user/`, a peer of the other services'.
//!
//! It runs in **both** deployment modes (ADR-084 §决策 6): under `local` it
//! serves profiles and avatars only; under `multi_user` it adds accounts,
//! tokens and chat. Keeping one owner for profile code is why `local` pays
//! for a process at all.
//!
//! ## Design reference
//!
//! - ADR: `docs/adr/zh/ADR-084-user-standalone-process.md`
//! - Plan: `docs/plan/zh/user-dev-plan.md` (M0 → M5)

pub mod account;
pub mod auth;
pub mod chat;
pub mod chat_sessions;
pub mod cli;
pub mod config;
pub mod error;
pub mod health;
pub mod http;
pub mod mqtt_publisher;
pub mod profiles;
pub mod search_index;
pub mod server;
pub mod state;
pub mod types;


#[cfg(test)]
mod test_support;

// ─────────────────────────────────────────────────────────────────────────
// Re-exports: crate-level public API
// ─────────────────────────────────────────────────────────────────────────

// Config
pub use config::{AuthMode, UserServiceConfig};

// Errors
pub use error::ApiError;

// Identity (Gateway-injected)
pub use auth::context::AuthContext;

// Service
pub use server::UserService;

// State
pub use state::{AppState, ResourceCache, SharedState};

// Account system
pub use auth::{AuthError, AuthPrincipal, AuthService, BootstrapAdmin, PasswordPolicy};

// Chat domain
pub use chat::{Attachment, ChatMessage};

// Derived profile view
pub use profiles::{
    load_user_profile_list, rebuild_and_save_user_profile_cache, save_user_profile_list,
    user_profile_list_path,
};
