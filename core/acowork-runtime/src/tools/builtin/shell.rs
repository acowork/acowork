//! Shell command execution tool
//!
//! Platform-aware shell registration: on Windows, Git Bash and PowerShell are
//! registered as separate tools so the LLM can prefer bash and fall back to
//! PowerShell. On Linux/macOS, a single "shell" tool uses the system shell.
//!
//! Runtime safety: each invocation checks whether the shell binary still exists
//! (e.g. user uninstalled Git) and returns an LLM-actionable error pointing to
//! the fallback tool instead of a cryptic "command not found".

use acowork_core::tools::traits::{Tool, ToolResult, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::tools::output;

/// Guard that kills the child process on drop, ensuring cleanup on
/// timeout, abort, or interrupt — no orphaned shell processes.
///
/// # Usage
/// - **Normal path**: call `guard.take()` → marks completed → takes child
///   → `wait_with_output()` → `drop(guard)` is a no-op.
/// - **Abort path**: `spawn_blocking` future is cancelled → `guard` drops
///   with `completed=false` → `child.kill()` + `child.wait()`.
struct ProcessGuard {
    child: Mutex<Option<std::process::Child>>,
    /// Set to true when the child was explicitly taken for normal completion.
    /// Prevents Drop from kill+wating an already-finished child.
    completed: AtomicBool,
}

impl ProcessGuard {
    fn new(child: std::process::Child) -> Self {
        Self {
            child: Mutex::new(Some(child)),
            completed: AtomicBool::new(false),
        }
    }

    /// Take ownership of the child for normal `wait_with_output`.
    /// Marks the guard as completed so that Drop becomes a no-op.
    fn take(&self) -> Option<std::process::Child> {
        self.completed.store(true, Ordering::Release);
        self.child.lock().unwrap().take()
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        // If the child was already taken via take() (normal path), skip.
        if self.completed.load(Ordering::Acquire) {
            return;
        }
        // Abort path: the spawn_blocking future was cancelled before take()
        // was called. Kill the child to prevent orphaned processes.
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// A concrete shell executor registered as a tool.
///
/// Different instances represent different shells (bash, powershell, etc.)
/// so the LLM sees distinct tools with distinct descriptions and can make
/// informed choices about which to use.
pub struct ShellTool {
    /// Tool name exposed to LLM (e.g. "bash", "powershell", "shell")
    tool_name: String,
    /// Human-readable shell identifier for error messages
    shell_name: String,
    /// Shell binary to invoke (e.g. "bash", "pwsh", "/bin/zsh")
    shell_binary: String,
    /// Full path resolved at registration time (used for existence check)
    shell_path: String,
    /// CLI flag for passing a command string (e.g. "-c", "-Command")
    shell_arg: String,
}

impl ShellTool {
    /// Create a shell tool with explicit binary path.
    ///
    /// `shell_path` is the fully-resolved path used for existence checks.
    /// `shell_binary` is what's passed to `std::process::Command::new()`.
    pub fn new(
        tool_name: &str,
        shell_name: &str,
        shell_binary: &str,
        shell_path: &str,
        shell_arg: &str,
    ) -> Self {
        Self {
            tool_name: tool_name.to_string(),
            shell_name: shell_name.to_string(),
            shell_binary: shell_binary.to_string(),
            shell_path: shell_path.to_string(),
            shell_arg: shell_arg.to_string(),
        }
    }

    /// Build the ToolSpec with a platform-appropriate description.
    ///
    /// Bash tools get Unix-style guidance; PowerShell tools get Windows-style
    /// guidance so the LLM produces syntactically correct commands. All three
    /// branches share the head+tail budget hint so the LLM sees the same
    /// output contract regardless of platform — this is the *only* signal
    /// it has about the 4 KB + 1 KB truncation behaviour.
    fn build_spec(&self) -> ToolSpec {
        // Shared budget hint: must mention "4 KB" + "1 KB" (test guard) and
        // both "grep" (for Unix/zsh/bash) and "Select-String" (for
        // PowerShell) so the test assertion holds on every platform.
        const HEAD_TAIL_HINT: &str = "IMPORTANT: Output shows the first 4 KB \
            and the last 1 KB — anything in between is replaced with an \
            '[... N bytes omitted from middle]' marker. For diagnostic commands \
            (compile errors, test failures, stack traces) the actionable error \
            is almost always at the tail and is preserved. To avoid the marker \
            entirely, pre-filter with 'grep', 'head', 'tail', or narrower \
            command flags. To re-query the omitted middle, use 'grep -n' on \
            Unix-like shells or 'Select-String' on PowerShell.";
        let description = match self.tool_name.as_str() {
            "bash" => format!(
                "Execute a command in Git Bash (Unix-style shell on Windows). \
                 {HEAD_TAIL_HINT} \
                 For absolute paths outside the workspace, prefer Windows format (e.g. 'C:/Users/...'). \
                 {fallback}",
                fallback = self.fallback_hint()
            ),
            "powershell" => format!(
                "Execute a command in {shell_name} ({shell_binary}). \
                 {HEAD_TAIL_HINT} \
                 Use this if 'bash' is unavailable or for Windows-specific tasks. \
                 {fallback}",
                shell_name = self.shell_name,
                shell_binary = self.shell_binary,
                fallback = self.fallback_hint()
            ),
            _ => format!(
                "Execute a command in {shell_name} ({shell_binary}). \
                 {HEAD_TAIL_HINT} \
                 Use with caution.",
                shell_name = self.shell_name,
                shell_binary = self.shell_binary
            ),
        };

        ToolSpec {
            name: self.tool_name.clone(),
            description,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute"
                    }
                },
                "required": ["command"]
            }),
        }
    }

    /// Hint about fallback tools when this shell is unavailable at runtime.
    fn fallback_hint(&self) -> String {
        match self.tool_name.as_str() {
            "bash" => "If this tool returns an error about 'bash' not found, \
                       use the 'powershell' tool instead — Git Bash may have \
                       been uninstalled or moved."
                .to_string(),
            "powershell" => "If this tool returns an error about 'powershell' not found, \
                             try the 'bash' tool if available."
                .to_string(),
            _ => String::new(),
        }
    }

    /// Check whether the shell binary still exists on disk.
    ///
    /// Covers the case where Git (bash.exe) or PowerShell was uninstalled
    /// after the agent process started.
    fn binary_exists(&self) -> bool {
        // Fast path: check the resolved path from registration time
        if Path::new(&self.shell_path).exists() {
            return true;
        }
        // Slow path: try executing a trivial command using the shell's own
        // argument convention. This is the same approach used by detect_shell()
        // and is more reliable than `where`/`which` on Windows where PowerShell
        // may be registered differently.
        let test_cmd = if cfg!(windows) { "echo ok" } else { "true" };
        std::process::Command::new(&self.shell_binary)
            .arg(&self.shell_arg)
            .arg(test_cmd)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

/// ADR-117 D4 layer 3: misuse mitigation for writes into the platform data
/// directory, NOT a security boundary. Plain string matching cannot stop
/// variable interpolation, a `cd` followed by a relative path, or an
/// interpreter one-liner (`python -c ...`). The threat model is an agent
/// accidentally writing `~/.acowork/...`, not a malicious attacker; the end
/// state is OS-level sandboxing (Windows Job Object / AppContainer, Linux
/// namespaces), which is out of scope here.
///
/// All three shell tool names (`bash`, `powershell`, `shell`) funnel through
/// this one `ShellTool::execute`, so a single check covers every platform.
fn protected_prefix_hit(command: &str) -> Option<String> {
    let normalized = command.replace('\\', "/").to_ascii_lowercase();
    let mut needles = vec![
        "~/.acowork".to_string(),
        "$home/.acowork".to_string(),
        "${home}/.acowork".to_string(),
        "$userprofile/.acowork".to_string(),
        "${userprofile}/.acowork".to_string(),
        "%userprofile%/.acowork".to_string(),
    ];
    if let Some(dir) = acowork_core::workspace::platform_data_dir() {
        needles.push(dir.to_string_lossy().replace('\\', "/").to_ascii_lowercase());
    }
    needles
        .into_iter()
        .find(|needle| normalized.contains(needle.as_str()))
}

#[async_trait]
impl Tool for ShellTool {
    fn spec(&self) -> ToolSpec {
        self.build_spec()
    }

    async fn execute(
        &self,
        params: Value,
        work_dir: Option<&str>,
    ) -> acowork_core::error::Result<ToolResult> {
        let command = params["command"].as_str().unwrap_or("");

        if command.is_empty() {
            return Ok(ToolResult {
                content_parts: None,
                ok: false,
                content: String::new(),
                error: Some("Missing 'command' parameter".to_string()),
                token_usage: None,
            });
        }

        if let Some(hit) = protected_prefix_hit(command) {
            tracing::warn!(
                tool = %self.tool_name,
                matched = %hit,
                "shell: blocked access to the platform data directory"
            );
            return Ok(ToolResult {
                content_parts: None,
                ok: false,
                content: String::new(),
                error: Some(format!(
                    "Command rejected: it targets the platform data directory (`{hit}`). \
                     Files under ~/.acowork/ are owned by the platform services — use the \
                     corresponding app's API (PM: pm_* tools / REST, memory, doc) instead of \
                     writing to them directly. This command was NOT executed: {command}"
                )),
                token_usage: None,
            });
        }

        // Runtime existence check — handles post-startup uninstall of Git/PowerShell
        if !self.binary_exists() {
            let hint = self.fallback_hint();
            let error_msg = if hint.is_empty() {
                format!(
                    "Shell binary '{}' ({}) not found. \
                     This may happen if the shell was uninstalled or moved. \
                     This command was NOT executed: {}",
                    self.shell_name, self.shell_path, command
                )
            } else {
                format!(
                    "Shell binary '{}' ({}) not found. {} \
                     This command was NOT executed: {}",
                    self.shell_name, self.shell_path, hint, command
                )
            };
            tracing::warn!(
                tool = %self.tool_name,
                shell_binary = %self.shell_binary,
                shell_path = %self.shell_path,
                "shell: binary not found at runtime"
            );
            return Ok(ToolResult {
                content_parts: None,
                ok: false,
                content: String::new(),
                error: Some(error_msg),
                token_usage: None,
            });
        }

        let effective_work_dir = work_dir.unwrap_or(".");
        tracing::debug!(
            command = %command,
            shell = %self.shell_binary,
            work_dir = %effective_work_dir,
            "shell: executing command"
        );

        // Spawn child process synchronously, then wait for completion in a
        // blocking thread. We use std::process::Command instead of
        // tokio::process::Command for cross-platform compatibility:
        //
        // - On Windows, tokio::process::Command uses async named-pipe I/O
        //   which is incompatible with MinGW/MSYS2 programs like Git Bash
        //   (bash.exe), causing the process to hang until timeout.
        // - On Linux/macOS, std::process::Command is equally reliable and
        //   avoids an unnecessary dependency on tokio's process layer.
        //
        // ProcessGuard wraps the Child with kill-on-drop, so that when the
        // outer tokio::time::timeout (or handle.abort()) cancels the
        // spawn_blocking future, the guard drops → kill → wait, preventing
        // orphaned shell processes.
        let shell_path = self.shell_path.clone();
        let shell_arg = self.shell_arg.clone();
        let command_owned = command.to_string();
        let work_dir_owned = effective_work_dir.to_string();
        let tool_name = self.tool_name.clone();

        let output =
            tokio::task::spawn_blocking(move || -> std::io::Result<std::process::Output> {
                // Use shell_path (fully-resolved path) instead of shell_binary
                // (just "bash") to avoid PATH resolution finding WSL bash
                // instead of Git Bash on Windows.
                let mut cmd = std::process::Command::new(&shell_path);
                cmd.arg(&shell_arg)
                    .arg(&command_owned)
                    .current_dir(&work_dir_owned)
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped());

                // Ensure MSYS2 environment is properly initialized for Git Bash
                // so drive letter mounts (/c/, /d/) and Unix paths work correctly.
                if tool_name == "bash" {
                    cmd.env("MSYSTEM", "MINGW64");
                    cmd.env("CHERE_INVOKING", "1");
                }

                let child = cmd.spawn()?;

                // ProcessGuard kills the child on drop — covers timeout,
                // abort, and interrupt scenarios uniformly.
                let guard = ProcessGuard::new(child);
                // Normal path: take() marks guard as completed, then we
                // wait for the child. Drop is now a no-op.
                let output = guard.take().unwrap().wait_with_output()?;
                Ok(output)
            })
            .await;

        match output {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();

                let content = if stderr.is_empty() {
                    stdout
                } else {
                    format!("STDOUT:\n{stdout}\nSTDERR:\n{stderr}")
                };

                // Guard against unbounded shell output (e.g. `cat` on a
                // multi-GB file or `dir /s` in a large tree) that would
                // exhaust the LLM token budget and crash the session task.
                //
                // Use head+tail truncation (DeepSeek-style aggressive):
                // keep the first 4 KB (command echo + setup) and the last
                // 1 KB (final result / error message). Diagnostic commands
                // (cargo / pytest / npm) almost always put the actionable
                // error at the tail, so this is dramatically more useful
                // than naive head-only truncation.
                let (content, _truncated) = output::truncate_head_tail_output(
                    &content,
                    output::MAX_SHELL_HEAD_BYTES,
                    output::MAX_SHELL_TAIL_BYTES,
                );

                Ok(ToolResult {
                    content_parts: None,
                    ok: output.status.success(),
                    content,
                    error: if output.status.success() {
                        None
                    } else {
                        Some(format!("Exit code: {}", output.status.code().unwrap_or(-1)))
                    },
                    token_usage: None,
                })
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    command = %command,
                    error = %e,
                    "shell: failed to execute command"
                );
                Ok(ToolResult {
                    content_parts: None,
                    ok: false,
                    content: String::new(),
                    error: Some(format!("Failed to execute command: {e}")),
                    token_usage: None,
                })
            }
            Err(join_err) => {
                tracing::warn!(
                    command = %command,
                    error = %join_err,
                    "shell: spawn_blocking task panicked"
                );
                Ok(ToolResult {
                    content_parts: None,
                    ok: false,
                    content: String::new(),
                    error: Some(format!("Internal error: shell task panicked: {join_err}")),
                    token_usage: None,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tool(name: &str, binary: &str) -> ShellTool {
        ShellTool::new(name, name, binary, binary, "-c")
    }

    #[test]
    fn protected_prefix_guard_rejects_platform_data_writes() {
        for cmd in [
            "echo x >> ~/.acowork/acowork-pm/projects/p-1/tasks/t-1/task.json",
            "echo x >> $HOME/.acowork/acowork-pm/task.json",
            "echo x >> ${HOME}/.acowork/acowork-node/x",
            r"echo x >> %USERPROFILE%\.acowork\acowork-user\x.json",
            "echo x >> $USERPROFILE/.acowork/x",
        ] {
            assert!(protected_prefix_hit(cmd).is_some(), "should reject: {cmd}");
        }
    }

    #[test]
    fn protected_prefix_guard_allows_unrelated_commands() {
        for cmd in [
            "echo hello",
            "ls -la ./workspace",
            "cat ./src/.acowork-notes.txt",
            "python -c 'print(1)'",
        ] {
            assert!(protected_prefix_hit(cmd).is_none(), "should allow: {cmd}");
        }
    }

    #[test]
    fn protected_prefix_guard_matches_expanded_home_when_available() {
        if let Some(dir) = acowork_core::workspace::platform_data_dir() {
            let cmd = format!("echo x >> {}/acowork-pm/task.json", dir.display());
            assert!(protected_prefix_hit(&cmd).is_some(), "should reject: {cmd}");
        }
    }

    #[tokio::test]
    async fn protected_prefix_guard_runs_before_the_binary_check() {
        // A non-existent binary proves the guard rejects before any spawn.
        let tool = ShellTool::new(
            "powershell",
            "PowerShell",
            "definitely_does_not_exist_shell_xyz",
            "/definitely/not/a/real/path/pwsh.exe",
            "-Command",
        );
        let result = tool
            .execute(
                serde_json::json!({"command": "echo x >> ~/.acowork/acowork-pm/task.json"}),
                None,
            )
            .await
            .unwrap();
        assert!(!result.ok);
        let err = result.error.unwrap();
        assert!(err.contains("platform data directory"));
        assert!(!err.contains("not found"), "guard must run before binary_exists");
    }

    #[tokio::test]
    async fn test_missing_command_parameter() {
        let tool = make_tool("bash", "bash");
        let result = tool.execute(serde_json::json!({}), None).await.unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("Missing 'command'"));
    }

    #[tokio::test]
    async fn test_empty_command_parameter() {
        let tool = make_tool("bash", "bash");
        let result = tool
            .execute(serde_json::json!({"command": ""}), None)
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("Missing 'command'"));
    }

    #[tokio::test]
    async fn test_runtime_binary_missing_error_includes_hint() {
        // Use a binary name that definitely does not exist
        let tool = ShellTool::new(
            "bash",
            "Git Bash",
            "definitely_does_not_exist_shell_xyz",
            "/definitely/not/a/real/path/bash.exe",
            "-c",
        );
        let result = tool
            .execute(serde_json::json!({"command": "echo hello"}), None)
            .await
            .unwrap();
        assert!(!result.ok);
        let err = result.error.unwrap();
        assert!(
            err.contains("not found"),
            "Error should mention binary not found: {}",
            err
        );
        assert!(
            err.contains("powershell"),
            "Error should hint at fallback tool: {}",
            err
        );
        assert!(
            err.contains("NOT executed"),
            "Error should state command was not executed: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_valid_command_executes() {
        // Use detected_shells() which provides the fully-resolved path
        let shells = crate::platform::detected_shells();
        let primary = shells
            .first()
            .expect("Should have at least one available shell");

        let tool = ShellTool::new(
            &primary.tool_name,
            &primary.display_name,
            &primary.binary,
            &primary.path,
            &primary.arg,
        );
        let result = tool
            .execute(
                serde_json::json!({"command": "echo hello_agentcowork"}),
                Some("."),
            )
            .await
            .unwrap();
        assert!(result.ok, "echo should succeed: {:?}", result.error);
        assert!(
            result.content.contains("hello_agentcowork"),
            "Output should contain echo text: {}",
            result.content
        );
    }

    // ── E2E: output truncation behaviour ─────────────────────────────
    //
    // These tests go through `Tool::execute()` (the actual LLM-facing
    // entry point) and verify the head+tail truncation behaviour at the
    // 4 KB + 1 KB budget. They are critical because the previous
    // constant-driven design (`truncate_output` with a single byte cap
    // would throw away stderr at the tail — see git history of
    // `tools/output.rs::truncate_head_tail_output`).

    /// Build a real `ShellTool` from the platform-detected shell. Panics
    /// with a clear message if no shell is available (which is itself a
    /// CI-environment bug worth surfacing).
    fn real_shell() -> ShellTool {
        let shells = crate::platform::detected_shells();
        let primary = shells
            .first()
            .expect("E2E shell test requires at least one shell on PATH");
        ShellTool::new(
            &primary.tool_name,
            &primary.display_name,
            &primary.binary,
            &primary.path,
            &primary.arg,
        )
    }

    /// Generate N bytes of 'x' on stdout. Cross-platform (bash + sh).
    fn cmd_stdout_n_bytes(n: usize) -> String {
        format!("head -c {n} /dev/zero | tr '\\0' 'x'")
    }

    /// Generate N bytes of 'x' on stderr.
    fn cmd_stderr_n_bytes(n: usize) -> String {
        format!("head -c {n} /dev/zero | tr '\\0' 'x' >&2")
    }

    #[tokio::test]
    async fn e2e_small_output_returns_full_content_no_marker() {
        let tool = real_shell();
        let result = tool
            .execute(serde_json::json!({"command": "echo hello"}), None)
            .await
            .unwrap();
        assert!(result.ok);
        assert!(result.content.contains("hello"));
        assert!(
            !result.content.contains("omitted from middle"),
            "small output must not trigger marker: {}",
            result.content
        );
    }

    #[tokio::test]
    async fn e2e_large_stdout_truncated_with_head_tail_marker() {
        // 50 KB stdout ≫ 5 KB budget → must be truncated.
        let tool = real_shell();
        let n = 50_000;
        let result = tool
            .execute(serde_json::json!({"command": cmd_stdout_n_bytes(n)}), None)
            .await
            .unwrap();
        assert!(result.ok);

        let content_len = result.content.len();
        // After truncation: budget (5 KB) + marker (~250 bytes). Anything
        // noticeably under n proves truncation happened.
        assert!(
            content_len < n / 4,
            "expected truncation; got {content_len} bytes (orig {n})"
        );
        // Head + tail were both kept
        assert!(
            result.content.contains("omitted from middle"),
            "marker must be present: {}",
            &result.content[..result.content.len().min(500)]
        );
        // Marker carries the omitted byte count
        assert!(
            result.content.contains("bytes omitted"),
            "marker must show byte count"
        );
        // Marker offers re-query commands (this is the whole point of head+tail over naive head)
        assert!(
            result.content.contains("grep -n"),
            "marker must teach LLM how to fetch the missing middle"
        );
    }

    #[tokio::test]
    async fn e2e_large_stderr_only_preserves_stderr_at_tail() {
        // When stderr is the only signal (e.g. cargo/pytest failure
        // pattern), it must survive head+tail truncation. Critical
        // regression guard.
        let tool = real_shell();
        let n = 30_000;
        // Stderr has a unique sentinel at the very end so we can detect
        // whether the tail was preserved.
        let sentinel = "FATAL_ERROR_SENTINEL_AT_END";
        let cmd = format!("{{ {}; echo '{sentinel}' >&2; }}", cmd_stderr_n_bytes(n));
        let result = tool
            .execute(serde_json::json!({"command": cmd}), None)
            .await
            .unwrap();
        assert!(result.ok);
        assert!(
            result.content.contains(sentinel),
            "stderr tail sentinel must survive truncation (1 KB tail budget)"
        );
        assert!(result.content.contains("STDERR:"));
    }

    #[tokio::test]
    async fn e2e_large_stdout_short_stderr_preserves_stderr_at_tail() {
        // THE central bug the head+tail design solves: a large stdout
        // used to crowd the short stderr off the end of a head-only
        // truncate. With head+tail, stderr sits at the very end (after
        // the "STDERR:" wrapper) and falls inside the 1 KB tail budget.
        let tool = real_shell();
        let sentinel = "PYTEST_FAIL_SENTINEL";
        let cmd = format!(
            "{{ {}; echo '{sentinel}' >&2; }}",
            cmd_stdout_n_bytes(50_000)
        );
        let result = tool
            .execute(serde_json::json!({"command": cmd}), None)
            .await
            .unwrap();
        assert!(result.ok);
        assert!(
            result.content.contains(sentinel),
            "stderr must be preserved in tail even when stdout is huge"
        );
    }

    #[tokio::test]
    async fn e2e_exact_budget_no_marker() {
        // Output exactly MAX_SHELL_HEAD_BYTES + MAX_SHELL_TAIL_BYTES
        // bytes → no marker (boundary case). Regression guard: the
        // off-by-one that would make this trigger a marker is easy to
        // introduce.
        let tool = real_shell();
        let n = output::MAX_SHELL_HEAD_BYTES + output::MAX_SHELL_TAIL_BYTES;
        let result = tool
            .execute(serde_json::json!({"command": cmd_stdout_n_bytes(n)}), None)
            .await
            .unwrap();
        assert!(result.ok);
        assert!(
            !result.content.contains("omitted from middle"),
            "exact-budget output must not trigger marker (n={n}, got {} bytes)",
            result.content.len()
        );
        // And the content should be roughly the full input (no marker bytes added)
        assert!(result.content.len() >= n);
    }

    #[tokio::test]
    async fn e2e_just_over_budget_triggers_marker() {
        let tool = real_shell();
        let n = output::MAX_SHELL_HEAD_BYTES + output::MAX_SHELL_TAIL_BYTES + 100;
        let result = tool
            .execute(serde_json::json!({"command": cmd_stdout_n_bytes(n)}), None)
            .await
            .unwrap();
        assert!(result.ok);
        assert!(
            result.content.contains("omitted from middle"),
            "output one byte over budget must trigger marker"
        );
        assert!(result.content.contains("100 bytes omitted"));
    }

    #[tokio::test]
    async fn e2e_spec_value_describes_head_tail_budget() {
        // The spec text is the LLM's only signal about output shape. If
        // this drifts from the actual behaviour, the LLM mispredicts.
        let tool = real_shell();
        let spec = tool.spec();
        let desc = &spec.description;

        // The bash description must mention both the head and tail
        // budget so the LLM knows what's preserved.
        assert!(
            desc.contains("4 KB") && desc.contains("1 KB"),
            "bash spec must mention both 4 KB and 1 KB budgets; got: {desc}"
        );
        // Must teach the LLM how to recover the omitted middle.
        assert!(
            desc.contains("grep") || desc.contains("Select-String"),
            "bash spec must offer a concrete re-query hint; got: {desc}"
        );
        // Should NOT lie about being a simple byte cap (that would be
        // the old behaviour and contradict the actual head+tail design).
        assert!(
            !desc.contains("capped at"),
            "bash spec must not claim a single 'capped at' byte cap; got: {desc}"
        );
    }

    #[tokio::test]
    async fn e2e_spec_value_schema_does_not_change_with_constant_drift() {
        // Regression guard: spec schema must stay stable so LLM tool
        // calls keep working when the constants change.
        let tool = real_shell();
        let schema = tool.spec().input_schema;
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["command"].is_object());
        let required = schema["required"].as_array().unwrap();
        assert_eq!(
            required,
            &vec![serde_json::Value::String("command".to_string())]
        );
    }
}
