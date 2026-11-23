//! Process kill/health-check utilities for agent processes.
//!
//! ADR-055 Phase 2b.3: `spawn_agent_process` / `AgentChild` were deleted —
//! Runtime processes are now spawned by the node (`acowork-node`). What
//! remains are the process-signal / liveness helpers still used by the
//! Gateway's own embedded services (embed, LSP relay) and their
//! supervisors.

use crate::error::GatewayError;
use std::process::Stdio;

/// Kill a process by PID
///
/// On Unix: sends SIGTERM via the `kill` command
/// On Windows: uses `taskkill /F /T /PID` to forcefully terminate the process tree
pub async fn kill_agent_process(pid: u32) -> Result<(), GatewayError> {
    if cfg!(unix) {
        let output = tokio::process::Command::new("kill")
            .arg(pid.to_string())
            .output()
            .await
            .map_err(|e| {
                GatewayError::Lifecycle(format!("Failed to execute kill for PID {}: {}", pid, e))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(GatewayError::Lifecycle(format!(
                "kill command failed for PID {}: {}",
                pid,
                stderr.trim()
            )));
        }
    } else {
        let output = tokio::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output()
            .await
            .map_err(|e| {
                GatewayError::Lifecycle(format!(
                    "Failed to execute taskkill for PID {}: {}",
                    pid, e
                ))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(GatewayError::Lifecycle(format!(
                "taskkill command failed for PID {}: {}",
                pid,
                stderr.trim()
            )));
        }
    }

    tracing::info!("Killed process: PID {}", pid);
    Ok(())
}

/// Check if a process with the given PID is still running (async version).
///
/// On Linux: checks if `/proc/{pid}` exists
/// On Windows: uses `tasklist` to check for the process
/// On macOS: uses `ps -p {pid}` (no /proc filesystem)
pub async fn check_health(pid: u32) -> bool {
    if cfg!(target_os = "linux") {
        // Linux: check /proc/{pid}
        tokio::fs::metadata(format!("/proc/{}", pid)).await.is_ok()
    } else if cfg!(windows) {
        // Windows: use tasklist
        match tokio::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {}", pid), "/NH"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .await
        {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                stdout.contains(&pid.to_string())
            }
            Err(_) => false,
        }
    } else {
        // macOS / other Unix: use ps -p
        match tokio::process::Command::new("ps")
            .args(["-p", &pid.to_string()])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .await
        {
            Ok(output) => output.status.success(),
            Err(_) => false,
        }
    }
}

/// Synchronous process liveness check (no await, safe inside locks).
///
/// On Linux: checks if `/proc/{pid}` exists (instant).
/// On other platforms: returns `true` (assumes alive — self-corrects on next call).
///
/// Use this instead of `check_health` when you need to call inside a
/// write-lock scope (e.g. to clear stale process state on death).
#[cfg(target_os = "linux")]
pub fn is_process_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{}", pid)).exists()
}

#[cfg(not(target_os = "linux"))]
pub fn is_process_alive(_pid: u32) -> bool {
    true // fallback: assume alive if we have a PID record
}

/// The executable's file name for `pid`, or `None` when no such process runs.
///
/// A recorded PID is only safe to act on once you know it still names the same
/// program: after a hard kill the record outlives the process, and Windows
/// recycles PIDs, so the number can come back as something unrelated.
pub async fn process_image_name(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    if cfg!(windows) {
        let out = tokio::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {}", pid), "/NH", "/FO", "CSV"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .await
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let line = text.trim();
        // CSV output always starts with a quoted field; anything else is
        // tasklist's "no tasks match" notice.
        if !line.starts_with('"') {
            return None;
        }
        let name = line.trim_start_matches('"').split('"').next()?.to_string();
        return (!name.is_empty()).then_some(name);
    }
    if cfg!(target_os = "linux") {
        let name = tokio::fs::read_to_string(format!("/proc/{}/comm", pid))
            .await
            .ok()?;
        return Some(name.trim().to_string());
    }
    let out = tokio::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .await
        .ok()?;
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!name.is_empty()).then_some(name)
}

/// Path of a sibling binary next to the running executable.
///
/// Every supervised standalone service is built into the same `target/`
/// directory, so `current_exe()`'s parent is the reliable lookup — a `PATH`
/// search would be wrong for the desktop bundle, which ships them side by
/// side and never installs them globally. Falls back to the bare name so a
/// `PATH` lookup can still succeed (dev setups running `cargo run`).
pub fn sibling_binary(name: &str) -> std::path::PathBuf {
    let file_name = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.join(&file_name)))
        .unwrap_or_else(|| std::path::PathBuf::from(file_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_check_health_current_process() {
        // Current process should be alive
        let pid = std::process::id();
        assert!(check_health(pid).await);
    }

    #[tokio::test]
    async fn test_check_health_nonexistent_pid() {
        // A very large PID is unlikely to exist
        assert!(!check_health(999999999).await);
    }

    /// The stale-PID reap is only safe because this names the image: a record
    /// that no longer points at our app must not be killed.
    #[tokio::test]
    async fn process_image_name_names_the_calling_binary() {
        let name = process_image_name(std::process::id())
            .await
            .expect("own process must have an image name");
        let ours = std::env::current_exe().expect("current exe");
        let ours = ours.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.eq_ignore_ascii_case(&ours), "{name} != {ours}");
        assert!(process_image_name(999999999).await.is_none());
    }

    #[tokio::test]
    async fn test_kill_nonexistent_pid() {
        // Killing a non-existent PID should fail
        let result = kill_agent_process(999999999).await;
        assert!(result.is_err());
    }
}
