//! `dual` backend — the ADR-117 D6 grey-rollout observation mode.
//!
//! This is **not** a dual write. There is exactly one commit point: the tree.
//! Every `PmStore` mutation and every `PmMaintenance` derived read is
//! delegated to the tree, and only *after* a tree commit succeeds do we
//! best-effort mirror the tree's full visible state into SQLite. The four
//! iron rules from ADR-117 §3 D6:
//!
//! 1. A projection failure only warns and increments a counter. It never
//!    changes the write result and never rolls the tree back.
//! 2. The DB is disposable derived data. A missing / stale / corrupt /
//!    version-skewed DB is dropped and rebuilt from the tree at startup.
//! 3. The DB never serves a business read — every read goes to the tree.
//! 4. Switching to `db` requires a separate, explicit maintenance window
//!    (`migrate --apply` + `reconcile`); nothing here reads from SQLite.
//!
//! The projection is a **full replace-all scan** rather than 31 hand-written
//! per-method hooks. That deliberately trades write amplification (O(tree)
//! per mutation) for the one property a grey rollout actually needs: it is
//! impossible for a projection hook to miss a field, a sidecar or a subtree.
//! The tree is small at this scale (85 tasks / 55 ms cold rebuild) and `dual`
//! is a temporary observation mode. If the rollout ever runs at a scale where
//! this matters, the fix is incremental projection — not skipping steps ⑤/⑥.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::config::{PmConfig, StorageBackend};
use crate::error::Result;
use crate::store::corrupt::CorruptRegistry;
use crate::store::sqlite::SqlitePmStore;
use crate::types::{
    AttachmentId, AttachmentMeta, CreateProject, CreateTask, MemberKind, Project, ProjectId,
    ReparentTask, Task, TaskActivity, TaskFilter, TaskId, UpdateProject, UpdateTask,
};

use super::tree::{PmMaintenance, PmStore};

// ── Projection metrics ──────────────────────────────────────────────
//
// Process-global, like `dispatch::metrics_snapshot()`: there is one PM store
// per process, and `/health` needs to read the counters without a handle to
// the concrete `DualPmStore`.

static PROJECTION_OK: AtomicU64 = AtomicU64::new(0);
static PROJECTION_FAILED: AtomicU64 = AtomicU64::new(0);
static PROJECTION_LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);
static PROJECTION_LAST_REPORT: Mutex<Option<String>> = Mutex::new(None);

/// `/health` payload for the `dual` rollout. `failed > 0` means the DB is
/// stale — a reconciliation problem, **not** a service problem.
pub fn projection_snapshot() -> Value {
    json!({
        "ok": PROJECTION_OK.load(Ordering::Relaxed),
        "failed": PROJECTION_FAILED.load(Ordering::Relaxed),
        "last_error": (*PROJECTION_LAST_ERROR.lock()).clone(),
        "last_report": (*PROJECTION_LAST_REPORT.lock()).clone(),
    })
}

/// Delegate a read straight to the tree. SQLite is never consulted.
macro_rules! dual_read {
    ($store:expr, $method:ident $(, $arg:expr)* $(,)?) => {
        $store.tree.$method($($arg),*).await
    };
}

/// Commit to the tree, then mirror. The `?` is on the tree call only; the
/// projection is infallible by contract and its result is discarded.
macro_rules! dual_write {
    ($store:expr, $method:ident $(, $arg:expr)* $(,)?) => {{
        let out = $store.tree.$method($($arg),*).await?;
        $store.project();
        Ok(out)
    }};
}

pub struct DualPmStore {
    tree: Arc<dyn PmStore>,
    config: PmConfig,
    /// `None` when even the disposable DB could not be opened. The tree still
    /// serves everything: a broken projection never takes PM down.
    db: Option<SqlitePmStore>,
    /// Serializes the whole "scan tree → replace-all transaction" pair.
    /// Without it, two concurrent writes can interleave their projections so
    /// that the *older* scan commits last and leaves the DB behind the tree
    /// while both writes report success.
    projection_lock: Mutex<()>,
}

