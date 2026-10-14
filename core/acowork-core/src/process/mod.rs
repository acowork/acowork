//! Process management utilities with idle-timeout monitoring.
//!
//! Provides [`run_with_idle_timeout`] and [`run_command_with_idle_timeout`]
//! for running child processes that should be killed if they produce no
//! stdout/stderr output for a configurable duration.
//!
//! # Idle timeout vs absolute timeout
//!
//! An **absolute timeout** kills the process after a fixed wall-clock duration,
//! regardless of whether it is making progress. This forces the caller to guess
//! a "reasonable" upper bound, which is fragile — a slow network can make a
//! 5-minute download take 10 minutes.
//!
//! An **idle timeout** only kills the process when it has produced *zero output*
//! for the specified duration. As long as the process keeps printing to stdout
//! or stderr, the timer resets. This means:
//!
//! - A `curl` download printing progress bars every second → never times out
//! - A `cargo build` streaming compiler output → never times out
//! - An `npm install` stuck on a TCP handshake with 60s of silence → killed
//!
//! # Example
//!
//! ```rust,no_run
//! use acowork_core::process::{run_command_with_idle_timeout, ProcessOutput};
//! use std::time::Duration;
//!
//! # async fn example() {
//! let mut cmd = tokio::process::Command::new("bash");
//! cmd.arg("some_install_script.sh");
//!
//! match run_command_with_idle_timeout(&mut cmd, Duration::from_secs(120)).await {
//!     Ok(ProcessOutput { exit_code, stdout, stderr }) => {
//!         println!("Script finished with exit code: {:?}", exit_code);
//!     }
//!     Err(e) => {
//!         eprintln!("Script timed out: {e}");
//!         // e.stdout() and e.stderr() contain partial output captured before timeout
//!     }
//! }
//! # }
//! ```

use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

mod tree;
use tree::TreeGuard;

// ── Public types ──────────────────────────────────────────────────────────

/// Output captured from a completed child process.
#[derive(Debug, Clone)]
pub struct ProcessOutput {
    /// Exit code, or `None` if terminated by a signal.
    pub exit_code: Option<i32>,
    /// Full stdout captured line-by-line (lines separated by `\n`).
    pub stdout: String,
    /// Full stderr captured line-by-line (lines separated by `\n`).
    pub stderr: String,
}

/// Error returned when a process exceeds the idle timeout.
///
/// Contains any stdout/stderr captured *before* the timeout fired,
/// which is useful for diagnosing what the process was doing when it got stuck.
#[derive(Debug, Clone)]
pub struct IdleTimeoutError {
    /// How long the process was idle before being killed.
    pub idle_secs: u64,
    /// Partial stdout captured before timeout.
    pub stdout: String,
    /// Partial stderr captured before timeout.
    pub stderr: String,
}

impl std::fmt::Display for IdleTimeoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Process idle timeout after {}s with no output",
            self.idle_secs
        )
    }
}

impl std::error::Error for IdleTimeoutError {}

impl IdleTimeoutError {
    /// Access partial stdout captured before the timeout.
    pub fn stdout(&self) -> &str {
        &self.stdout
    }

    /// Access partial stderr captured before the timeout.
    pub fn stderr(&self) -> &str {
        &self.stderr
    }
}

// ── Internal channel type ─────────────────────────────────────────────────

/// A line of output from either stdout or stderr.
enum OutputLine {
    Stdout(String),
    Stderr(String),
}

// ── Public API ────────────────────────────────────────────────────────────

