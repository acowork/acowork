//! User account types (ADR-076 §决策 1).
//!
//! `UserAccount` upgrades the presentation-only [`UserProfile`] into a
//! first-class identity: a login handle (`username`), a [`Role`], and
//! credential / lifecycle fields. The presentation fields (language,
//! timezone, avatar, …) are preserved so the same account can produce the
//! [`UserProfile`] pushed to Runtime as `last_user_profile`.
//!
//! Persistence (see ADR-076 §决策 2): the account record lives in the
//! Gateway's own store — `user_profiles.json` keeps the *public* view,
//! the credential file keeps the hash. This module is pure types (no I/O,
//! no hashing) so both Gateway and Desktop can share it.

use crate::protocol::UserProfile;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Login role (ADR-076 §决策 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Ordinary user: sees only their own sessions.
    #[default]
    User,
    /// Administrator: sees all sessions, may manage accounts.
    Admin,
}

impl Role {
    /// The canonical lowercase spelling carried in a token claim
    /// (ADR-076 §决策 3) and in the `accounts.json` record.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Admin => "admin",
        }
    }

    /// Parse the token-claim spelling. Unknown values fall back to the
    /// least-privileged role — a malformed claim must never widen access.
    pub fn from_claim(s: &str) -> Self {
        match s {
            "admin" => Self::Admin,
            _ => Self::User,
        }
    }
}

/// Sentinel `password_hash` for accounts that have not yet set a password.
///
/// Used for accounts an admin created but whose owner has not completed
/// first-login via `invite_token` (ADR-076 §决策 6). It is never a valid
/// PHC string, so a verify against it always fails — the account is
/// structurally un-loggable-in rather than merely disabled.
pub const DISABLED_PASSWORD_HASH: &str = "$disabled$";

/// Prefix of a hash this service can actually verify. `hash_password()`
/// writes Argon2id PHC strings; anything else (the sentinel, an empty
/// string, a truncated or hand-corrupted record) makes `verify_password`
/// fail, so it must never be counted as "somebody can log in".
pub const ARGON2ID_PHC_PREFIX: &str = "$argon2id$";

/// Whether a stored credential looks like a verifiable Argon2id PHC string.
///
/// This is a *shape* check, not a parse: `acowork-core` deliberately does
/// not depend on the argon2 crate. The login path still parses and verifies
/// the hash for real; this only keeps the first-boot state from claiming a
/// store is usable when no password could possibly work.
pub fn has_verifiable_password_hash(stored: &str) -> bool {
    stored.starts_with(ARGON2ID_PHC_PREFIX) && stored.len() > ARGON2ID_PHC_PREFIX.len()
}

/// The lifecycle state of the account system itself (ADR-114).
///
/// Replaces the old `AUTH_MODE` knob. The account system is **always on** —
/// what varies is how far along its first-boot lifecycle it is, and that is
/// a *runtime fact about the store*, not a deployment mode and not something
/// the network topology may decide.
///
/// The distinction that matters: `Uninitialized` means "there is no account
/// system here yet" (an operator has to create the first account), while
/// `NeedsSetup` means "accounts exist but none of them can be logged into
/// yet" (a seeded administrator still needs a password). Collapsing both
/// into one boolean is what made the first-boot gate undecidable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountSystemState {
    /// No account store yet, or the store is empty. The first administrator
    /// has to be created before anything else can happen.
    Uninitialized,
    /// Accounts exist, but no administrator has a usable password — nobody
    /// can log in until the first-boot password is set.
    NeedsSetup,
    /// At least one administrator can log in. The account system is usable.
    Ready,
}

impl AccountSystemState {
    /// The canonical lowercase spelling used on the wire and in logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Uninitialized => "uninitialized",
            Self::NeedsSetup => "needs_setup",
            Self::Ready => "ready",
        }
    }

    /// Derive the state from the persisted account list.
    ///
    /// The empty list is `Uninitialized` (nothing to log into *and* nothing
    /// to set up) — distinct from a list that holds only passwordless
    /// accounts, which is `NeedsSetup`.
    pub fn from_accounts(list: &AccountListFile) -> Self {
        if list.accounts.is_empty() {
            return Self::Uninitialized;
        }
        // One single predicate, shared with the login path. Keeping a second
        // ad-hoc "is this admin usable?" loop here drifted from
        // `is_login_capable()` on `disabled_at` and on malformed hashes,
        // which let `/health` and the CLI disagree about the same store.
        if list
            .accounts
            .iter()
            .any(|a| a.is_admin() && a.is_login_capable())
        {
            Self::Ready
        } else {
            // Accounts exist but nobody can log in: no admin at all, only
            // passwordless / disabled admins, or a store with unusable
            // credential records.
            Self::NeedsSetup
        }
    }

    /// Whether the first-boot gate is closed — no administrator can log in.
    pub fn requires_setup(self) -> bool {
        !matches!(self, Self::Ready)
    }
}

