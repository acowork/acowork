//! Permission declaration and validation types
//!
//! Permissions follow a string-based format inspired by Android:
//! - `"network:https://api.weather.com"` — Network access with URL scope
//! - `"filesystem:read:~/Documents"` — Filesystem read with path scope
//! - `"filesystem:write:~/workdir"` — Filesystem write with path scope
//! - `"intent:send:com.example.calendar"` — Intent send
//! - `"intent:receive:com.example.weather"` — Intent receive
//! - `"identity:read"` / `"identity:write"` — Identity access
//! - `"shell"` — Shell command execution
//! - `"wasm"` — WASM tool execution
//! - `"rag:query"` — RAG tool query permission (Phase 4 S4.6)

use serde::{Deserialize, Deserializer, Serialize, Serializer};

// ── Permission binary encoding helpers (S5.5) ────────────────────────────

/// Permission type tags for compact binary encoding.
/// Each variant gets a unique 1-byte tag for efficient binary encoding
/// in the gateway transport.
#[repr(u8)]
enum PermissionTag {
    Network = 0,
    FilesystemRead = 1,
    FilesystemWrite = 2,
    // Tags 3 and 4 are retired, not renumbered (ADR-100 moved memory behind a hosted
    // service, so no permission gates it any more). Leaving the gap in place keeps
    // every other tag at its historic byte value.
    IntentSend = 5,
    IntentReceive = 6,
    IdentityRead = 7,
    IdentityWrite = 8,
    Shell = 9,
    Wasm = 10,
    RagQuery = 11,
}

impl Permission {
    fn tag(&self) -> u8 {
        match self {
            Permission::Network(_) => PermissionTag::Network as u8,
            Permission::FilesystemRead(_) => PermissionTag::FilesystemRead as u8,
            Permission::FilesystemWrite(_) => PermissionTag::FilesystemWrite as u8,
            Permission::IntentSend(_) => PermissionTag::IntentSend as u8,
            Permission::IntentReceive(_) => PermissionTag::IntentReceive as u8,
            Permission::IdentityRead => PermissionTag::IdentityRead as u8,
            Permission::IdentityWrite => PermissionTag::IdentityWrite as u8,
            Permission::Shell => PermissionTag::Shell as u8,
            Permission::Wasm => PermissionTag::Wasm as u8,
            Permission::RagQuery(_) => PermissionTag::RagQuery as u8,
        }
    }
}

// ── S5.5: Compact binary encoding for Permission ─────────────────────────

impl Permission {
    /// Encode this permission into a compact binary format.
    /// Format: [1 byte tag] [optional: 4 bytes len + UTF-8 value]
    pub fn encode_binary(&self, buf: &mut Vec<u8>) {
        buf.push(self.tag());
        if let Some(val) = self.type_value() {
            let len = val.len() as u32;
            buf.extend_from_slice(&len.to_le_bytes());
            buf.extend_from_slice(val.as_bytes());
        } else {
            buf.extend_from_slice(&0u32.to_le_bytes());
        }
    }

    /// Decode a permission from compact binary format.
    pub fn decode_binary(data: &[u8]) -> Result<(Self, usize), String> {
        if data.is_empty() {
            return Err("empty data".to_string());
        }
        let tag = data[0];
        if data.len() < 5 {
            return Err("truncated data".to_string());
        }
        let val_len = u32::from_le_bytes([data[1], data[2], data[3], data[4]]) as usize;
        let value = if val_len > 0 {
            if data.len() < 5 + val_len {
                return Err("truncated value".to_string());
            }
            Some(String::from_utf8(data[5..5 + val_len].to_vec()).map_err(|e| e.to_string())?)
        } else {
            None
        };
        let consumed = 5 + val_len;
        Ok((Permission::from_tag_simple(tag, value)?, consumed))
    }