impl DualPmStore {
    /// Assemble the observation backend and immediately replace the DB with a
    /// fresh tree snapshot (iron rule 2). Never fails on a DB problem.
    pub fn new(tree: Arc<dyn PmStore>, config: PmConfig) -> Self {
        let db = open_projection(&config);
        let store = Self {
            tree,
            config,
            db,
            projection_lock: Mutex::new(()),
        };
        store.project();
        store
    }

    pub fn backend(&self) -> StorageBackend {
        StorageBackend::Dual
    }

    /// Best-effort, synchronous projection. Errors are logged and counted,
    /// never propagated — this method deliberately returns `()`.
    fn project(&self) {
        // Held for the entire scan + replace transaction. A write that lands
        // while this runs queues its own projection behind this lock, so the
        // final DB state always comes from the freshest completed scan.
        let _guard = self.projection_lock.lock();
        let Some(db) = &self.db else {
            PROJECTION_FAILED.fetch_add(1, Ordering::Relaxed);
            *PROJECTION_LAST_ERROR.lock() = Some("sqlite projection is unavailable".into());
            return;
        };
        match db.rebuild_projection(&self.config) {
            Ok(report) => {
                PROJECTION_OK.fetch_add(1, Ordering::Relaxed);
                *PROJECTION_LAST_REPORT.lock() = Some(report.summary());
                *PROJECTION_LAST_ERROR.lock() = None;
            }
            Err(e) => {
                let msg = e.to_string();
                PROJECTION_FAILED.fetch_add(1, Ordering::Relaxed);
                *PROJECTION_LAST_ERROR.lock() = Some(msg.clone());
                tracing::warn!(
                    error = %msg,
                    "dual projection failed — tree commit stands, DB is stale"
                );
            }
        }
    }
}

/// Open the projection DB, dropping and recreating it if it is missing,
/// corrupt or from a schema version this binary does not understand. Only
/// legal because in `dual` mode SQLite is derived data; `db` mode must never
/// do this.
fn open_projection(config: &PmConfig) -> Option<SqlitePmStore> {
    if let Ok(db) = SqlitePmStore::open(config.clone()) {
        return Some(db);
    }
    let path = config.sqlite_path();
    for suffix in ["", "-wal", "-shm"] {
        let candidate = PathBuf::from(format!("{}{suffix}", path.display()));
        let _ = std::fs::remove_file(candidate);
    }
    match SqlitePmStore::open(config.clone()) {
        Ok(db) => Some(db),
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "could not open the dual projection; serving from the tree only"
            );
            None
        }
    }
}

#[async_trait]
impl PmMaintenance for DualPmStore {
    async fn rebuild_index(&self) -> Result<()> {
        // `new()` already projected from disk; rebuilding the in-memory index
        // changes no entity, so a second projection would be pure waste.
        self.tree.rebuild_index().await
    }

    async fn migrate_null_start_at(&self) -> Result<usize> {
        dual_write!(self, migrate_null_start_at)
    }

    async fn shutdown(&self) -> Result<()> {
        let out = self.tree.shutdown().await?;
        self.project();
        Ok(out)
    }

    fn dispatch_due_ids(&self, now: chrono::DateTime<chrono::Utc>, limit: usize) -> Vec<TaskId> {
        self.tree.dispatch_due_ids(now, limit)
    }

    fn task_depth(&self, task_id: &TaskId) -> u8 {
        self.tree.task_depth(task_id)
    }

    fn parent_of(&self, task_id: &TaskId) -> Option<TaskId> {
        self.tree.parent_of(task_id)
    }

    fn corrupt_registry(&self) -> CorruptRegistry {
        self.tree.corrupt_registry()
    }

    fn project_task_count(&self, project_id: &ProjectId) -> usize {
        self.tree.project_task_count(project_id)
    }

    async fn list_project_icons(&self, project_id: &ProjectId) -> Result<Vec<String>> {
        dual_read!(self, list_project_icons, project_id)
    }

    async fn write_project_icon(
        &self,
        project_id: &ProjectId,
        name: &str,
        bytes: &[u8],
    ) -> Result<()> {
        // Icons are filesystem assets in every backend (ADR-061 decision 5);
        // they are never projected into SQLite.
        dual_read!(self, write_project_icon, project_id, name, bytes)
    }

