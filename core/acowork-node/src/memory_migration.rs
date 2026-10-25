//! One-shot memory migration of this node's installed agents (ADR-101 decision 10).
//!
//! Until ADR-101 an instance's memories lived inside its own workspace,
//! `{install_path}/workspace/memory/private.sqlite`, opened by the Runtime.
//! They now live in the memory service's instance namespace, and the rows have
//! to cross that boundary exactly once per instance. The **Node** is the only
//! component allowed to do the moving: it owns the node-local filesystem
//! (ADR-055), while the Gateway is barred from reading agent-private data
//! outright (ADR-009 §5). The Runtime is deliberately not involved at all — by
//! the time this pass runs, memory is no longer its subsystem, and a Runtime
//! side-channel would have to re-derive data the file already holds.
//!
//! # What one instance's migration does
//!
//! 1. Read the legacy store (after a WAL checkpoint, so nothing is left in the
//!    sidecar file) and count its memory rows. Zero rows means there is nothing
//!    to hand over — record the sentinel and stop asking.
//! 2. `POST /api/memory/admin/import?instance_id=` on the Gateway, raw
//!    `application/x-sqlite3` bytes. The service refuses to merge into a
//!    namespace that already has memories and answers `skipped_existing`.
//! 3. On a **real** import, delete the memory rows from the local file
//!    ([`acowork_sqlite::SqliteStore::purge_memory_rows`]) so the workspace
//!    stops holding a second copy of a history the service now owns. The file
//!    itself stays: it still carries the session meta and the conversation
//!    index the Runtime reads (ADR-082).
//! 4. Write `{workspace}/memory/.migrated` recording the outcome, which is what
//!    keeps every later Node start from re-uploading a store already dealt with.
//!
//! # What it never does
//!
//! It never deletes local rows on `skipped_existing`. A namespace can already
//! hold memories because the manifest bootstrap wrote identity facts on the
//! agent's first post-upgrade start — *not* because an earlier pass imported
//! them — and deleting on that reading would destroy the only copy of a user's
//! history. The conservative answer is to keep the rows, say so in the log, and
//! leave the file (and therefore the escape hatch) in place.
//!
//! # Failure model
//!
//! Transport failures and 503 mean "the Gateway or its memory service has not
//! come up yet", so the pass re-tries them on a bounded backoff — the Desktop
//! starts both processes and a Node that boots first must not give up. Any
//! other error is a decision (refused, too large, unreadable): it is logged,
//! recorded as a failed attempt (no sentinel), and left for
//! `acowork-node migrate-memory` to retry once an operator has acted.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use acowork_core::workspace;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;
use tokio::sync::RwLock;

use crate::config::NodeConfig;
use crate::error::{NodeError, Result};
use crate::identity::NodeIdentity;
use crate::state::{NodeState, SharedNodeState};

/// How many rounds a pass waits for the memory service to answer, and how long
/// it sleeps between them. Sized for the Desktop boot order (Gateway spawns the
/// memory service as one of its first steps), not for a long outage.
pub const DEFAULT_ROUNDS: usize = 6;
/// Sleep between retry rounds.
pub const DEFAULT_PAUSE: Duration = Duration::from_secs(5);

/// One installed instance the pass may look at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Instance identity (ADR-073) — also the service namespace key.
    pub instance_id: String,
    /// `{install_path}/workspace`: the Runtime work dir holding the legacy store.
    pub work_dir: PathBuf,
    /// Whether a Runtime process for this instance is live right now.
    pub running: bool,
}

/// How one instance's migration attempt ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Memories handed over and the local copies removed.
    Migrated {
        /// Memory nodes the service accepted.
        imported_nodes: u64,
        /// Local `nodes` rows deleted afterwards.
        purged_rows: usize,
    },
    /// A sentinel from an earlier pass is already there (`--force` overrides).
    AlreadyMigrated,
    /// No memory rows to hand over — either no store file, or an empty one.
    NothingToMigrate,
    /// The service namespace already had memories, so the local rows were kept.
    SkippedExisting,
    /// The agent is running: its store is not safe to read (and it is the
    /// only party that could still be writing to it).
    Running,
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Outcome::Migrated {
                imported_nodes,
                purged_rows,
            } => write!(
                f,
                "migrated {imported_nodes} memories, removed {purged_rows} local rows"
            ),
            Outcome::AlreadyMigrated => write!(f, "already dealt with (sentinel present)"),
            Outcome::NothingToMigrate => write!(f, "nothing to migrate"),
            Outcome::SkippedExisting => write!(
                f,
                "skipped: the service already has memories for this instance, local rows kept"
            ),
            Outcome::Running => write!(f, "skipped: the agent is running"),
        }
    }
}

