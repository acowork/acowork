//! acowork-memory shared service state (ADR-101).
//!
//! The service is **multi-tenant by Agent instance**: every Agent instance owns
//! one SQLite file, `{data_dir}/instances/{instance_id}/memory.db`, keyed by the
//! immutable instance UUID assigned at install time (ADR-073). This replaces the
//! former `{workspace}/memory/private.grafeo`, and it is what makes memory
//! addressable from outside the Runtime: the instance id arrives with the
//! request (`X-MCP-Actor` for MCP, `?instance_id=` for the panel REST API, both
//! injected/validated upstream), so one process serves every Agent and no
//! request can cross into another Agent's store.
//!
//! Stores open lazily on first touch and stay cached for the process lifetime —
//! a node can host dozens of instances, and opening a SQLite file per request
//! would dominate the cost of a recall. The embedding backend and the
//! distiller / forgetting scheduling live in [`crate::service::pipeline`], which
//! this state owns and delegates to.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use acowork_core::EmbeddingProvider;

use crate::service::config::MemoryServiceConfig;
use crate::service::error::{Result, ServiceError};
use crate::service::pipeline::Pipeline;
use crate::sqlite::SqliteStore;
use crate::wire::RebuildProgress;

/// Longest accepted `instance_id` — bounds the path the service builds from it.
pub const MAX_INSTANCE_ID_LEN: usize = 64;

/// File name of an instance store inside its namespace directory.
pub const INSTANCE_DB_NAME: &str = "memory.db";

/// Directory name holding all instance namespaces under `data_dir`.
pub const INSTANCES_DIR_NAME: &str = "instances";

/// Validate an instance id before it becomes a directory name.
///
/// The id comes from a request header / query parameter, so it is untrusted
/// input on the path-construction route. Only `[A-Za-z0-9._-]` is admitted —
/// which excludes `/` and `\` outright — and the `.`-leading / `..`-containing
/// forms are rejected so no id can escape `{data_dir}/instances/` (ADR-101
/// decision 3). A UUID v4 (the ADR-073 shape) always passes.
pub fn validate_instance_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > MAX_INSTANCE_ID_LEN {
        return Err(ServiceError::InvalidInstanceId(format!(
            "length {} (allowed 1..={MAX_INSTANCE_ID_LEN})",
            id.len()
        )));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(ServiceError::InvalidInstanceId(id.to_string()));
    }
    // Hidden directories and any `..` hop are never admissible.
    if id.starts_with('.') || id.contains("..") {
        return Err(ServiceError::InvalidInstanceId(id.to_string()));
    }
    Ok(())
}

/// Cache of open per-instance stores.
pub struct InstanceRegistry {
    /// `{data_dir}/instances`.
    root: PathBuf,
    /// Dimension recorded when a **brand-new** store is created; an existing file
    /// keeps the dimension in its own `meta` table (see [`SqliteStore::open`]).
    /// Live, because the embedding backend can be installed (or swapped by a
    /// Gateway snapshot) long after the registry was built — see
    /// [`InstanceRegistry::set_embedding_dim`].
    embedding_dim: RwLock<usize>,
    /// `instance_id` → open store.
    stores: RwLock<HashMap<String, Arc<SqliteStore>>>,
}

impl InstanceRegistry {
    fn new(root: PathBuf, embedding_dim: usize) -> Self {
        Self {
            root,
            embedding_dim: RwLock::new(embedding_dim),
            stores: RwLock::new(HashMap::new()),
        }
    }