    async fn read_project_icon(
        &self,
        project_id: &ProjectId,
        name: &str,
    ) -> Result<Option<(String, Vec<u8>)>> {
        dual_read!(self, read_project_icon, project_id, name)
    }

    fn is_dispatch_blocked(&self, task: &Task) -> bool {
        self.tree.is_dispatch_blocked(task)
    }

    async fn dispatch_identity(&self, id: &TaskId) -> Result<Option<String>> {
        dual_write!(self, dispatch_identity, id)
    }

    async fn mark_dispatched(&self, id: &TaskId) -> Result<()> {
        dual_write!(self, mark_dispatched, id)
    }

    async fn record_dispatch_failure(&self, id: &TaskId, reason: &str) -> Result<u32> {
        dual_write!(self, record_dispatch_failure, id, reason)
    }
}

#[async_trait]
impl PmStore for DualPmStore {
    // ── Projects ───────────────────────────────────────────────────
    async fn create_project_as(
        &self,
        input: CreateProject,
        created_by: &str,
        created_by_kind: MemberKind,
    ) -> Result<Project> {
        dual_write!(self, create_project_as, input, created_by, created_by_kind)
    }

    async fn get_project(&self, id: &ProjectId) -> Result<Option<Project>> {
        dual_read!(self, get_project, id)
    }

    async fn list_projects(&self) -> Result<Vec<Project>> {
        dual_read!(self, list_projects)
    }

    async fn update_project(&self, id: &ProjectId, input: UpdateProject) -> Result<Project> {
        dual_write!(self, update_project, id, input)
    }

    async fn delete_project(&self, id: &ProjectId, cascade: bool) -> Result<()> {
        dual_write!(self, delete_project, id, cascade)
    }

    async fn project_counts(&self) -> Result<Vec<crate::types::ProjectCounts>> {
        dual_read!(self, project_counts)
    }

    // ── Members ─────────────────────────────────────────────────────
    async fn add_project_member(
        &self,
        project_id: &ProjectId,
        instance_id: &str,
        kind: MemberKind,
    ) -> Result<Project> {
        dual_write!(self, add_project_member, project_id, instance_id, kind)
    }

    async fn remove_project_member(
        &self,
        project_id: &ProjectId,
        instance_id: &str,
    ) -> Result<Project> {
        dual_write!(self, remove_project_member, project_id, instance_id)
    }

    async fn transfer_owner(&self, project_id: &ProjectId, new_owner: &str) -> Result<Project> {
        dual_write!(self, transfer_owner, project_id, new_owner)
    }

    // ── Tasks ──────────────────────────────────────────────────────
    async fn create_task_as(
        &self,
        project_id: &ProjectId,
        input: CreateTask,
        created_by: &str,
        created_by_kind: MemberKind,
    ) -> Result<Task> {
        dual_write!(
            self,
            create_task_as,
            project_id,
            input,
            created_by,
            created_by_kind
        )
    }

    async fn get_task(&self, id: &TaskId) -> Result<Option<Task>> {
        dual_read!(self, get_task, id)
    }

    async fn find_tasks(&self, filter: &TaskFilter) -> Result<Vec<Task>> {
        dual_read!(self, find_tasks, filter)
    }

    async fn update_task(&self, id: &TaskId, input: UpdateTask) -> Result<Task> {
        dual_write!(self, update_task, id, input)
    }

    async fn delete_task(&self, id: &TaskId, cascade: bool) -> Result<()> {
        dual_write!(self, delete_task, id, cascade)
    }

    async fn reparent_task(&self, id: &TaskId, input: ReparentTask) -> Result<()> {
        dual_write!(self, reparent_task, id, input)
    }

    async fn list_children(&self, parent: &TaskId) -> Result<Vec<Task>> {
        dual_read!(self, list_children, parent)
    }

    // ── Lifecycle ──────────────────────────────────────────────────
    async fn claim_task(&self, id: &TaskId, actor: &str) -> Result<Task> {
        dual_write!(self, claim_task, id, actor)
    }

    async fn submit_task(
        &self,
        id: &TaskId,
        text: &str,
        attachment_ids: Vec<AttachmentId>,
        doc_refs: Vec<String>,
        actor: &str,
    ) -> Result<Task> {
        dual_write!(self, submit_task, id, text, attachment_ids, doc_refs, actor)
    }