    fn from_tag_simple(tag: u8, value: Option<String>) -> Result<Self, String> {
        match tag {
            0 => Ok(Permission::Network(value)),
            1 => Ok(Permission::FilesystemRead(value)),
            2 => Ok(Permission::FilesystemWrite(value)),
            5 => Ok(Permission::IntentSend(value)),
            6 => Ok(Permission::IntentReceive(value)),
            7 => Ok(Permission::IdentityRead),
            8 => Ok(Permission::IdentityWrite),
            9 => Ok(Permission::Shell),
            10 => Ok(Permission::Wasm),
            11 => Ok(Permission::RagQuery(value)),
            _ => Err(format!("Unknown permission tag: {tag}")),
        }
    }
}

/// Permission types that Agents can declare in manifest
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Permission {
    /// Network access with optional URL whitelist
    /// None = full network access granted; Some(url) = restricted to that URL
    /// e.g., "network:https://api.weather.com"
    Network(Option<String>),
    /// Filesystem read access with optional path restriction
    /// None = full filesystem read; Some(path) = restricted to that path
    /// e.g., "filesystem:read:~/Documents"
    FilesystemRead(Option<String>),
    /// Filesystem write access with optional path restriction
    /// None = full filesystem write; Some(path) = restricted to that path
    /// e.g., "filesystem:write:~/workdir"
    FilesystemWrite(Option<String>),
    /// Intent send to specific agent
    /// e.g., "intent:send:com.example.calendar"
    IntentSend(Option<String>),
    /// Intent receive from specific agent
    /// e.g., "intent:receive:com.example.weather"
    IntentReceive(Option<String>),
    /// Identity read access (query user identity fields)
    /// e.g., "identity:read"
    IdentityRead,
    /// Identity write access (store/update user identity fields)
    /// e.g., "identity:write"
    IdentityWrite,
    /// Shell command execution
    Shell,
    /// WASM tool execution
    Wasm,
    /// RAG tool query permission
    /// Required for agents with `[[tools]] type = "rag"` declaration.
    /// The optional scope is the RAG endpoint URL pattern.
    /// e.g., "rag:query" or "rag:query:https://rag.corp.example.com"
    RagQuery(Option<String>),
}

/// Error type for permission string parsing.
///
/// S5.2: Provides detailed error information when a permission string
/// cannot be parsed, including the invalid input and expected format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionParseError {
    /// The invalid input string.
    pub input: String,
    /// Human-readable description of the error.
    pub reason: String,
}

impl std::fmt::Display for PermissionParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Invalid permission '{}': {}", self.input, self.reason)
    }
}

impl std::error::Error for PermissionParseError {}

impl PermissionParseError {
    /// Create a new parse error with the invalid input and reason.
    fn new(input: &str, reason: &str) -> Self {
        Self {
            input: input.to_string(),
            reason: reason.to_string(),
        }
    }

    /// Create an error for an unknown permission category.
    fn unknown_category(input: &str, category: &str) -> Self {
        Self::new(
            input,
            &format!(
                "unknown category '{}'. Expected one of: network, filesystem, intent, identity, shell, wasm, rag",
                category
            ),
        )
    }

    /// Create an error for a missing component (e.g., no access type after category).
    fn missing_component(input: &str, expected: &str) -> Self {
        Self::new(
            input,
            &format!(
                "missing '{}'. Expected format: {}",
                expected,
                Self::expected_format(input)
            ),
        )
    }

    /// Create an error for an invalid sub-component value.
    #[allow(dead_code)]
    fn invalid_value(input: &str, component: &str, valid: &str) -> Self {
        Self::new(
            input,
            &format!("invalid {} '{}'. Valid values: {}", component, input, valid),
        )
    }

    /// Determine the expected format hint based on the input prefix.
    fn expected_format(input: &str) -> &'static str {
        if input.starts_with("filesystem:") {
            "filesystem:<read|write>:<path>"
        } else if input.starts_with("intent:") {
            "intent:<send|receive>:<target>"
        } else if input.starts_with("rag:") {
            "rag:query[:<url>]"
        } else {
            "<category>:<spec>"
        }
    }
}

