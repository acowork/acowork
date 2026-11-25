//! Async app-install jobs (ADR-116).
//!
//! A `.app` package is 17–77 MB: the Gateway downloads it, verifies the
//! sha256, unpacks it and starts the process. Doing all of that inside the
//! request meant the Desktop had nothing to show for a minute or more, a
//! second click re-ran the whole download, and the client's view-local
//! progress vanished the moment the user switched panels mid-install.
//!
//! So the HTTP call only ACCEPTS the work: it registers a job, spawns it and
//! answers `202 {job_id}`. The job owns the lifecycle and the byte progress;
//! callers observe it with `GET /api/apps/install-jobs[/{job_id}]`.
//!
//! Server-owned state is the point: "switch away and come back" shows the
//! same install still running instead of a fresh Install button, and a
//! duplicate submit for the same card is answered with the running job id
//! instead of a second download.
//!
//! In-memory like the operation store (ADR-059 §6.2): a Gateway restart
//! forgets running jobs, and the Desktop simply sees no job and offers
//! Install again. Finished jobs are kept [`FINISHED_TTL_SECS`] so a client
//! that polls just after completion still reads the outcome.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::Serialize;

/// How long a terminal job stays readable, in seconds. Long enough for a
/// polling client to observe the outcome, short enough that a long-lived
/// Gateway does not accumulate one record per install forever.
const FINISHED_TTL_SECS: u64 = 300;

/// Shared handle to the install-job registry.
pub type SharedInstallJobs = Arc<InstallJobs>;

/// Lifecycle phase of one install job.
///
/// `Download` is the only phase the Gateway can put a percentage on
/// (`received_bytes` / `total_bytes`); the others are steps whose duration
/// depends on the package and the disk, so the client shows them as
/// indeterminate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallPhase {
    Download,
    Verify,
    Install,
    Start,
    Done,
    Failed,
}

impl InstallPhase {
    /// Whether the job is over (no further transitions).
    pub fn is_terminal(self) -> bool {
        matches!(self, InstallPhase::Done | InstallPhase::Failed)
    }
}

/// One install job, as served to the Desktop.
#[derive(Debug, Clone, Serialize)]
pub struct InstallJob {
    pub job_id: String,
    /// The card the Desktop asked to install (gallery id, or the local-upload
    /// sentinel). A DISPLAY key only: the installed identity always comes from
    /// the package manifest, never from the client.
    pub hint_id: Option<String>,
    pub phase: InstallPhase,
    pub received_bytes: u64,
    /// 0 when the server did not send `Content-Length`.
    pub total_bytes: u64,
    pub started_at: u64,
    pub finished_at: Option<u64>,
    /// Known once the manifest has been read.
    pub app_id: Option<String>,
    pub installed_version: Option<String>,
    /// Stable failure code (`package_malformed`, `unsigned_package`, …).
    pub error_code: Option<String>,
    /// Human detail from the failing step, kept verbatim so the Desktop can
    /// show *why* instead of guessing from the code alone.
    pub error: Option<String>,
}