    async fn review_task(
        &self,
        id: &TaskId,
        approved: bool,
        reviewer: &str,
        comment: Option<&str>,
    ) -> Result<Task> {
        dual_write!(self, review_task, id, approved, reviewer, comment)
    }

    // ── Activity ───────────────────────────────────────────────────
    async fn list_activity(&self, task_id: &TaskId) -> Result<Vec<TaskActivity>> {
        dual_read!(self, list_activity, task_id)
    }

    async fn add_task_comment(
        &self,
        task_id: &TaskId,
        text: &str,
        actor: &str,
    ) -> Result<TaskActivity> {
        dual_write!(self, add_task_comment, task_id, text, actor)
    }

    // ── Dependency graph / attachments / contexts ──────────────────
    async fn compute_blocked_by(&self, task_id: &TaskId) -> Result<Vec<TaskId>> {
        dual_read!(self, compute_blocked_by, task_id)
    }

    async fn list_attachments(&self, task_id: &TaskId) -> Result<Vec<AttachmentMeta>> {
        dual_read!(self, list_attachments, task_id)
    }

    async fn get_attachment(&self, att_id: &AttachmentId) -> Result<Option<AttachmentMeta>> {
        dual_read!(self, get_attachment, att_id)
    }

    async fn register_attachment(&self, task_id: &TaskId, meta: AttachmentMeta) -> Result<()> {
        dual_write!(self, register_attachment, task_id, meta)
    }

    async fn delete_attachment(&self, att_id: &AttachmentId) -> Result<()> {
        dual_write!(self, delete_attachment, att_id)
    }

    async fn write_attachment(
        &self,
        task_id: &TaskId,
        meta: AttachmentMeta,
        bytes: Vec<u8>,
        thumb_bytes: Option<Vec<u8>>,
    ) -> Result<AttachmentMeta> {
        dual_write!(self, write_attachment, task_id, meta, bytes, thumb_bytes)
    }

    async fn read_attachment_bytes(
        &self,
        att_id: &AttachmentId,
        thumb: bool,
    ) -> Result<Option<(AttachmentMeta, Vec<u8>)>> {
        dual_read!(self, read_attachment_bytes, att_id, thumb)
    }

    async fn closing_task_contexts(&self, task_id: &TaskId) -> Result<Vec<(ProjectId, Task)>> {
        dual_read!(self, closing_task_contexts, task_id)
    }