/// Why an attempt did not complete.
#[derive(Debug, Error)]
pub enum MigrationError {
    /// The Gateway was unreachable or its memory service was not up: a race,
    /// so the pass retries this instance rather than reporting a failure.
    #[error("memory service not ready: {0}")]
    NotReady(String),
    /// A refusal or an I/O problem. Retrying without changing something is
    /// pointless; the operator has to look.
    #[error(transparent)]
    Failed(#[from] NodeError),
}

/// A file that could not be read or written is a plain node failure, not a
/// race — but the migration reports it as one of its own.
impl From<std::io::Error> for MigrationError {
    fn from(error: std::io::Error) -> Self {
        Self::Failed(NodeError::Io(error))
    }
}

/// Knobs of [`run_pass`].
#[derive(Debug, Clone)]
pub struct PassOptions {
    /// Retry rounds for not-ready answers (see [`DEFAULT_ROUNDS`]).
    pub rounds: usize,
    /// Sleep between rounds.
    pub pause: Duration,
    /// Re-run past a sentinel. Needed when a manual fix (shrinking a store,
    /// clearing a namespace) makes a second attempt meaningful.
    pub force: bool,
}

impl Default for PassOptions {
    fn default() -> Self {
        Self {
            rounds: DEFAULT_ROUNDS,
            pause: DEFAULT_PAUSE,
            force: false,
        }
    }
}

/// Enumerate the installed instances, deterministically.
///
/// Sorted by instance id so a pass and its log read the same way twice, and so
/// the CLI output is stable. The running flag is taken from the live process
/// table, not from `state.json`.
pub fn plan(state: &NodeState) -> Vec<Target> {
    let mut targets: Vec<Target> = state
        .installed_agents
        .values()
        .map(|info| Target {
            instance_id: info.instance_id.clone(),
            work_dir: workspace::workspace_dir(&info.install_path),
            running: state.is_running(&info.instance_id),
        })
        .collect();
    targets.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
    targets
}

/// Run the migration over `targets`, retrying not-ready answers.
///
/// One outcome per target, in the order they were passed in. The node
/// credential is re-read at the start of every round: a first-boot machine
/// mints its token *while* this pass is waiting for the service, and a stale
/// `None` snapshot would fail the whole migration closed.
pub async fn run_pass(
    targets: &[Target],
    gateway_http: &str,
    identity: &Arc<RwLock<NodeIdentity>>,
    options: &PassOptions,
) -> Vec<(String, std::result::Result<Outcome, MigrationError>)> {
    let client = reqwest::Client::new();
    let mut pending: Vec<&Target> = targets.iter().collect();
    let mut done: Vec<(String, std::result::Result<Outcome, MigrationError>)> = Vec::new();

    for round in 0..options.rounds.max(1) {
        let node_token = identity.read().await.node_token.clone();
        let mut retry: Vec<&Target> = Vec::new();
        for target in pending {
            let outcome = migrate_one(
                &client,
                gateway_http,
                node_token.as_deref(),
                target,
                options.force,
            )
            .await;
            match outcome {
                Err(MigrationError::NotReady(_)) if round + 1 < options.rounds => {
                    tracing::debug!(
                        instance_id = %target.instance_id,
                        round,
                        "memory service not ready; will retry this instance"
                    );
                    retry.push(target);
                }
                Ok(outcome) => {
                    tracing::info!(instance_id = %target.instance_id, "{outcome}");
                    done.push((target.instance_id.clone(), Ok(outcome)));
                }
                Err(error) => {
                    tracing::warn!(instance_id = %target.instance_id, error = %error,
                        "memory migration did not complete for this instance");
                    done.push((target.instance_id.clone(), Err(error)));
                }
            }
        }
        if retry.is_empty() {
            break;
        }
        tokio::time::sleep(options.pause).await;
        pending = retry;
    }
    done
}

/// The pass the daemon runs in the background at startup.
///
/// Cheap and silent when there is nothing installed, so it is unconditionally
/// spawned: the sentinel check is a `stat`, and a machine that already migrated
/// pays nothing on later boots.
pub async fn startup_pass(
    state: SharedNodeState,
    config: NodeConfig,
    identity: Arc<RwLock<NodeIdentity>>,
) {
    let targets = {
        let node = state.read().await;
        plan(&node)
    };
    if targets.is_empty() {
        return;
    }
    let gateway_http = config.gateway_http_base();
    run_pass(&targets, &gateway_http, &identity, &PassOptions::default()).await;
}

/// Migrate one instance. See the module docs for the sequence.
pub async fn migrate_one(
    client: &reqwest::Client,
    gateway_http: &str,
    node_token: Option<&str>,
    target: &Target,
    force: bool,
) -> std::result::Result<Outcome, MigrationError> {
    if target.running {
        return Ok(Outcome::Running);
    }
    let marker = workspace::migrated_marker_path(&target.work_dir);
    if !force && marker.exists() {
        return Ok(Outcome::AlreadyMigrated);
    }
    let store = workspace::store_path(&target.work_dir);
    if !store.is_file() {
        // No store at all: this instance never persisted anything, so there is
        // no sentinel to write and nothing to look at again next boot.
        return Ok(Outcome::NothingToMigrate);
    }

    let size = std::fs::metadata(&store)?.len();
    if size > workspace::STORE_IMPORT_MAX_BYTES {
        return Err(NodeError::MemoryMigration(format!(
            "{} is {size} bytes, above the {} byte migration ceiling; \
                 the memories have to be moved by hand (ADR-101 decision 10)",
            store.display(),
            workspace::STORE_IMPORT_MAX_BYTES
        ))
        .into());
    }

    let (bytes, local_memories) = read_for_upload(&store)?;
    if local_memories == 0 {
        // Includes a store whose memory tables do not exist yet (an instance
        // created after the upgrade): uploading it would only register an
        // empty namespace for nothing.
        write_sentinel(&marker, "nothing-to-migrate", 0, 0, gateway_http)?;
        return Ok(Outcome::NothingToMigrate);
    }

    let reply = upload(client, gateway_http, node_token, &target.instance_id, bytes).await?;
    let skipped_existing = reply
        .get("skipped_existing")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if skipped_existing {
        // Deliberately NOT a local purge — see the module docs.
        write_sentinel(&marker, "skipped-existing", 0, 0, gateway_http)?;
        return Ok(Outcome::SkippedExisting);
    }
    let imported_nodes = reply
        .get("imported_nodes")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let purged_rows = purge_local(&store)?;
    write_sentinel(
        &marker,
        "migrated",
        imported_nodes,
        purged_rows,
        gateway_http,
    )?;
    Ok(Outcome::Migrated {
        imported_nodes,
        purged_rows,
    })
}

/// Checkpoint the legacy store and read it, along with its memory row count.
///
/// Opening the file is also what creates the schema a brand-new store would not
/// have had; that is the same thing the Runtime's own session-meta open does,
/// so this pass is not adding a state the file would not otherwise reach.
fn read_for_upload(store: &Path) -> std::result::Result<(Vec<u8>, u64), MigrationError> {
    let handle = acowork_sqlite::SqliteStore::open_dim_agnostic(store)
        .map_err(|e| NodeError::MemoryMigration(format!("open {}: {e}", store.display())))?;
    let memories = handle
        .memory_node_count()
        .map_err(|e| NodeError::MemoryMigration(format!("count memories: {e}")))?;
    // Fold the write-ahead log into the main file so the uploaded bytes are
    // the whole store, not a prefix of it.
    handle
        .close()
        .map_err(|e| NodeError::MemoryMigration(format!("checkpoint: {e}")))?;
    drop(handle);

    let bytes = std::fs::read(store)?;
    if !acowork_sqlite::has_sqlite_magic(&bytes) {
        return Err(NodeError::MemoryMigration(format!(
            "{} is not a SQLite database; a pre-ADR-082 store has to be \
                 exported by hand",
            store.display()
        ))
        .into());
    }
    Ok((bytes, memories))
}

/// Hand the bytes to the memory service through the Gateway.
async fn upload(
    client: &reqwest::Client,
    gateway_http: &str,
    node_token: Option<&str>,
    instance_id: &str,
    bytes: Vec<u8>,
) -> std::result::Result<Value, MigrationError> {
    let url = format!("{gateway_http}/api/memory/admin/import");
    let mut request = client
        .post(&url)
        .query(&[("instance_id", instance_id)])
        .header(axum::http::header::CONTENT_TYPE, "application/x-sqlite3")
        .body(bytes);
    if let Some(token) = node_token {
        request = request.header(acowork_core::auth::NODE_TOKEN_HEADER, token);
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(e) => return Err(MigrationError::NotReady(format!("POST {url}: {e}"))),
    };
    let status = response.status();
    if status == axum::http::StatusCode::SERVICE_UNAVAILABLE {
        // The Gateway is up but its memory service is not (ADR-101 §not-ready
        // contract: 503 + `Retry-After`), which is the startup race itself.
        return Err(MigrationError::NotReady(format!(
            "POST {url}: HTTP {status}"
        )));
    }
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        let detail: String = body.chars().take(400).collect();
        return Err(NodeError::MemoryMigration(format!(
            "import of {instance_id} refused: HTTP {status} {detail}"
        ))
        .into());
    }
    response.json::<Value>().await.map_err(|e| {
        MigrationError::NotReady(format!("unreadable import response from {url}: {e}"))
    })
}