/// A user account (ADR-076 §决策 1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserAccount {
    // ── Identity ──
    /// Unique user identifier (UUID v4) — same semantics as
    /// `UserProfile.user_id`.
    pub user_id: String,
    /// Login handle: lowercase letters, digits, `-` and `_`.
    pub username: String,
    /// Display name — what the user wants to be called.
    pub display_name: String,
    /// Authorization role.
    #[serde(default)]
    pub role: Role,

    // ── Credentials ──
    /// Argon2id PHC string (`$argon2id$v=19$m=...,t=...,p=...$<salt>$<hash>`),
    /// or [`DISABLED_PASSWORD_HASH`] for a seeded, not-yet-activated
    /// account (ADR-114 first-boot setup). Stored
    /// independently of the Vault so login verification works while the
    /// Vault is locked.
    pub password_hash: String,
    /// When the password was last changed (ISO 8601).
    #[serde(default)]
    pub password_changed_at: String,
    /// When the password expires (ISO 8601); `None` = never. Recorded but
    /// not enforced in this iteration (see ADR-076 §5.5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_expires_at: Option<String>,

    // ── Presentation (mirrors `UserProfile`) ──
    pub language: String,
    pub timezone: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occupation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builtin_avatar: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub communication_style: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub custom: HashMap<String, String>,

    // ── Lifecycle ──
    /// When the account was created (ISO 8601).
    pub created_at: String,
    /// When the account was last modified (ISO 8601).
    pub updated_at: String,
    /// When the account last logged in (ISO 8601).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_login_at: Option<String>,
    /// Soft-delete marker (ISO 8601). A disabled account cannot log in but
    /// its `user_id` and session ownership are preserved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_at: Option<String>,

    // ── First-login invite (ADR-076 §决策 6) ──
    /// SHA-256 hex of the one-time `invite_token` an admin minted (via
    /// `POST /api/users/{id}/reset-password` or account creation without a
    /// password). `None` = no pending activation. Burned (set to `None`) the
    /// moment `POST /api/auth/first-login` consumes it, so it is single-use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invite_token_hash: Option<String>,
    /// When the pending `invite_token` expires (ISO 8601, 24h).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invite_expires_at: Option<String>,
}

impl UserAccount {
    /// Whether this account may log in with a password: it has a real,
    /// verifiable Argon2id hash (not the [`DISABLED_PASSWORD_HASH`] sentinel
    /// or a corrupt record) and is not disabled.
    pub fn is_login_capable(&self) -> bool {
        has_verifiable_password_hash(&self.password_hash) && self.disabled_at.is_none()
    }

    /// Whether this account is an administrator.
    pub fn is_admin(&self) -> bool {
        matches!(self.role, Role::Admin)
    }

    /// Build the public [`UserProfile`] view of this account.
    ///
    /// This is the shape pushed to Runtime as `last_user_profile` — it
    /// carries no credential material. `is_active` marks whether this
    /// account is the active/online user.
    pub fn to_public_profile(&self, is_active: bool) -> UserProfile {
        UserProfile {
            user_id: self.user_id.clone(),
            display_name: self.display_name.clone(),
            language: self.language.clone(),
            timezone: self.timezone.clone(),
            city: self.city.clone(),
            country: self.country.clone(),
            occupation: self.occupation.clone(),
            avatar: self.avatar.clone(),
            builtin_avatar: self.builtin_avatar.clone(),
            communication_style: self.communication_style.clone(),
            custom: self.custom.clone(),
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
            is_active,
        }
    }
}

/// Redacted account view (ADR-076 §决策 5 / §决策 6 `/api/auth/me`).
///
/// Everything a client is allowed to see about an account: identity,
/// role and presentation fields — no credential material, no password
/// timestamps. `accounts.json` keeps the full record; this is the shape
/// that crosses the HTTP boundary (and, later, the Tauri IPC boundary).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountView {
    pub user_id: String,
    pub username: String,
    pub display_name: String,
    pub role: Role,
    pub language: String,
    pub timezone: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occupation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builtin_avatar: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub communication_style: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub custom: HashMap<String, String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_login_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_at: Option<String>,
}

impl From<&UserAccount> for AccountView {
    fn from(a: &UserAccount) -> Self {
        Self {
            user_id: a.user_id.clone(),
            username: a.username.clone(),
            display_name: a.display_name.clone(),
            role: a.role,
            language: a.language.clone(),
            timezone: a.timezone.clone(),
            city: a.city.clone(),
            country: a.country.clone(),
            occupation: a.occupation.clone(),
            avatar: a.avatar.clone(),
            builtin_avatar: a.builtin_avatar.clone(),
            communication_style: a.communication_style.clone(),
            custom: a.custom.clone(),
            created_at: a.created_at.clone(),
            updated_at: a.updated_at.clone(),
            last_login_at: a.last_login_at.clone(),
            disabled_at: a.disabled_at.clone(),
        }
    }
}