    async fn closing_project_contexts(&self, project_id: &ProjectId) -> Result<Vec<Task>> {
        dual_read!(self, closing_project_contexts, project_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::sqlite::import::reconcile;
    use crate::store::tree::TreePmStore;
    use crate::types::{
        AttachmentId, AttachmentKind, CreateProject, CreateTask, Dependency, Priority,
        ReviewStatus, TaskFilter, TaskStatus, TaskType, UpdateTask,
    };
    use std::path::Path;

    fn cfg(dir: &Path, backend: StorageBackend) -> PmConfig {
        PmConfig {
            data_dir: dir.to_path_buf(),
            storage_backend: backend,
            ..Default::default()
        }
    }

    fn task_input(title: &str, parent: Option<TaskId>) -> CreateTask {
        CreateTask {
            title: title.to_string(),
            description: String::new(),
            task_type: TaskType::Task,
            priority: Priority::Normal,
            parent_task_id: parent,
            depends_on: Vec::<Dependency>::new(),
            attachment_ids: Vec::new(),
            assignee: None,
            due_at: None,
            start_at: None,
        }
    }

    fn project_input(title: &str) -> CreateProject {
        CreateProject {
            title: title.to_string(),
            description: String::new(),
            metadata: Default::default(),
        }
    }

    async fn make_store(dir: &Path, backend: StorageBackend) -> Arc<dyn PmStore> {
        let config = cfg(dir, backend);
        match backend {
            StorageBackend::FileTree => {
                Arc::new(TreePmStore::new(config).await.unwrap()) as Arc<dyn PmStore>
            }
            StorageBackend::Db => {
                Arc::new(SqlitePmStore::open(config).unwrap()) as Arc<dyn PmStore>
            }
            StorageBackend::Dual => {
                let tree: Arc<dyn PmStore> =
                    Arc::new(TreePmStore::new(config.clone()).await.unwrap());
                Arc::new(DualPmStore::new(tree, config)) as Arc<dyn PmStore>
            }
        }
    }

    /// D6 step 5: one identical assertion set, run against all three
    /// backends. No backend-specific skips — a divergence here is a bug, not
    /// a "known difference".
    async fn assert_core_scenario(store: &Arc<dyn PmStore>, backend: StorageBackend) {
        let label = backend.as_str();
        let project = store
            .create_project_as(project_input("scenario"), "human", MemberKind::User)
            .await
            .unwrap();
        let pid = project.id.clone();
        let root = store
            .create_task_as(&pid, task_input("root", None), "human", MemberKind::User)
            .await
            .unwrap();
        let child = store
            .create_task_as(
                &pid,
                task_input("child", Some(root.id.clone())),
                "human",
                MemberKind::User,
            )
            .await
            .unwrap();

        assert_eq!(store.list_projects().await.unwrap().len(), 1, "{label}");
        assert_eq!(
            store.get_task(&root.id).await.unwrap().unwrap().title,
            "root",
            "{label}"
        );
        assert_eq!(store.task_depth(&root.id), 0, "{label}");
        assert_eq!(store.task_depth(&child.id), 1, "{label}");
        assert_eq!(
            store.parent_of(&child.id).as_ref(),
            Some(&root.id),
            "{label}"
        );
        assert_eq!(store.project_task_count(&pid), 2, "{label}");
        assert_eq!(store.project_counts().await.unwrap()[0].total, 2, "{label}");
        assert_eq!(
            store
                .find_tasks(&TaskFilter {
                    project_id: Some(pid.clone()),
                    ..Default::default()
                })
                .await
                .unwrap()
                .len(),
            2,
            "{label}"
        );

        // Lifecycle is the widest write surface: claim → submit → review,
        // with the activity stream and its comment append.
        assert_eq!(
            store.claim_task(&root.id, "human").await.unwrap().status,
            TaskStatus::InProgress,
            "{label}"
        );
        store
            .submit_task(&root.id, "done", vec![], vec![], "human")
            .await
            .unwrap();
        let reviewed = store
            .review_task(&root.id, true, "human", Some("LGTM"))
            .await
            .unwrap();
        assert_eq!(reviewed.status, TaskStatus::Done, "{label}");
        assert_eq!(reviewed.review_status, ReviewStatus::Approved, "{label}");
        assert_eq!(
            store.list_activity(&root.id).await.unwrap().len(),
            2,
            "{label}"
        );
        store
            .add_task_comment(&root.id, "note", "human")
            .await
            .unwrap();
        assert_eq!(
            store.list_activity(&root.id).await.unwrap().len(),
            3,
            "{label}"
        );

        // Reparent both directions, then delete the subtree.
        store
            .reparent_task(&child.id, ReparentTask { new_parent: None })
            .await
            .unwrap();
        assert_eq!(store.parent_of(&child.id), None, "{label}");
        assert_eq!(store.task_depth(&child.id), 0, "{label}");
        store
            .reparent_task(
                &child.id,
                ReparentTask {
                    new_parent: Some(root.id.clone()),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            store.parent_of(&child.id).as_ref(),
            Some(&root.id),
            "{label}"
        );
        assert_eq!(
            store.closing_task_contexts(&root.id).await.unwrap().len(),
            2,
            "{label}"
        );
        store.delete_task(&root.id, false).await.unwrap();
        assert!(
            store.get_task(&child.id).await.unwrap().is_none(),
            "{label}"
        );
        assert!(store.project_counts().await.unwrap().is_empty(), "{label}");

        store.delete_project(&pid, false).await.unwrap();
        assert!(store.get_project(&pid).await.unwrap().is_none(), "{label}");
    }

    #[tokio::test]
    async fn core_scenario_runs_identically_on_all_three_backends() {
        for backend in [
            StorageBackend::FileTree,
            StorageBackend::Db,
            StorageBackend::Dual,
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let store = make_store(tmp.path(), backend).await;
            assert_core_scenario(&store, backend).await;
        }
    }

    /// Iron rule 3: the DB never serves a business read. Iron rule 4's gate
    /// (`reconcile`) must notice the drift we just injected.
    #[tokio::test]
    async fn dual_reads_ignore_a_tampered_db_and_reconcile_detects_it() {
        let tmp = tempfile::tempdir().unwrap();
        let config = cfg(tmp.path(), StorageBackend::Dual);
        let tree: Arc<dyn PmStore> = Arc::new(TreePmStore::new(config.clone()).await.unwrap());
        let store = DualPmStore::new(tree, config.clone());
        let project = store
            .create_project_as(project_input("authoritative"), "human", MemberKind::User)
            .await
            .unwrap();

        let conn = rusqlite::Connection::open(config.sqlite_path()).unwrap();
        conn.execute(
            "UPDATE projects SET props = '{\"tampered\":true}' WHERE id = ?1",
            [project.id.as_str()],
        )
        .unwrap();
        drop(conn);

        let served = store.get_project(&project.id).await.unwrap().unwrap();
        assert_eq!(served.title, "authoritative");
        let report = reconcile(&config).unwrap();
        assert!(!report.is_consistent(), "{}", report.summary());
        assert_eq!(report.props_mismatch.len(), 1, "{}", report.summary());
    }

    /// Iron rule 1: a projection failure must not surface in the write
    /// result. A corrupt `task.json` in the tree makes the full-tree scan
    /// blocking, which is exactly the failure mode worth proving.
    #[tokio::test]
    async fn a_failed_projection_never_fails_the_tree_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let config = cfg(tmp.path(), StorageBackend::Dual);
        let tree: Arc<dyn PmStore> = Arc::new(TreePmStore::new(config.clone()).await.unwrap());
        let store = DualPmStore::new(tree, config.clone());
        let project = store
            .create_project_as(project_input("resilient"), "human", MemberKind::User)
            .await
            .unwrap();
        let task = store
            .create_task_as(
                &project.id,
                task_input("survivor", None),
                "human",
                MemberKind::User,
            )
            .await
            .unwrap();
        let before = projection_snapshot()["failed"].as_u64().unwrap();

        // Simulate the D4 incident: an external process writes a broken
        // task.json into the tree. The tree keeps serving; the scan cannot
        // faithfully project that file.
        let bad = config
            .projects_dir()
            .join(project.id.as_str())
            .join("tasks")
            .join("t-bad0001");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("task.json"), b"{ not json").unwrap();

        let updated = store
            .update_task(
                &task.id,
                UpdateTask {
                    title: Some("still committed".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.title, "still committed");
        assert_eq!(
            store.get_task(&task.id).await.unwrap().unwrap().title,
            "still committed"
        );

        let after = projection_snapshot();
        assert!(
            after["failed"].as_u64().unwrap() > before,
            "a blocked projection must be counted: {after}"
        );
        // Metrics are process-global and other tests may project concurrently,
        // so assert the durable signal: the stale DB must fail the cutover
        // gate rather than looking healthy.
        let report = reconcile(&config).unwrap();
        assert!(!report.is_consistent(), "{}", report.summary());
        assert!(!report.blocking.is_empty(), "{}", report.summary());
    }

    /// Iron rule 2: a stale / corrupt projection is overwritten at startup,
    /// never merged and never trusted.
    #[tokio::test]
    async fn startup_rebuilds_a_stale_projection_from_the_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let config = cfg(tmp.path(), StorageBackend::Dual);
        let tree: Arc<dyn PmStore> = Arc::new(TreePmStore::new(config.clone()).await.unwrap());
        let store = DualPmStore::new(tree, config.clone());
        store
            .create_project_as(project_input("real"), "human", MemberKind::User)
            .await
            .unwrap();

        // Inject a row the tree never had.
        let conn = rusqlite::Connection::open(config.sqlite_path()).unwrap();
        conn.execute(
            "INSERT INTO projects (id, status, created_at, updated_at, props)
             VALUES ('p-deadbeef', 'active', '2020-01-01T00:00:00+00:00',
                     '2020-01-01T00:00:00+00:00', '{}')",
            [],
        )
        .unwrap();
        drop(conn);
        assert!(!reconcile(&config).unwrap().is_consistent());

        let restarted_tree: Arc<dyn PmStore> =
            Arc::new(TreePmStore::new(config.clone()).await.unwrap());
        restarted_tree.rebuild_index().await.unwrap();
        let _restarted = DualPmStore::new(restarted_tree, config.clone());
        let report = reconcile(&config).unwrap();
        assert!(report.is_consistent(), "{}", report.summary());
        assert_eq!(report.db_projects, 1, "{}", report.summary());
    }

    /// The projection lock must keep concurrent writes reconcilable: without
    /// it an older full-tree scan can commit last and silently strand the DB.
    #[tokio::test]
    async fn concurrent_writes_leave_the_projection_reconcilable() {
        let tmp = tempfile::tempdir().unwrap();
        let config = cfg(tmp.path(), StorageBackend::Dual);
        let tree: Arc<dyn PmStore> = Arc::new(TreePmStore::new(config.clone()).await.unwrap());
        let store = Arc::new(DualPmStore::new(tree, config.clone()));
        let project = store
            .create_project_as(project_input("concurrent"), "human", MemberKind::User)
            .await
            .unwrap();

        let mut ids = Vec::new();
        for i in 0..8 {
            let task = store
                .create_task_as(
                    &project.id,
                    task_input(&format!("task-{i}"), None),
                    "human",
                    MemberKind::User,
                )
                .await
                .unwrap();
            ids.push(task.id);
        }

        let mut handles = Vec::new();
        for (i, id) in ids.into_iter().enumerate() {
            let store = store.clone();
            handles.push(tokio::spawn(async move {
                store
                    .add_task_comment(&id, &format!("note-{i}"), "human")
                    .await
                    .unwrap();
            }));
        }
        for handle in handles {
            handle.await.unwrap();
        }

        let report = reconcile(&config).unwrap();
        assert!(report.is_consistent(), "{}", report.summary());
        assert_eq!(report.db_tasks, 8, "{}", report.summary());
        assert_eq!(report.db_activity, 8, "{}", report.summary());
    }

    /// D6 step 6: the projection stays byte-for-byte reconcilable through a
    /// mixed write flow (lifecycle + activity + attachment metadata).
    #[tokio::test]
    async fn projection_reconciles_after_a_full_mixed_flow() {
        let tmp = tempfile::tempdir().unwrap();
        let config = cfg(tmp.path(), StorageBackend::Dual);
        let tree: Arc<dyn PmStore> = Arc::new(TreePmStore::new(config.clone()).await.unwrap());
        let store = DualPmStore::new(tree, config.clone());
        let project = store
            .create_project_as(project_input("mixed"), "human", MemberKind::User)
            .await
            .unwrap();
        let task = store
            .create_task_as(
                &project.id,
                task_input("root", None),
                "human",
                MemberKind::User,
            )
            .await
            .unwrap();
        store.claim_task(&task.id, "human").await.unwrap();
        store
            .submit_task(&task.id, "shipped", vec![], vec![], "human")
            .await
            .unwrap();
        store
            .review_task(&task.id, true, "human", None)
            .await
            .unwrap();
        store
            .add_task_comment(&task.id, "note", "human")
            .await
            .unwrap();

        let att_id = AttachmentId::generate();
        store
            .write_attachment(
                &task.id,
                AttachmentMeta {
                    id: att_id,
                    filename: "a.txt".into(),
                    kind: AttachmentKind::File,
                    content_type: "text/plain".into(),
                    size: 3,
                    sha256: "abc".into(),
                    storage_path: "attachments/x/original.txt".into(),
                    thumb_path: None,
                    width: None,
                    height: None,
                    uploaded_by: "human".into(),
                    uploaded_at: chrono::Utc::now(),
                },
                b"abc".to_vec(),
                None,
            )
            .await
            .unwrap();

        let report = reconcile(&config).unwrap();
        assert!(report.is_consistent(), "{}", report.summary());
        assert_eq!(report.db_tasks, 1, "{}", report.summary());
        assert_eq!(report.db_attachments, 1, "{}", report.summary());
        assert_eq!(report.db_activity, 3, "{}", report.summary());
    }
}