/// Run an already-spawned [`Child`] process with idle-timeout monitoring.
///
/// The child **must** have its stdout and stderr set to `Stdio::piped()`.
/// Use [`run_command_with_idle_timeout`] for a convenience wrapper that
/// configures this automatically.
///
/// # How it works
///
/// Two background tasks read stdout and stderr line-by-line, sending each
/// line through an mpsc channel. The main loop does a `select!` between
/// receiving the next line and an idle timer:
///
/// - **Line received** → appended to the output buffer, timer resets
/// - **Channel closed** (both readers reached EOF) → process finished, wait for exit code
/// - **Timer fires** → process killed, partial output returned as error
///
/// # Panics
///
/// Panics if `child.stdout` or `child.stderr` is `None` (i.e. not piped).
pub async fn run_with_idle_timeout(
    mut child: Child,
    idle_timeout: Duration,
) -> Result<ProcessOutput, IdleTimeoutError> {
    // Contain the process tree so the timeout kill reaches descendants
    // too — surviving grandchildren keep the stdio pipes open and
    // block runtime teardown (see `process/tree.rs`).
    let tree = TreeGuard::attach(&child);

    let stdout = child
        .stdout
        .take()
        .expect("run_with_idle_timeout requires child.stdout to be Stdio::piped()");
    let stderr = child
        .stderr
        .take()
        .expect("run_with_idle_timeout requires child.stderr to be Stdio::piped()");

    let (tx, mut rx) = mpsc::unbounded_channel::<OutputLine>();

    // Spawn a task to read stdout line-by-line.
    let tx_stdout = tx.clone();
    let stdout_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            if tx_stdout.send(OutputLine::Stdout(line)).is_err() {
                break; // Receiver dropped — parent task finished
            }
        }
    });

    // Spawn a task to read stderr line-by-line.
    let stderr_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            if tx.send(OutputLine::Stderr(line)).is_err() {
                break; // Receiver dropped
            }
        }
    });

    let mut stdout_buf = String::new();
    let mut stderr_buf = String::new();

    loop {
        // Wrap channel recv in a timeout — each successful recv resets the
        // timer implicitly because the next loop iteration creates a fresh timeout.
        match tokio::time::timeout(idle_timeout, rx.recv()).await {
            // A line arrived before the timeout — reset happens on next iteration.
            Ok(Some(OutputLine::Stdout(line))) => {
                stdout_buf.push_str(&line);
                stdout_buf.push('\n');
            }
            Ok(Some(OutputLine::Stderr(line))) => {
                stderr_buf.push_str(&line);
                stderr_buf.push('\n');
            }
            // Channel closed — both readers reached EOF, process finished.
            Ok(None) => {
                break;
            }
            // No output for `idle_timeout` — kill the process.
            Err(_elapsed) => {
                tracing::warn!(
                    idle_secs = idle_timeout.as_secs(),
                    "Process idle timeout — killing child process"
                );
                // Kill the whole tree first: grandchildren hold inherited
                // stdio write handles, and leaving them alive blocks
                // pipe teardown (and, on Windows, the tokio runtime drop
                // in `#[tokio::test]`) until they exit on their own.
                tree.kill_tree();
                let _ = child.kill().await;
                // Abort reader tasks rather than joining them — the killed
                // process may have grandchildren still holding stdout/stderr
                // open, which would block the readers indefinitely.
                stdout_task.abort();
                stderr_task.abort();
                return Err(IdleTimeoutError {
                    idle_secs: idle_timeout.as_secs(),
                    stdout: stdout_buf,
                    stderr: stderr_buf,
                });
            }
        }
    }

    // Both readers finished — wait for them and the process.
    let _ = tokio::join!(stdout_task, stderr_task);

    let status = child
        .wait()
        .await
        .map_err(|e| IdleTimeoutError {
            idle_secs: 0,
            stdout: format!("Failed to wait for child process: {e}"),
            stderr: String::new(),
        })?;

    Ok(ProcessOutput {
        exit_code: status.code(),
        stdout: stdout_buf,
        stderr: stderr_buf,
    })
}