/// Versioned account list persisted to disk (ADR-076 §决策 2).
///
/// Follows the same pattern as [`crate::protocol::UserProfileListFile`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AccountListFile {
    /// Monotonic version counter — bumped on every mutation.
    #[serde(default)]
    pub version: u64,
    /// All accounts (including soft-deleted, `disabled_at` set).
    #[serde(default)]
    pub accounts: Vec<UserAccount>,
}

impl AccountListFile {
    /// Look up an account by `user_id`.
    pub fn find(&self, user_id: &str) -> Option<&UserAccount> {
        self.accounts.iter().find(|a| a.user_id == user_id)
    }

    /// Look up an account by `username` (case-insensitive).
    pub fn find_by_username(&self, username: &str) -> Option<&UserAccount> {
        self.accounts
            .iter()
            .find(|a| a.username.eq_ignore_ascii_case(username))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> UserAccount {
        UserAccount {
            user_id: "u-1".into(),
            username: "alice".into(),
            display_name: "Alice".into(),
            role: Role::User,
            password_hash: "$argon2id$v=19$m=65536,t=3,p=4$c2FsdA$aGFzaA".into(),
            password_changed_at: "2026-10-15T00:00:00Z".into(),
            password_expires_at: None,
            language: "zh-CN".into(),
            timezone: "Asia/Shanghai".into(),
            city: Some("上海".into()),
            country: None,
            occupation: None,
            avatar: None,
            builtin_avatar: None,
            communication_style: None,
            custom: HashMap::new(),
            created_at: "2026-10-15T00:00:00Z".into(),
            updated_at: "2026-10-15T00:00:00Z".into(),
            last_login_at: None,
            disabled_at: None,
            invite_token_hash: None,
            invite_expires_at: None,
        }
    }

    #[test]
    fn sentinel_hash_is_not_login_capable() {
        let mut a = sample();
        assert!(a.is_login_capable());
        a.password_hash = DISABLED_PASSWORD_HASH.into();
        assert!(!a.is_login_capable());
    }

    #[test]
    fn disabled_account_is_not_login_capable() {
        let mut a = sample();
        a.disabled_at = Some("2026-10-16T00:00:00Z".into());
        assert!(!a.is_login_capable());
    }

    #[test]
    fn role_serde_roundtrip() {
        let json = serde_json::to_string(&Role::Admin).unwrap();
        assert_eq!(json, "\"admin\"");
        let back: Role = serde_json::from_str("\"user\"").unwrap();
        assert_eq!(back, Role::User);
        // Missing role defaults to User (legacy records).
        let a: UserAccount = serde_json::from_str(
            r#"{"user_id":"x","username":"y","display_name":"Y",
                "password_hash":"$disabled$","language":"en","timezone":"UTC",
                "created_at":"t","updated_at":"t"}"#,
        )
        .unwrap();
        assert_eq!(a.role, Role::User);
        assert_eq!(a.password_changed_at, "");
    }

    #[test]
    fn public_profile_carries_no_credentials() {
        let a = sample();
        let p = a.to_public_profile(true);
        assert_eq!(p.user_id, a.user_id);
        assert_eq!(p.display_name, a.display_name);
        assert!(p.is_active);
        // Serialized profile must not leak username / role / hash.
        let json = serde_json::to_string(&p).unwrap();
        assert!(!json.contains("password"));
        assert!(!json.contains("alice"));
        assert!(!json.contains("username"));
    }

    #[test]
    fn account_view_carries_no_credentials() {
        let a = sample();
        let view = AccountView::from(&a);
        assert_eq!(view.user_id, a.user_id);
        assert_eq!(view.username, a.username);
        assert_eq!(view.role, a.role);
        // The serialized view must not leak the hash or its timestamps.
        let json = serde_json::to_string(&view).unwrap();
        assert!(!json.contains("password"), "leaked password field: {json}");
        assert!(!json.contains("argon2"), "leaked hash: {json}");
    }

    #[test]
    fn account_list_lookup() {
        let list = AccountListFile {
            version: 1,
            accounts: vec![sample()],
        };
        assert!(list.find("u-1").is_some());
        assert!(list.find("nope").is_none());
        assert!(list.find_by_username("ALICE").is_some());
    }

    // ── AccountSystemState (ADR-114) ────────────────────────────────

