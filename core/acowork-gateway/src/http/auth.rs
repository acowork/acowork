//! HTTP API authentication
//!
//! Phase 4 basic auth: optional Bearer token.
//! Token is generated on Gateway start and written to `http_token` file.

use secrecy::{ExposeSecret, SecretString};
use std::path::Path;

/// HTTP authentication manager
pub struct HttpAuth {
    /// Whether auth is enabled
    enabled: bool,
    /// The bearer token (only set when auth_enabled = true)
    token: Option<SecretString>,
}

impl HttpAuth {
    /// Create a new auth instance
    pub fn new(enabled: bool) -> Self {
        let token = if enabled {
            Some(Self::generate_token())
        } else {
            None
        };
        Self { enabled, token }
    }

    /// Generate a random 256-bit (32-byte) token as hex string
    fn generate_token() -> SecretString {
        use rand::RngExt;
        let bytes: [u8; 32] = rand::rng().random();
        let hex: String = bytes.iter().map(|b| format!("{:02x}", b)).collect();
        SecretString::new(hex.into_boxed_str())
    }

    /// Write the token to a file for Desktop App discovery
    pub fn write_token_file(&self, data_dir: &Path) -> std::io::Result<()> {
        if let Some(token) = &self.token {
            let token_path = data_dir.join("http_token");
            std::fs::write(&token_path, token.expose_secret())?;
            tracing::info!("HTTP auth token written to {}", token_path.display());
        }
        Ok(())
    }

    /// Validate a bearer token from an incoming request
    pub fn validate_token(&self, provided: &str) -> bool {
        if !self.enabled {
            return true; // Auth disabled — all requests pass
        }
        match &self.token {
            Some(expected) => {
                // Constant-time comparison to prevent timing attacks
                let expected_bytes = expected.expose_secret().as_bytes();
                let provided_bytes = provided.as_bytes();
                // Simple constant-time compare
                if expected_bytes.len() != provided_bytes.len() {
                    return false;
                }
                let mut result = 0u8;
                for (a, b) in expected_bytes.iter().zip(provided_bytes.iter()) {
                    result |= a ^ b;
                }
                result == 0
            }
            None => false, // Auth enabled but no token — should not happen
        }
    }

    /// Check if auth is enabled
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// The generated bearer token (None when auth is disabled).
    /// Exposed for the MQTT broker auth handler and `/api/status`
    /// (ADR-055 Phase 5a: Desktop uses it as its MQTT password).
    pub fn token(&self) -> Option<&str> {
        self.token.as_ref().map(|t| t.expose_secret())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auth_disabled_allows_all() {
        let auth = HttpAuth::new(false);
        assert!(!auth.is_enabled());
        assert!(auth.validate_token("anything"));
        assert!(auth.validate_token(""));
    }

    #[test]
    fn test_auth_enabled_validates_token() {
        let auth = HttpAuth::new(true);
        assert!(auth.is_enabled());
        // We don't know the exact token, but it should reject wrong ones
        assert!(!auth.validate_token("wrong-token"));
        assert!(!auth.validate_token(""));
    }

    #[test]
    fn test_auth_token_file_write() {
        let dir = crate::test_support::unique_temp_dir("auth-token");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let auth = HttpAuth::new(true);
        auth.write_token_file(&dir).unwrap();

        let token_path = dir.join("http_token");
        let content = std::fs::read_to_string(&token_path).unwrap();
        assert_eq!(content.len(), 64); // 32 bytes = 64 hex chars

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_auth_disabled_no_token_file() {
        let dir = crate::test_support::unique_temp_dir("auth-no-token");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let auth = HttpAuth::new(false);
        auth.write_token_file(&dir).unwrap();

        let token_path = dir.join("http_token");
        assert!(!token_path.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