/// Drop the migrated memory rows from the local legacy store.
fn purge_local(store: &Path) -> Result<usize> {
    let handle = acowork_sqlite::SqliteStore::open_dim_agnostic(store)
        .map_err(|e| NodeError::MemoryMigration(format!("open {}: {e}", store.display())))?;
    let purged = handle
        .purge_memory_rows()
        .map_err(|e| NodeError::MemoryMigration(format!("purge local memory rows: {e}")))?;
    // Shrink the WAL back into the file: a purged store left at its
    // high-water mark would still hold the deleted content on disk.
    handle
        .close()
        .map_err(|e| NodeError::MemoryMigration(format!("checkpoint after purge: {e}")))?;
    Ok(purged)
}

/// The sentinel body, written once per dealt-with instance.
#[derive(Debug, Serialize)]
struct Sentinel {
    at: DateTime<Utc>,
    /// `migrated` | `nothing-to-migrate` | `skipped-existing`.
    outcome: String,
    /// Memory nodes the service accepted (`0` unless `migrated`).
    imported_nodes: u64,
    /// Local rows deleted afterwards (`0` unless `migrated`).
    purged_rows: usize,
    /// Where the rows went — recorded so an operator can tell which Gateway a
    /// machine was migrated against (the instance id is the directory name the
    /// service used, so it is not repeated here).
    gateway_http: String,
}

