//! Account store persistence (ADR-076 §决策 2).
//!
//! Accounts live in `data_dir/accounts.json` — an [`AccountListFile`],
//! the same versioned-list shape as `user_profiles.json`. The credential
//! (`password_hash`) is a one-way Argon2id PHC string, so the file is
//! kept unencrypted: login must work while the Vault is locked
//! (ADR-076 §决策 1).
//!
//! ADR-114: the account system is always on, so `accounts.json` is the
//! authoritative store in every deployment. `user_profiles.json` is a
//! derived presentation view rebuilt from it by `sync_profiles()`.

use acowork_core::account::AccountListFile;
use std::path::{Path, PathBuf};

/// Path of the account list file.
pub fn account_list_path(data_dir: &Path) -> PathBuf {
    data_dir.join("accounts.json")
}

/// Load the account list.
///
/// A **missing** file yields an empty list (fresh Gateway boots cleanly),
/// mirroring the `user_profiles.json` loader's tolerance. A **corrupt /
/// unreadable** file is *not* silently tolerated: the bad file is backed up
/// aside and an error is returned, so a later `save_accounts` can never
/// overwrite the only copy of the account list with an empty one (which
/// would lock every user out). Callers should treat the error as fatal at
/// startup.
pub fn load_accounts(data_dir: &Path) -> Result<AccountListFile, String> {
    let path = account_list_path(data_dir);
    match std::fs::read_to_string(&path) {
        Ok(raw) => match serde_json::from_str(&raw) {
            Ok(list) => Ok(list),
            Err(e) => {
                backup_corrupt(&path);
                Err(format!(
                    "Failed to parse {}: {e}. The corrupt file was preserved as \
                     accounts.json.corrupt-<ts>; refusing to boot with an empty \
                     account list.",
                    path.display()
                ))
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!("accounts.json not found, initializing empty");
            Ok(AccountListFile::default())
        }
        Err(e) => Err(format!("Failed to read {}: {e}", path.display())),
    }
}

/// Preserve a corrupt account list before the error propagates, so the
/// operator can recover the data instead of losing it to the next save.
fn backup_corrupt(path: &Path) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let backup = path.with_file_name(format!(
        "{}.corrupt-{ts}",
        path.file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default()
    ));
    match std::fs::copy(path, &backup) {
        Ok(_) => tracing::error!(
            path = %path.display(),
            backup = %backup.display(),
            "accounts.json is corrupt; original preserved for recovery"
        ),
        Err(copy_err) => tracing::error!(
            path = %path.display(),
            error = %copy_err,
            "accounts.json is corrupt AND backing it up failed; data may be unrecoverable"
        ),
    }
}

/// Atomically persist the account list.
///
/// Writes to a sibling temp file then renames over the target, so a crash
/// mid-write can never truncate the store (unlike a bare
/// `std::fs::write`, which the presentation cache can get away with but
/// losing a password hash would lock every user out).
pub fn save_accounts(data_dir: &Path, list: &AccountListFile) -> Result<(), String> {
    let path = account_list_path(data_dir);
    let json = serde_json::to_string_pretty(list)
        .map_err(|e| format!("Failed to serialize account list: {e}"))?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create account store dir: {e}"))?;
    }

    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json).map_err(|e| format!("Failed to write accounts.json.tmp: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("Failed to commit accounts.json: {e}")
    })?;

    tracing::info!(
        version = list.version,
        count = list.accounts.len(),
        "Account list saved"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use acowork_core::account::{Role, UserAccount};
    use std::collections::HashMap;

    fn tmp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("acowork-acct-test-{tag}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn account(id: &str) -> UserAccount {
        UserAccount {
            user_id: id.into(),
            username: "alice".into(),
            display_name: "Alice".into(),
            role: Role::Admin,
            password_hash: "$argon2id$v=19$m=8,t=1,p=1$c2FsdA$aGFzaA".into(),
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
            custom: HashMap::new(),
            created_at: "t".into(),
            updated_at: "t".into(),
            last_login_at: None,
            disabled_at: None,
            invite_token_hash: None,
            invite_expires_at: None,
        }
    }

    #[test]
    fn missing_file_loads_empty() {
        let dir = tmp_dir("missing");
        let list = load_accounts(&dir).unwrap();
        assert_eq!(list.version, 0);
        assert!(list.accounts.is_empty());
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = tmp_dir("roundtrip");
        let list = AccountListFile {
            version: 3,
            accounts: vec![account("u-1")],
        };
        save_accounts(&dir, &list).unwrap();

        let back = load_accounts(&dir).unwrap();
        assert_eq!(back.version, 3);
        assert_eq!(back.accounts.len(), 1);
        assert_eq!(back.accounts[0].username, "alice");
        assert!(back.accounts[0].is_admin());
    }

    #[test]
    fn save_is_atomic_no_tmp_left_behind() {
        let dir = tmp_dir("atomic");
        save_accounts(&dir, &AccountListFile::default()).unwrap();
        // The temp file must be renamed away, not left dangling.
        assert!(!account_list_path(&dir).with_extension("json.tmp").exists());
        assert!(account_list_path(&dir).exists());
    }

    #[test]
    fn corrupt_file_errors_and_backs_up_not_silent_empty() {
        let dir = tmp_dir("corrupt");
        std::fs::write(account_list_path(&dir), "{ not json").unwrap();

        // A corrupt store is a hard error — never a silent empty list.
        let err = load_accounts(&dir).unwrap_err();
        assert!(err.contains("Failed to parse"), "unexpected error: {err}");

        // The original bytes are preserved aside for operator recovery, so
        // a later save can never overwrite the only copy.
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        let backup = entries
            .iter()
            .find(|n| n.starts_with("accounts.json.corrupt-"))
            .expect("corrupt file must be backed up");
        let raw = std::fs::read_to_string(account_list_path(&dir).with_file_name(backup)).unwrap();
        assert_eq!(raw, "{ not json");
    }

    #[test]
    fn unreadable_file_is_an_error() {
        let dir = tmp_dir("unreadable");
        let path = account_list_path(&dir);
        std::fs::write(&path, "[]").unwrap();
        // Make the file unreadable; a directory as the read target also
        // fails, but a chmod is the clearest stand-in on unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
            assert!(load_accounts(&dir).is_err());
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
}
