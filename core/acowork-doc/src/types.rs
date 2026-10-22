//! acowork-doc domain models — pure data, no behavior beyond serde + id gen.
//!
//! Mirrors design §3.2 (`library.json`) and §5.3 (`.requests/{id}.json`).
//! Serialization form hits disk verbatim — no transform layer in between.
//!
//! ## Layering (ADR-040 style)
//!
//! - `types.rs`  — data + id generation + cheap invariants
//! - `path.rs`   — canonicalization & id whitelist (next slice)
//! - `service/`  — traits + impls for CRUD, version concurrency, PR review
//! - `api/`      — HTTP handlers depend on service traits only

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ── Constants ─────────────────────────────────────────────────────────────

/// The root directory's special id — does **not** carry the `dir-` prefix.
pub const ROOT_DIR_ID: &str = "root";

pub const DOC_ID_PREFIX: &str = "doc-";
pub const DIR_ID_PREFIX: &str = "dir-";
pub const REQUEST_ID_PREFIX: &str = "r-";

fn default_schema_version() -> u32 {
    1
}

// ── ID type aliases ───────────────────────────────────────────────────────

pub type DocId = String;
pub type DirId = String;
pub type RequestId = String;

// ── ACL (design §9 node-level permissions) ────────────────────────────────

/// Permission tier on a library node. `Edit` implies `View`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AclRole {
    View,
    Edit,
}

impl AclRole {
    /// Wire name used in 403 `required` payloads and error messages.
    pub fn as_str(self) -> &'static str {
        match self {
            AclRole::View => "view",
            AclRole::Edit => "edit",
        }
    }
    /// `needed ⊑ held` check: an `Edit` grant satisfies a `View` demand,
    /// never the other way around.
    pub fn satisfies(self, needed: AclRole) -> bool {
        matches!(self, AclRole::Edit) || matches!(needed, AclRole::View)
    }
}

/// One access-control entry: a principal (user id, e.g. `u-alice`) and
/// the tier granted to it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Ace {
    pub principal: String,
    pub role: AclRole,
}

// ── `library.json` top-level ──────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LibraryIndex {
    pub dir_id: DirId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<DirId>,
    #[serde(default)]
    pub files: Vec<DocMeta>,
    #[serde(default)]
    pub dirs: Vec<DirMeta>,
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// Who created this directory (user id / actor string). `None` for
    /// pre-ACL libraries and synthesised entries — only admins may then
    /// manage its permissions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creator: Option<String>,
    /// Explicit ACL for this directory. `None` = inherit from the closest
    /// ancestor that sets one; no ancestor sets one = public (design §9).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acl: Option<Vec<Ace>>,
}

impl LibraryIndex {
    pub fn root() -> Self {
        Self {
            dir_id: ROOT_DIR_ID.to_string(),
            parent: None,
            files: vec![],
            dirs: vec![],
            schema_version: default_schema_version(),
            creator: None,
            acl: None,
        }
    }
}

// ── File metadata ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DocMeta {
    pub doc_id: DocId,
    pub name: String,
    pub version: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import: Option<ImportSource>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted: bool,
    /// Who created this document (user id / actor string). See
    /// [`LibraryIndex::creator`] for the `None` semantics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creator: Option<String>,
    /// Explicit ACL (same inheritance contract as
    /// [`LibraryIndex::acl`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acl: Option<Vec<Ace>>,
}

impl DocMeta {
    pub fn new(
        doc_id: DocId,
        name: String,
        import: Option<ImportSource>,
        now: DateTime<Utc>,
    ) -> Self {
        Self {
            doc_id,
            name,
            version: 1,
            import,
            created_at: now,
            updated_at: now,
            deleted: false,
            creator: None,
            acl: None,
        }
    }
}

