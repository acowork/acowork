//! Spawning and reaping a Gateway — and everything it supervises — in a test.
//!
//! A Gateway under test is a *tree*: the Gateway process plus the user, doc,
//! pm, memory and (for some suites) runtime and node services it spawns. The
//! naive teardown — `child.kill()`, which is `SIGKILL` — stops the root and
//! nothing else. The supervised children survive, get reparented to init, and
//! keep the loopback ports they bound. That is not a tidy-room problem: those
//! services bind a fixed base port (`18082/18083/18084`) and auto-increment on
//! conflict, so every later test in the same run starts further up the range,
//! its restart backoff grows (3 s, then 25 s), and the Gateway answers
//! `503 user service not ready` for longer than any test's boot timeout.
//! Suites that passed alone then failed in a full run, in an order-dependent
//! way that looks exactly like a product defect.
//!
//! Two mechanisms, one polite and one guaranteed:
//!
//! - `group_leader` puts the child in a new process group at fork time, so the
//!   group id equals the child pid, and every service the Gateway spawns
//!   inherits that group.
//! - `terminate_tree` sends `SIGTERM` to the group — `tokio::signal::ctrl_c()`
//!   fires on SIGTERM on Unix, so the Gateway runs its real shutdown and reaps
//!   its children — and then, if anything is still alive, `SIGKILL`s the group.
//!   The sweep matters: a child that hangs during shutdown would otherwise
//!   outlive the test just as quietly as before.
//!
//! Windows has no process groups; `taskkill /T /F` walks the child list
//! instead, which is the same guarantee from a different API.

use std::process::{Child, Command};

/// How long the Gateway gets to shut its own tree down before it is taken away.
///
/// Graceful shutdown is the interesting path — it is what the desktop app and
/// the service manager rely on — but the test must not hang on it. Only the
/// Unix path waits on it: Windows has no signal to send first, `taskkill /T`
/// is the whole operation.
#[cfg(unix)]
const GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Make the child a process-group leader (Unix) so `terminate_tree` can reach
/// its whole subtree. No-op on Windows, where the tree is walked by `taskkill`.
pub(crate) fn group_leader(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // `0` = create a new group whose id is the child's pid. This is a
        // fork-time attribute of `posix_spawn`/`fork+exec`, applied in the
        // child before exec, so there is no window in which the Gateway is
        // already running in the old group.
        cmd.process_group(0);
    }
    #[cfg(not(unix))]
    let _ = cmd;
}

/// Stop a spawned Gateway and every process it started.
pub(crate) fn terminate_tree(child: &mut Child) {
    let pid = child.id();
    #[cfg(unix)]
    {
        let pgid = -(pid as libc::pid_t);
        // Safety: `kill` on a negative pid signals the process group we created
        // in `group_leader`; a group that no longer exists returns ESRCH, which
        // is ignored below. Nothing here dereferences memory.
        unsafe {
            libc::kill(pgid, libc::SIGTERM);
        }
        let deadline = std::time::Instant::now() + GRACE;
        while std::time::Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(25)),
                Err(_) => break,
            }
        }
        unsafe {
            // The sweep. Also reaches a Gateway that already exited but left
            // children behind, because the group outlives its leader.
            libc::kill(pgid, libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output();
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Builder-chain form of [`group_leader`], for the sites that construct the
/// command inline (`Command::new(x).args(..).grouped().spawn()`) rather than
/// holding a mutable variable.
pub(crate) trait Grouped {
    fn grouped(&mut self) -> &mut Self;
}

impl Grouped for Command {
    fn grouped(&mut self) -> &mut Self {
        group_leader(self);
        self
    }
}
