//! Gateway-lifetime containment for the services the Gateway spawns.
//!
//! Every child the Gateway starts (the hosted services and the installed
//! generic apps) is owned by the Gateway process: when it dies — cleanly, by
//! `taskkill`, or by a crash — the children must die with it. Without that a
//! restart leaves a second copy of every app alive on its old port, and on
//! Windows the orphan keeps a lock on `versions/<v>/bin/<app>.exe`, which
//! blocks the next install of that version and the next build into the same
//! directory.
//!
//! - **Windows**: one unnamed Job Object per process, created with
//!   `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` and never closed, so the kernel
//!   terminates every member when the Gateway's handle table goes away. This
//!   needs no cooperation from the Gateway's own shutdown path, which is what
//!   makes it cover the force-kill and crash cases too.
//! - **Unix**: no-op. The Gateway's SIGINT handler kills its children before
//!   exiting, and a `SIGKILL`ed Gateway is reclaimed by
//!   [`crate::process::lifetime`]'s caller recording the child's PID: the next
//!   start reaps a live PID from the last run (`app::supervisor`). A
//!   `PR_SET_PDEATHSIG` would be the direct equivalent, but it fires when the
//!   *thread* that spawned dies, not the process, so it silently misses the
//!   common case; the PID record is the honest mechanism.
//!
//! Deliberately separate from [`super::tree`]: that job is per command and
//! must NOT kill detached grandchildren on drop, which is the opposite
//! lifetime rule.

/// Put `pid` under the Gateway's lifetime: it is killed by the OS when the
/// Gateway process dies, however it dies. Best-effort — a failure is logged
/// and leaves the explicit shutdown paths as the only guarantee.
#[cfg(windows)]
pub fn contain_in_lifetime_job(pid: u32) {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    if pid == 0 {
        return;
    }
    // The handle is intentionally never closed: closing the last handle is
    // exactly what triggers the kill, and that must happen only when this
    // process ends.
    static JOB: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let job = *JOB.get_or_init(|| {
        // SAFETY: null attributes/name, all handles are checked below.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                tracing::warn!(
                    error = GetLastError(),
                    "lifetime job unavailable (CreateJobObjectW failed)"
                );
                return 0;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
            {
                tracing::warn!(
                    error = GetLastError(),
                    "lifetime job unavailable (SetInformationJobObject failed); \
                     children will only die on the Gateway's clean shutdown"
                );
                CloseHandle(job);
                return 0;
            }
            job as usize
        }
    });
    if job == 0 {
        return;
    }
    // SAFETY: `job` came from `CreateJobObjectW` and is alive for the process
    // lifetime; `OpenProcess`'s handle is closed on every path below.
    unsafe {
        let proc = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
        if proc.is_null() {
            // Already exited — nothing left to contain.
            return;
        }
        if AssignProcessToJobObject(job as _, proc) == 0 {
            // Typically ERROR_ACCESS_DENIED: this process is itself inside a
            // job that forbids nesting (some CI supervisors).
            tracing::warn!(
                pid,
                error = GetLastError(),
                "lifetime job assignment failed; this child can outlive the Gateway"
            );
        }
        CloseHandle(proc);
    }
}

/// Unix has no kill-on-parent-exit primitive that survives the common cases;
/// see the module docs. The Gateway's SIGINT handler and the next start's PID
/// reap are the guarantees there.
#[cfg(not(windows))]
pub fn contain_in_lifetime_job(_pid: u32) {}