impl Permission {
    /// Parse a permission string into a Permission enum.
    ///
    /// Returns `Result<Permission, PermissionParseError>` with detailed
    /// error information on failure.
    ///
    /// # Examples
    /// ```
    /// use acowork_core::permission::Permission;
    /// let p = Permission::parse("network:https://api.weather.com").unwrap();
    /// assert!(matches!(p, Permission::Network(Some(_))));
    ///
    /// let err = Permission::parse("invalid").unwrap_err();
    /// assert!(err.reason.contains("missing category delimiter"));
    ///
    /// let err = Permission::parse("foo:bar").unwrap_err();
    /// assert!(err.reason.contains("unknown category"));
    /// ```
    pub fn parse(s: &str) -> Result<Self, PermissionParseError> {
        // Handle simple single-word permissions first
        if s == "shell" {
            return Ok(Permission::Shell);
        }
        if s == "wasm" {
            return Ok(Permission::Wasm);
        }
        if s == "rag:query" {
            return Ok(Permission::RagQuery(None));
        }

        // Split on the first colon only to get the category
        let (category, rest) = s.split_once(':')
            .ok_or_else(|| PermissionParseError::new(s, "missing category delimiter ':'. Expected format: <category>:<spec> or 'shell'/'wasm'"))?;
        match category {
            "network" => Ok(Permission::Network(Some(rest.to_string()))),
            "filesystem" => {
                // Split rest on first colon: "read:~/Documents" or "write:~/workdir"
                let (access, path) = rest
                    .split_once(':')
                    .ok_or_else(|| PermissionParseError::missing_component(s, "access:path"))?;
                let path = Some(path.to_string());
                match access {
                    "read" => Ok(Permission::FilesystemRead(path)),
                    "write" => Ok(Permission::FilesystemWrite(path)),
                    other => Err(PermissionParseError::new(
                        s,
                        &format!(
                            "invalid filesystem access '{}'. Expected 'read' or 'write'",
                            other
                        ),
                    )),
                }
            }
            "intent" => {
                let (direction, target) = rest.split_once(':').ok_or_else(|| {
                    PermissionParseError::missing_component(s, "direction:target")
                })?;
                let target = Some(target.to_string());
                match direction {
                    "send" => Ok(Permission::IntentSend(target)),
                    "receive" => Ok(Permission::IntentReceive(target)),
                    other => Err(PermissionParseError::new(
                        s,
                        &format!(
                            "invalid intent direction '{}'. Expected 'send' or 'receive'",
                            other
                        ),
                    )),
                }
            }
            "identity" => match rest {
                "read" => Ok(Permission::IdentityRead),
                "write" => Ok(Permission::IdentityWrite),
                other => Err(PermissionParseError::new(
                    s,
                    &format!(
                        "invalid identity operation '{}'. Expected 'read' or 'write'",
                        other
                    ),
                )),
            },
            "rag" => match rest {
                "query" => Ok(Permission::RagQuery(None)),
                query_part => {
                    // "rag:query:https://rag.corp.example.com"
                    if let Some(url) = query_part.strip_prefix("query:") {
                        Ok(Permission::RagQuery(Some(url.to_string())))
                    } else {
                        Err(PermissionParseError::new(
                            s,
                            &format!(
                                "invalid rag operation '{}'. Expected 'query' or 'query:<url>'",
                                query_part
                            ),
                        ))
                    }
                }
            },
            other => Err(PermissionParseError::unknown_category(s, other)),
        }
    }