    fn admin_with(hash: &str, disabled: bool) -> UserAccount {
        let mut a = sample();
        a.role = Role::Admin;
        a.password_hash = hash.into();
        a.disabled_at = disabled.then(|| "2026-10-16T00:00:00Z".into());
        a
    }

    #[test]
    fn empty_store_is_uninitialized() {
        let state = AccountSystemState::from_accounts(&AccountListFile::default());
        assert_eq!(state, AccountSystemState::Uninitialized);
        assert!(state.requires_setup());
    }

    #[test]
    fn passwordless_admin_is_needs_setup_not_uninitialized() {
        // The distinction the old boolean could not express: accounts DO
        // exist, they just cannot be logged into yet.
        let list = AccountListFile {
            version: 1,
            accounts: vec![admin_with(DISABLED_PASSWORD_HASH, false)],
        };
        let state = AccountSystemState::from_accounts(&list);
        assert_eq!(state, AccountSystemState::NeedsSetup);
        assert!(state.requires_setup());
    }

    #[test]
    fn admin_with_a_password_is_ready() {
        let list = AccountListFile {
            version: 1,
            accounts: vec![admin_with("$argon2id$v=19$m=1,t=1,p=1$salt$hash", false)],
        };
        let state = AccountSystemState::from_accounts(&list);
        assert_eq!(state, AccountSystemState::Ready);
        assert!(!state.requires_setup());
    }

    #[test]
    fn disabled_admin_does_not_count_as_ready() {
        let list = AccountListFile {
            version: 1,
            accounts: vec![admin_with("$argon2id$v=19$m=1,t=1,p=1$salt$hash", true)],
        };
        assert_eq!(
            AccountSystemState::from_accounts(&list),
            AccountSystemState::NeedsSetup
        );
    }

    /// Regression (ADR-114 review): `from_accounts()` used to treat *any*
    /// non-sentinel string as a usable password, so a corrupt record made
    /// the Gateway report `ready` while `verify_password` could never
    /// succeed — an unrecoverable lockout with no setup path.
    #[test]
    fn a_corrupt_hash_does_not_count_as_ready() {
        for hash in ["", "plaintext", DISABLED_PASSWORD_HASH, "$argon2id$"] {
            let list = AccountListFile {
                version: 1,
                accounts: vec![admin_with(hash, false)],
            };
            assert_eq!(
                AccountSystemState::from_accounts(&list),
                AccountSystemState::NeedsSetup,
                "hash = {hash:?}"
            );
            assert!(!list.accounts[0].is_login_capable(), "hash = {hash:?}");
        }
    }

    #[test]
    fn state_and_login_capability_agree_for_a_mixed_store() {
        // A disabled admin with a good hash plus a passwordless admin is
        // still `NeedsSetup`; a second, enabled admin makes it `Ready`.
        let mut disabled = admin_with("$argon2id$v=19$m=1,t=1,p=1$salt$hash", true);
        disabled.user_id = "u-disabled".into();
        let mut seeded = admin_with(DISABLED_PASSWORD_HASH, false);
        seeded.user_id = "u-seeded".into();
        let mut enabled = admin_with("$argon2id$v=19$m=1,t=1,p=1$salt$hash", false);
        enabled.user_id = "u-enabled".into();

        let list = AccountListFile {
            version: 1,
            accounts: vec![disabled.clone(), seeded.clone()],
        };
        assert_eq!(
            AccountSystemState::from_accounts(&list),
            AccountSystemState::NeedsSetup
        );
        assert!(!disabled.is_login_capable());
        assert!(!seeded.is_login_capable());

        let list = AccountListFile {
            version: 2,
            accounts: vec![disabled, seeded, enabled.clone()],
        };
        assert_eq!(
            AccountSystemState::from_accounts(&list),
            AccountSystemState::Ready
        );
        assert!(enabled.is_login_capable());
    }

    #[test]
    fn accounts_without_an_admin_cannot_log_in() {
        let list = AccountListFile {
            version: 1,
            accounts: vec![sample()],
        };
        assert_eq!(
            AccountSystemState::from_accounts(&list),
            AccountSystemState::NeedsSetup
        );
    }

    #[test]
    fn state_serde_uses_the_wire_spelling() {
        assert_eq!(
            serde_json::to_string(&AccountSystemState::Uninitialized).unwrap(),
            "\"uninitialized\""
        );
        assert_eq!(
            serde_json::to_string(&AccountSystemState::NeedsSetup).unwrap(),
            "\"needs_setup\""
        );
        assert_eq!(
            serde_json::to_string(&AccountSystemState::Ready).unwrap(),
            "\"ready\""
        );
        let back: AccountSystemState = serde_json::from_str("\"ready\"").unwrap();
        assert_eq!(back, AccountSystemState::Ready);
    }
}