/// Write `{workspace}/memory/.migrated`.
///
/// Written last, after the local purge: a sentinel without the deletion behind
/// it would hide a store the next pass should still clean up.
fn write_sentinel(
    marker: &Path,
    outcome: &str,
    imported_nodes: u64,
    purged_rows: usize,
    gateway_http: &str,
) -> Result<()> {
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let sentinel = Sentinel {
        at: Utc::now(),
        outcome: outcome.to_string(),
        imported_nodes,
        purged_rows,
        gateway_http: gateway_http.to_string(),
    };
    let body = serde_json::to_string_pretty(&sentinel)
        .map_err(|e| NodeError::MemoryMigration(format!("serialize sentinel: {e}")))?;
    std::fs::write(marker, body)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    use acowork_memory::{CONVERSATION_LABEL, Episode};
    use axum::extract::{Query, State};
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;
    use axum::{Json, Router};
    use serde_json::json;

    use super::*;

    /// The workspace of one instance under test.
    fn work_dir(root: &Path, name: &str) -> PathBuf {
        let install = root.join(name);
        let workspace = workspace::workspace_dir(&install);
        std::fs::create_dir_all(workspace.join(workspace::MEMORY_DIR)).unwrap();
        workspace
    }

    /// Seed a legacy store the way the Runtime used to: memory rows plus the
    /// session-meta and conversation-index rows that share the file (ADR-082).
    fn seed_store(workspace: &Path, memories: u64, with_conversation: bool) -> PathBuf {
        let path = workspace::store_path(workspace);
        let store = acowork_sqlite::SqliteStore::open(&path, 2).unwrap();
        for i in 0..memories {
            store
                .store_episode(&Episode {
                    session_id: "s1".into(),
                    turn_index: i as u32,
                    role: "user".into(),
                    content: format!("memory number {i}"),
                    embedding: Some(vec![0.25, 0.75]),
                    timestamp: chrono::Utc::now(),
                    consolidated: false,
                    metadata: Default::default(),
                    importance: 0.8,
                    knowledge_subtype: None,
                    normalized: None,
                })
                .unwrap();
        }
        if with_conversation {
            let conn = store.lock();
            conn.execute(
                "INSERT INTO nodes(id, label, status, props) VALUES (900, ?1, 'Active', '{}')",
                [CONVERSATION_LABEL],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO sessions(session_id, agent_id, created_at, last_active_at) \
                 VALUES ('sess-1', 'com.acowork.x', 1, 2)",
                [],
            )
            .unwrap();
        }
        store.close().unwrap();
        drop(store);
        path
    }

    /// A stand-in for the Gateway's `/api/memory/admin/import` route.
    #[derive(Clone)]
    struct Stub {
        hits: Arc<AtomicU64>,
        seen_ids: Arc<Mutex<Vec<String>>>,
        /// Answer 503 for this many requests before accepting one.
        refuse_first: u64,
        /// What a successful call reports.
        reply: Value,
        /// The node credential the last request carried, if any.
        last_token: Arc<Mutex<Option<String>>>,
        last_content_type: Arc<Mutex<Option<String>>>,
        last_body_len: Arc<Mutex<usize>>,
    }

    impl Default for Stub {
        fn default() -> Self {
            Self {
                hits: Arc::new(AtomicU64::new(0)),
                seen_ids: Arc::new(Mutex::new(Vec::new())),
                refuse_first: 0,
                reply: json!({
                    "imported_nodes": 2,
                    "imported_vectors": 0,
                    "imported_lexical_rows": 0,
                    "imported_purge_log_rows": 0,
                    "skipped_existing": false,
                }),
                last_token: Arc::new(Mutex::new(None)),
                last_content_type: Arc::new(Mutex::new(None)),
                last_body_len: Arc::new(Mutex::new(0)),
            }
        }
    }

    /// Bind the stub and return its base URL.
    async fn serve(stub: Stub) -> String {
        async fn handle(
            State(stub): State<Stub>,
            Query(query): Query<HashMap<String, String>>,
            headers: axum::http::HeaderMap,
            body: axum::body::Bytes,
        ) -> Response {
            let n = stub.hits.fetch_add(1, Ordering::SeqCst);
            stub.seen_ids
                .lock()
                .unwrap()
                .push(query.get("instance_id").cloned().unwrap_or_default());
            *stub.last_token.lock().unwrap() = headers
                .get(acowork_core::auth::NODE_TOKEN_HEADER)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            *stub.last_content_type.lock().unwrap() = headers
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            *stub.last_body_len.lock().unwrap() = body.len();
            if n < stub.refuse_first {
                return (StatusCode::SERVICE_UNAVAILABLE, "memory service not ready")
                    .into_response();
            }
            Json(stub.reply.clone()).into_response()
        }

        let app = Router::new()
            .route("/api/memory/admin/import", post(handle))
            .with_state(stub);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://127.0.0.1:{port}")
    }

    fn identity() -> Arc<RwLock<NodeIdentity>> {
        Arc::new(RwLock::new(NodeIdentity {
            node_id: "11111111-1111-4111-8111-111111111111".into(),
            node_name: "migration-test".into(),
            gateway_managed: false,
            node_token: Some("node-token-1".into()),
            gateway_addr: None,
            enrollment: crate::identity::EnrollmentState::Enrolled,
            created_at: chrono::Utc::now(),
            enrolled_at: None,
        }))
    }

    /// A process-table entry, so `plan` can flag a running instance.
    fn slot(instance_id: &str) -> crate::state::AgentSlot {
        crate::state::AgentSlot {
            instance_id: instance_id.to_string(),
            agent_id: "com.acowork.x".into(),
            pid: 4242,
            started_at: chrono::Utc::now(),
            workspace: String::new(),
            dev_mode: false,
            debug_port: None,
            http_port: 19901,
        }
    }

    fn target(workspace: &Path, instance_id: &str) -> Target {
        Target {
            instance_id: instance_id.to_string(),
            work_dir: workspace.to_path_buf(),
            running: false,
        }
    }

    fn options(rounds: usize) -> PassOptions {
        PassOptions {
            rounds,
            pause: Duration::from_millis(10),
            force: false,
        }
    }

    #[test]
    fn plan_sorts_by_instance_id_and_marks_running() {
        let root = std::env::temp_dir();
        let mut state = NodeState::new(16);
        for id in ["bbb", "aaa"] {
            state.add_installed(crate::state::InstalledAgent {
                instance_id: id.to_string(),
                agent_id: "com.acowork.x".into(),
                version: "1.0.0".into(),
                name: "X".into(),
                install_path: root.join(id).to_string_lossy().to_string(),
                manifest: acowork_core::AgentManifest::from_toml(
                    r#"
                    agent_id = "com.acowork.x"
                    version = "1.0.0"
                    name = "X"
                    description = "t"
                    author = "t"
                    runtime_version = "0.1.0"
                    [llm]
                    provider = "openai"
                    model = "gpt-4"
                    "#,
                )
                .unwrap(),
            });
        }
        state.add_agent(slot("aaa"));

        let targets = plan(&state);
        assert_eq!(
            targets
                .iter()
                .map(|t| t.instance_id.as_str())
                .collect::<Vec<_>>(),
            vec!["aaa", "bbb"]
        );
        assert!(targets[0].running, "the running instance is flagged");
        assert!(!targets[1].running);
        // The store a target points at is the instance WORKSPACE store, not a
        // file beside the package.
        assert_eq!(
            workspace::store_path(&targets[1].work_dir),
            root.join("bbb")
                .join(workspace::WORKSPACE_DIR)
                .join("memory")
                .join(workspace::STORE_FILE)
        );
    }

    #[tokio::test]
    async fn a_real_import_deletes_only_the_memory_rows_and_writes_the_sentinel() {
        let root = tempfile::tempdir().unwrap();
        let workspace = work_dir(root.path(), "imported");
        let store = seed_store(&workspace, 2, true);
        // `seed_store` closes the handle, which checkpoints the WAL, so the file
        // on disk is exactly the byte string the pass is supposed to upload.
        let uploaded_len = std::fs::metadata(&store).unwrap().len() as usize;
        let stub = Stub::default();
        let http = serve(stub.clone()).await;

        let outcome = migrate_one(
            &reqwest::Client::new(),
            &http,
            Some("node-token-1"),
            &target(&workspace, "inst-1"),
            false,
        )
        .await
        .unwrap();
        assert_eq!(
            outcome,
            Outcome::Migrated {
                imported_nodes: 2,
                purged_rows: 2
            }
        );

        // Raw bytes, the node credential, and the namespace in the query.
        assert_eq!(stub.seen_ids.lock().unwrap().as_slice(), ["inst-1"]);
        assert_eq!(
            stub.last_token.lock().unwrap().as_deref(),
            Some("node-token-1")
        );
        assert_eq!(
            stub.last_content_type.lock().unwrap().as_deref(),
            Some("application/x-sqlite3")
        );
        assert_eq!(*stub.last_body_len.lock().unwrap(), uploaded_len);

        let handle = acowork_sqlite::SqliteStore::open_dim_agnostic(&store).unwrap();
        assert_eq!(handle.memory_node_count().unwrap(), 0, "memories left");
        assert_eq!(
            handle.node_count_by_label(CONVERSATION_LABEL).unwrap(),
            1,
            "the conversation index is the Runtime's, not ours to delete"
        );
        let sessions: i64 = handle
            .lock()
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sessions, 1, "session meta stays");
        drop(handle);

        let marker = workspace::migrated_marker_path(&workspace);
        let text = std::fs::read_to_string(&marker).unwrap();
        let sentinel: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(sentinel["outcome"], "migrated");
        assert_eq!(sentinel["imported_nodes"], 2);
        assert_eq!(sentinel["purged_rows"], 2);
        assert_eq!(sentinel["gateway_http"], http);
    }

    /// The service refusing to merge is NOT a licence to delete: a namespace
    /// can be non-empty because the manifest bootstrap seeded it, and the local
    /// rows would then be the only copy of the history.
    #[tokio::test]
    async fn a_skipped_existing_reply_keeps_the_local_rows() {
        let root = tempfile::tempdir().unwrap();
        let workspace = work_dir(root.path(), "skipped");
        let store = seed_store(&workspace, 2, false);
        let stub = Stub {
            reply: json!({"imported_nodes": 0, "skipped_existing": true}),
            ..Stub::default()
        };
        let http = serve(stub).await;

        let outcome = migrate_one(
            &reqwest::Client::new(),
            &http,
            None,
            &target(&workspace, "inst-2"),
            false,
        )
        .await
        .unwrap();
        assert_eq!(outcome, Outcome::SkippedExisting);
        assert_eq!(
            acowork_sqlite::SqliteStore::open_dim_agnostic(&store)
                .unwrap()
                .memory_node_count()
                .unwrap(),
            2,
            "nothing was deleted locally"
        );
        let sentinel: Value = serde_json::from_str(
            &std::fs::read_to_string(workspace::migrated_marker_path(&workspace)).unwrap(),
        )
        .unwrap();
        assert_eq!(sentinel["outcome"], "skipped-existing");
    }

    #[tokio::test]
    async fn a_store_without_memories_is_recorded_and_never_uploaded() {
        let root = tempfile::tempdir().unwrap();
        let workspace = work_dir(root.path(), "empty");
        seed_store(&workspace, 0, false);
        let stub = Stub::default();
        let http = serve(stub.clone()).await;

        let outcome = migrate_one(
            &reqwest::Client::new(),
            &http,
            None,
            &target(&workspace, "inst-3"),
            false,
        )
        .await
        .unwrap();
        assert_eq!(outcome, Outcome::NothingToMigrate);
        assert_eq!(
            stub.hits.load(Ordering::SeqCst),
            0,
            "no point asking the service"
        );
        assert!(workspace::migrated_marker_path(&workspace).is_file());
    }

    #[tokio::test]
    async fn an_instance_without_a_store_is_left_alone() {
        let root = tempfile::tempdir().unwrap();
        let workspace = work_dir(root.path(), "no-store");
        let http = serve(Stub::default()).await;

        let outcome = migrate_one(
            &reqwest::Client::new(),
            &http,
            None,
            &target(&workspace, "inst-4"),
            false,
        )
        .await
        .unwrap();
        assert_eq!(outcome, Outcome::NothingToMigrate);
        assert!(
            !workspace::migrated_marker_path(&workspace).exists(),
            "no sentinel for a file that does not exist — a later boot must still find it"
        );
    }

    #[tokio::test]
    async fn a_running_instance_is_not_read() {
        let root = tempfile::tempdir().unwrap();
        let workspace = work_dir(root.path(), "running");
        seed_store(&workspace, 2, false);
        let stub = Stub::default();
        let http = serve(stub.clone()).await;

        let mut target = target(&workspace, "inst-5");
        target.running = true;
        let outcome = migrate_one(&reqwest::Client::new(), &http, None, &target, false)
            .await
            .unwrap();
        assert_eq!(outcome, Outcome::Running);
        assert_eq!(stub.hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_second_pass_skips_the_sentinel_until_forced() {
        let root = tempfile::tempdir().unwrap();
        let workspace = work_dir(root.path(), "twice");
        seed_store(&workspace, 2, false);
        let stub = Stub::default();
        let http = serve(stub.clone()).await;
        let client = reqwest::Client::new();
        let target = target(&workspace, "inst-6");

        migrate_one(&client, &http, None, &target, false)
            .await
            .unwrap();
        let outcome = migrate_one(&client, &http, None, &target, false)
            .await
            .unwrap();
        assert_eq!(outcome, Outcome::AlreadyMigrated);
        assert_eq!(
            stub.hits.load(Ordering::SeqCst),
            1,
            "the sentinel stopped the re-upload"
        );

        let forced = migrate_one(&client, &http, None, &target, true).await;
        // `--force` re-reads the store but does not re-upload it: the rows are
        // already gone, so the pass takes the zero-memory branch and stops.
        assert!(
            matches!(forced.unwrap(), Outcome::NothingToMigrate),
            "a forced pass cannot resurrect deleted rows"
        );
        assert_eq!(
            stub.hits.load(Ordering::SeqCst),
            1,
            "an emptied store never reaches the service again"
        );
    }

    #[tokio::test]
    async fn an_oversized_store_is_refused_before_the_network() {
        let root = tempfile::tempdir().unwrap();
        let workspace = work_dir(root.path(), "huge");
        let store = workspace::store_path(&workspace);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"SQLite format 3\0");
        bytes.resize(workspace::STORE_IMPORT_MAX_BYTES as usize + 1, 0);
        std::fs::write(&store, bytes).unwrap();
        let stub = Stub::default();
        let http = serve(stub.clone()).await;

        let err = migrate_one(
            &reqwest::Client::new(),
            &http,
            None,
            &target(&workspace, "inst-7"),
            false,
        )
        .await
        .expect_err("over the ceiling");
        assert!(
            matches!(err, MigrationError::Failed(NodeError::MemoryMigration(_))),
            "{err}"
        );
        assert!(err.to_string().contains("ceiling"), "{err}");
        assert_eq!(stub.hits.load(Ordering::SeqCst), 0);
        assert!(
            !workspace::migrated_marker_path(&workspace).exists(),
            "a refusal is not an outcome to remember"
        );
    }

    /// 503 is the Desktop boot race: the Gateway answers it while the memory
    /// service is still starting, and the pass must come back.
    #[tokio::test]
    async fn not_ready_answers_are_retried_within_one_pass() {
        let root = tempfile::tempdir().unwrap();
        let workspace = work_dir(root.path(), "retry");
        seed_store(&workspace, 2, false);
        let stub = Stub {
            refuse_first: 1,
            ..Stub::default()
        };
        let http = serve(stub.clone()).await;

        let results = run_pass(
            &[target(&workspace, "inst-8")],
            &http,
            &identity(),
            &options(3),
        )
        .await;
        assert_eq!(results.len(), 1);
        assert!(matches!(
            results[0].1,
            Ok(Outcome::Migrated {
                imported_nodes: 2,
                ..
            })
        ));
        assert_eq!(
            stub.hits.load(Ordering::SeqCst),
            2,
            "one refusal, one success"
        );
    }

    /// Giving up is loud but bounded: the last round reports the failure
    /// instead of sleeping forever.
    #[tokio::test]
    async fn not_ready_is_reported_once_the_rounds_run_out() {
        let root = tempfile::tempdir().unwrap();
        let workspace = work_dir(root.path(), "never-ready");
        seed_store(&workspace, 2, false);
        let stub = Stub {
            refuse_first: u64::MAX,
            ..Stub::default()
        };
        let http = serve(stub.clone()).await;

        let results = run_pass(
            &[target(&workspace, "inst-9")],
            &http,
            &identity(),
            &options(2),
        )
        .await;
        assert!(matches!(results[0].1, Err(MigrationError::NotReady(_))));
        assert_eq!(stub.hits.load(Ordering::SeqCst), 2);
        assert!(
            !workspace::migrated_marker_path(&workspace).exists(),
            "no sentinel on a failure: the next start tries again"
        );
    }

    /// A non-2xx that is not 503 is a decision, not a race: it must not be
    /// retried, and it must not be swallowed.
    #[tokio::test]
    async fn a_refusal_is_not_retried() {
        let root = tempfile::tempdir().unwrap();
        let workspace = work_dir(root.path(), "refused");
        seed_store(&workspace, 2, false);
        // A Gateway build without the memory proxy answers 404 for the path.
        let http = serve_dead().await;

        let results = run_pass(
            &[target(&workspace, "inst-10")],
            &http,
            &identity(),
            &options(3),
        )
        .await;
        assert!(
            matches!(results[0].1, Err(MigrationError::Failed(_))),
            "{}",
            results[0].1.as_ref().err().unwrap()
        );
        assert!(
            !workspace::migrated_marker_path(&workspace).exists(),
            "the store is untouched, so the next attempt is still possible"
        );
        assert_eq!(
            acowork_sqlite::SqliteStore::open_dim_agnostic(workspace::store_path(&workspace))
                .unwrap()
                .memory_node_count()
                .unwrap(),
            2,
            "a refusal must never delete local memories"
        );
    }

    /// A Gateway whose memory service is not proxied at all (404).
    async fn serve_dead() -> String {
        let app = Router::new().fallback(|| async { (StatusCode::NOT_FOUND, "no such route") });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://127.0.0.1:{port}")
    }

    #[tokio::test]
    async fn a_response_that_is_not_json_is_treated_as_not_ready() {
        let root = tempfile::tempdir().unwrap();
        let workspace = work_dir(root.path(), "garbage");
        seed_store(&workspace, 2, false);
        let app = Router::new().route(
            "/api/memory/admin/import",
            post(|| async { (StatusCode::OK, "<html>proxy error</html>") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let err = migrate_one(
            &reqwest::Client::new(),
            &format!("http://127.0.0.1:{port}"),
            None,
            &target(&workspace, "inst-11"),
            false,
        )
        .await
        .expect_err("unreadable reply");
        assert!(matches!(err, MigrationError::NotReady(_)), "{err}");
        assert!(
            !workspace::migrated_marker_path(&workspace).exists(),
            "the local rows must survive an answer we could not read"
        );
    }

    #[tokio::test]
    async fn the_startup_pass_is_quiet_with_nothing_installed() {
        let root = tempfile::tempdir().unwrap();
        let state: SharedNodeState = Arc::new(RwLock::new(NodeState::new(16)));
        let config = NodeConfig {
            home: root.path().to_path_buf(),
            ..NodeConfig::default()
        };
        // No installed agents and no Gateway listening: the pass returns
        // before it touches the network.
        startup_pass(state, config, identity()).await;
    }
}