impl InstallJob {
    fn new(job_id: String, hint_id: Option<String>) -> Self {
        Self {
            job_id,
            hint_id,
            phase: InstallPhase::Download,
            received_bytes: 0,
            total_bytes: 0,
            started_at: now_secs(),
            finished_at: None,
            app_id: None,
            installed_version: None,
            error_code: None,
            error: None,
        }
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// In-memory `job_id → job` map.
pub struct InstallJobs {
    inner: Mutex<HashMap<String, InstallJob>>,
}

impl InstallJobs {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Create a shared registry.
    pub fn new_shared() -> SharedInstallJobs {
        Arc::new(Self::new())
    }

    /// Register a fresh job and return its id. Sweeps terminal jobs older
    /// than [`FINISHED_TTL_SECS`] on the way in.
    pub fn create(&self, hint_id: Option<String>) -> String {
        let job_id = uuid::Uuid::new_v4().to_string();
        let mut inner = self.inner.lock();
        let cutoff = now_secs().saturating_sub(FINISHED_TTL_SECS);
        inner.retain(|_, j| match j.finished_at {
            Some(at) => at >= cutoff,
            None => true,
        });
        inner.insert(job_id.clone(), InstallJob::new(job_id.clone(), hint_id));
        job_id
    }

    /// The still-running job registered for `hint_id`, if any — the
    /// single-flight answer to a duplicate submit.
    pub fn running_for_hint(&self, hint_id: &str) -> Option<InstallJob> {
        self.inner
            .lock()
            .values()
            .find(|j| j.hint_id.as_deref() == Some(hint_id) && !j.phase.is_terminal())
            .cloned()
    }

    /// Apply `edit` to a job. No-op on an unknown or terminal job: a late
    /// progress tick must never resurrect a finished install.
    pub fn update(&self, job_id: &str, edit: impl FnOnce(&mut InstallJob)) {
        let mut inner = self.inner.lock();
        if let Some(job) = inner.get_mut(job_id) {
            if job.phase.is_terminal() {
                return;
            }
            edit(job);
        }
    }

    /// Mark a job done.
    pub fn finish_ok(&self, job_id: &str, app_id: &str, version: &str) {
        self.update(job_id, |j| {
            j.phase = InstallPhase::Done;
            j.app_id = Some(app_id.to_string());
            j.installed_version = Some(version.to_string());
            j.finished_at = Some(now_secs());
        });
    }

    /// Mark a job failed, keeping the step's own code and detail.
    pub fn finish_err(&self, job_id: &str, code: &str, message: &str) {
        self.update(job_id, |j| {
            j.phase = InstallPhase::Failed;
            j.error_code = Some(code.to_string());
            j.error = Some(message.to_string());
            j.finished_at = Some(now_secs());
        });
    }

    /// One job by id.
    pub fn get(&self, job_id: &str) -> Option<InstallJob> {
        self.inner.lock().get(job_id).cloned()
    }

    /// Every known job, running first and newest first inside each group —
    /// the Desktop paints the list in this order.
    pub fn list(&self) -> Vec<InstallJob> {
        let mut jobs: Vec<InstallJob> = self.inner.lock().values().cloned().collect();
        jobs.sort_by(|a, b| {
            a.phase
                .is_terminal()
                .cmp(&b.phase.is_terminal())
                .then(b.started_at.cmp(&a.started_at))
        });
        jobs
    }
}

impl Default for InstallJobs {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_job_is_single_flight_per_hint() {
        let jobs = InstallJobs::new();
        let first = jobs.create(Some("gridcraft".into()));
        assert_eq!(
            jobs.running_for_hint("gridcraft").map(|j| j.job_id),
            Some(first.clone()),
            "a second submit for the same card must reuse the running job"
        );
        assert!(jobs.running_for_hint("vectorcraft").is_none());

        jobs.finish_ok(&first, "gridcraft", "0.19.4");
        assert!(
            jobs.running_for_hint("gridcraft").is_none(),
            "a finished job must not block the next install"
        );
    }

    #[test]
    fn progress_accumulates_and_terminal_state_is_sticky() {
        let jobs = InstallJobs::new();
        let id = jobs.create(Some("gridcraft".into()));
        jobs.update(&id, |j| {
            j.total_bytes = 1_000;
            j.received_bytes = 400;
        });
        assert_eq!(jobs.get(&id).unwrap().received_bytes, 400);

        jobs.finish_err(&id, "package_malformed", "download body: connection reset");
        // A late tick from the spawned task must not reopen a failed job.
        jobs.update(&id, |j| {
            j.phase = InstallPhase::Download;
            j.received_bytes = 999;
        });
        let job = jobs.get(&id).unwrap();
        assert_eq!(job.phase, InstallPhase::Failed);
        assert_eq!(job.received_bytes, 400);
        assert_eq!(job.error_code.as_deref(), Some("package_malformed"));
        assert!(job.finished_at.is_some());
    }
}