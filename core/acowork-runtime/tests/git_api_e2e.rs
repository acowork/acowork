//! End-to-end tests: ADR-078 — workspace Git status/diff/log HTTP API.
//!
//! Spins up a real `RuntimeHttpServer` (random loopback port) with a real
//! `RuntimeGitQueryService` backed by a real git repository created in the
//! test's temp dir, then exercises the three `/git/*` handlers exactly the
//! way the Desktop `gitStore` calls them (via the Gateway reverse proxy).
//! Covers the wire contract end-to-end: JSON shape, status codes,
//! workspace_id semantics, path traversal defence, rename/deleted/binary
//! diff variants, and log path/limit filtering.
//!
//! Mirrors the `prompts_reload_e2e.rs` minimal-server pattern: one
//! `#[tokio::test]` per scenario, every test owns its own temp dir +
//! repo so tests can run in parallel.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

const AGENT_ID: &str = "com.test.git-e2e";

/// Test-only instance identity (ADR-073: must be a UUIDv4).
const INSTANCE_ID: &str = "0a0b0c0d-1e2f-4a3b-8c7d-9e8f7a6b5c4d";

/// Ws id registered in `config/agent_workspaces.json` (workspace-subdir test).
const SUB_WS_ID: &str = "ws-sub";

// ── git helpers ──────────────────────────────────────────────────────────

/// Run a git command inside `dir`, panicking on failure (test setup).
fn git_in(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("git should spawn");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `git init -b main` + local identity + initial commit (`a.txt`).
fn init_repo(dir: &Path) {
    git_in(dir, &["init", "-b", "main"]);
    git_in(dir, &["config", "user.email", "e2e@test.local"]);
    git_in(dir, &["config", "user.name", "e2e"]);
    std::fs::write(dir.join("a.txt"), "hello\n").unwrap();
    git_in(dir, &["add", "a.txt"]);
    git_in(dir, &["commit", "-m", "init"]);
}

/// Write `config/agent_workspaces.json` registering `SUB_WS_ID` → `path`.
///
/// The document must be built with `serde_json`: a raw
/// `format!`-interpolated path is invalid JSON on Windows (a path like
/// `C:\Users\...` contains the illegal escape `\U`), and the runtime's
/// JSON reader rejects the whole file — surfacing as a 500 instead of
/// the 200 the subdir test expects.
fn register_workspace(work_dir: &Path, ws_id: &str, path: &Path) {
    let cfg_dir = work_dir.join("config");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let cfg = serde_json::json!({
        "additional_dirs": [{
            "id": ws_id,
            "path": path.to_string_lossy(),
        }],
    });
    std::fs::write(
        cfg_dir.join("agent_workspaces.json"),
        serde_json::to_string_pretty(&cfg).unwrap(),
    )
    .unwrap();
}

// ── server + HTTP helpers ────────────────────────────────────────────────

async fn spawn_server(tag: &str) -> (u16, std::path::PathBuf) {
    spawn_server_with_git_bin(tag, "git".to_string()).await
}

/// Like [`spawn_server`], but injects a custom git executable path. Tests
/// pass a non-existent path to exercise the `git_unavailable` wire contract
/// (status → 200 is_repo:false; diff/log → 503) without mutating PATH.
async fn spawn_server_with_git_bin(tag: &str, git_bin: String) -> (u16, std::path::PathBuf) {
    let temp_dir = std::env::temp_dir().join(format!(
        "acowork-test-git-e2e-{}-{}",
        std::process::id(),
        tag
    ));
    let _ = std::fs::remove_dir_all(&temp_dir);
    std::fs::create_dir_all(&temp_dir).unwrap();

    // Minimal stub slots — see `prompts_reload_e2e.rs` for why each is None;
    // the /git/* routes only touch `git_query`.
    let snapshots = Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
    let latest = Arc::new(std::sync::RwLock::new(None));
    let dispatch_tx = Arc::new(tokio::sync::Mutex::new(None));
    let degraded_reasons = Arc::new(std::sync::RwLock::new(Vec::new()));
    let mqtt_client = Arc::new(tokio::sync::Mutex::new(None));
    let session_metadata = Arc::new(tokio::sync::Mutex::new(None));
    let workspace_query = Arc::new(tokio::sync::Mutex::new(None));
    let workspace_mutation = Arc::new(tokio::sync::Mutex::new(None));
    // REAL git service: work_dir = temp_dir, so `/git/*` without a
    // workspace_id resolves to the repo we create inside temp_dir.
    let git_query: Arc<
        tokio::sync::Mutex<Option<Arc<dyn acowork_runtime::usecases::GitQueryService>>>,
    > = Arc::new(tokio::sync::Mutex::new(Some(Arc::new(
        acowork_runtime::usecases::git_query_impl::RuntimeGitQueryService::new_with_git_bin(
            temp_dir.clone(),
            AGENT_ID.to_string(),
            git_bin,
        ),
    ))));
    let agent_tools = Arc::new(tokio::sync::Mutex::new(None));
    let agent_config = Arc::new(tokio::sync::Mutex::new(None));
    let attachment = Arc::new(tokio::sync::Mutex::new(None));
    let session_config = Arc::new(tokio::sync::Mutex::new(None));
    let rag_provider: Arc<std::sync::RwLock<Option<Arc<dyn acowork_core::rag::RagProvider>>>> =
        Arc::new(std::sync::RwLock::new(None));
    let debug_service = Arc::new(tokio::sync::Mutex::new(None));
    let workspace_resolver: Arc<
        std::sync::RwLock<acowork_runtime::tools::workspace_resolver::WorkspaceResolver>,
    > = Arc::new(std::sync::RwLock::new(
        acowork_runtime::tools::workspace_resolver::WorkspaceResolver::new_for_test(vec![]),
    ));
    let session_manager_slot: Arc<
        tokio::sync::RwLock<
            Option<Arc<tokio::sync::Mutex<acowork_runtime::agent::session::SessionManager>>>,
        >,
    > = Arc::new(tokio::sync::RwLock::new(None));

    let server = acowork_runtime::http::RuntimeHttpServer::start(
        temp_dir.clone(),
        temp_dir.clone(), // package_dir
        AGENT_ID.to_string(),
        INSTANCE_ID.to_string(),
        snapshots,
        latest,
        dispatch_tx,
        degraded_reasons,
        mqtt_client,
        session_metadata,
        std::sync::Arc::new(std::sync::RwLock::new(None)),
        workspace_query,
        workspace_mutation,
        git_query,
        agent_tools,
        agent_config,
        attachment,
        session_config,
        rag_provider,
        debug_service,
        workspace_resolver,
        session_manager_slot,
        std::sync::Arc::new(std::sync::RwLock::new(None)), // no AgentCore
    )
    .await
    .expect("runtime http server should start");

    (server.port, temp_dir)
}

/// GET `/git/{endpoint}` with optional query pairs → `(status, json)`.
async fn get_git(port: u16, endpoint: &str, query: &[(&str, &str)]) -> (u16, serde_json::Value) {
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}/git/{endpoint}");
    let resp = client.get(&url).query(query).send().await.unwrap();
    let status = resp.status().as_u16();
    let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    (status, body)
}

