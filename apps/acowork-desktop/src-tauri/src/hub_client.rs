//! Agent Hub catalog client (`https://hub.acowork.ai`).
//!
//! Why the fetch lives on this side of the IPC boundary (PRD
//! docs/prd/zh/prd-agent-hub-install-dialog.md §5 D6 + §9 F1): the Hub is
//! served by Cloudflare Pages, which answers an unknown path with
//! `200 + HTML` — the SPA fallback. A webview `fetch` therefore cannot tell
//! "the catalog" from "a 200 that is not JSON" by status code alone. Here the
//! content-type is checked before the body is parsed, the package download is
//! sha256-verified against the catalog entry, and both requests carry an
//! explicit timeout.
//!
//! This module deliberately knows nothing about the Gateway: it only reads a
//! public static site. Submitting the downloaded package is the Gateway's job
//! (`gateway_client::ensure_agent` / `install_agent`).

use std::sync::OnceLock;

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Fallback Hub site root (R2).
const DEFAULT_HUB_BASE_URL: &str = "https://hub.acowork.ai";

/// Catalog document path relative to the site root.
const INDEX_PATH: &str = "data/index.json";

/// Catalog request timeout — a cold CDN round trip, nothing more.
const INDEX_TIMEOUT_SECS: u64 = 15;

/// Package download timeout. Packages are currently 5–86 KB (R5), so this is
/// a generous ceiling for a slow link rather than a large-file budget.
const PACKAGE_TIMEOUT_SECS: u64 = 60;

/// Hub site root. Kept as one function instead of a literal so the value can
/// be pointed at a staging site without touching the call sites.
pub fn hub_base_url() -> String {
    let raw = std::env::var("ACOWORK_HUB_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_HUB_BASE_URL.to_string());
    raw.trim_end_matches('/').to_string()
}

// ── Catalog contract ───────────────────────────────────────────────────────
//
// Field names mirror the published JSON (snake_case) so the IPC payload and
// the frontend types line up without a rename layer. Everything except the
// identity/package essentials is optional: the rule is "read, never guess"
// (PRD §8) — a missing field renders as absent, not as a inferred default.

/// `GET /data/index.json` — the whole registry for now (8 entries / ~26 KB).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubIndex {
    pub schema_version: u32,
    /// Deliberately NOT `#[serde(default)]`: a document that has no `agents`
    /// key (a renamed field, a future schema) is a shape we cannot interpret,
    /// not an empty library. REQ-012 requires "genuinely empty" and "this is
    /// not a catalog" to stay different states, and defaulting the field would
    /// collapse them into the first one.
    pub agents: Vec<HubAgent>,
    /// Site root the catalog was read from, injected after parsing.
    ///
    /// Not part of the published JSON: the webview needs it to resolve the
    /// site-relative `avatar` path into an `<img src>`, and hard-coding a
    /// second copy of the Hub origin on the display side would drift from
    /// `ACOWORK_HUB_URL` the moment the catalog is pointed at staging.
    #[serde(default)]
    pub base_url: String,
}

/// One published agent package.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubAgent {
    /// Package identity (NOT the instance id — ADR-073).
    pub agent_id: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<HubCapability>,
    #[serde(default)]
    pub skills: Vec<String>,
    /// Site-relative avatar path.
    #[serde(default)]
    pub avatar: Option<String>,
    #[serde(default)]
    pub permissions: Vec<String>,
    /// The subset of `permissions` the Hub classifies as high-risk; drives
    /// the pre-install confirmation (REQ-007).
    #[serde(default)]
    pub high_risk_permissions: Vec<String>,
    /// Absent only if the site publishes an entry it cannot install —
    /// surfaced as "no package", never as a guessed download.
    #[serde(default)]
    pub package: Option<HubPackage>,
    #[serde(default)]
    pub signature: Option<HubSignature>,
    /// Shape is not pinned by the published data yet, so it is passed through
    /// verbatim and interpreted on the display side.
    #[serde(default)]
    pub deprecated: Option<Value>,
}

