//! Argon2id password hashing for user accounts (ADR-076 §决策 1).
//!
//! Uses the same KDF family as the Vault (ADR-059) — a single cryptographic
//! surface — but produces self-describing PHC strings
//! (`$argon2id$v=19$m=...,t=...,p=...$<salt>$<hash>`) so verification needs
//! no out-of-band parameters: the cost parameters, salt and algorithm are
//! all read back from the stored string.
//!
//! The hash is one-way, so it can be stored unencrypted — this is exactly
//! what lets login work while the Vault is locked (ADR-076 §决策 1).

use acowork_core::account::DISABLED_PASSWORD_HASH;
use argon2::password_hash::{
    PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng,
};
use argon2::{Algorithm, Argon2, Params, Version};

/// Production Argon2id parameters — identical to the Vault's
/// (m = 64 MiB, t = 3, p = 4, 32-byte output).
pub fn default_params() -> Params {
    Params::new(65_536, 3, 4, Some(32)).expect("static argon2 params are valid")
}

/// Hash a password into a PHC string with the production parameters.
pub fn hash_password(password: &str) -> Result<String, String> {
    hash_password_with(password, default_params())
}

/// Hash a password with explicit parameters.
///
/// Tests pass weak parameters for speed; production callers use
/// [`hash_password`]. Because the parameters are embedded in the returned
/// PHC string, [`verify_password`] never needs to know them.
pub fn hash_password_with(password: &str, params: Params) -> Result<String, String> {
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    argon2
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| format!("argon2 hash failed: {e}"))
}

/// Verify `password` against a stored PHC string.
///
/// Returns `Ok(false)` for a correct-shape-but-wrong password, and for the
/// [`DISABLED_PASSWORD_HASH`] sentinel (a seeded, not-yet-activated account
/// cannot log in until first-boot setup gives it a real password).
/// Returns `Err` only when the stored string is unparseable — i.e. the
/// account store is corrupt.
pub fn verify_password(password: &str, stored_phc: &str) -> Result<bool, String> {
    if stored_phc == DISABLED_PASSWORD_HASH {
        return Ok(false);
    }
    let parsed = PasswordHash::new(stored_phc)
        .map_err(|e| format!("invalid stored password hash: {e}"))?;
    // `Argon2::default()` re-reads the algorithm + cost params from `parsed`,
    // and the crate compares in constant time.
    Ok(Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Weak params so the test suite stays fast. Verification reads the
    /// cost params back out of the PHC string, so this exercises the same
    /// code path as production.
    fn weak() -> Params {
        Params::new(8, 1, 1, Some(32)).unwrap()
    }

    #[test]
    fn hash_verify_roundtrip() {
        let phc = hash_password_with("correct horse", weak()).unwrap();
        assert!(phc.starts_with("$argon2id$"));
        assert!(verify_password("correct horse", &phc).unwrap());
        assert!(!verify_password("wrong horse", &phc).unwrap());
    }

    #[test]
    fn salt_is_random_per_hash() {
        let a = hash_password_with("same", weak()).unwrap();
        let b = hash_password_with("same", weak()).unwrap();
        assert_ne!(a, b, "each hash must use a fresh salt");
        assert!(verify_password("same", &a).unwrap());
        assert!(verify_password("same", &b).unwrap());
    }

    #[test]
    fn disabled_sentinel_never_verifies() {
        assert!(!verify_password("anything", DISABLED_PASSWORD_HASH).unwrap());
        assert!(!verify_password("", DISABLED_PASSWORD_HASH).unwrap());
    }

    #[test]
    fn corrupt_hash_is_an_error() {
        assert!(verify_password("x", "not-a-phc-string").is_err());
    }

    #[test]
    fn production_params_differ_from_weak() {
        // Guard against accidentally shipping the test parameters.
        assert!(default_params().m_cost() >= 65_536);
    }
}