/// Convenience wrapper: configure a [`Command`] for idle-timeout monitoring
/// and spawn it.
///
/// Automatically sets `stdout(Stdio::piped())`, `stderr(Stdio::piped())`,
/// and `kill_on_drop(true)`, then calls [`run_with_idle_timeout`].
///
/// # Example
///
/// ```rust,no_run
/// use acowork_core::process::run_command_with_idle_timeout;
/// use std::time::Duration;
///
/// # async fn example() {
/// let mut cmd = tokio::process::Command::new("echo");
/// cmd.arg("hello");
/// let output = run_command_with_idle_timeout(&mut cmd, Duration::from_secs(5))
///     .await
///     .unwrap();
/// assert_eq!(output.stdout.trim(), "hello");
/// # }
/// ```
pub async fn run_command_with_idle_timeout(
    cmd: &mut Command,
    idle_timeout: Duration,
) -> Result<ProcessOutput, IdleTimeoutError> {
    // Unix: make the child a process-group leader BEFORE spawn so the
    // timeout kill can signal the whole group (Windows containment is
    // attached post-spawn inside run_with_idle_timeout).
    TreeGuard::configure(cmd);

    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);

    let child = cmd.spawn().map_err(|e| IdleTimeoutError {
        idle_secs: 0,
        stdout: String::new(),
        stderr: format!("Failed to spawn process: {e}"),
    })?;

    run_with_idle_timeout(child, idle_timeout).await
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Short idle timeout for tests — keeps test suite fast.
    const TEST_IDLE: Duration = Duration::from_secs(2);

    // ── Helper ─────────────────────────────────────────────────────────

    /// Build a [`Command`] that runs the given shell snippet.
    ///
    /// This wrapper is **platform-aware**: on Unix it shells out to `bash -c`,
    /// on Windows it shells out to `cmd.exe /C`. Because cmd.exe has very
    /// different syntax from bash, callers should keep the snippet trivial —
    /// `echo ...`, `exit N`, `ping -n ... > NUL` — or branch via
    /// [`shell_sleep`] / [`shell_true`] below for the handful of operations
    /// (loops, redirections, sleep) that diverge between shells.
    fn bash_cmd(script: &str) -> Command {
        // Trim leading/trailing whitespace before handing the script to the
        // shell. **cmd.exe does NOT tolerate a leading newline or leading
        // whitespace**: it treats a blank line as a complete statement and
        // then fails to parse the indented `for` on the next line, returning
        // silently with empty output. Bash tolerates leading whitespace, so
        // normalizing for both shells is a safe no-op on Unix.
        let script = script.trim();

        #[cfg(unix)]
        {
            let mut cmd = Command::new("bash");
            cmd.args(["-c", script]);
            cmd
        }
        #[cfg(windows)]
        {
            // Echo the snippet verbatim to the user's terminal on test failures
            // so that Windows users can re-run it manually under `cmd.exe`.
            let mut cmd = Command::new("cmd.exe");
            // `/D /S /C` mirrors what `bash -c` does: run, then exit.
            cmd.args(["/D", "/S", "/C", script]);
            cmd
        }
    }

    /// `sleep` in the active shell — Unix uses `sleep`, Windows uses `ping`.
    ///
    /// Returns a true no-op once the requested number of seconds is zero so
    /// tests can swap this in without changing the surrounding bash_cmd
    /// callsite.
    #[cfg(windows)]
    fn shell_sleep(secs: u64) -> String {
        if secs == 0 {
            // `rem` is the cmd.exe comment marker; runs instantly.
            return "rem noop".to_string();
        }
        // `ping` to a non-routable address is the canonical portable "sleep":
        // one ping takes roughly one second on every Windows version.
        // Round up so callers asking for N seconds get at least N ticks.
        let ticks = secs.max(1);
        format!("ping -n {} 127.0.0.1 > NUL", ticks + 1)
    }

    #[cfg(unix)]
    fn shell_sleep(secs: u64) -> String {
        format!("sleep {}", secs)
    }

    /// Statement separator for joining snippets passed to [`bash_cmd`].
    ///
    /// bash uses `;` (always run the next). **cmd.exe does NOT recognise
    /// `;` as a separator** — it would parse `cmd /C "echo a; echo b"` as
    /// `echo a` followed by an invalid `;` literal, erroring out without
    /// running `echo b`. cmd.exe's unconditional separator is `&`. Using the
    /// wrong one is a silent test failure on Windows.
    const SHELL_SEP: &str = {
        #[cfg(unix)]
        {
            "; "
        }
        #[cfg(windows)]
        {
            "& "
        }
    };

    /// `true` — exit immediately with code 0. Bash- and cmd-compatible.
    fn shell_true() -> &'static str {
        // cmd.exe `rem .` exits 0 with no output; bash accepts `rem` as the
        // name of an unset variable (prints nothing) followed by `.` (the
        // current directory's path) which we discard with `: >/dev/null`.
        // Simpler: rely on the shell's default exit 0 when handed a no-op.
        // On bash, the literal string `:` is a built-in no-op (exit 0).
        // On cmd.exe, the literal string `rem .` exits 0 with no side effects.
        #[cfg(unix)]
        {
            ":" // bash builtin no-op
        }
        #[cfg(windows)]
        {
            "rem ."
        }
    }

    /// Exit with a non-zero status from the active shell, portably.
    fn shell_exit(code: i32) -> String {
        #[cfg(unix)]
        {
            format!("exit {code}")
        }
        #[cfg(windows)]
        {
            // cmd.exe's `exit /B` sets ERRORLEVEL without closing the calling
            // shell — which is irrelevant here since `/C` already terminated
            // the process — but the leading `/B` keeps semantics correct
            // should the script ever be inlined elsewhere.
            format!("exit /B {code}")
        }
    }

    /// Emit `text` to stdout portably. Bash wraps it in single quotes; cmd.exe
    /// emits it bare (with double quotes if `text` contains whitespace).
    fn echo_line(text: &str) -> String {
        #[cfg(unix)]
        {
            if text.contains('\'') {
                // bash: fall back to double quotes and escape `$`, `\`, `"`.
                let escaped = text
                    .replace('\\', r"\\")
                    .replace('"', "\\\"")
                    .replace('$', "\\$");
                format!("echo \"{escaped}\"")
            } else {
                format!("echo '{text}'")
            }
        }
        #[cfg(windows)]
        {
            // cmd.exe's `echo` prints argv verbatim — no quote stripping.
            // Use `echo(text)` (cmd extension) to safely emit content with
            // any character, including spaces and `&`.
            format!("echo({text})")
        }
    }

    /// Emit `text` to stderr portably. bash redirects with `>&2`; cmd.exe
    /// uses the same syntax.
    fn echo_stderr(text: &str) -> String {
        #[cfg(unix)]
        {
            if text.contains('\'') {
                let escaped = text
                    .replace('\\', r"\\")
                    .replace('"', "\\\"")
                    .replace('$', "\\$");
                format!("echo \"{escaped}\" >&2")
            } else {
                format!("echo '{text}' >&2")
            }
        }
        #[cfg(windows)]
        {
            // `echo(...)` syntax (cmd.exe extension) accepts arbitrary text;
            // `1>&2` redirects stdout to stderr.
            format!("echo({text}) 1>&2")
        }
    }

    // ── Normal exit ────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_normal_exit_success() {
        let mut cmd = bash_cmd("echo hello");
        let output = run_command_with_idle_timeout(&mut cmd, TEST_IDLE)
            .await
            .expect("should complete normally");
        assert_eq!(output.exit_code, Some(0));
        assert!(output.stdout.contains("hello"));
    }

    #[tokio::test]
    async fn test_normal_exit_failure() {
        let mut cmd = bash_cmd(&shell_exit(1));
        let output = run_command_with_idle_timeout(&mut cmd, TEST_IDLE)
            .await
            .expect("should complete normally (even with non-zero exit)");
        assert_eq!(output.exit_code, Some(1));
    }

    #[tokio::test]
    async fn test_empty_output() {
        let mut cmd = bash_cmd(shell_true()); // produces no output, exits 0
        let output = run_command_with_idle_timeout(&mut cmd, TEST_IDLE)
            .await
            .expect("should complete normally");
        assert_eq!(output.exit_code, Some(0));
        assert!(output.stdout.is_empty() || output.stdout == "\n");
    }

    // ── Idle timeout ───────────────────────────────────────────────────

    /// Regression test for process-tree containment (see `process/tree.rs`):
    /// the idle-timeout kill must reach **grandchildren**, not just the
    /// direct child. Surviving descendants keep the inherited stdio pipe
    /// write handles open, which historically blocked the tokio runtime
    /// teardown in `#[tokio::test]` for the full lifetime of the
    /// grandchild (~120 s per test on Windows).
    ///
    /// Shape: the parent hangs silently (idle timeout fires at ~2 s)
    /// after launching a grandchild that writes a marker file at ~4 s.
    /// If containment works, the marker never appears.
    #[tokio::test]
    async fn test_idle_timeout_kills_process_tree() {
        let unique = uuid::Uuid::new_v4().simple().to_string();
        let dir = std::env::temp_dir();
        let marker = dir.join(format!("acowork-tree-kill-marker-{unique}.txt"));
        let marker_path = marker.to_string_lossy().into_owned();

        #[cfg(windows)]
        let bat = dir.join(format!("acowork-tree-kill-grandchild-{unique}.bat"));

        #[cfg(windows)]
        let script = {
            // The script is handed to `cmd.exe /S /C` as a single argv
            // string; embedded quotes do not survive std's `\"` escaping
            // (cmd does not understand backslash escapes), so the
            // grandchild lives in a bare-path .bat file. Skip if either
            // path needs quoting.
            let bat_path = bat.to_string_lossy().into_owned();
            if bat_path.contains(' ') || marker_path.contains(' ') {
                eprintln!(
                    "skipping test_idle_timeout_kills_process_tree: \
                     temp path contains spaces ({bat_path})"
                );
                return;
            }
            // Quotes inside the .bat itself are fine — cmd parses the
            // file directly, not through argv escaping.
            std::fs::write(
                &bat,
                "@echo off\r\nping -n 5 127.0.0.1 > NUL\r\necho alive> \"%~1\"\r\n",
            )
            .expect("write grandchild .bat");
            format!("start /B {bat_path} {marker_path}{}ping -n 121 127.0.0.1 > NUL", SHELL_SEP)
        };
        #[cfg(unix)]
        let script = format!("( sleep 4; touch '{marker_path}' ) & sleep 120");

        let mut cmd = bash_cmd(&script);
        let result = run_command_with_idle_timeout(&mut cmd, TEST_IDLE).await;
        assert!(
            result.is_err(),
            "expected idle timeout, got Ok({result:?})"
        );

        // A surviving grandchild would create the marker at ~4 s; the
        // kill fired at ~2 s. Wait past that point and assert absence.
        tokio::time::sleep(Duration::from_secs(6)).await;
        assert!(
            !marker.exists(),
            "grandchild survived the idle-timeout kill — process tree not contained"
        );

        #[cfg(windows)]
        let _ = std::fs::remove_file(&bat);
        let _ = std::fs::remove_file(&marker);
    }

    #[tokio::test]
    async fn test_idle_timeout_no_output() {
        // shell_sleep produces zero output — should trigger idle timeout.
        let mut cmd = bash_cmd(&shell_sleep(120));
        let start = tokio::time::Instant::now();
        let result = run_command_with_idle_timeout(&mut cmd, TEST_IDLE).await;
        let elapsed = start.elapsed();

        match result {
            Err(e) => {
                assert_eq!(e.idle_secs, TEST_IDLE.as_secs());
                // Should fire close to the idle timeout, not the shell_sleep duration.
                assert!(elapsed < Duration::from_secs(5));
            }
            Ok(_) => panic!("Expected idle timeout, but process completed"),
        }
    }

    #[tokio::test]
    async fn test_output_then_hang() {
        // Print one line, then hang — should timeout and capture the line.
        let script = format!("{}{}{}", echo_line("started setup..."), SHELL_SEP, shell_sleep(120));
        let mut cmd = bash_cmd(&script);
        let result = run_command_with_idle_timeout(&mut cmd, TEST_IDLE).await;

        match result {
            Err(e) => {
                assert!(e.stdout.contains("started setup"));
                assert_eq!(e.idle_secs, TEST_IDLE.as_secs());
            }
            Ok(_) => panic!("Expected idle timeout"),
        }
    }

    // ── Long-running with continuous output ────────────────────────────

    #[tokio::test]
    async fn test_continuous_output_no_timeout() {
        // Print 10 lines at small intervals — total elapsed > idle timeout,
        // but should NOT timeout because output is continuous.
        //
        // Unix: bash `for` loop with shell_sleep(0) between echoes.
        // Windows: cmd.exe `for /L` loop (the only portable loop primitive
        // that supports incrementing numeric counters without an external
        // binary).
        #[cfg(unix)]
        let script = {
            let mut parts = Vec::with_capacity(20);
            for i in 1..=10 {
                parts.push(echo_line(&format!("line {i}")));
                parts.push(shell_sleep(0));
            }
            parts.join(SHELL_SEP)
        };
        #[cfg(windows)]
        let script = r#"
            for /L %i in (1,1,10) do @(echo line %i & ping -n 1 127.0.0.1 > NUL)
        "#
        .to_string();
        let mut cmd = bash_cmd(&script);
        let output = run_command_with_idle_timeout(&mut cmd, TEST_IDLE)
            .await
            .expect("should NOT timeout — output is continuous");

        assert_eq!(output.exit_code, Some(0));
        // All 10 lines should be present.
        for i in 1..=10 {
            assert!(
                output.stdout.contains(&format!("line {i}")),
                "Missing line {i} in output: {}",
                output.stdout
            );
        }
    }

    // ── stderr ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_stderr_captured() {
        let script = format!(
            "{}{}{}",
            echo_line("to stdout"),
            SHELL_SEP,
            echo_stderr("to stderr")
        );
        let mut cmd = bash_cmd(&script);
        let output = run_command_with_idle_timeout(&mut cmd, TEST_IDLE)
            .await
            .expect("should complete normally");

        assert!(output.stdout.contains("to stdout"));
        assert!(output.stderr.contains("to stderr"));
    }

    #[tokio::test]
    async fn test_stderr_only() {
        let mut cmd = bash_cmd(&echo_stderr("error message"));
        let output = run_command_with_idle_timeout(&mut cmd, TEST_IDLE)
            .await
            .expect("should complete normally");

        assert!(output.stderr.contains("error message"));
        // stdout may be empty or just a newline.
    }

    // ── Mixed stdout/stderr interleaving ───────────────────────────────

    #[tokio::test]
    async fn test_mixed_stdout_stderr() {
        // Interleave stdout and stderr output.
        let script = [
            echo_line("out1"),
            echo_stderr("err1"),
            echo_line("out2"),
            echo_stderr("err2"),
        ]
        .join(SHELL_SEP);
        let mut cmd = bash_cmd(&script);
        let output = run_command_with_idle_timeout(&mut cmd, TEST_IDLE)
            .await
            .expect("should complete normally");

        assert!(output.stdout.contains("out1"));
        assert!(output.stdout.contains("out2"));
        assert!(output.stderr.contains("err1"));
        assert!(output.stderr.contains("err2"));
    }

    // ── stdout EOF before stderr ───────────────────────────────────────

    #[tokio::test]
    async fn test_stdout_eof_before_stderr() {
        // stdout closes first, stderr continues briefly.
        //
        // `exec 1>&-` is a POSIX-only fd operation; cmd.exe has no equivalent
        // way to close just stdout while keeping stderr open. The behavioral
        // intent — "process produces output on both streams, neither times
        // out" — is exhaustively covered by `test_mixed_stdout_stderr` and
        // `test_stderr_captured` above. So skip on Windows and trust those
        // simpler tests; the Linux coverage here is for the more exotic
        // fd-shutdown path.
        #[cfg(unix)]
        {
            let script = format!(
                "{}{}exec 1>&-{}{}{}{}",
                echo_line("out"),
                SHELL_SEP,
                SHELL_SEP,
                shell_sleep(0),
                SHELL_SEP,
                echo_stderr("err")
            );
            let mut cmd = bash_cmd(&script);
            let output = run_command_with_idle_timeout(&mut cmd, TEST_IDLE)
                .await
                .expect("should complete normally");

            assert!(output.stdout.contains("out"));
            assert!(output.stderr.contains("err"));
        }
        #[cfg(windows)]
        {
            // Covered by test_mixed_stdout_stderr / test_stderr_captured.
            eprintln!(
                "test_stdout_eof_before_stderr: skipped on Windows \
                 (no equivalent of `exec 1>&-` in cmd.exe)"
            );
        }
    }

    // ── Large output ───────────────────────────────────────────────────

    #[tokio::test]
    async fn test_large_output() {
        // Generate 1000 lines — should not OOM or lose data.
        //
        // Unix: a single `seq 1 1000 | head -c ...` would also work, but we
        // want to exercise the line-buffered reader path — emitting one
        // newline-terminated line per loop iteration is closer to a real
        // compiler's stdout pattern.
        #[cfg(unix)]
        let script = r#"
            for i in $(seq 1 1000); do
                echo "line $i"
            done
        "#;
        #[cfg(windows)]
        let script = r#"
            for /L %i in (1,1,1000) do @echo line %i
        "#;
        let mut cmd = bash_cmd(script);
        let output = run_command_with_idle_timeout(&mut cmd, TEST_IDLE)
            .await
            .expect("should complete normally");

        assert_eq!(output.exit_code, Some(0));
        let line_count = output.stdout.lines().count();
        assert_eq!(line_count, 1000);
    }

    // ── kill_on_drop ───────────────────────────────────────────────────

    /// Verify that `tokio::time::timeout` + `UnboundedReceiver::recv()` works
    /// as expected: after consuming the only message, the next recv should
    /// time out (channel still open, no messages).
    #[tokio::test]
    async fn test_timeout_pattern_sanity() {
        let (tx, mut rx) = mpsc::unbounded_channel::<i32>();

        // Send one message, then nothing.
        tx.send(1).unwrap();

        // First recv — should get the message immediately.
        let result = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await;
        assert!(result.is_ok(), "first recv should succeed");
        assert_eq!(result.unwrap(), Some(1));

        // Second recv — no messages, channel still open (tx alive).
        // Should time out after ~2s.
        let start = std::time::Instant::now();
        let result = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await;
        let elapsed = start.elapsed();

        assert!(result.is_err(), "second recv should time out");
        assert!(
            elapsed < Duration::from_secs(4),
            "timeout should fire in ~2s, took {elapsed:?}"
        );
    }

    #[tokio::test]
    #[cfg(unix)] // Uses `sleep` and `kill -0` POSIX commands; Windows has no direct equivalent.
    async fn test_kill_on_drop() {
        // Verify that dropping the future kills the child process.
        // We spawn a long-sleeping process, drop the future, and verify
        // the process is gone.
        let mut cmd = Command::new("sleep");
        cmd.arg("120")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let child = cmd.spawn().expect("should spawn");
        let pid = child.id().expect("should have pid");

        // Wrap in a task we can abort.
        let handle = tokio::spawn(run_with_idle_timeout(child, Duration::from_secs(60)));

        // Give it a moment to start, then drop.
        tokio::time::sleep(Duration::from_millis(100)).await;
        handle.abort();

        // Give the OS a moment to reap the process.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Verify the process is gone.
        let still_running = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        assert!(
            !still_running,
            "Child process {pid} should have been killed on drop"
        );
    }

    /// Windows equivalent of [`test_kill_on_drop`]: spawn a long-running
    /// cmd.exe process, abort the task, then verify the PID is no longer
    /// listed by `tasklist`.
    #[tokio::test]
    #[cfg(windows)]
    async fn test_kill_on_drop() {
        use std::process::Stdio;

        // `timeout /T 999` will block for 999 seconds unless killed.
        let mut cmd = Command::new("cmd.exe");
        cmd.args(["/D", "/S", "/C", "timeout /T 999 /NOBREAK < nul > nul 2>&1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let child = cmd.spawn().expect("should spawn cmd.exe timeout");
        let pid = child.id().expect("should have pid");

        let handle = tokio::spawn(run_with_idle_timeout(child, Duration::from_secs(60)));

        // Give it a moment to start, then abort.
        tokio::time::sleep(Duration::from_millis(100)).await;
        handle.abort();

        // Give the OS a moment to reap the process.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Verify via `tasklist /FI "PID eq <pid>"`: it returns 0 if found,
        // 1 if not. We assert NOT found.
        let output = std::process::Command::new("tasklist.exe")
            .args([
                "/NH",
                "/FI",
                &format!("PID eq {pid}"),
            ])
            .output()
            .expect("tasklist should run");
        let stdout = String::from_utf8_lossy(&output.stdout);
        // `tasklist /NH` prints "INFO: No tasks are running..." when empty,
        // otherwise prints "<process>   <pid>   ..." for a match.
        let still_running = !stdout.contains("No tasks")
            && stdout.lines().any(|line| line.contains(&pid.to_string()));

        assert!(
            !still_running,
            "Child process {pid} should have been killed on drop; tasklist output: {stdout}"
        );
    }
}