// ── Directory metadata ────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DirMeta {
    pub dir_id: DirId,
    pub name: String,
    pub updated_at: DateTime<Utc>,
    pub deleted: bool,
    /// Creator of this dir (design §9). Display cache for tree responses —
    /// the canonical copy lives in the dir's own `library.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creator: Option<String>,
    /// Explicit ACL (design §9). Same display-cache caveat as `creator`:
    /// `AclService` reads/writes the canonical copy on the dir's own index
    /// and patches this entry so trees stay fresh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acl: Option<Vec<Ace>>,
}

// ── Import source ─────────────────────────────────────────────────────────

/// Records the provenance of an add-to-doc snapshot.
///
/// `instance_id` is the **runtime instance identity** (UUID, ADR-073) carried
/// over the `X-MCP-Actor` header by the Gateway catalog template
/// (`{instance_id}` substitution). It is the unique identifier for the
/// calling agent *instance* — not the package id (reverse-DNS), which can
/// legitimately appear in multiple installed instances.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImportSource {
    pub instance_id: String,
    pub workspace_path: String,
}

// ── Update request model ──────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateRequest {
    pub request_id: RequestId,
    pub doc_id: DocId,
    pub path: String,
    pub base_version: u64,
    pub content: String,
    pub submitted_by: String,
    pub status: RequestStatus,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewed_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewed_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_note: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RequestStatus {
    Pending,
    Approved,
    Rejected,
    Expired,
}

// ── Composite service-layer return values ────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DocRead {
    pub meta: DocMeta,
    pub content: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TreeNode {
    pub dir_id: DirId,
    pub name: String,
    pub path: String,
    pub files: Vec<DocMeta>,
    pub dirs: Vec<DirMeta>,
}

// ── Trash (design §3.3 "30 天后清理") ───────────────────────────────────

/// One entry in `.trash/` — covers both soft-deleted documents and
/// directory deletions (per-directory meta is one entry; per-file entries
/// stay flat inside `.trash/{timestamp}_{name}.md`).
///
/// A `trash_id` is generated server-side and **independent** of the
/// original `doc_id` — when the doc is restored under a new directory we
/// mint a fresh `doc_id`, so the trash key must not collide with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrashEntry {
    /// Unique id for this trash slot (`tr-` + 12 hex).
    pub trash_id: String,
    /// Original document id (None for directory-level entries).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub doc_id: Option<String>,
    /// Parent directory the document was deleted from — the restore
    /// target. `None` for the root.
    pub original_dir_id: String,
    /// Document title (filename stem) at deletion time.
    pub original_name: String,
    /// Trash slot deletion timestamp (UTC).
    pub deleted_at: DateTime<Utc>,
    /// File size in bytes (`0` for directory entries).
    pub file_size_bytes: u64,
    /// Creator snapshot at deletion time — re-applied on restore so a
    /// private node does not silently lose its owner (and thus its manage
    /// gate) across a delete→restore round-trip (design §9).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creator: Option<String>,
    /// Explicit-ACL snapshot at deletion time. `None` = the node was
    /// inheriting (restore re-creates it as inheriting); `Some(vec![])`
    /// = it was private. Re-applied onto the restored document so its
    /// visibility survives the round-trip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acl: Option<Vec<Ace>>,
}

/// Trash-specific id prefix.
pub const TRASH_ID_PREFIX: &str = "tr-";

/// Generate a fresh trash id.
pub fn generate_trash_id() -> String {
    format!("{}{}", TRASH_ID_PREFIX, short_uuid())
}

// ── Search (design §4 `GET /api/search`) ────────────────────────────────

/// One hit in a keyword search.
///
/// `score` is a coarse ranking: `title` match weights 10, each `content`
/// occurrence weights 1. `snippet` is the first content match with a
/// little surrounding context (UTF-8 safe — we slice on char boundaries).
#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub doc_id: String,
    pub name: String,
    /// Path relative to the library root (`项目A/PRD.md`).
    pub path: String,
    pub snippet: String,
    pub score: i32,
}

// ── ID generators ────────────────────────────────────────────────────────

const SHORT_ID_LEN: usize = 12;