    /// Convert permission to string representation
    pub fn to_permission_string(&self) -> String {
        match self {
            Permission::Network(Some(url)) => format!("network:{url}"),
            Permission::Network(None) => "network".to_string(),
            Permission::FilesystemRead(Some(path)) => format!("filesystem:read:{path}"),
            Permission::FilesystemRead(None) => "filesystem:read".to_string(),
            Permission::FilesystemWrite(Some(path)) => format!("filesystem:write:{path}"),
            Permission::FilesystemWrite(None) => "filesystem:write".to_string(),
            Permission::IntentSend(Some(target)) => format!("intent:send:{target}"),
            Permission::IntentSend(None) => "intent:send".to_string(),
            Permission::IntentReceive(Some(source)) => format!("intent:receive:{source}"),
            Permission::IntentReceive(None) => "intent:receive".to_string(),
            Permission::IdentityRead => "identity:read".to_string(),
            Permission::IdentityWrite => "identity:write".to_string(),
            Permission::Shell => "shell".to_string(),
            Permission::Wasm => "wasm".to_string(),
            Permission::RagQuery(None) => "rag:query".to_string(),
            Permission::RagQuery(Some(url)) => format!("rag:query:{url}"),
        }
    }

    /// Check if this permission matches (covers) a requested permission.
    /// A broader permission (e.g., `Network(None)`) matches a narrower one
    /// (e.g., `Network(Some("https://api.weather.com"))`).
    ///
    /// Broad→narrow semantics: `Network(None)` = "all network access",
    /// so it covers any `Network(Some(_))`. Conversely, `Network(Some(url))`
    /// only covers the exact same URL or `Network(None)` is required for broader.
    pub fn matches(&self, requested: &Permission) -> bool {
        match (self, requested) {
            // Same type: broader scope (None) matches narrower scope (Some)
            (Permission::Network(None), Permission::Network(_)) => true,
            (Permission::Network(a), Permission::Network(b)) => a == b,
            (Permission::FilesystemRead(None), Permission::FilesystemRead(_)) => true,
            (Permission::FilesystemRead(a), Permission::FilesystemRead(b)) => a == b,
            (Permission::FilesystemWrite(None), Permission::FilesystemWrite(_)) => true,
            (Permission::FilesystemWrite(a), Permission::FilesystemWrite(b)) => a == b,
            (Permission::IntentSend(None), Permission::IntentSend(_)) => true,
            (Permission::IntentSend(a), Permission::IntentSend(b)) => a == b,
            (Permission::IntentReceive(None), Permission::IntentReceive(_)) => true,
            (Permission::IntentReceive(a), Permission::IntentReceive(b)) => a == b,
            (Permission::IdentityRead, Permission::IdentityRead) => true,
            (Permission::IdentityWrite, Permission::IdentityWrite) => true,
            (Permission::Shell, Permission::Shell) => true,
            (Permission::Wasm, Permission::Wasm) => true,
            (Permission::RagQuery(None), Permission::RagQuery(_)) => true,
            (Permission::RagQuery(a), Permission::RagQuery(b)) => a == b,
            _ => false,
        }
    }

    /// Get the category of this permission (e.g., "network", "filesystem")
    pub fn category(&self) -> &str {
        match self {
            Permission::Network(_) => "network",
            Permission::FilesystemRead(_) | Permission::FilesystemWrite(_) => "filesystem",
            Permission::IntentSend(_) | Permission::IntentReceive(_) => "intent",
            Permission::IdentityRead | Permission::IdentityWrite => "identity",
            Permission::Shell => "shell",
            Permission::Wasm => "wasm",
            Permission::RagQuery(_) => "rag",
        }
    }

    /// Get the type name for DB/serialization storage.
    /// Returns the PascalCase variant name (e.g., "Network", "FilesystemRead").
    pub fn type_name(&self) -> &str {
        match self {
            Permission::Network(_) => "Network",
            Permission::FilesystemRead(_) => "FilesystemRead",
            Permission::FilesystemWrite(_) => "FilesystemWrite",
            Permission::IntentSend(_) => "IntentSend",
            Permission::IntentReceive(_) => "IntentReceive",
            Permission::IdentityRead => "IdentityRead",
            Permission::IdentityWrite => "IdentityWrite",
            Permission::Shell => "Shell",
            Permission::Wasm => "Wasm",
            Permission::RagQuery(_) => "RagQuery",
        }
    }