/// A declared capability (`capabilities[].key` feeds the chips + search —
/// the published schema has no separate `capability_keys` field).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubCapability {
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub skills: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubPackage {
    /// Site-relative download path.
    pub file: String,
    #[serde(default)]
    pub size_bytes: u64,
    /// Lowercase hex sha256 of the package bytes (REQ-015).
    #[serde(default)]
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubSignature {
    /// Channel label: `official-channel` | `third-party`. Displayed as-is —
    /// it is the Hub's own statement, not something this app verified (R3).
    #[serde(default)]
    pub status: String,
}

/// Summary of a `.agent` file on the local disk, shown in the Local tab after
/// the user picks or drops one.
#[derive(Debug, Clone, Serialize)]
pub struct LocalPackageInfo {
    pub file_name: String,
    pub size_bytes: u64,
    /// Resolved from the embedded `manifest.toml` — the filename is never
    /// trusted (see `commands::agent::read_agent_id_from_package`).
    pub agent_id: String,
}

// ── HTTP ───────────────────────────────────────────────────────────────────

fn index_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(INDEX_TIMEOUT_SECS))
            .build()
            .expect("failed to build hub index client")
    })
}

fn package_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(PACKAGE_TIMEOUT_SECS))
            .build()
            .expect("failed to build hub package client")
    })
}

/// Fetch and parse the catalog. A parse failure is an error, never an empty
/// list — "no agents" and "the Hub answered something that is not the
/// catalog" are different states for the UI (REQ-012).
pub async fn fetch_index() -> Result<HubIndex> {
    let url = format!("{}/{}", hub_base_url(), INDEX_PATH);
    let resp = send_checked(index_client().get(&url), &url).await?;
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    ensure_json_content_type(&content_type, &url)?;
    let text = resp
        .text()
        .await
        .map_err(|e| anyhow!("Hub request failed ({url}): {e}"))?;
    let mut index: HubIndex = serde_json::from_str(&text)
        .map_err(|e| anyhow!("Hub returned a catalog that could not be parsed ({url}): {e}"))?;
    index.base_url = hub_base_url();
    Ok(index)
}

/// Download a package's bytes. The caller verifies them against the catalog's
/// `package.sha256` before anything is submitted to the Gateway.
pub async fn download_package(rel_path: &str) -> Result<Vec<u8>> {
    let url = join_asset_url(rel_path)?;
    let resp = send_checked(package_client().get(&url), &url).await?;
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    // Same fallback guard as the catalog: an unknown path answers 200 + HTML.
    if content_type.to_ascii_lowercase().contains("text/html") {
        bail!("Hub returned an HTML page instead of a package ({url})");
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| anyhow!("Hub download failed ({url}): {e}"))?;
    if bytes.is_empty() {
        bail!("Hub returned an empty package ({url})");
    }
    Ok(bytes.to_vec())
}

/// Shared send wrapper so both requests surface the URL in the error — the
/// failure is almost always DNS / TLS / offline, and the log has to say which
/// host gave up (PRD §10: Hub pull success rate is only observable from logs).
async fn send_checked(req: reqwest::RequestBuilder, url: &str) -> Result<reqwest::Response> {
    let resp = req
        .send()
        .await
        .map_err(|e| anyhow!("Hub request failed ({url}): {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        bail!("Hub answered HTTP {status} ({url})");
    }
    Ok(resp)
}

// ── Validation helpers ─────────────────────────────────────────────────────

/// Reject anything that is not JSON. Cloudflare's SPA fallback is a 200 HTML
/// page, so the status code alone proves nothing (F1).
fn ensure_json_content_type(content_type: &str, url: &str) -> Result<()> {
    let ct = content_type.to_ascii_lowercase();
    if ct.contains("application/json") || ct.contains("+json") {
        return Ok(());
    }
    bail!("Hub answered content-type \"{content_type}\" instead of JSON ({url})")
}

/// Join a site-relative asset path onto the Hub root.
///
/// Refuses anything that could redirect the request off the Hub origin: an
/// absolute URL, a protocol-relative `//host/...`, a backslash, a percent sign
/// or a `..` segment. A leading `/` is accepted and normalized.
///
/// The percent check is not paranoia about readability: the URL parser applies
/// WHATWG path normalization AFTER our literal segment scan, so `%2e%2e%2f`
/// would otherwise decode into a traversing `../`. The Hub publishes plain
/// paths, so anything encoded is rejected rather than interpreted.
pub fn join_asset_url(rel_path: &str) -> Result<String> {
    let rel = rel_path.trim();
    if rel.is_empty() {
        bail!("Empty Hub asset path");
    }
    let lower = rel.to_ascii_lowercase();
    if lower.contains("://") || rel.starts_with("//") || rel.contains('\\') || rel.contains('%') {
        bail!("Invalid Hub asset path: {rel_path}");
    }
    let trimmed = rel.trim_start_matches('/');
    if trimmed.split('/').any(|seg| seg == "..") {
        bail!("Hub asset path must not traverse the site root: {rel_path}");
    }
    Ok(format!("{}/{}", hub_base_url(), trimmed))
}

/// Verify the downloaded bytes against the catalog hash (REQ-015).
pub fn verify_sha256(bytes: &[u8], expected_hex: &str) -> Result<()> {
    let expected = expected_hex.trim().to_ascii_lowercase();
    if expected.is_empty() {
        bail!("Hub catalog carries no sha256 for this package — refusing to install unchecked");
    }
    if expected.len() != 64 || !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("Hub catalog carries a malformed sha256 (\"{expected_hex}\")");
    }
    let digest = Sha256::digest(bytes);
    let actual: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    if actual != expected {
        bail!(
            "Package checksum mismatch — downloaded bytes are not what the Hub catalog \
             describes (expected {expected}, got {actual}); nothing was submitted"
        );
    }
    Ok(())
}