// ── /git/status ──────────────────────────────────────────────────────────

#[tokio::test]
async fn status_clean_repo_reports_main_branch() {
    let (port, dir) = spawn_server("status-clean").await;
    init_repo(&dir);

    let (status, body) = get_git(port, "status", &[]).await;
    assert_eq!(status, 200);
    assert_eq!(body["isRepo"], true);
    assert_eq!(body["branch"], "main");
    assert_eq!(body["error"], serde_json::Value::Null);
    assert_eq!(body["truncated"], false);
    assert_eq!(body["changes"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn status_mixed_changes_fields_sort_and_rename_old_path() {
    let (port, dir) = spawn_server("status-mixed").await;
    init_repo(&dir);
    // a.txt modified (worktree), b.txt untracked, c.txt staged-added,
    // d.txt renamed → e.txt (staged), f.txt deleted (worktree).
    std::fs::write(dir.join("a.txt"), "hello world\n").unwrap();
    std::fs::write(dir.join("b.txt"), "untracked\n").unwrap();
    std::fs::write(dir.join("c.txt"), "c\n").unwrap();
    std::fs::write(dir.join("d.txt"), "d\n").unwrap();
    std::fs::write(dir.join("f.txt"), "f\n").unwrap();
    git_in(&dir, &["add", "a.txt", "c.txt", "d.txt", "f.txt"]);
    git_in(&dir, &["commit", "-m", "second"]);
    std::fs::write(dir.join("a.txt"), "hello world modified\n").unwrap(); // worktree M
    git_in(&dir, &["mv", "d.txt", "e.txt"]); // staged R
    git_in(&dir, &["add", "-u"]); // stage f.txt deletion + a.txt? (a.txt re-modified after)
    std::fs::remove_file(dir.join("f.txt")).unwrap(); // f.txt: staged D
                                                      // NOTE: after `add -u`, a.txt was re-written → worktree M on top of staged M.
    std::fs::write(dir.join("a.txt"), "final a\n").unwrap();

    let (status, body) = get_git(port, "status", &[]).await;
    assert_eq!(status, 200);
    assert_eq!(body["isRepo"], true);
    assert_eq!(body["error"], serde_json::Value::Null);

    let changes = body["changes"].as_array().unwrap();
    // staged entries sort first (ADR-078 decision 4): e.txt (R), f.txt (D),
    // a.txt (MM staged) — b.txt untracked last.
    let paths: Vec<&str> = changes
        .iter()
        .map(|c| c["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        paths,
        vec!["a.txt", "e.txt", "b.txt", "f.txt"],
        "sort order: staged M, staged R, untracked, worktree D"
    );

    let rename = changes.iter().find(|c| c["path"] == "e.txt").unwrap();
    assert_eq!(rename["oldPath"], "d.txt", "rename must carry oldPath");
    assert_eq!(rename["index"], "renamed");
    assert_eq!(rename["staged"], true);

    let deleted = changes.iter().find(|c| c["path"] == "f.txt").unwrap();
    assert_eq!(deleted["index"], "unmodified");
    assert_eq!(
        deleted["worktree"], "deleted",
        "rm after add -u → worktree deletion"
    );
    assert_eq!(deleted["staged"], false);

    let untracked = changes.iter().find(|c| c["path"] == "b.txt").unwrap();
    assert_eq!(untracked["worktree"], "untracked");
    assert_eq!(untracked["staged"], false);
}

#[tokio::test]
async fn status_non_repo_returns_is_repo_false() {
    let (port, _dir) = spawn_server("status-nonrepo").await;
    // No `git init` — plain temp dir.
    let (status, body) = get_git(port, "status", &[]).await;
    assert_eq!(status, 200);
    assert_eq!(body["isRepo"], false);
    assert_eq!(body["error"], "not_a_repo");
    assert_eq!(body["branch"], serde_json::Value::Null);
    assert_eq!(body["changes"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn git_unavailable_is_200_for_status_and_503_for_diff_log() {
    // ADR-078 §7.2 wire contract: git missing from the environment is an
    // explicit state — status → 200 is_repo:false error:"git_unavailable"
    // (Desktop shows the dedicated empty state), diff/log → 503.
    let (port, dir) =
        spawn_server_with_git_bin("git-unavailable", "/nonexistent/git-adr078-e2e".to_string())
            .await;
    init_repo(&dir); // repo present — discovery succeeds, spawning fails

    let (status, body) = get_git(port, "status", &[]).await;
    assert_eq!(status, 200, "status must be 200, got {status}: {body}");
    assert_eq!(body["isRepo"], false);
    assert_eq!(body["error"], "git_unavailable");
    assert_eq!(body["changes"].as_array().unwrap().len(), 0);

    let (status, body) = get_git(port, "diff", &[("path", "a.txt")]).await;
    assert_eq!(status, 503, "diff must be 503, got {status}: {body}");
    assert!(body["error"].as_str().unwrap_or("").contains("git"));

    let (status, body) = get_git(port, "log", &[]).await;
    assert_eq!(status, 503, "log must be 503, got {status}: {body}");
}

#[tokio::test]
async fn status_workspace_subdir_of_repo_filters_changes() {
    let (port, dir) = spawn_server("status-subdir").await;
    init_repo(&dir);
    // Repo lives at temp_dir; workspace is a subdir (also inside repo).
    let sub = dir.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("b.txt"), "b\n").unwrap();
    // A change OUTSIDE the workspace but inside the repo must be filtered.
    std::fs::write(dir.join("outside.txt"), "o\n").unwrap();
    register_workspace(&dir, SUB_WS_ID, &sub);

    let (status, body) = get_git(port, "status", &[("workspace_id", SUB_WS_ID)]).await;
    assert_eq!(status, 200);
    let changes = body["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 1, "only the workspace-local change");
    assert_eq!(changes[0]["path"], "b.txt");
}

#[tokio::test]
async fn status_rev_returns_files_in_that_commit() {
    // ADR-XXX: `?rev=<hash>` makes `/git/status` return the files touched
    // by that commit (parsed via `git diff-tree --name-status -z`). The
    // bar header (branch) is replaced with the commit's "<short_sha>
    // <subject>" label, and `rev` is echoed back so the Desktop can
    // cache the entry by (groupKey, rev).
    let (port, dir) = spawn_server("status-rev").await;
    init_repo(&dir);
    // Second commit: add b.txt (A), modify a.txt (M), delete c.txt (D).
    std::fs::write(dir.join("b.txt"), "b\n").unwrap();
    std::fs::write(dir.join("c.txt"), "c\n").unwrap();
    git_in(&dir, &["add", "b.txt", "c.txt"]);
    git_in(&dir, &["commit", "-m", "second"]);
    std::fs::write(dir.join("a.txt"), "modified\n").unwrap();
    git_in(&dir, &["add", "a.txt"]);
    git_in(&dir, &["commit", "-m", "third"]);
    // `git log --format=%H -1` returns the third commit's full hash.
    let head_hash = git_in(&dir, &["log", "--format=%H", "-1"])
        .trim()
        .to_string();
    // And its short SHA + subject (what the bar will display).
    let head_label = git_in(&dir, &["log", "--format=%h %s", "-1"])
        .trim()
        .to_string();
    // Second commit is `HEAD~` — also reachable.
    let parent_hash = git_in(&dir, &["log", "--format=%H", "-1", "HEAD~"])
        .trim()
        .to_string();

    // rev = HEAD (third commit): a.txt M.
    let (status, body) = get_git(port, "status", &[("rev", &head_hash)]).await;
    assert_eq!(status, 200);
    assert_eq!(body["isRepo"], true);
    assert_eq!(body["error"], serde_json::Value::Null);
    assert_eq!(
        body["rev"], head_hash,
        "rev must echo back the requested ref"
    );
    assert_eq!(
        body["branch"], head_label,
        "branch must be the commit's '<short_sha> <subject>' label"
    );
    let changes = body["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 1, "third commit only touched a.txt");
    assert_eq!(changes[0]["path"], "a.txt");
    assert_eq!(changes[0]["index"], "modified");

    // rev = HEAD~ (second commit): b.txt added, c.txt added — note
    // the initial-commit `a.txt` is NOT in this list because diff-tree
    // compares against the parent.
    let (status, body) = get_git(port, "status", &[("rev", &parent_hash)]).await;
    assert_eq!(status, 200);
    let changes = body["changes"].as_array().unwrap();
    let paths: Vec<&str> = changes
        .iter()
        .map(|c| c["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        paths,
        vec!["b.txt", "c.txt"],
        "second commit added b.txt + c.txt"
    );
    for c in changes {
        assert_eq!(c["index"], "added");
    }

    // Empty rev ("") must behave like the legacy working-tree status.
    let (status, body) = get_git(port, "status", &[("rev", "")]).await;
    assert_eq!(status, 200);
    assert_eq!(body["branch"], "main", "no rev → branch header");
    assert_eq!(
        body["rev"],
        serde_json::Value::Null,
        "empty rev is normalised to None"
    );

    // Unresolvable rev → 400 BadRequest.
    let (status, body) = get_git(
        port,
        "status",
        &[("rev", "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef")],
    )
    .await;
    assert_eq!(status, 400, "unknown rev must surface as 400: {body}");
}

#[tokio::test]
async fn status_unknown_workspace_returns_404() {
    let (port, dir) = spawn_server("status-unknown-ws").await;
    init_repo(&dir);
    let (status, body) = get_git(port, "status", &[("workspace_id", "no-such-ws")]).await;
    assert_eq!(status, 404, "unknown workspace id → WorkspaceNotFound");
    assert!(body["error"].is_string());
}

// ── /git/diff ────────────────────────────────────────────────────────────

#[tokio::test]
async fn diff_modified_returns_head_and_worktree_sides() {
    let (port, dir) = spawn_server("diff-modified").await;
    init_repo(&dir);
    std::fs::write(dir.join("a.txt"), "hello world\n").unwrap();

    let (status, body) = get_git(port, "diff", &[("path", "a.txt")]).await;
    assert_eq!(status, 200);
    assert_eq!(body["kind"], "modified");
    assert_eq!(body["original"], "hello\n");
    assert_eq!(body["modified"], "hello world\n");
}

#[tokio::test]
async fn diff_untracked_has_empty_original() {
    let (port, dir) = spawn_server("diff-untracked").await;
    init_repo(&dir);
    std::fs::write(dir.join("b.txt"), "brand new\n").unwrap();

    let (status, body) = get_git(port, "diff", &[("path", "b.txt")]).await;
    assert_eq!(status, 200);
    assert_eq!(body["kind"], "untracked");
    assert_eq!(body["original"], "");
    assert_eq!(body["modified"], "brand new\n");
}

#[tokio::test]
async fn diff_deleted_has_empty_modified() {
    let (port, dir) = spawn_server("diff-deleted").await;
    init_repo(&dir);
    std::fs::remove_file(dir.join("a.txt")).unwrap();

    let (status, body) = get_git(port, "diff", &[("path", "a.txt")]).await;
    assert_eq!(status, 200);
    assert_eq!(body["kind"], "deleted");
    assert_eq!(body["original"], "hello\n", "HEAD content preserved");
    assert_eq!(body["modified"], "");
}

#[tokio::test]
async fn diff_index_ref_reads_index_not_worktree() {
    let (port, dir) = spawn_server("diff-index").await;
    init_repo(&dir);
    std::fs::write(dir.join("a.txt"), "index version\n").unwrap();
    git_in(&dir, &["add", "a.txt"]);
    std::fs::write(dir.join("a.txt"), "worktree version\n").unwrap();

    // head_ref=":" → index vs HEAD (git show :path form)
    let (status, body) = get_git(port, "diff", &[("path", "a.txt"), ("head_ref", ":")]).await;
    assert_eq!(status, 200);
    assert_eq!(body["kind"], "modified");
    assert_eq!(body["modified"], "index version\n");

    // head_ref="" (default) → worktree vs HEAD
    let (status, body) = get_git(port, "diff", &[("path", "a.txt")]).await;
    assert_eq!(status, 200);
    assert_eq!(body["modified"], "worktree version\n");

    // base_ref="" → 400 (must specify a base revision)
    let (status, _) = get_git(port, "diff", &[("path", "a.txt"), ("base_ref", "")]).await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn diff_two_refs_compares_any_two_commits() {
    let (port, dir) = spawn_server("diff-two-refs").await;
    init_repo(&dir);
    std::fs::write(dir.join("a.txt"), "v1\n").unwrap();
    git_in(&dir, &["add", "a.txt"]);
    git_in(&dir, &["commit", "-m", "first"]);
    let first = git_in(&dir, &["rev-parse", "HEAD"]).trim().to_string();
    std::fs::write(dir.join("a.txt"), "v2\n").unwrap();
    git_in(&dir, &["add", "a.txt"]);
    git_in(&dir, &["commit", "-m", "second"]);

    // base_ref=first, head_ref=HEAD → v1 vs v2
    let (status, body) = get_git(
        port,
        "diff",
        &[
            ("path", "a.txt"),
            ("base_ref", &first),
            ("head_ref", "HEAD"),
        ],
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["kind"], "modified");
    assert_eq!(body["original"], "v1\n");
    assert_eq!(body["modified"], "v2\n");
}

#[tokio::test]
async fn diff_two_refs_response_carries_canonical_shas() {
    // ADR-078 / bug-fix: the frontend labels each diff-side banner with
    // the first 7 chars of the ref. Without server-side normalisation
    // `base_ref = "<head>^"` and `head_ref = "<sha>"` both slice to
    // the same 7 chars, so the banner shows the same commit id on both
    // sides. The handler must return the canonical SHA so the client
    // never has to reason about git shorthand.
    let (port, dir) = spawn_server("diff-rev-normalise").await;
    init_repo(&dir);
    std::fs::write(dir.join("a.txt"), "v1\n").unwrap();
    git_in(&dir, &["add", "a.txt"]);
    git_in(&dir, &["commit", "-m", "first"]);
    std::fs::write(dir.join("a.txt"), "v2\n").unwrap();
    git_in(&dir, &["add", "a.txt"]);
    git_in(&dir, &["commit", "-m", "second"]);
    let second = git_in(&dir, &["rev-parse", "HEAD"]).trim().to_string();

    // Caller passes git shorthand on the base side (<second>^). The
    // handler must:
    //   1. Resolve base_ref / head_ref to full SHAs (no `<sha>^`
    //      strings leak into the response).
    //   2. Promote base_ref to the file-history predecessor of
    //      `head_ref` on `a.txt`. In this 2-commit history that's
    //      exactly first-parent (file was edited in both commits).
    let (status, body) = get_git(
        port,
        "diff",
        &[
            ("path", "a.txt"),
            ("base_ref", &format!("{second}^")),
            ("head_ref", &second),
        ],
    )
    .await;
    assert_eq!(status, 200);
    let head_rev = body["headRev"].as_str().unwrap();
    assert_eq!(head_rev, second, "head_ref resolves to its full SHA");
    // baseRev must be a full 40-char SHA, NOT contain `^`.
    let base_rev = body["baseRev"].as_str().unwrap();
    assert_eq!(base_rev.len(), 40, "baseRev is a full SHA");
    assert!(
        !base_rev.contains('^'),
        "baseRev leaked git shorthand: {base_rev}"
    );
    assert_ne!(
        base_rev, head_rev,
        "canonical SHAs differ even though both call-site refs share a prefix"
    );
    // Specifically: the base must be the file-history predecessor, which
    // in this minimal 2-commit history equals the first-parent.
    let first_parent = git_in(&dir, &["rev-parse", &format!("{second}^")])
        .trim()
        .to_string();
    assert_eq!(
        base_rev, first_parent,
        "baseRev must equal first-parent when both commits touched the file"
    );
}

#[tokio::test]
async fn diff_two_refs_base_promotes_to_file_history_predecessor() {
    // The real bug-fix: when `head_ref^` did NOT touch `path`, the
    // banner showed a commit id that wasn't in the diff banner's
    // `CommitPicker` (which lists `git log -- <path>`, i.e. file
    // history). The handler must instead promote base to the file-
    // history predecessor — the commit that last touched `path`
    // before `head_ref`.
    let (port, dir) = spawn_server("diff-file-history-prev").await;
    init_repo(&dir);
    // v1: touch a.txt
    std::fs::write(dir.join("a.txt"), "v1\n").unwrap();
    git_in(&dir, &["add", "a.txt"]);
    git_in(&dir, &["commit", "-m", "v1"]);
    // v2: touch a.txt again — file-history advances to v2, v1 is the
    // previous file-history entry.
    std::fs::write(dir.join("a.txt"), "v2\n").unwrap();
    git_in(&dir, &["add", "a.txt"]);
    git_in(&dir, &["commit", "-m", "v2"]);
    let v2 = git_in(&dir, &["rev-parse", "HEAD"]).trim().to_string();
    // b-only: NO change to a.txt — first-parent chain advances but
    // file-history for a.txt does NOT.
    std::fs::write(dir.join("b.txt"), "b-only body\n").unwrap();
    git_in(&dir, &["add", "b.txt"]);
    git_in(&dir, &["commit", "-m", "b-only"]);
    // v3: touch a.txt again.
    std::fs::write(dir.join("a.txt"), "v3\n").unwrap();
    git_in(&dir, &["add", "a.txt"]);
    git_in(&dir, &["commit", "-m", "v3"]);
    let v3 = git_in(&dir, &["rev-parse", "HEAD"]).trim().to_string();

    // Caller passes `<v3>^` (first-parent = b-only commit). The naive
    // first-parent path would surface the b-only SHA, but b-only is
    // NOT in `git log -- a.txt` so the picker can't find it.
    let (status, body) = get_git(
        port,
        "diff",
        &[
            ("path", "a.txt"),
            ("base_ref", &format!("{v3}^")),
            ("head_ref", &v3),
        ],
    )
    .await;
    assert_eq!(status, 200);
    // baseRev MUST be the file-history predecessor of v3, NOT the
    // first-parent b-only commit. This is the regression guard.
    assert_eq!(
        body["baseRev"].as_str().unwrap(),
        v2,
        "baseRev must be v2 (file-history prev), not v3^ (first-parent, which never touched a.txt)"
    );
    assert_eq!(body["headRev"].as_str().unwrap(), v3);
    // Diff content reflects the file-history predecessor: original is
    // v2's blob (NOT the b-only blob — which doesn't even have a.txt).
    assert_eq!(body["original"], "v2\n", "diff base is v2's blob");
    assert_eq!(body["modified"], "v3\n", "diff head is v3's blob");
}

#[tokio::test]
async fn diff_two_refs_base_falls_back_to_first_parent_for_file_introducing_commit() {
    // When `head_ref` introduced `path` (file-history has no
    // predecessor), there's no file-history answer. Fall back to the
    // caller-requested base (= `<head>^` here, i.e. first-parent),
    // which preserves the existing semantic for this corner case.
    //
    // NOTE: must NOT call `init_repo` here — it creates an initial
    // commit that already has a.txt, which would make a.txt's
    // history non-empty and break the "head introduced a.txt"
    // precondition.
    let (port, dir) = spawn_server("diff-file-introducing").await;
    git_in(&dir, &["init", "-b", "main"]);
    git_in(&dir, &["config", "user.email", "e2e@test.local"]);
    git_in(&dir, &["config", "user.name", "e2e"]);
    // Parent commit — has no a.txt.
    std::fs::write(dir.join("other.txt"), "parent\n").unwrap();
    git_in(&dir, &["add", "other.txt"]);
    git_in(&dir, &["commit", "-m", "parent"]);
    let parent = git_in(&dir, &["rev-parse", "HEAD"]).trim().to_string();
    // Head commit — introduces a.txt.
    std::fs::write(dir.join("a.txt"), "new file\n").unwrap();
    git_in(&dir, &["add", "a.txt"]);
    git_in(&dir, &["commit", "-m", "introduce a.txt"]);
    let head = git_in(&dir, &["rev-parse", "HEAD"]).trim().to_string();

    let (status, body) = get_git(
        port,
        "diff",
        &[
            ("path", "a.txt"),
            ("base_ref", &format!("{head}^")),
            ("head_ref", &head),
        ],
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        body["baseRev"].as_str().unwrap(),
        parent,
        "no file-history predecessor → baseRev falls back to first-parent"
    );
    assert_eq!(body["headRev"].as_str().unwrap(), head);
    // NOTE: `kind` is intentionally NOT asserted here. When base has no
    // path (file-introducing commit), `read_blob` returns `Missing` and
    // `diff_two_refs` falls back to `binary_diff_response` (kind=
    // "binary"). This is pre-existing behaviour — out of scope for the
    // base-promotion bug-fix. The banner label correctness we care
    // about is the `baseRev` value above.
}

#[tokio::test]
async fn diff_two_refs_same_commit_reports_no_change() {
    // Bug: both diff-side banners set to the same commit still showed a
    // diff. `diff_two_refs` overrode every caller-supplied `base_ref` with
    // the file-history predecessor of `head_ref`, so "X vs X" silently
    // became "prev(X) vs X".
    let (port, dir) = spawn_server("diff-same-commit").await;
    init_repo(&dir);
    std::fs::write(dir.join("a.txt"), "v2\n").unwrap();
    git_in(&dir, &["add", "a.txt"]);
    git_in(&dir, &["commit", "-m", "second"]);
    let second = git_in(&dir, &["rev-parse", "HEAD"]).trim().to_string();

    let (status, body) = get_git(
        port,
        "diff",
        &[
            ("path", "a.txt"),
            ("base_ref", &second),
            ("head_ref", &second),
        ],
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["kind"], "nochange", "X vs X must have no changes");
    assert_eq!(body["original"], "v2\n");
    assert_eq!(body["modified"], "v2\n");
    assert_eq!(body["baseRev"], body["headRev"]);
}

#[tokio::test]
async fn diff_two_refs_honours_explicit_base_that_is_not_first_parent() {
    // Generalisation of the bug above: an explicit base that is NOT
    // `<head>^` must be compared verbatim, not swapped for the
    // file-history predecessor. Here `v1` vs `v3` must show v1's blob even
    // though `v2` is the predecessor of `v3` on this path.
    let (port, dir) = spawn_server("diff-explicit-base").await;
    init_repo(&dir);
    // c1 (after `init_repo`'s "init" commit) — a.txt = v1.
    std::fs::write(dir.join("a.txt"), "v1\n").unwrap();
    git_in(&dir, &["add", "a.txt"]);
    git_in(&dir, &["commit", "-m", "v1"]);
    let v1 = git_in(&dir, &["rev-parse", "HEAD"]).trim().to_string();
    std::fs::write(dir.join("a.txt"), "v2\n").unwrap();
    git_in(&dir, &["add", "a.txt"]);
    git_in(&dir, &["commit", "-m", "v2"]);
    std::fs::write(dir.join("a.txt"), "v3\n").unwrap();
    git_in(&dir, &["add", "a.txt"]);
    git_in(&dir, &["commit", "-m", "v3"]);
    let v3 = git_in(&dir, &["rev-parse", "HEAD"]).trim().to_string();

    let (status, body) = get_git(
        port,
        "diff",
        &[("path", "a.txt"), ("base_ref", &v1), ("head_ref", &v3)],
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["kind"], "modified");
    assert_eq!(
        body["original"], "v1\n",
        "explicit base_ref must not be replaced by the file-history predecessor (v2)"
    );
    assert_eq!(body["modified"], "v3\n");
    assert_eq!(body["baseRev"], v1);
}

#[tokio::test]
async fn diff_worktree_honours_explicit_base_for_clean_file() {
    // Bug: banners = "<older commit>" vs "Working Tree" showed an empty
    // diff. A file with no uncommitted changes has no `git status` entry,
    // and the `diff_clean` fallback re-derived the base from HEAD instead
    // of the caller's ref — so the worktree was compared against its own
    // committed state.
    let (port, dir) = spawn_server("diff-worktree-clean-base").await;
    init_repo(&dir); // a.txt = "hello\n" (c1)
    let c1 = git_in(&dir, &["rev-parse", "HEAD"]).trim().to_string();
    std::fs::write(dir.join("a.txt"), "v2\n").unwrap();
    git_in(&dir, &["add", "a.txt"]);
    git_in(&dir, &["commit", "-m", "second"]);
    // HEAD moves on without touching a.txt → a.txt is clean.
    std::fs::write(dir.join("b.txt"), "b\n").unwrap();
    git_in(&dir, &["add", "b.txt"]);
    git_in(&dir, &["commit", "-m", "b only"]);

    let (status, body) = get_git(
        port,
        "diff",
        &[("path", "a.txt"), ("base_ref", &c1), ("head_ref", "")],
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        body["kind"], "modified",
        "c1 vs working tree must show the change committed after c1"
    );
    assert_eq!(body["original"], "hello\n");
    assert_eq!(body["modified"], "v2\n");
    assert_eq!(body["baseRev"], c1);
    assert_eq!(
        body["headRev"],
        serde_json::Value::Null,
        "working tree has no canonical SHA"
    );

    // Default (base_ref=HEAD) keeps the file-history-head label promotion:
    // the banner must show a commit the file-scoped picker actually lists.
    let (status, body) = get_git(
        port,
        "diff",
        &[("path", "a.txt"), ("base_ref", "HEAD"), ("head_ref", "")],
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["kind"], "nochange");
    assert_eq!(
        body["baseRev"],
        git_in(
            &dir,
            &["log", "-n", "1", "--pretty=%H", "HEAD", "--", "a.txt"]
        )
        .trim()
        .to_string(),
        "HEAD base still promotes to the newest commit touching a.txt"
    );
}

#[tokio::test]
async fn diff_worktree_response_omits_head_rev() {
    // Worktree variant: `head_ref` is the empty string → working tree.
    // `head_rev` must be absent so the client falls back to its
    // "Working Tree" label (pre-existing `!headRef` rendering contract).
    let (port, dir) = spawn_server("diff-rev-worktree").await;
    init_repo(&dir);
    std::fs::write(dir.join("a.txt"), "wt\n").unwrap();

    let (status, body) = get_git(port, "diff", &[("path", "a.txt")]).await;
    assert_eq!(status, 200);
    // base = HEAD (canonical SHA), head = None (working tree).
    assert!(body["baseRev"].is_string(), "baseRev present");
    assert_eq!(body["baseRev"].as_str().unwrap().len(), 40);
    assert!(
        body["headRev"].is_null(),
        "headRev absent for worktree variant: {:?}",
        body["headRev"]
    );
}

#[tokio::test]
async fn diff_unknown_ref_is_400_not_silent_missing() {
    // rev_parse_commit runs BEFORE read_blob so a typo'd ref surfaces as
    // a clean 4xx instead of being swallowed by `read_blob`'s
    // `BlobRead::Missing` mapping (which would render an empty diff).
    let (port, dir) = spawn_server("diff-bad-ref").await;
    init_repo(&dir);
    let (status, body) = get_git(
        port,
        "diff",
        &[
            ("path", "a.txt"),
            ("base_ref", "HEAD"),
            ("head_ref", "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"),
        ],
    )
    .await;
    assert_eq!(status, 400);
    assert!(body["error"].is_string());
}

#[tokio::test]
async fn diff_binary_returns_empty_sides() {
    let (port, dir) = spawn_server("diff-binary").await;
    init_repo(&dir);
    // Committed PNG, then replaced with different binary content.
    std::fs::write(dir.join("img.png"), b"\x89PNG\r\n\x1a\n\x00BINARY0").unwrap();
    git_in(&dir, &["add", "img.png"]);
    git_in(&dir, &["commit", "-m", "add png"]);
    std::fs::write(dir.join("img.png"), b"\x89PNG\r\n\x1a\n\x00BINARY1").unwrap();

    let (status, body) = get_git(port, "diff", &[("path", "img.png")]).await;
    assert_eq!(status, 200);
    assert_eq!(body["kind"], "binary");
    assert_eq!(
        body["original"], "",
        "binary content never leaked (decision 4)"
    );
    assert_eq!(body["modified"], "");
}

#[tokio::test]
async fn diff_rename_uses_old_path_for_head() {
    let (port, dir) = spawn_server("diff-rename").await;
    init_repo(&dir);
    std::fs::write(dir.join("a.txt"), "renamed body\n").unwrap();
    git_in(&dir, &["mv", "a.txt", "b.txt"]);

    // `git show HEAD:b.txt` fails (HEAD still holds a.txt) — the service
    // must fall back to the rename's old path for the original side.
    let (status, body) = get_git(port, "diff", &[("path", "b.txt")]).await;
    assert_eq!(status, 200);
    assert_eq!(body["kind"], "modified");
    assert_eq!(
        body["original"], "hello\n",
        "HEAD content read from old path a.txt"
    );
    assert_eq!(
        body["modified"], "renamed body\n",
        "worktree content from new path b.txt"
    );
}

#[tokio::test]
async fn diff_path_traversal_rejected() {
    let (port, dir) = spawn_server("diff-traversal").await;
    init_repo(&dir);
    let (status, _) = get_git(port, "diff", &[("path", "../evil.txt")]).await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn diff_missing_path_is_400() {
    let (port, dir) = spawn_server("diff-nopath").await;
    init_repo(&dir);
    let (status, _) = get_git(port, "diff", &[]).await;
    assert_eq!(status, 400, "path is required for /git/diff");
}

#[tokio::test]
async fn diff_non_repo_is_400() {
    let (port, _dir) = spawn_server("diff-nonrepo").await;
    // No repo in temp_dir.
    let (status, body) = get_git(port, "diff", &[("path", "a.txt")]).await;
    assert_eq!(status, 400);
    assert!(body["error"].is_string());
}

#[tokio::test]
async fn diff_utf8_path_roundtrips() {
    let (port, dir) = spawn_server("diff-utf8").await;
    init_repo(&dir);
    std::fs::write(dir.join("中文.txt"), "内容\n").unwrap();

    let (status, body) = get_git(port, "diff", &[("path", "中文.txt")]).await;
    assert_eq!(status, 200);
    assert_eq!(body["kind"], "untracked");
    assert_eq!(body["modified"], "内容\n");
}

// ── /git/log ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn log_repository_wide_returns_all_commits() {
    let (port, dir) = spawn_server("log-all").await;
    init_repo(&dir);
    std::fs::write(dir.join("b.txt"), "b\n").unwrap();
    git_in(&dir, &["add", "b.txt"]);
    git_in(&dir, &["commit", "-m", "add b"]);

    let (status, body) = get_git(port, "log", &[]).await;
    assert_eq!(status, 200);
    let commits = body["commits"].as_array().unwrap();
    assert_eq!(commits.len(), 2);
    // Newest first.
    assert_eq!(commits[0]["subject"], "add b");
    assert_eq!(commits[1]["subject"], "init");
    // Wire-contract fields for the Desktop log tab.
    let c = &commits[0];
    for field in ["hash", "shortHash", "author", "date", "subject"] {
        assert!(c[field].is_string(), "commit must carry {field}");
    }
    assert_eq!(c["author"], "e2e");
    assert!(!c["hash"].as_str().unwrap().is_empty());
}

#[tokio::test]
async fn log_path_scoped_filters_commits() {
    let (port, dir) = spawn_server("log-path").await;
    init_repo(&dir);
    std::fs::write(dir.join("b.txt"), "b\n").unwrap();
    git_in(&dir, &["add", "b.txt"]);
    git_in(&dir, &["commit", "-m", "add b"]);
    std::fs::write(dir.join("a.txt"), "changed a\n").unwrap();
    git_in(&dir, &["add", "a.txt"]);
    git_in(&dir, &["commit", "-m", "mod a"]);

    let (status, body) = get_git(port, "log", &[("path", "b.txt")]).await;
    assert_eq!(status, 200);
    let commits = body["commits"].as_array().unwrap();
    assert_eq!(commits.len(), 1, "only the commit touching b.txt");
    assert_eq!(commits[0]["subject"], "add b");
}

#[tokio::test]
async fn log_limit_respected() {
    let (port, dir) = spawn_server("log-limit").await;
    init_repo(&dir);
    std::fs::write(dir.join("b.txt"), "b\n").unwrap();
    git_in(&dir, &["add", "b.txt"]);
    git_in(&dir, &["commit", "-m", "add b"]);

    let (status, body) = get_git(port, "log", &[("limit", "1")]).await;
    assert_eq!(status, 200);
    let commits = body["commits"].as_array().unwrap();
    assert_eq!(commits.len(), 1);
    assert_eq!(commits[0]["subject"], "add b", "limit keeps newest");
    // Pagination metadata must accompany every response so the client
    // can decide whether to render search + "Page X of Y" chrome.
    let p = &body["pagination"];
    assert_eq!(p["pageSize"], 1);
    assert_eq!(p["totalCount"], 2);
    assert_eq!(p["totalPages"], 2);
    assert_eq!(p["currentPage"], 1);
}

#[tokio::test]
async fn log_skip_returns_subsequent_page_with_pagination() {
    // Build a 5-commit history, fetch with limit=2 + skip=2 → page 2
    // of 3, contents must be the 3rd + 4th-newest commits. This is
    // what the diff banner / Git status bar history dropdown uses to
    // navigate older commits.
    let (port, dir) = spawn_server("log-skip").await;
    init_repo(&dir);
    for i in 0..4 {
        std::fs::write(dir.join(format!("file{i}.txt")), format!("v{i}\n")).unwrap();
        git_in(&dir, &["add", &format!("file{i}.txt")]);
        git_in(&dir, &["commit", "-m", &format!("c{i}")]);
    }
    // 5 commits total: init + c0..c3 (newest first → c3, c2, c1, c0, init)
    let (status, body) = get_git(port, "log", &[("limit", "2"), ("skip", "2")]).await;
    assert_eq!(status, 200);
    let commits = body["commits"].as_array().unwrap();
    assert_eq!(commits.len(), 2, "page size = 2");
    // skip=2 → third + fourth entries from the top: c1, c0.
    assert_eq!(commits[0]["subject"], "c1");
    assert_eq!(commits[1]["subject"], "c0");
    let p = &body["pagination"];
    assert_eq!(p["currentPage"], 2);
    assert_eq!(p["totalPages"], 3, "5 / 2 → 3 pages");
    assert_eq!(p["pageSize"], 2);
    assert_eq!(p["totalCount"], 5);
}

#[tokio::test]
async fn log_pagination_total_count_matches_path_filter() {
    // `totalCount` must reflect the SAME path scope the page query used,
    // so the dropdown's "Page X of Y" reflects file-history length, not
    // repo-history length. Without this, a file with 3 historical edits
    // in a repo of 5000 commits would render "Page 1 of 100" and
    // collapse the search/pagination chrome incorrectly.
    let (port, dir) = spawn_server("log-pagination-path").await;
    init_repo(&dir);
    for i in 0..3 {
        std::fs::write(dir.join("hot.txt"), format!("v{i}\n")).unwrap();
        git_in(&dir, &["add", "hot.txt"]);
        git_in(&dir, &["commit", "-m", &format!("hot {i}")]);
    }
    // Add 10 commits that don't touch hot.txt.
    for i in 0..10 {
        std::fs::write(dir.join(format!("cold{i}.txt")), "x\n").unwrap();
        git_in(&dir, &["add", &format!("cold{i}.txt")]);
        git_in(&dir, &["commit", "-m", &format!("cold {i}")]);
    }
    let (status, body) = get_git(port, "log", &[("path", "hot.txt"), ("limit", "50")]).await;
    assert_eq!(status, 200);
    assert_eq!(
        body["pagination"]["totalCount"], 3,
        "file-history commits only, not repo-wide"
    );
    assert_eq!(body["pagination"]["totalPages"], 1);
}

#[tokio::test]
async fn log_non_repo_is_400() {
    let (port, _dir) = spawn_server("log-nonrepo").await;
    let (status, body) = get_git(port, "log", &[]).await;
    assert_eq!(status, 400);
    assert!(body["error"].is_string());
}

#[tokio::test]
async fn log_path_scoped_first_commit_preserved_with_uncommitted_change() {
    // Regression for the "file-history picker loses its first commit"
    // bug. Scenario: a file has 3 historical commits (c2 = newest,
    // touches the file alone; c1 touches file + sibling; c0 = initial).
    // The user is on a Working-Tree side diff (HEAD == c2, file has
    // uncommitted modification), so the diff banner's base picker
    // currently shows c2's hash.
    //
    // When the user re-opens the picker, `GET /git/log?path=<file>` must
    // still return c2 as `commits[0]` — not silently drop it. The
    // previous behaviour (loss) was traced to a `current_page` math
    // bug in `log()`: `(skip + 1)` was used to derive `currentPage`,
    // which rounded page=1 up to currentPage=2 and made the client
    // treat the first entry as already past the window.
    let (port, dir) = spawn_server("log-first-commit").await;
    init_repo(&dir);
    // c0: init (already done by init_repo → a.txt)
    // c1: a + sibling
    std::fs::write(dir.join("a.txt"), "a-v1\n").unwrap();
    std::fs::write(dir.join("b.txt"), "b-v1\n").unwrap();
    git_in(&dir, &["add", "a.txt", "b.txt"]);
    git_in(&dir, &["commit", "-m", "c1: a and b"]);
    // c2: a only
    std::fs::write(dir.join("a.txt"), "a-v2\n").unwrap();
    git_in(&dir, &["add", "a.txt"]);
    git_in(&dir, &["commit", "-m", "c2: only a"]);
    // Working-Tree state: a.txt is dirty. Mirrors the user's setup
    // (right side = "Working Tree" in the diff banner).
    std::fs::write(dir.join("a.txt"), "a-uncommitted\n").unwrap();

    // (1) Path-scoped log for the file the user is editing.
    let (status, body) = get_git(port, "log", &[("path", "a.txt"), ("limit", "50")]).await;
    assert_eq!(status, 200);
    let commits = body["commits"].as_array().unwrap();
    assert_eq!(
        commits.len(),
        3,
        "file-history must include all 3 commits even with uncommitted change"
    );
    assert_eq!(
        commits[0]["subject"], "c2: only a",
        "newest file-history commit must be commits[0]"
    );
    assert_eq!(commits[1]["subject"], "c1: a and b");
    assert_eq!(commits[2]["subject"], "init");
    let p = &body["pagination"];
    assert_eq!(p["totalCount"], 3);
    assert_eq!(p["totalPages"], 1);
    assert_eq!(p["currentPage"], 1);

    // (2) Sanity: repo-wide log still returns 3 commits and the
    // ordering is identical (newest-first). If a future refactor of
    // path filtering accidentally also drops the first commit when
    // no path is given, this catches it.
    let (status, body) = get_git(port, "log", &[("limit", "50")]).await;
    assert_eq!(status, 200);
    let commits = body["commits"].as_array().unwrap();
    assert_eq!(commits.len(), 3);
    assert_eq!(commits[0]["subject"], "c2: only a");
    assert_eq!(commits[2]["subject"], "init");
}

#[tokio::test]
async fn diff_worktree_base_matches_file_history_head_not_repo_head() {
    // Bug: opening the diff for a dirty file whose LAST commit is not
    // HEAD (HEAD only touched a sibling). The worktree diff hard-coded
    // `base_rev = HEAD`, so the banner's left label showed a commit that
    // is absent from the banner's file-scoped `CommitPicker`
    // (`git log -- <path>`): user sees "the first commit id vanished",
    // and searching for it in the menu finds nothing (it's not in the
    // server payload at all). The repo-wide history in the status bar
    // *does* contain HEAD — hence "the status bar menu is right".
    //
    // Contract: base label MUST equal the picker's row 1, i.e. the
    // newest commit touching `path` reachable from the base ref.
    let (port, dir) = spawn_server("diff-wt-base-file-head").await;
    init_repo(&dir);
    // c1: modify a.txt (this is the newest commit touching a.txt).
    std::fs::write(dir.join("a.txt"), "a-v1\n").unwrap();
    git_in(&dir, &["add", "a.txt"]);
    git_in(&dir, &["commit", "-m", "c1: a only"]);
    let c1 = git_in(&dir, &["rev-parse", "HEAD"]).trim().to_string();
    // c2: HEAD moves forward but only touches b.txt.
    std::fs::write(dir.join("b.txt"), "b\n").unwrap();
    git_in(&dir, &["add", "b.txt"]);
    git_in(&dir, &["commit", "-m", "c2: b only"]);
    let c2 = git_in(&dir, &["rev-parse", "HEAD"]).trim().to_string();
    assert_ne!(c1, c2);
    // Dirty a.txt (working-tree modification) → worktree diff.
    std::fs::write(dir.join("a.txt"), "a-dirty\n").unwrap();

    // (1) Picker source: file-scoped log. Row 1 must be c1 (not c2).
    let (status, log) = get_git(port, "log", &[("path", "a.txt"), ("limit", "50")]).await;
    assert_eq!(status, 200);
    assert_eq!(
        log["commits"][0]["hash"], c1,
        "file-history newest commit is c1, not repo HEAD (c2)"
    );

    // (2) Banner source: worktree diff. baseRev must match the picker row 1.
    let (status, diff) = get_git(
        port,
        "diff",
        &[("path", "a.txt"), ("base_ref", "HEAD"), ("head_ref", "")],
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(diff["kind"], "modified");
    assert_eq!(
        diff["baseRev"], c1,
        "worktree base label must be the file-history head (c1), so it is visible in the picker"
    );
    // Content is unchanged by the label fix: c1:a.txt == HEAD:a.txt here.
    assert_eq!(diff["original"], "a-v1\n");
}
