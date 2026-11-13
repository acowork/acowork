//! acowork-user service configuration.
//!
//! The account system is **always on** (ADR-114). The service owns the
//! account store, the signing key, the password policy, the bootstrap
//! administrator and the registration flag — everything the account
//! domain needs. There is no deployment mode to configure: the old
//! `--auth-mode` flag (and the `AuthMode` mirror of the Gateway's knob)
//! is gone, and with it the possibility of starting a service that serves
//! profiles but no accounts.
//!
//! Config precedence (highest first):
//!
//! 1. CLI flags (`--data-dir`, `--port`)
//! 2. TOML config file (`--config <path>`, default `./acowork-user.toml`)
//! 3. [`UserServiceConfig::default`]
//!
//! The data directory resolves to `$HOME/.acowork/acowork-user/`, peer of
//! `acowork-gateway/`, `acowork-node/`, `acowork-pm/` and `acowork-doc/`
//! (ADR-084 §3 goal 2).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::auth::service::{BootstrapAdmin, PasswordPolicy};

/// acowork-user service runtime config.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserServiceConfig {
    /// Data root: accounts, `user_profiles.json`, `users/` (chat trees),
    /// `assets/avatars/`, `auth/ed25519.{key,pub}`.
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,

    /// Standalone-process HTTP listen port. Default `18083`. On conflict the
    /// port auto-increments (max +20); the actual bound port is reported via
    /// `--port-file` to the Gateway supervisor.
    #[serde(default = "default_port")]
    pub port: u16,

    /// Gate for the process itself. The Gateway also gates the spawn via
    /// `[user].enabled`.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// First-boot administrator (ADR-076 §决策 5). Optional: when absent the
    /// service seeds a passwordless `admin` and reports the account system
    /// as `needs_setup`, so the operator (or the Desktop's onboarding flow)
    /// can create the first usable administrator.
    #[serde(default)]
    pub bootstrap_admin: Option<BootstrapAdmin>,

    /// Password policy enforced on change / bootstrap (ADR-076 §决策 6).
    #[serde(default)]
    pub password_policy: PasswordPolicy,

    /// Whether non-admins may self-register (ADR-076 §决策 6). Default
    /// `false`: only an admin creates accounts.
    #[serde(default)]
    pub registration_open: bool,
}

impl Default for UserServiceConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            port: default_port(),
            enabled: default_true(),
            bootstrap_admin: None,
            password_policy: PasswordPolicy::default(),
            registration_open: false,
        }
    }
}

impl UserServiceConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.port == 0 {
            return Err("port must be non-zero".to_string());
        }
        Ok(())
    }

    /// Load a TOML config file, layering it over the defaults.
    pub fn load_file(path: &Path) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read config {}: {e}", path.display()))?;
        toml::from_str(&raw).map_err(|e| format!("invalid config {}: {e}", path.display()))
    }
}

/// Default data directory: `$HOME/.acowork/acowork-user/` (ADR-084 §决策 5).
///
/// `ACOWORK_HOME` wins when set. That variable is what the Gateway's
/// `--home` writes into its own environment before spawning us, so a Gateway
/// booted with `--home X` keeps the whole install — including this store —
/// under `X` instead of reaching into the operator's real home. Without it
/// (the normal case) this is exactly the path the ADR specifies.
pub fn default_data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("ACOWORK_HOME")
        && !dir.is_empty()
    {
        return PathBuf::from(dir).join("acowork-user");
    }
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(|h| PathBuf::from(h).join(".acowork").join("acowork-user"))
        .unwrap_or_else(|_| PathBuf::from("./data/acowork-user"))
}

fn default_port() -> u16 {
    18083
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_dir_is_a_peer_of_the_other_services() {
        let dir = UserServiceConfig::default().data_dir;
        assert!(dir.ends_with("acowork-user"), "got {}", dir.display());
        assert!(
            dir.parent().is_some_and(|p| p.ends_with(".acowork")),
            "data dir must sit beside acowork-gateway/ (ADR-084 §3): {}",
            dir.display()
        );
    }

    #[test]
    fn default_port_matches_the_adr() {
        assert_eq!(UserServiceConfig::default().port, 18083);
    }

    /// ADR-114: the deployment-mode knob is gone from the service config
    /// too. `deny_unknown_fields` means a stale `auth_mode` key is now a
    /// *hard* config error rather than a silently ignored one — which is
    /// intentional: the value meant something different before and must not
    /// be honoured by accident.
    #[test]
    fn stale_deployment_mode_key_is_rejected() {
        let toml = r#"
port = 18083
auth_mode = "multi_user"
"#;
        let err = UserServiceConfig::load_file(Path::new("/nonexistent")).unwrap_err();
        assert!(err.contains("failed to read config"), "{err}");

        // And parsing such a document directly fails loudly.
        let parsed: Result<UserServiceConfig, _> = toml::from_str(toml);
        assert!(parsed.is_err(), "a stale auth_mode key must not parse");
    }
}
