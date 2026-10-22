//! `AclService` — node-level view/edit permissions (design §9).
//!
//! Model (locked with the product owner):
//! - Two tiers, `view` and `edit`; `edit` implies `view`.
//! - A principal is a user id (`u-…`). Directory ACLs live on the
//!   directory's own `library.json` ([`crate::types::LibraryIndex::acl`]),
//!   document ACLs on [`crate::types::DocMeta::acl`].
//! - `acl: None` = inherit from the closest ancestor that sets one; no
//!   ancestor sets one = **public** — pre-ACL libraries keep working with
//!   zero migration.
//! - Management (reading/writing a node's ACL) is restricted to the node's
//!   **creator** and **admins** — a granted `edit` alone does not allow
//!   re-sharing.
//!
//! Enforcement scope: the REST surface (human users). Trusted MCP agents
//! keep the pre-ACL behavior in v1 (their access is governed by the agent
//! install boundary); anonymous MCP callers stay read-only as before.
//! Local single-user mode (`X-Actor: human`) is always allowed — the whole
//! account system is off there (ADR-076 §决策 12).

use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::error::{DocError, Result};
use crate::store::library::LibraryStore;
use crate::types::{Ace, AclRole, DocMeta, TrashEntry, TreeNode};

/// The legacy constant actor injected under `local` mode — never gated.
pub const LOCAL_HUMAN_ACTOR: &str = "human";

/// Safety cap for the ancestor walk: a corrupt `parent` cycle must not
/// hang a request.
const MAX_DEPTH: usize = 64;

/// The caller identity as resolved by the API layer from the trusted
/// headers the Gateway reverse proxy injects (`X-Actor` / `X-Actor-Role`).
#[derive(Debug, Clone)]
pub struct ActorCtx {
    /// Trusted user id, `human`, or `None` (no header — direct dev access).
    pub id: Option<String>,
    /// `X-Actor-Role: admin` — admins bypass every check below.
    pub is_admin: bool,
}

impl ActorCtx {
    /// Whether ACL enforcement applies to this caller at all. Missing
    /// identity and the `local` `human` constant both mean legacy
    /// allow-all, so a single-machine deployment is never locked out.
    fn enforce(&self) -> bool {
        match &self.id {
            Some(id) => id != LOCAL_HUMAN_ACTOR,
            None => false,
        }
    }

    fn id_str(&self) -> &str {
        self.id.as_deref().unwrap_or("<anonymous>")
    }
}