    /// The dimension a newly created store would get.
    pub fn embedding_dim(&self) -> usize {
        *self
            .embedding_dim
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Retarget newly created stores at a different vector width.
    ///
    /// Never rewrites an existing database — widening an index is a rebuild
    /// (`POST /memory/rebuild-embeddings`), and a background credential refresh
    /// must not trigger one by itself.
    pub fn set_embedding_dim(&self, dimension: usize) {
        if dimension == 0 {
            return;
        }
        let mut slot = self
            .embedding_dim
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *slot != dimension {
            tracing::info!(
                previous = *slot,
                dimension,
                "new instance stores will use this embedding dimension"
            );
            *slot = dimension;
        }
    }

    /// Namespace directory for a validated `instance_id`.
    pub fn instance_dir(&self, id: &str) -> Result<PathBuf> {
        validate_instance_id(id)?;
        Ok(self.root.join(id))
    }

    /// Store path for a validated `instance_id`.
    pub fn db_path(&self, id: &str) -> Result<PathBuf> {
        Ok(self.instance_dir(id)?.join(INSTANCE_DB_NAME))
    }

    /// Whether a store file already exists for this instance.
    ///
    /// Used by the first-touch paths (bootstrap import, legacy migration) to
    /// decide whether there is anything to do. An invalid id is "absent" rather
    /// than an error — callers that must distinguish get [`Self::get`].
    pub fn exists(&self, id: &str) -> bool {
        self.db_path(id).map(|p| p.exists()).unwrap_or(false)
    }

    /// Get (opening on first use) the store for `instance_id`.
    pub fn get(&self, id: &str) -> Result<Arc<SqliteStore>> {
        if let Some(hit) = self.peek(id) {
            return Ok(hit);
        }
        let path = self.db_path(id)?;
        let store = Arc::new(
            SqliteStore::open(&path, self.embedding_dim()).map_err(|e| {
                tracing::error!(
                    instance_id = %id,
                    path = %path.display(),
                    error = %e,
                    "failed to open instance memory store"
                );
                ServiceError::Storage(e)
            })?,
        );
        // Re-check under the write lock: a concurrent first touch of the same
        // instance must not hand out two handles over two connections.
        let mut cache = self
            .stores
            .write()
            .map_err(|_| ServiceError::Internal("instance cache poisoned".into()))?;
        if let Some(existing) = cache.get(id) {
            return Ok(existing.clone());
        }
        tracing::info!(instance_id = %id, path = %path.display(), "opened memory store");
        cache.insert(id.to_string(), store.clone());
        Ok(store)
    }

    /// Cached handle only — never opens, never touches the filesystem.
    pub fn peek(&self, id: &str) -> Option<Arc<SqliteStore>> {
        self.stores.read().ok().and_then(|c| c.get(id).cloned())
    }

    /// Forget a cached handle (used by the import path, which replaces the file
    /// underneath the process and must not keep writing to the old connection).
    pub fn evict(&self, id: &str) -> Option<Arc<SqliteStore>> {
        self.stores.write().ok().and_then(|mut c| c.remove(id))
    }

    /// Known instances: those with a store on disk, plus any currently open.
    ///
    /// Scanning the directory is the authority — a handle can exist for an
    /// instance whose file was migrated away, and a file can exist for an
    /// instance the process has not served yet.
    pub fn list(&self) -> Vec<String> {
        let mut ids: Vec<String> = read_dir_instance_ids(&self.root);
        if let Ok(cache) = self.stores.read() {
            for id in cache.keys() {
                if !ids.iter().any(|known| known == id) {
                    ids.push(id.clone());
                }
            }
        }
        ids.sort();
        ids
    }

    /// Number of currently open handles.
    pub fn open_count(&self) -> usize {
        self.stores.read().map(|c| c.len()).unwrap_or(0)
    }
}

/// List instance ids that have a store under `root` (a directory containing
/// `memory.db`). A missing or unreadable root means "no instances".
fn read_dir_instance_ids(root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut ids = Vec::new();
    for entry in entries.flatten() {
        if !entry.path().join(INSTANCE_DB_NAME).exists() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if validate_instance_id(&name).is_ok() {
            ids.push(name);
        } else {
            tracing::warn!(instance_id = %name, "ignoring instance directory with invalid id");
        }
    }
    ids
}

/// Shared state handed to every axum handler / MCP tool via `State<T>`.
#[derive(Clone)]
pub struct MemoryState {
    /// Service config (validated at startup).
    pub config: Arc<MemoryServiceConfig>,
    /// Per-instance store registry.
    instances: Arc<InstanceRegistry>,
    /// Credentials (embedding backend + Gateway snapshot) and the per-instance
    /// consolidation scheduling. Built with the state, started by `serve`.
    pipeline: Arc<Pipeline>,
    /// In-flight / finished embedding rebuilds, keyed by instance.
    ///
    /// The Runtime held one slot per process because it served one Agent; one
    /// memory process serves every instance, so the guard and the progress
    /// read by the panel's polling loop must be per namespace or two Agents
    /// rebuilding at once would overwrite each other's numbers.
    rebuilds: Arc<Mutex<HashMap<String, RebuildProgress>>>,
}

impl MemoryState {
    /// Build the state from a validated config.
    ///
    /// Materialises `{data_dir}/instances` so the first request finds a
    /// writable root; individual stores still open lazily.
    pub fn new(config: MemoryServiceConfig) -> Result<Self> {
        let instances_dir = config.instances_dir();
        std::fs::create_dir_all(&instances_dir)?;
        let config = Arc::new(config);
        let instances = Arc::new(InstanceRegistry::new(
            instances_dir.clone(),
            config.embedding_dim,
        ));
        tracing::info!(
            instances_dir = %instances_dir.display(),
            known_instances = instances.list().len(),
            "memory instance namespace ready"
        );
        let pipeline = Arc::new(Pipeline::new(config.clone(), instances.clone()));
        // Retrieval must work before the Gateway answers a resources pull, so
        // the local `[embedding]` endpoint is installed up front.
        pipeline.install_embedding_from_config();
        Ok(Self {
            config,
            instances,
            pipeline,
            rebuilds: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// The per-instance store registry.
    pub fn instances(&self) -> &Arc<InstanceRegistry> {
        &self.instances
    }

    /// The credential + scheduling pipeline (P1e).
    pub fn pipeline(&self) -> &Arc<Pipeline> {
        &self.pipeline
    }

    /// Store for one instance namespace.
    pub fn store(&self, instance_id: &str) -> Result<Arc<SqliteStore>> {
        self.instances.get(instance_id)
    }

    /// Install the embedding backend. `None` keeps every path lexical.
    ///
    /// Delegates to the pipeline, which owns the slot and the credential
    /// snapshot it was derived from.
    pub fn set_embedding(&self, provider: Option<Arc<dyn EmbeddingProvider>>) {
        self.pipeline.set_embedding(provider);
    }

    /// The embedding backend clone, if one is wired.
    pub fn embedding(&self) -> Option<Arc<dyn EmbeddingProvider>> {
        self.pipeline.embedding()
    }

    /// Record agent activity for one instance, so the distiller's idle gate
    /// restarts. The Runtime called `notify_active` on every chat turn; in this
    /// process the equivalent signal is the Agent writing a memory through MCP.
    ///
    /// Fire-and-forget and never an error: a recall or a store must not fail
    /// because a scheduling timestamp could not be taken.
    pub fn note_activity(&self, instance_id: &str) {
        self.pipeline.note_activity(instance_id);
    }

    /// Reserve the rebuild slot for `instance_id`, rejecting a run that is
    /// already in flight (`409`).
    ///
    /// Check and reservation happen in one critical section: the Runtime read
    /// the slot, dropped the lock, then wrote it, so two requests could both
    /// pass the check. A *finished* run (`done == true`) is not a collision —
    /// re-running is how a partially-completed rebuild gets retried.
    pub fn begin_rebuild(&self, instance_id: &str) -> Result<()> {
        crate::service::state::validate_instance_id(instance_id)?;
        let mut map = self
            .rebuilds
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if map.get(instance_id).is_some_and(|p| !p.done) {
            return Err(ServiceError::Conflict(format!(
                "an embedding rebuild is already running for instance {instance_id}"
            )));
        }
        map.insert(instance_id.to_string(), RebuildProgress::running());
        Ok(())
    }

    /// Record progress (or the terminal state) of a background rebuild.
    pub fn report_rebuild(&self, instance_id: &str, progress: RebuildProgress) {
        let mut map = self
            .rebuilds
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        map.insert(instance_id.to_string(), progress);
    }

    /// Last known rebuild state for `instance_id`; `None` when that instance
    /// has never run one (the endpoint answers 404).
    pub fn rebuild_progress(&self, instance_id: &str) -> Option<RebuildProgress> {
        self.rebuilds
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(instance_id)
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_in(dir: &Path) -> MemoryState {
        let config = MemoryServiceConfig {
            data_dir: dir.to_path_buf(),
            ..Default::default()
        };
        MemoryState::new(config).expect("state")
    }

    /// Minimal episodic node for the isolation checks.
    fn episode(content: &str) -> crate::Episode {
        crate::Episode {
            session_id: "s1".to_string(),
            turn_index: 0,
            role: "user".to_string(),
            content: content.to_string(),
            embedding: None,
            timestamp: chrono::Utc::now(),
            consolidated: false,
            metadata: Default::default(),
            importance: 0.5,
            knowledge_subtype: None,
            normalized: None,
        }
    }

    #[test]
    fn accepts_uuid_like_ids() {
        for id in [
            "0f1e2d3c-4b5a-6789-abcd-ef0123456789",
            "inst_1",
            "com.acowork.senior-engineer",
            "A1",
        ] {
            assert!(validate_instance_id(id).is_ok(), "{id} should be valid");
        }
    }

    #[test]
    fn rejects_traversal_and_shape_invalid_ids() {
        for id in [
            "",
            "..",
            "../evil",
            "a/../b",
            "a/b",
            "a\\b",
            ".hidden",
            "with space",
            "with%punct",
            &"x".repeat(MAX_INSTANCE_ID_LEN + 1),
        ] {
            assert!(
                validate_instance_id(id).is_err(),
                "{id:?} must be rejected as an instance id"
            );
        }
    }

    #[test]
    fn store_path_stays_under_instances_dir() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path());
        let path = state
            .instances()
            .db_path("0f1e2d3c-4b5a-6789-abcd-ef0123456789")
            .unwrap();
        assert!(path.starts_with(state.config.instances_dir()));
        assert_eq!(path.file_name().unwrap(), INSTANCE_DB_NAME);
    }

    #[test]
    fn store_opens_lazily_and_caches() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path());
        let id = "11111111-1111-4111-8111-111111111111";
        assert!(!state.instances().exists(id), "no file before first touch");
        assert_eq!(state.instances().open_count(), 0);

        let first = state.store(id).expect("open");
        assert!(state.instances().exists(id), "memory.db created on demand");
        assert_eq!(state.instances().open_count(), 1);
        let second = state.store(id).expect("open again");
        assert!(
            Arc::ptr_eq(&first, &second),
            "one handle per instance, not one per request"
        );
    }