/// Read a local `.agent` file's display summary. The extension check is the
/// user-facing half of REQ-011: a wrong file should fail here with a clear
/// message, not after a Gateway round trip.
pub fn inspect_local_package(path: &str) -> Result<LocalPackageInfo> {
    let file = std::path::Path::new(path);
    let ext_matches = file
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("agent"))
        .unwrap_or(false);
    if !ext_matches {
        bail!("Not an .agent package: {path}");
    }
    let bytes = std::fs::read(file).map_err(|e| anyhow!("Failed to read {path}: {e}"))?;
    let agent_id = crate::commands::agent::read_agent_id_from_package(&bytes)
        .map_err(|e| anyhow!("{e}"))?;
    Ok(LocalPackageInfo {
        file_name: file
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(path)
            .to_string(),
        size_bytes: bytes.len() as u64,
        agent_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_asset_url_normalizes_root_relative_paths() {
        // The published catalog uses site-relative paths without a leading
        // slash; a leading slash is equally valid and must not double up.
        let a = join_asset_url("downloads/com.acowork.software-architect.agent").unwrap();
        let b = join_asset_url("/downloads/com.acowork.software-architect.agent").unwrap();
        assert_eq!(a, b);
        assert!(a.starts_with("https://"));
        assert!(a.ends_with("/downloads/com.acowork.software-architect.agent"));
    }

    #[test]
    fn join_asset_url_refuses_paths_that_leave_the_hub_origin() {
        for bad in [
            "",
            "..",
            "downloads/../../etc/passwd",
            "https://evil.example/x.agent",
            "//evil.example/x.agent",
            "downloads\\..\\secrets.agent",
            // Percent-encoded traversal: the literal segment scan above runs
            // before the URL parser normalizes, so these must be rejected by
            // the encoding check rather than decoded and judged afterwards.
            "downloads/%2e%2e/secrets.agent",
            "downloads/..%2f..%2fsecrets.agent",
            "%2f%2fevil.example%2fx.agent",
        ] {
            assert!(
                join_asset_url(bad).is_err(),
                "expected rejection for {bad:?}"
            );
        }
    }

    #[test]
    fn index_without_the_agents_key_is_an_error_not_an_empty_library() {
        // REQ-012: "the Hub has nothing" and "this document is not a catalog"
        // must not collapse into the same state — a renamed or dropped field
        // has to surface as a retryable error.
        let json = r#"{ "schema_version": 2, "entries": [] }"#;
        assert!(serde_json::from_str::<HubIndex>(json).is_err());
        // An explicitly empty list still parses as the genuine empty state.
        let empty = r#"{ "schema_version": 2, "agents": [] }"#;
        assert!(serde_json::from_str::<HubIndex>(empty).unwrap().agents.is_empty());
    }

    #[test]
    fn verify_sha256_accepts_the_matching_digest_only() {
        let bytes = b"package-bytes";
        let expected = {
            let digest = Sha256::digest(bytes);
            digest.iter().map(|b| format!("{b:02x}")).collect::<String>()
        };
        assert!(verify_sha256(bytes, &expected).is_ok());
        // Uppercase hex from the catalog is still the same digest.
        assert!(verify_sha256(bytes, &expected.to_ascii_uppercase()).is_ok());
        assert!(verify_sha256(b"tampered", &expected).is_err());
    }

    #[test]
    fn verify_sha256_refuses_a_missing_or_malformed_digest() {
        assert!(verify_sha256(b"x", "").is_err());
        assert!(verify_sha256(b"x", "   ").is_err());
        assert!(verify_sha256(b"x", "deadbeef").is_err());
        assert!(verify_sha256(b"x", &format!("{}z", "0".repeat(63))).is_err());
    }

    #[test]
    fn content_type_gate_rejects_the_spa_fallback() {
        assert!(ensure_json_content_type("application/json", "u").is_ok());
        assert!(ensure_json_content_type("application/json; charset=utf-8", "u").is_ok());
        assert!(ensure_json_content_type("application/geo+json", "u").is_ok());
        // The exact shape Cloudflare Pages returns for an unknown path.
        assert!(ensure_json_content_type("text/html; charset=utf-8", "u").is_err());
        assert!(ensure_json_content_type("", "u").is_err());
    }

    #[test]
    fn index_fixture_parses_with_optional_fields_absent() {
        // The contract is read-only: a minimal entry (id + package) must still
        // parse, and every absent optional must land at its default instead of
        // failing the whole catalog.
        let json = r#"{
            "schema_version": 2,
            "unknown_facets": { "category": [] },
            "agents": [
                {
                    "agent_id": "com.acowork.minimal",
                    "package": { "file": "downloads/com.acowork.minimal.agent", "size_bytes": 12, "sha256": "ab" }
                },
                {
                    "agent_id": "com.acowork.full",
                    "version": "1.0.0",
                    "display_name": "Full",
                    "labels": ["architecture"],
                    "capabilities": [{ "key": "risk", "skills": ["risk-skill"] }],
                    "permissions": ["Shell"],
                    "high_risk_permissions": ["Shell"],
                    "signature": { "status": "official-channel" },
                    "deprecated": { "reason": "superseded" },
                    "package": { "file": "downloads/com.acowork.full.agent" }
                }
            ]
        }"#;
        let index: HubIndex = serde_json::from_str(json).unwrap();
        assert_eq!(index.schema_version, 2);
        assert_eq!(index.agents.len(), 2);
        // `base_url` is injected by `fetch_index`, never read from the JSON.
        assert_eq!(index.base_url, "");

        let minimal = &index.agents[0];
        assert_eq!(minimal.display_name, "");
        assert!(minimal.permissions.is_empty());
        assert!(minimal.signature.is_none());
        assert_eq!(minimal.package.as_ref().unwrap().size_bytes, 12);

        let full = &index.agents[1];
        assert_eq!(full.capabilities[0].key, "risk");
        assert_eq!(full.high_risk_permissions, vec!["Shell".to_string()]);
        assert_eq!(
            full.signature.as_ref().unwrap().status,
            "official-channel"
        );
        assert_eq!(
            full.deprecated
                .as_ref()
                .and_then(|v| v.get("reason"))
                .and_then(|v| v.as_str()),
            Some("superseded")
        );
        // `size_bytes` / `sha256` default rather than break the entry.
        assert_eq!(full.package.as_ref().unwrap().size_bytes, 0);
    }

    #[test]
    fn local_package_inspection_rejects_other_extensions() {
        let dir = std::env::temp_dir().join("acowork-hub-client-inspect-test");
        std::fs::create_dir_all(&dir).unwrap();
        let not_agent = dir.join("notes.txt");
        std::fs::write(&not_agent, b"hello").unwrap();
        let err = inspect_local_package(not_agent.to_str().unwrap()).unwrap_err();
        assert!(err.to_string().contains("Not an .agent package"));

        let fake_agent = dir.join("fake.agent");
        std::fs::write(&fake_agent, b"not a zip").unwrap();
        // Right extension, broken package: fails on the manifest read, which is
        // the point — the filename is never trusted.
        assert!(inspect_local_package(fake_agent.to_str().unwrap()).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