/// Which library node an ACL check targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AclTarget<'a> {
    Doc(&'a str),
    Dir(&'a str),
}

/// Resolution result for one node.
#[derive(Debug, Clone, Default)]
pub struct Effective {
    /// ACL set directly on this node (`None` = inheriting).
    pub explicit: Option<Vec<Ace>>,
    /// ACL in force for this node: own explicit value, else the closest
    /// ancestor's; `None` = public.
    pub effective: Option<Vec<Ace>>,
    /// Creator recorded on this node itself (not inherited).
    pub creator: Option<String>,
}

/// GET `/api/permissions/*` response payload.
#[derive(Debug, Serialize, Deserialize)]
pub struct PermissionsView {
    /// The node's own explicit ACL — what the editor binds to.
    pub explicit: Option<Vec<Ace>>,
    /// The ACL actually in force (inherited resolution applied).
    pub effective: Option<Vec<Ace>>,
    /// Whether the calling actor may change this node's ACL.
    pub can_manage: bool,
    /// Creator of the node (display hint for the UI).
    pub creator: Option<String>,
}

/// Resolve + enforce node ACLs against the on-disk library indexes.
pub struct AclService {
    store: LibraryStore,
}

impl AclService {
    pub fn new(store: LibraryStore) -> Self {
        Self { store }
    }

    // ── Resolution ────────────────────────────────────────────────────

    /// Effective ACL for a directory: its own `library.json` `acl`,
    /// else the closest ancestor's (walk up via `parent`).
    pub async fn dir_effective(&self, dir_id: &str) -> Result<Effective> {
        let idx = self.store.load(dir_id).await?;
        let mut eff = Effective {
            explicit: idx.acl.clone(),
            effective: idx.acl.clone(),
            creator: idx.creator.clone(),
        };
        let mut cursor = idx.parent.clone();
        let mut depth = 0usize;
        while eff.effective.is_none() {
            let Some(parent_id) = cursor else { break };
            let parent_idx = self.store.load(&parent_id).await?;
            if parent_idx.acl.is_some() {
                eff.effective = parent_idx.acl.clone();
            }
            cursor = parent_idx.parent.clone();
            depth += 1;
            if depth > MAX_DEPTH {
                return Err(DocError::CorruptIndex(format!(
                    "parent cycle detected above dir {dir_id}"
                )));
            }
        }
        Ok(eff)
    }

    /// Effective ACL for a document: own `DocMeta.acl`, else its
    /// directory chain's resolution.
    pub async fn doc_effective(&self, doc_id: &str) -> Result<Effective> {
        let (parent_dir_id, idx) = self.store.locate_doc(doc_id).await?;
        let entry = idx
            .files
            .iter()
            .find(|f| f.doc_id == doc_id && !f.deleted)
            .ok_or_else(|| DocError::DocNotFound(doc_id.to_string()))?;
        let mut eff = Effective {
            explicit: entry.acl.clone(),
            effective: entry.acl.clone(),
            creator: entry.creator.clone(),
        };
        if eff.effective.is_none() {
            eff.effective = self.dir_effective(&parent_dir_id).await?.effective;
        }
        Ok(eff)
    }

    pub async fn effective_of(&self, target: AclTarget<'_>) -> Result<Effective> {
        match target {
            AclTarget::Doc(id) => self.doc_effective(id).await,
            AclTarget::Dir(id) => self.dir_effective(id).await,
        }
    }

    // ── Grant helpers ─────────────────────────────────────────────────

    /// Tier test against one ACL list: any ace for `actor` whose role
    /// satisfies `need`.
    fn grants(acl: Option<&Vec<Ace>>, actor: &str, need: AclRole) -> bool {
        acl.is_some_and(|list| {
            list.iter()
                .any(|ace| ace.principal == actor && ace.role.satisfies(need))
        })
    }

    /// Public (no effective ACL anywhere on the chain) or explicitly
    /// granted — also passes when the actor is the node's creator.
    fn allows(eff: &Effective, actor: Option<&str>, need: AclRole) -> bool {
        let Some(actor) = actor else {
            return false;
        };
        if eff.effective.is_none() {
            return true; // public
        }
        if eff.creator.as_deref() == Some(actor) {
            return true; // creators never lock themselves out of their node
        }
        Self::grants(eff.effective.as_ref(), actor, need)
    }

    // ── Enforcement ───────────────────────────────────────────────────

    /// Gate a read (`view`) or write (`edit`) against the node's
    /// effective ACL. See the module docs for the bypass rules.
    pub async fn check(&self, actor: &ActorCtx, target: AclTarget<'_>, need: AclRole) -> Result<()> {
        if !actor.enforce() || actor.is_admin {
            return Ok(());
        }
        let eff = self.effective_of(target).await?;
        if Self::allows(&eff, actor.id.as_deref(), need) {
            return Ok(());
        }
        Err(DocError::AclDenied {
            actor: actor.id_str().to_string(),
            required: need.as_str(),
        })
    }

    /// Gate an ACL read/write itself: creator ∨ admin ∨ local human.
    /// A plain `edit` grant is deliberately *not* enough — sharing is an
    /// ownership right (mirrors ADR-087 D9: guests may manage content but
    /// not re-attribute).
    pub async fn check_manage(&self, actor: &ActorCtx, target: AclTarget<'_>) -> Result<()> {
        if !actor.enforce() || actor.is_admin {
            return Ok(());
        }
        let eff = self.effective_of(target).await?;
        if eff.creator.as_deref() == actor.id.as_deref() && actor.id.is_some() {
            return Ok(());
        }
        Err(DocError::AclDenied {
            actor: actor.id_str().to_string(),
            required: "manage",
        })
    }

    // ── Visibility filtering ──────────────────────────────────────────

    /// Drop tree children the actor cannot `view`. Files test their own
    /// `DocMeta.acl` (else the requested dir's effective ACL); sub-dirs
    /// additionally load their own index to find an explicit override.
    /// The requested dir itself must already have passed [`Self::check`].
    pub async fn filter_tree(&self, actor: &ActorCtx, tree: &mut TreeNode) -> Result<()> {
        if !actor.enforce() || actor.is_admin {
            return Ok(());
        }
        let dir_eff = self.dir_effective(&tree.dir_id).await?;
        tree.files.retain(|f| file_visible(f, &dir_eff, actor));
        let store = self.store.clone();
        let mut visible_dirs = Vec::with_capacity(tree.dirs.len());
        for d in std::mem::take(&mut tree.dirs) {
            let (own_acl, own_creator) = match store.load(&d.dir_id).await {
                Ok(idx) => (idx.acl, idx.creator),
                // Unreadable child index: keep it (the dir entry exists in
                // the parent index; failing the whole tree on one corrupt
                // child would hide everything).
                Err(_) => (None, None),
            };
            let visible = match &own_acl {
                None => {
                    // Inherits the parent's effective ACL, but the child's
                    // own creator still passes (mirrors `dir_effective`).
                    let eff = Effective {
                        explicit: None,
                        effective: dir_eff.effective.clone(),
                        creator: own_creator,
                    };
                    Self::allows(&eff, actor.id.as_deref(), AclRole::View)
                }
                Some(acl) => {
                    let eff = Effective {
                        explicit: Some(acl.clone()),
                        effective: Some(acl.clone()),
                        // The child dir's own creator still passes —
                        // same rule as everywhere else.
                        creator: own_creator,
                    };
                    Self::allows(&eff, actor.id.as_deref(), AclRole::View)
                }
            };
            if visible {
                visible_dirs.push(d);
            }
        }
        tree.dirs = visible_dirs;
        Ok(())
    }

    /// Drop documents the actor cannot `view` (same rule as the file
    /// branch of [`Self::filter_tree`], without the dir loads).
    pub async fn filter_docs(&self, actor: &ActorCtx, dir_id: &str, files: &mut Vec<DocMeta>) {
        if !actor.enforce() || actor.is_admin {
            return;
        }
        let dir_eff = match self.dir_effective(dir_id).await {
            Ok(eff) => eff,
            Err(_) => {
                files.clear(); // fail closed — the dir itself is broken
                return;
            }
        };
        files.retain(|f| file_visible(f, &dir_eff, actor));
    }

    /// Whether a search hit (doc id) is visible to `actor`. Errors resolve
    /// to "not visible" (fail closed for auxiliary listings).
    pub async fn can_view_doc(&self, actor: &ActorCtx, doc_id: &str) -> bool {
        if !actor.enforce() || actor.is_admin {
            return true;
        }
        match self.doc_effective(doc_id).await {
            Ok(eff) => Self::allows(&eff, actor.id.as_deref(), AclRole::View),
            Err(_) => false,
        }
    }

    /// Whether `actor` satisfies `need` for a trashed slot. A deleted
    /// document no longer resolves through the live index (`locate_doc`
    /// skips `deleted` entries), so visibility is recomputed from the
    /// slot's snapshotted ACL — falling back to the original directory's
    /// effective ACL when the node itself had no explicit one. Fails closed
    /// if that directory can no longer be read.
    pub async fn check_trashed(
        &self,
        actor: &ActorCtx,
        e: &TrashEntry,
        need: AclRole,
    ) -> Result<()> {
        if !actor.enforce() || actor.is_admin {
            return Ok(());
        }
        let eff = match &e.acl {
            Some(acl) => Effective {
                explicit: Some(acl.clone()),
                effective: Some(acl.clone()),
                creator: e.creator.clone(),
            },
            None => self.dir_effective(&e.original_dir_id).await.map(|dir_eff| Effective {
                explicit: None,
                effective: dir_eff.effective,
                creator: e.creator.clone(),
            })?,
        };
        if Self::allows(&eff, actor.id.as_deref(), need) {
            return Ok(());
        }
        Err(DocError::AclDenied {
            actor: actor.id_str().to_string(),
            required: need.as_str(),
        })
    }

    // ── Permissions endpoints ─────────────────────────────────────────

    pub async fn get(&self, actor: &ActorCtx, target: AclTarget<'_>) -> Result<PermissionsView> {
        // Reading a node's ACL requires at least viewing the node.
        self.check(actor, target, AclRole::View).await?;
        let eff = self.effective_of(target).await?;
        let can_manage = if !actor.enforce() || actor.is_admin {
            true
        } else {
            eff.creator.as_deref() == actor.id.as_deref() && actor.id.is_some()
        };
        Ok(PermissionsView {
            explicit: eff.explicit,
            effective: eff.effective,
            can_manage,
            creator: eff.creator,
        })
    }

    /// Replace a node's explicit ACL. `None` clears it (back to
    /// inheritance); `Some(vec![])` makes the node private — visible and
    /// editable only by the creator and admins.
    pub async fn set(
        &self,
        actor: &ActorCtx,
        target: AclTarget<'_>,
        acl: Option<Vec<Ace>>,
    ) -> Result<()> {
        self.check_manage(actor, target).await?;
        if let Some(list) = &acl {
            for ace in list {
                if ace.principal.trim().is_empty() {
                    return Err(DocError::BadRequest("acl principal must not be empty".into()));
                }
            }
        }
        let acl = Arc::new(acl);
        match target {
            AclTarget::Doc(doc_id) => {
                let (parent_dir_id, mut idx) = self.store.locate_doc(doc_id).await?;
                let entry = idx
                    .files
                    .iter_mut()
                    .find(|f| f.doc_id == doc_id && !f.deleted)
                    .ok_or_else(|| DocError::DocNotFound(doc_id.to_string()))?;
                entry.acl = (*acl).clone();
                self.store.save(&idx).await?;
                crate::mqtt_publisher::notify_tree_changed(vec![parent_dir_id]);
            }
            AclTarget::Dir(dir_id) => {
                let mut idx = self.store.load(dir_id).await?;
                idx.acl = (*acl).clone();
                self.store.save(&idx).await?;
                let parent = idx
                    .parent
                    .clone()
                    .unwrap_or_else(|| crate::types::ROOT_DIR_ID.to_string());
                // Keep the parent's DirMeta display cache fresh (tree
                // responses read ACL state from there; the canonical copy
                // is the dir's own index, updated above).
                if idx.parent.is_some() {
                    let mut parent_idx = self.store.load(&parent).await?;
                    if let Some(entry) =
                        parent_idx.dirs.iter_mut().find(|d| d.dir_id == dir_id)
                    {
                        entry.acl = (*acl).clone();
                        self.store.save(&parent_idx).await?;
                    }
                }
                crate::mqtt_publisher::notify_tree_changed(vec![parent]);
            }
        }
        Ok(())
    }
}

/// File visibility: an explicit ACL on the file fully replaces the
/// inherited one (creators still pass); without one, the parent dir's
/// effective ACL decides.
fn file_visible(f: &DocMeta, dir_eff: &Effective, actor: &ActorCtx) -> bool {
    match &f.acl {
        Some(_) => {
            let eff = Effective {
                explicit: f.acl.clone(),
                effective: f.acl.clone(),
                creator: f.creator.clone(),
            };
            AclService::allows(&eff, actor.id.as_deref(), AclRole::View)
        }
        None => {
            // Inherits the dir's effective ACL; the file's own creator
            // still passes (mirrors `doc_effective` + `check`).
            let eff = Effective {
                explicit: None,
                effective: dir_eff.effective.clone(),
                creator: f.creator.clone(),
            };
            AclService::allows(&eff, actor.id.as_deref(), AclRole::View)
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::directory::{CreateDirectoryInput, DirectoryService};
    use crate::service::directory_impl::LibraryDirectoryService;
    use crate::service::document::{CreateDocumentInput, DocumentService};
    use crate::service::document_impl::LibraryDocumentService;
    use crate::types::{ROOT_DIR_ID, TreeNode};
    use tempfile::TempDir;

    fn actor(id: Option<&str>, admin: bool) -> ActorCtx {
        ActorCtx { id: id.map(str::to_string), is_admin: admin }
    }

    struct Harness {
        _tmp: TempDir,
        acl: AclService,
        dirs: LibraryDirectoryService,
        docs: LibraryDocumentService,
    }

    async fn setup() -> Harness {
        let tmp = TempDir::new().unwrap();
        let store = LibraryStore::new(tmp.path().to_path_buf());
        store.ensure_root().await.unwrap();
        Harness {
            _tmp: tmp,
            acl: AclService::new(store.clone()),
            dirs: LibraryDirectoryService::new(store.clone()),
            docs: LibraryDocumentService::new(store),
        }
    }

    async fn mk_dir(h: &Harness, parent: &str, name: &str, creator: Option<&str>) -> String {
        h.dirs
            .create(CreateDirectoryInput {
                parent_dir_id: parent.into(),
                name: name.into(),
                creator: creator.map(str::to_string),
            })
            .await
            .unwrap()
            .dir_id
    }

    async fn mk_doc(h: &Harness, parent: &str, title: &str, creator: Option<&str>) -> String {
        h.docs
            .create(CreateDocumentInput {
                parent_dir_id: parent.into(),
                title: title.into(),
                content: "x".into(),
                import: None,
                creator: creator.map(str::to_string),
            })
            .await
            .unwrap()
            .doc_id
    }

    fn aces(list: &[(&str, AclRole)]) -> Vec<Ace> {
        list.iter()
            .map(|(p, r)| Ace { principal: (*p).to_string(), role: *r })
            .collect()
    }

    #[tokio::test]
    async fn public_when_no_acl_anywhere() {
        let h = setup().await;
        let doc = mk_doc(&h, ROOT_DIR_ID, "open", Some("u-a")).await;
        // Stranger passes: pre-ACL libraries keep working (zero migration).
        h.acl
            .check(&actor(Some("u-z"), false), AclTarget::Doc(&doc), AclRole::Edit)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn human_and_admin_always_pass() {
        let h = setup().await;
        let dir = mk_dir(&h, ROOT_DIR_ID, "locked", Some("u-a")).await;
        h.acl
            .set(
                &actor(Some("u-a"), false),
                AclTarget::Dir(&dir),
                Some(aces(&[("u-b", AclRole::View)])),
            )
            .await
            .unwrap();
        // local single-user machine: never locked out
        h.acl
            .check(&actor(Some("human"), false), AclTarget::Dir(&dir), AclRole::Edit)
            .await
            .unwrap();
        h.acl
            .check(&actor(Some("u-stranger"), true), AclTarget::Dir(&dir), AclRole::Edit)
            .await
            .unwrap();
        // missing header (dev access) — legacy allow
        h.acl
            .check(&actor(None, false), AclTarget::Dir(&dir), AclRole::Edit)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn dir_acl_gates_users_edit_implies_view() {
        let h = setup().await;
        let dir = mk_dir(&h, ROOT_DIR_ID, "team", Some("u-a")).await;
        h.acl
            .set(
                &actor(Some("u-a"), false),
                AclTarget::Dir(&dir),
                Some(aces(&[("u-v", AclRole::View), ("u-e", AclRole::Edit)])),
            )
            .await
            .unwrap();
        // view-only user can read, not write
        h.acl
            .check(&actor(Some("u-v"), false), AclTarget::Dir(&dir), AclRole::View)
            .await
            .unwrap();
        let err = h
            .acl
            .check(&actor(Some("u-v"), false), AclTarget::Dir(&dir), AclRole::Edit)
            .await
            .unwrap_err();
        assert!(matches!(err, DocError::AclDenied { required: "edit", .. }));
        // editor passes both
        h.acl
            .check(&actor(Some("u-e"), false), AclTarget::Dir(&dir), AclRole::Edit)
            .await
            .unwrap();
        // unlisted user is locked out of reads too
        let err = h
            .acl
            .check(&actor(Some("u-x"), false), AclTarget::Dir(&dir), AclRole::View)
            .await
            .unwrap_err();
        assert!(matches!(err, DocError::AclDenied { required: "view", .. }));
        // the dir creator never locks itself out
        h.acl
            .check(&actor(Some("u-a"), false), AclTarget::Dir(&dir), AclRole::Edit)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn inheritance_flows_down_and_child_overrides() {
        let h = setup().await;
        let top = mk_dir(&h, ROOT_DIR_ID, "top", Some("u-a")).await;
        let sub = mk_dir(&h, &top, "sub", Some("u-s")).await;
        let doc = mk_doc(&h, &sub, "leaf", Some("u-d")).await;
        h.acl
            .set(
                &actor(Some("u-a"), false),
                AclTarget::Dir(&top),
                Some(aces(&[("u-v", AclRole::View)])),
            )
            .await
            .unwrap();
        // Inherited: the doc under two levels sees top's ACL.
        h.acl
            .check(&actor(Some("u-v"), false), AclTarget::Doc(&doc), AclRole::View)
            .await
            .unwrap();
        let err = h
            .acl
            .check(&actor(Some("u-x"), false), AclTarget::Doc(&doc), AclRole::View)
            .await
            .unwrap_err();
        assert!(matches!(err, DocError::AclDenied { .. }));
        // Child override replaces the inherited list entirely.
        h.acl
            .set(
                &actor(Some("u-s"), false),
                AclTarget::Dir(&sub),
                Some(aces(&[("u-w", AclRole::Edit)])),
            )
            .await
            .unwrap();
        let err = h
            .acl
            .check(&actor(Some("u-v"), false), AclTarget::Doc(&doc), AclRole::View)
            .await
            .unwrap_err();
        assert!(matches!(err, DocError::AclDenied { .. }));
        h.acl
            .check(&actor(Some("u-w"), false), AclTarget::Doc(&doc), AclRole::Edit)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn clearing_acl_restores_inheritance() {
        let h = setup().await;
        let top = mk_dir(&h, ROOT_DIR_ID, "top", Some("u-a")).await;
        let doc = mk_doc(&h, &top, "d", Some("u-a")).await;
        h.acl
            .set(&actor(Some("u-a"), false), AclTarget::Doc(&doc), Some(vec![]))
            .await
            .unwrap();
        // Private: only creator / admin pass.
        let err = h
            .acl
            .check(&actor(Some("u-x"), false), AclTarget::Doc(&doc), AclRole::View)
            .await
            .unwrap_err();
        assert!(matches!(err, DocError::AclDenied { .. }));
        // Clear → inherits the (public) top dir again.
        h.acl
            .set(&actor(Some("u-a"), false), AclTarget::Doc(&doc), None)
            .await
            .unwrap();
        h.acl
            .check(&actor(Some("u-x"), false), AclTarget::Doc(&doc), AclRole::View)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn only_creator_or_admin_may_manage() {
        let h = setup().await;
        let dir = mk_dir(&h, ROOT_DIR_ID, "mine", Some("u-a")).await;
        let doc = mk_doc(&h, &dir, "theirs", Some("u-b")).await;
        // Editor grant is NOT a sharing right.
        h.acl
            .set(
                &actor(Some("u-a"), false),
                AclTarget::Dir(&dir),
                Some(aces(&[("u-e", AclRole::Edit)])),
            )
            .await
            .unwrap();
        let err = h
            .acl
            .check_manage(&actor(Some("u-e"), false), AclTarget::Dir(&dir))
            .await
            .unwrap_err();
        assert!(matches!(err, DocError::AclDenied { required: "manage", .. }));
        // Doc creator manages its own node even inside a locked dir.
        h.acl
            .check_manage(&actor(Some("u-b"), false), AclTarget::Doc(&doc))
            .await
            .unwrap();
        // Admin manages anything.
        h.acl
            .check_manage(&actor(Some("u-adm"), true), AclTarget::Dir(&dir))
            .await
            .unwrap();
        // set() itself rejects non-creators.
        let err = h
            .acl
            .set(
                &actor(Some("u-e"), false),
                AclTarget::Dir(&dir),
                Some(vec![]),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, DocError::AclDenied { required: "manage", .. }));
    }

    #[tokio::test]
    async fn filter_tree_hides_invisible_children() {
        let h = setup().await;
        let pub_dir = mk_dir(&h, ROOT_DIR_ID, "pub", Some("u-a")).await;
        let secret_dir = mk_dir(&h, ROOT_DIR_ID, "secret", Some("u-a")).await;
        let secret_doc = mk_doc(&h, ROOT_DIR_ID, "secret-doc", Some("u-a")).await;
        let _ = pub_dir;
        h.acl
            .set(
                &actor(Some("u-a"), false),
                AclTarget::Dir(&secret_dir),
                Some(aces(&[("u-insider", AclRole::View)])),
            )
            .await
            .unwrap();
        h.acl
            .set(
                &actor(Some("u-a"), false),
                AclTarget::Doc(&secret_doc),
                Some(aces(&[("u-insider", AclRole::View)])),
            )
            .await
            .unwrap();

        let mut tree = TreeNode {
            dir_id: ROOT_DIR_ID.into(),
            name: "root".into(),
            path: String::new(),
            files: h.docs.list(ROOT_DIR_ID).await.unwrap(),
            dirs: h.dirs.list_tree(ROOT_DIR_ID).await.unwrap().dirs,
        };
        assert_eq!(tree.files.len(), 1);
        assert_eq!(tree.dirs.len(), 2);
        h.acl
            .filter_tree(&actor(Some("u-out"), false), &mut tree)
            .await
            .unwrap();
        assert!(tree.files.is_empty());
        assert_eq!(tree.dirs.len(), 1);
        assert_eq!(tree.dirs[0].name, "pub");
        // The insider sees everything; the creator too.
        for who in ["u-insider", "u-a"] {
            let mut t2 = TreeNode {
                dir_id: ROOT_DIR_ID.into(),
                name: "root".into(),
                path: String::new(),
                files: h.docs.list(ROOT_DIR_ID).await.unwrap(),
                dirs: h.dirs.list_tree(ROOT_DIR_ID).await.unwrap().dirs,
            };
            h.acl.filter_tree(&actor(Some(who), false), &mut t2).await.unwrap();
            assert_eq!(t2.files.len(), 1, "{who} should see secret-doc");
            assert_eq!(t2.dirs.len(), 2, "{who} should see both dirs");
        }
    }

    #[tokio::test]
    async fn get_permissions_reports_can_manage() {
        let h = setup().await;
        let dir = mk_dir(&h, ROOT_DIR_ID, "shared", Some("u-a")).await;
        h.acl
            .set(
                &actor(Some("u-a"), false),
                AclTarget::Dir(&dir),
                Some(aces(&[("u-v", AclRole::View)])),
            )
            .await
            .unwrap();
        let v = h.acl.get(&actor(Some("u-v"), false), AclTarget::Dir(&dir)).await.unwrap();
        assert!(!v.can_manage);
        assert_eq!(v.effective.as_ref().unwrap().len(), 1);
        let v = h.acl.get(&actor(Some("u-a"), false), AclTarget::Dir(&dir)).await.unwrap();
        assert!(v.can_manage);
        assert_eq!(v.creator.as_deref(), Some("u-a"));
        // No view grant → GET is denied outright (no ACL probing).
        let err = h
            .acl
            .get(&actor(Some("u-x"), false), AclTarget::Dir(&dir))
            .await
            .unwrap_err();
        assert!(matches!(err, DocError::AclDenied { required: "view", .. }));
    }

    #[tokio::test]
    async fn set_rejects_blank_principal() {
        let h = setup().await;
        let dir = mk_dir(&h, ROOT_DIR_ID, "d", Some("u-a")).await;
        let err = h
            .acl
            .set(
                &actor(Some("u-a"), false),
                AclTarget::Dir(&dir),
                Some(aces(&[("  ", AclRole::View)])),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, DocError::BadRequest(_)));
    }
}