    /// Get the scoped value for DB/serialization storage.
    /// Returns Some(value) for permissions with scope, None otherwise.
    pub fn type_value(&self) -> Option<&str> {
        match self {
            Permission::Network(v) => v.as_deref(),
            Permission::FilesystemRead(v) => v.as_deref(),
            Permission::FilesystemWrite(v) => v.as_deref(),
            Permission::IntentSend(v) => v.as_deref(),
            Permission::IntentReceive(v) => v.as_deref(),
            Permission::RagQuery(v) => v.as_deref(),
            Permission::IdentityRead
            | Permission::IdentityWrite
            | Permission::Shell
            | Permission::Wasm => None,
        }
    }
}

// Custom TOML serialization using tagged enum format
impl Serialize for Permission {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct PermissionRepr {
            #[serde(rename = "type")]
            perm_type: String,
            #[serde(skip_serializing_if = "Option::is_none")]
            value: Option<String>,
        }

        let repr = PermissionRepr {
            perm_type: self.type_name().to_string(),
            value: self.type_value().map(|s| s.to_string()),
        };
        repr.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Permission {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct PermissionRepr {
            #[serde(rename = "type")]
            perm_type: String,
            #[serde(default)]
            value: Option<String>,
        }

        let repr = PermissionRepr::deserialize(deserializer)?;
        match repr.perm_type.as_str() {
            "Network" => Ok(Permission::Network(repr.value)),
            "FilesystemRead" => Ok(Permission::FilesystemRead(repr.value)),
            "FilesystemWrite" => Ok(Permission::FilesystemWrite(repr.value)),
            "IntentSend" => Ok(Permission::IntentSend(repr.value)),
            "IntentReceive" => Ok(Permission::IntentReceive(repr.value)),
            "IdentityRead" => Ok(Permission::IdentityRead),
            "IdentityWrite" => Ok(Permission::IdentityWrite),
            "Shell" => Ok(Permission::Shell),
            "Wasm" => Ok(Permission::Wasm),
            "RagQuery" => Ok(Permission::RagQuery(repr.value)),
            other => Err(serde::de::Error::custom(format!(
                "Unknown permission type: {other}"
            ))),
        }
    }
}

// ── Shell Approval Threshold ─────────────────────────────────────────────

/// Threshold for shell command risk approval.
///
/// Controls which risk levels require user confirmation before execution.
/// Shared between Gateway (agent_config.rs) and Runtime (loop_tools.rs)
/// to avoid duplicating type definitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ShellApprovalThreshold {
    /// Prompt for Low risk and above (most conservative).
    Low,
    /// Prompt for Medium risk and above.
    Medium,
    /// Prompt for High risk only (default).
    #[default]
    High,
    /// Never prompt — auto-approve all shell commands.
    ///
    /// Serialized as "auto_approve"; accepts the legacy "never" spelling
    /// when reading existing agent_config.json / API payloads.
    #[serde(rename = "auto_approve", alias = "never")]
    AutoApprove,
}

