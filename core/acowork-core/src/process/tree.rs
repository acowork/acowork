//! Process-tree containment for spawned children.
//!
//! Killing only the **direct child** is not enough: its descendants
//! survive, keep inherited stdio pipe write handles open (which blocks
//! the tokio runtime's pipe teardown until they exit on their own — the
//! historical 121 s test-hang in this repo), and leak as orphans in
//! production (e.g. a killed `npm install` leaving `node` children).
//!
//! This module provides the containment primitive used by the
//! idle-timeout kill path in [`super`]:
//!
//! - **Windows**: an unnamed Job Object. The child is assigned right
//!   after spawn; every process the tree creates afterwards joins the
//!   job automatically (job membership is inherited at creation), so a
//!   timeout kill via `TerminateJobObject` reaches the whole tree. The
//!   window between `spawn()` returning and the assignment is the only
//!   escape route — microseconds, and the caller's direct `child.kill()`
//!   still covers the leader itself.
//! - **Unix**: the child is spawned as a process-group leader
//!   ([`TreeGuard::configure`] must run before spawn); `killpg` then
//!   signals the entire group.
//!
//! Containment only kills on the explicit [`TreeGuard::kill_tree`]
//! call. Dropping the guard on the normal-completion path deliberately
//! does *not* kill (no `KILL_ON_JOB_CLOSE`, no group signal), so
//! detached processes spawned by a successfully completed command keep
//! running — preserving the historical semantics of
//! [`super::run_with_idle_timeout`].

use tokio::process::{Child, Command};

/// A spawned child with its process tree contained.
///
/// Constructed via [`TreeGuard::configure`] (before spawn) +
/// [`TreeGuard::attach`] (after spawn). See the module docs for the
/// per-platform containment mechanism.
pub(crate) struct TreeGuard {
    #[cfg(unix)]
    pid: u32,
    #[cfg(windows)]
    _job: Option<JobHandle>,
}

impl TreeGuard {
    /// Pre-spawn configuration: make the child its own process-group
    /// leader on Unix so [`Self::kill_tree`] can signal the whole group.
    /// Windows containment is established at [`Self::attach`] time (Job
    /// Object), so this is a no-op there.
    pub(crate) fn configure(cmd: &mut Command) {
        #[cfg(unix)]
        cmd.process_group(0);
        #[cfg(not(unix))]
        let _ = cmd;
    }

    /// Establish containment for an already-spawned child.
    ///
    /// Best-effort by design: if the OS refuses containment (the child
    /// already exited, or a parent Job Object forbids nesting), the
    /// guard degrades to the historical direct-child-only kill.
    pub(crate) fn attach(child: &Child) -> Self {
        let pid = child.id().unwrap_or(0);
        Self {
            #[cfg(unix)]
            pid,
            #[cfg(windows)]
            _job: if pid == 0 {
                None
            } else {
                JobHandle::assign_new_job(pid)
            },
        }
    }

    /// Kill the child **and every contained descendant**.
    ///
    /// Best-effort: errors are ignored because the caller follows up
    /// with a direct `child.kill()` that always covers the group leader
    /// / job member itself.
    pub(crate) fn kill_tree(&self) {
        #[cfg(unix)]
        if self.pid != 0 {
            // SAFETY: killpg on an unknown pgid is a defined no-op
            // (ESRCH). If the child was spawned without `configure` it
            // is not a group leader, so this cannot signal the parent's
            // own group — the caller's `child.kill()` still applies.
            unsafe {
                libc::killpg(self.pid as libc::pid_t, libc::SIGKILL);
            }
        }
        #[cfg(windows)]
        if let Some(job) = &self._job {
            job.terminate();
        }
    }
}

// ── Windows: Job Object containment ──────────────────────────────────────

/// Owns an unnamed Job Object handle. Closing it (Drop) releases the
/// containment without killing anything (`KILL_ON_JOB_CLOSE` is
/// intentionally not set — see the module docs).
#[cfg(windows)]
struct JobHandle(windows_sys::Win32::Foundation::HANDLE);

// A kernel HANDLE is process-global, not thread-affine: closing or
// terminating the job from any thread is safe.
#[cfg(windows)]
unsafe impl Send for JobHandle {}

#[cfg(windows)]
impl JobHandle {
    /// Create a fresh job and pull the child (plus its existing
    /// descendants) into it. `None` on any failure — the caller
    /// degrades to a direct-child kill.
    fn assign_new_job(pid: u32) -> Option<Self> {
        use windows_sys::Win32::Foundation::{CloseHandle, GetLastError};
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW,
        };
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
        };

        // SAFETY: every handle created below is closed on all paths;
        // the pointers are either null or valid handles we own.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                tracing::warn!(
                    pid,
                    error = GetLastError(),
                    "process-tree containment unavailable (CreateJobObjectW failed); \
                     timeout kill will only reach the direct child"
                );
                return None;
            }
            // PROCESS_SET_QUOTA | PROCESS_TERMINATE are the access
            // rights AssignProcessToJobObject requires.
            let proc = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
            if proc.is_null() {
                // Child already exited — nothing to contain.
                CloseHandle(job);
                return None;
            }
            let assigned = AssignProcessToJobObject(job, proc);
            CloseHandle(proc);
            if assigned == 0 {
                // Typically ERROR_ACCESS_DENIED: our own process runs
                // inside a job that disallows nesting (some CI
                // supervisors). Degrade to the single-process kill —
                // loudly, because the surviving grandchildren can then
                // stall pipe teardown exactly like the pre-fix bug.
                tracing::warn!(
                    pid,
                    error = GetLastError(),
                    "process-tree containment unavailable (AssignProcessToJobObject failed); \
                     timeout kill will only reach the direct child"
                );
                CloseHandle(job);
                return None;
            }
            Some(Self(job))
        }
    }

    /// Kill every process in the job (the whole tree).
    fn terminate(&self) {
        // SAFETY: self.0 is a live job handle owned by this guard.
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.0, 1);
        }
    }
}

#[cfg(windows)]
impl Drop for JobHandle {
    fn drop(&mut self) {
        // SAFETY: handle is owned by this guard and not used after drop.
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}