fn short_uuid() -> String {
    Uuid::new_v4().simple().to_string()[..SHORT_ID_LEN].to_string()
}

pub fn generate_doc_id() -> DocId {
    format!("{}{}", DOC_ID_PREFIX, short_uuid())
}

pub fn generate_dir_id() -> DirId {
    format!("{}{}", DIR_ID_PREFIX, short_uuid())
}

pub fn generate_request_id() -> RequestId {
    format!("{}{}", REQUEST_ID_PREFIX, short_uuid())
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-08-30T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn library_index_root_roundtrip() {
        let idx = LibraryIndex::root();
        let json = serde_json::to_string(&idx).unwrap();
        let back: LibraryIndex = serde_json::from_str(&json).unwrap();
        assert_eq!(idx, back);
        assert_eq!(idx.dir_id, ROOT_DIR_ID);
        assert!(idx.parent.is_none());
        assert!(
            !json.contains("parent"),
            "parent=None should be skipped: {}",
            json
        );
    }

    #[test]
    fn library_index_full_matches_design_example() {
        let now = fixed_now();
        let idx = LibraryIndex {
            dir_id: "dir-2001".into(),
            parent: Some("root".into()),
            files: vec![DocMeta::new(
                "doc-1001".into(),
                "产品方案".into(),
                Some(ImportSource {
                    instance_id: "inst-2001-0000-0000-0000-aaaa".into(),
                    workspace_path: "notes/方案.md".into(),
                }),
                now,
            )],
            dirs: vec![DirMeta {
                dir_id: "dir-2002".into(),
                name: "项目A".into(),
                updated_at: now,
                deleted: false,
                creator: None,
                acl: None,
            }],
            schema_version: 1,
            creator: None,
            acl: None,
        };
        let json = serde_json::to_string(&idx).unwrap();
        let back: LibraryIndex = serde_json::from_str(&json).unwrap();
        assert_eq!(idx, back);
        assert!(json.contains("\"dir_id\":\"dir-2001\""), "{}", json);
        assert!(json.contains("\"name\":\"产品方案\""), "{}", json);
        assert!(json.contains("\"version\":1"), "{}", json);
        assert!(
            json.contains("\"instance_id\":\"inst-2001-0000-0000-0000-aaaa\""),
            "{}",
            json
        );
        // ADR-073: `agent_id` field name is gone — the wire format now keys
        // on `instance_id` (UUID). A regression that re-introduces the old
        // field name must fail this assertion so the wire contract stays
        // consistent across doc server + desktop + any future consumer.
        assert!(
            !json.contains("\"agent_id\":"),
            "wire JSON must not contain legacy `agent_id` key: {json}"
        );
        assert!(
            json.contains("\"workspace_path\":\"notes/方案.md\""),
            "{}",
            json
        );
        assert!(
            json.contains("\"created_at\":\"2026-08-30T10:00:00Z\""),
            "{}",
            json
        );
    }

    #[test]
    fn doc_meta_skips_none_import() {
        let meta = DocMeta::new("doc-1001".into(), "无来源".into(), None, fixed_now());
        let json = serde_json::to_string(&meta).unwrap();
        assert!(
            !json.contains("import"),
            "None import must be omitted: {}",
            json
        );
        let back: DocMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(meta, back);
        assert!(back.import.is_none());
    }

    #[test]
    fn request_status_serde_lowercase_all_variants() {
        assert_eq!(
            serde_json::to_string(&RequestStatus::Pending).unwrap(),
            "\"pending\""
        );
        assert_eq!(
            serde_json::to_string(&RequestStatus::Approved).unwrap(),
            "\"approved\""
        );
        assert_eq!(
            serde_json::to_string(&RequestStatus::Rejected).unwrap(),
            "\"rejected\""
        );
        assert_eq!(
            serde_json::to_string(&RequestStatus::Expired).unwrap(),
            "\"expired\""
        );
        for s in ["pending", "approved", "rejected", "expired"] {
            let r: RequestStatus = serde_json::from_str(&format!("\"{}\"", s)).unwrap();
            assert_eq!(serde_json::to_string(&r).unwrap(), format!("\"{}\"", s));
        }
    }

    #[test]
    fn update_request_roundtrip_matches_design_example() {
        let req = UpdateRequest {
            request_id: "r-001".into(),
            doc_id: "doc-1001".into(),
            path: "项目A/PRD.md".into(),
            base_version: 4,
            content: "# 新版本".into(),
            submitted_by: "agent:com.example.agent".into(),
            status: RequestStatus::Pending,
            created_at: fixed_now(),
            reviewed_at: None,
            reviewed_by: None,
            review_note: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: UpdateRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, back);
        assert!(json.contains("\"request_id\":\"r-001\""), "{}", json);
        assert!(json.contains("\"base_version\":4"), "{}", json);
        assert!(
            json.contains("\"submitted_by\":\"agent:com.example.agent\""),
            "{}",
            json
        );
        assert!(json.contains("\"status\":\"pending\""), "{}", json);
        assert!(!json.contains("reviewed_at"), "{}", json);
        assert!(!json.contains("reviewed_by"), "{}", json);
        assert!(!json.contains("review_note"), "{}", json);
    }

    #[test]
    fn update_request_with_review_fields_roundtrip() {
        let reviewed = DateTime::parse_from_rfc3339("2026-08-31T09:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let req = UpdateRequest {
            request_id: "r-002".into(),
            doc_id: "doc-1002".into(),
            path: "doc-1002.md".into(),
            base_version: 1,
            content: "fixed".into(),
            submitted_by: "agent:a".into(),
            status: RequestStatus::Approved,
            created_at: fixed_now(),
            reviewed_at: Some(reviewed),
            reviewed_by: Some("human:alice".into()),
            review_note: Some("LGTM".into()),
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: UpdateRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, back);
        assert!(json.contains("\"reviewed_at\":\"2026-08-31T09:00:00Z\""));
        assert!(json.contains("\"reviewed_by\":\"human:alice\""));
        assert!(json.contains("\"review_note\":\"LGTM\""));
        assert!(json.contains("\"status\":\"approved\""));
    }

    #[test]
    fn id_generators_use_correct_prefix_and_length() {
        for _ in 0..16 {
            let doc = generate_doc_id();
            assert!(doc.starts_with(DOC_ID_PREFIX), "{}", doc);
            assert_eq!(doc.len(), DOC_ID_PREFIX.len() + SHORT_ID_LEN, "{}", doc);

            let dir = generate_dir_id();
            assert!(dir.starts_with(DIR_ID_PREFIX), "{}", dir);
            assert_eq!(dir.len(), DIR_ID_PREFIX.len() + SHORT_ID_LEN, "{}", dir);

            let req = generate_request_id();
            assert!(req.starts_with(REQUEST_ID_PREFIX), "{}", req);
            assert_eq!(
                req.len(),
                REQUEST_ID_PREFIX.len() + SHORT_ID_LEN,
                "{}",
                req
            );

            assert!(
                doc[DOC_ID_PREFIX.len()..]
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "{}",
                doc
            );
        }
    }

    #[test]
    fn doc_read_and_tree_node_roundtrip() {
        let now = fixed_now();
        let dr = DocRead {
            meta: DocMeta::new("doc-1".into(), "t".into(), None, now),
            content: "body".into(),
            path: "t.md".into(),
        };
        let json = serde_json::to_string(&dr).unwrap();
        assert_eq!(dr, serde_json::from_str::<DocRead>(&json).unwrap());

        let tn = TreeNode {
            dir_id: "dir-1".into(),
            name: "n".into(),
            path: "n".into(),
            files: vec![],
            dirs: vec![],
        };
        let json = serde_json::to_string(&tn).unwrap();
        assert_eq!(tn, serde_json::from_str::<TreeNode>(&json).unwrap());
    }
}