impl ShellApprovalThreshold {
    /// Parse from string (case-insensitive). Returns None for unrecognized values.
    pub fn from_str_loose(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "auto_approve" | "never" => Some(Self::AutoApprove),
            _ => None,
        }
    }

    /// Convert to the label used in API/protocol.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::AutoApprove => "auto_approve",
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_permission_parse_network() {
        let p = Permission::parse("network:https://api.weather.com").unwrap();
        assert!(matches!(p, Permission::Network(Some(_))));
        assert_eq!(p.to_permission_string(), "network:https://api.weather.com");
    }

    #[test]
    fn test_permission_parse_filesystem() {
        let p = Permission::parse("filesystem:read:~/Documents").unwrap();
        assert!(matches!(p, Permission::FilesystemRead(Some(_))));

        let p2 = Permission::parse("filesystem:write:~/workdir").unwrap();
        assert!(matches!(p2, Permission::FilesystemWrite(Some(_))));
    }

    #[test]
    fn memory_permissions_are_retired_not_parsed() {
        // ADR-100: memory lives behind a hosted service now, so no permission gates it.
        // A package that still declares `memory:*` has to fail loudly at parse time
        // instead of carrying a permission that nothing reads any more.
        for input in ["memory:read", "memory:write"] {
            let err = Permission::parse(input).unwrap_err();
            assert!(err.reason.contains("unknown category"), "{input}");
        }
        assert!(serde_json::from_str::<Permission>(r#"{"type":"MemoryRead"}"#).is_err());
    }

    #[test]
    fn test_permission_parse_shell() {
        let p = Permission::parse("shell").unwrap();
        assert_eq!(p, Permission::Shell);
    }

    #[test]
    fn test_permission_parse_intent() {
        let p = Permission::parse("intent:send:com.example.calendar").unwrap();
        assert!(matches!(p, Permission::IntentSend(Some(_))));

        let p2 = Permission::parse("intent:receive:com.example.weather").unwrap();
        assert!(matches!(p2, Permission::IntentReceive(Some(_))));
    }

    #[test]
    fn test_permission_parse_invalid() {
        assert!(Permission::parse("invalid").is_err());
        assert!(Permission::parse("filesystem:execute").is_err());
    }

    #[test]
    fn test_permission_matches_broad_narrow() {
        // Broad permission (no scope) matches narrow (with scope)
        let broad = Permission::Network(None);
        let narrow = Permission::Network(Some("https://api.weather.com".into()));
        assert!(broad.matches(&narrow));

        // Narrow doesn't match broad
        assert!(!narrow.matches(&broad));

        // Exact match
        let same = Permission::Network(Some("https://api.weather.com".into()));
        assert!(narrow.matches(&same));
    }

    #[test]
    fn test_permission_matches_different_types() {
        let network = Permission::Network(None);
        let shell = Permission::Shell;
        assert!(!network.matches(&shell));
    }

    #[test]
    fn test_permission_toml_roundtrip() {
        let perms = vec![
            Permission::Network(Some("https://api.weather.com".into())),
            Permission::IdentityRead,
            Permission::Shell,
        ];
        // Use JSON for Vec roundtrip (TOML array-of-tables format is verbose)
        let json_str = serde_json::to_string(&perms).unwrap();
        let parsed: Vec<Permission> = serde_json::from_str(&json_str).unwrap();
        assert_eq!(perms, parsed);
    }

    #[test]
    fn test_permission_category() {
        assert_eq!(Permission::Network(None).category(), "network");
        assert_eq!(Permission::FilesystemRead(None).category(), "filesystem");
        assert_eq!(Permission::IntentSend(None).category(), "intent");
        assert_eq!(Permission::IdentityRead.category(), "identity");
        assert_eq!(Permission::Shell.category(), "shell");
        assert_eq!(Permission::Wasm.category(), "wasm");
    }

    // ── New Permission type tests ─────────────────────────────────────

    #[test]
    fn test_permission_parse_identity() {
        let p = Permission::parse("identity:read").unwrap();
        assert_eq!(p, Permission::IdentityRead);

        let p2 = Permission::parse("identity:write").unwrap();
        assert_eq!(p2, Permission::IdentityWrite);
    }

    #[test]
    fn test_permission_parse_wasm() {
        let p = Permission::parse("wasm").unwrap();
        assert_eq!(p, Permission::Wasm);
    }

    #[test]
    fn test_identity_wasm_toml_roundtrip() {
        let perms = vec![
            Permission::IdentityRead,
            Permission::IdentityWrite,
            Permission::Wasm,
        ];
        let json_str = serde_json::to_string(&perms).unwrap();
        let parsed: Vec<Permission> = serde_json::from_str(&json_str).unwrap();
        assert_eq!(perms, parsed);
    }

    // ── RagQuery permission tests (S4.6) ─────────────────────────────────

    #[test]
    fn test_permission_parse_rag_query() {
        let p = Permission::parse("rag:query").unwrap();
        assert_eq!(p, Permission::RagQuery(None));

        let p2 = Permission::parse("rag:query:https://rag.corp.example.com").unwrap();
        assert_eq!(
            p2,
            Permission::RagQuery(Some("https://rag.corp.example.com".into()))
        );
    }

    #[test]
    fn test_rag_query_permission_string() {
        assert_eq!(
            Permission::RagQuery(None).to_permission_string(),
            "rag:query"
        );
        assert_eq!(
            Permission::RagQuery(Some("https://rag.corp.example.com".into()))
                .to_permission_string(),
            "rag:query:https://rag.corp.example.com"
        );
    }

    #[test]
    fn test_rag_query_matches_broad_narrow() {
        let broad = Permission::RagQuery(None);
        let narrow = Permission::RagQuery(Some("https://rag.corp.example.com".into()));
        assert!(broad.matches(&narrow));
        assert!(!narrow.matches(&broad));
    }

    #[test]
    fn test_rag_query_toml_roundtrip() {
        let perms = vec![
            Permission::RagQuery(None),
            Permission::RagQuery(Some("https://rag.corp.example.com".into())),
        ];
        let json_str = serde_json::to_string(&perms).unwrap();
        let parsed: Vec<Permission> = serde_json::from_str(&json_str).unwrap();
        assert_eq!(perms, parsed);
    }

    #[test]
    fn test_rag_query_category() {
        assert_eq!(Permission::RagQuery(None).category(), "rag");
    }

    // ── S5.2: PermissionParseError tests ──────────────────────────────────

    #[test]
    fn test_parse_error_unknown_category() {
        let err = Permission::parse("foobar:baz").unwrap_err();
        assert!(err.reason.contains("unknown category"));
        assert!(err.reason.contains("foobar"));
        assert_eq!(err.input, "foobar:baz");
    }

    #[test]
    fn test_parse_error_missing_colon() {
        let err = Permission::parse("network").unwrap_err();
        assert!(err.reason.contains("missing category delimiter"));
    }

    #[test]
    fn test_parse_error_invalid_filesystem_access() {
        let err = Permission::parse("filesystem:execute:/tmp").unwrap_err();
        assert!(err.reason.contains("invalid filesystem access"));
        assert!(err.reason.contains("execute"));
    }

    #[test]
    fn test_parse_error_missing_path() {
        let err = Permission::parse("filesystem:read").unwrap_err();
        assert!(err.reason.contains("missing"));
    }

    #[test]
    fn test_parse_error_display() {
        let err = Permission::parse("foobar:baz").unwrap_err();
        let display = format!("{}", err);
        assert!(display.contains("foobar:baz"));
        assert!(display.contains("unknown category"));
    }

    #[test]
    fn test_shell_approval_threshold_parse() {
        use ShellApprovalThreshold as T;
        assert_eq!(T::from_str_loose("low"), Some(T::Low));
        assert_eq!(T::from_str_loose("medium"), Some(T::Medium));
        assert_eq!(T::from_str_loose("high"), Some(T::High));
        // New canonical spelling.
        assert_eq!(T::from_str_loose("auto_approve"), Some(T::AutoApprove));
        // Legacy spelling still accepted.
        assert_eq!(T::from_str_loose("never"), Some(T::AutoApprove));
        assert_eq!(T::from_str_loose("bogus"), None);
    }

    #[test]
    fn test_shell_approval_threshold_serde_roundtrip() {
        use ShellApprovalThreshold as T;
        // Canonical serialization.
        assert_eq!(
            serde_json::to_string(&T::AutoApprove).unwrap(),
            "\"auto_approve\""
        );
        // Legacy "never" still deserializes (serde alias).
        let parsed: T = serde_json::from_str("\"never\"").unwrap();
        assert_eq!(parsed, T::AutoApprove);
        // Canonical value round-trips.
        let parsed: T = serde_json::from_str("\"auto_approve\"").unwrap();
        assert_eq!(parsed, T::AutoApprove);
    }
}