    #[test]
    fn instances_are_isolated() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path());
        let a = state.store("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa").unwrap();
        let b = state.store("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb").unwrap();

        a.store_episode(&episode("only in A"))
            .expect("store episode in A");

        assert_eq!(a.node_count().unwrap(), 1);
        assert_eq!(b.node_count().unwrap(), 0, "B must not see A's node");
    }

    #[test]
    fn list_reflects_directories_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path());
        assert!(state.instances().list().is_empty());

        state.store("cccccccc-cccc-4ccc-8ccc-cccccccccccc").unwrap();
        let listed = state.instances().list();
        assert_eq!(listed, vec!["cccccccc-cccc-4ccc-8ccc-cccccccccccc"]);

        // A bare directory without `memory.db` is not an instance.
        std::fs::create_dir_all(dir.path().join("instances").join("not-an-instance")).unwrap();
        assert_eq!(state.instances().list().len(), 1);
    }

    #[test]
    fn evict_forces_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path());
        let id = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
        let first = state.store(id).unwrap();
        assert!(state.instances().evict(id).is_some());
        assert_eq!(state.instances().open_count(), 0);
        let again = state.store(id).unwrap();
        assert!(!Arc::ptr_eq(&first, &again), "a fresh handle over the file");
    }

    #[test]
    fn rebuild_slot_reserves_and_rejects_a_second_run() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path());
        let id = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";
        let other = "ffffffff-ffff-4fff-8fff-ffffffffffff";

        assert!(state.rebuild_progress(id).is_none(), "never ran");
        state.begin_rebuild(id).expect("first run reserves");
        let reserved = state.rebuild_progress(id).unwrap();
        assert!(!reserved.done && reserved.rebuilt == 0, "reserved as live");

        // Same instance, still in flight → conflict.
        let err = state.begin_rebuild(id).unwrap_err();
        assert!(matches!(err, ServiceError::Conflict(_)), "got {err:?}");
        // A different instance is not blocked by this one.
        state.begin_rebuild(other).expect("per-instance guard");

        // A finished run frees the slot so a partial rebuild can be retried.
        state.report_rebuild(
            id,
            crate::wire::RebuildProgress {
                rebuilt: 3,
                total: 10,
                done: true,
                error: None,
            },
        );
        state.begin_rebuild(id).expect("re-run after completion");
        let again = state.rebuild_progress(id).unwrap();
        assert!(
            !again.done && again.rebuilt == 0,
            "the reservation resets the counters"
        );
    }

    #[test]
    fn rebuild_slot_rejects_an_unusable_instance_id() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path());
        assert!(state.begin_rebuild("../escape").is_err());
    }
}

