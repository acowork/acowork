//! LSP process pool — process lifecycle bound to LSP Relay, not WebSocket session.
//!
//! Maintains a map of `(command, workspace_root) → LspProcessEntry`.
//! Multiple WebSocket clients can share a single LSP process.
//! Idle processes are reaped after a configurable timeout.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex, broadcast, mpsc};

use std::process::Stdio;

/// Default idle timeout before a pooled LSP process is reaped (10 minutes).
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(600);

/// Default reaper tick interval (60 seconds).
const REAPER_INTERVAL: Duration = Duration::from_secs(60);

/// Key for pool lookup: "{command}:{args_joined}:{workspace_root}"
type PoolKey = String;

/// A pooled LSP process entry shared across WebSocket clients.
pub struct LspProcessEntry {
    /// Send JSON-RPC messages to LSP stdin
    pub stdin_tx: mpsc::UnboundedSender<String>,
    /// Subscribe to receive JSON-RPC messages from LSP stdout
    pub stdout_tx: broadcast::Sender<String>,
    /// Number of active WebSocket clients using this process
    pub active_clients: AtomicUsize,
    /// When last client disconnected (None if clients are active)
    pub last_idle_since: Mutex<Option<Instant>>,
    /// Resolved LSP command (e.g. "rust-analyzer")
    pub command: String,
    /// Workspace root directory
    pub workspace_root: String,
    /// Process ID (for logging)
    pub pid: u32,
    /// Cached InitializeResult JSON from the first successful handshake.
    pub init_result: Mutex<Option<String>>,
}

/// Shared LSP process pool.
pub struct LspPool {
    entries: Mutex<HashMap<PoolKey, Arc<LspProcessEntry>>>,
}

impl Default for LspPool {
    fn default() -> Self {
        Self::new()
    }
}

impl LspPool {
    /// Create a new empty pool.
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Build the pool key from command, args, and workspace root.
    fn make_key(command: &str, args: &[String], workspace_root: &str) -> PoolKey {
        let args_joined = args.join(" ");
        format!("{}:{}:{}", command, args_joined, workspace_root)
    }

    /// Get an existing LSP process or spawn a new one.
    pub async fn get_or_spawn(
        &self,
        command: &str,
        args: &[String],
        workspace_root: &str,
    ) -> anyhow::Result<Arc<LspProcessEntry>> {
        let key = Self::make_key(command, args, workspace_root);
        let mut entries = self.entries.lock().await;

        if let Some(entry) = entries.get(&key) {
            if !entry.stdin_tx.is_closed() {
                entry.active_clients.fetch_add(1, Ordering::Relaxed);
                *entry.last_idle_since.lock().await = None;
                tracing::info!(
                    "[LSP Pool] Reusing '{}' (PID {}) for workspace '{}'",
                    command,
                    entry.pid,
                    workspace_root
                );
                return Ok(Arc::clone(entry));
            }
            tracing::warn!(
                "[LSP Pool] Stale entry for '{}' in '{}' (PID {}), removing",
                command,
                workspace_root,
                entry.pid
            );
            entries.remove(&key);
        }

        let entry = Self::spawn_pooled(command, args, workspace_root).await?;
        entries.insert(key, Arc::clone(&entry));
        Ok(entry)
    }

    /// Mark a client as disconnected from the given pool entry.
    pub async fn client_disconnected(&self, command: &str, args: &[String], workspace_root: &str) {
        let key = Self::make_key(command, args, workspace_root);
        let entries = self.entries.lock().await;
        if let Some(entry) = entries.get(&key) {
            let prev = entry.active_clients.fetch_sub(1, Ordering::Relaxed);
            if prev <= 1 {
                *entry.last_idle_since.lock().await = Some(Instant::now());
                tracing::info!(
                    "[LSP Pool] '{}' (PID {}) now idle, workspace '{}'",
                    entry.command,
                    entry.pid,
                    entry.workspace_root,
                );
            }
        }
    }

    /// Evict processes that have been idle longer than `timeout`.
    pub async fn reap_idle(&self, timeout: Duration) {
        let mut entries = self.entries.lock().await;
        let mut to_remove = Vec::new();

        for (key, entry) in entries.iter() {
            let idle_since = *entry.last_idle_since.lock().await;
            if let Some(since) = idle_since
                && since.elapsed() > timeout
            {
                tracing::info!(
                    "[LSP Pool] Evicting idle '{}' (PID {}), idle for {:?}",
                    entry.command,
                    entry.pid,
                    since.elapsed(),
                );
                to_remove.push(key.clone());
            }
        }

        for key in to_remove {
            entries.remove(&key);
        }
    }

    /// Spawn a new LSP process and set up stdin/stdout relay tasks.
    async fn spawn_pooled(
        command: &str,
        args: &[String],
        workspace_root: &str,
    ) -> anyhow::Result<Arc<LspProcessEntry>> {
        let mut cmd = Command::new(command);
        cmd.args(args);
        // Pooled servers outlive WebSocket sessions by design, but they
        // must not outlive the relay process itself. Without `kill_on_drop`,
        // dropping the wait task's `Child` on runtime shutdown leaks the
        // server as an orphan — and on Windows it also blocks shutdown
        // until the child exits, because dropping the handle unregisters
        // the process wait and only returns once it completes.
        let mut child = cmd
            .current_dir(workspace_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;

        let pid = child.id().unwrap_or(0);
        tracing::info!(
            "[LSP Pool] Spawned '{}' (PID {}) in workspace '{}', args={:?}",
            command,
            pid,
            workspace_root,
            args
        );

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to take stdin from child process"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to take stdout from child process"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to take stderr from child process"))?;

        let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<String>();
        let (stdout_tx, _) = broadcast::channel::<String>(256);

        // Background task: read from mpsc channel → write to child stdin
        let stdin_pid = pid;
        tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(msg) = stdin_rx.recv().await {
                let frame = format!("Content-Length: {}\r\n\r\n{}", msg.len(), msg);
                if stdin.write_all(frame.as_bytes()).await.is_err() {
                    break;
                }
                let _ = stdin.flush().await;
            }
            let _ = stdin.shutdown().await;
            tracing::info!("[LSP Pool] stdin writer ended for PID {}", stdin_pid);
        });

        // Background task: read LSP Base Protocol frames from stdout → broadcast
        let stdout_tx_clone = stdout_tx.clone();
        let stdout_cmd = command.to_string();
        let stdout_pid = pid;
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut content_length: usize = 0;
                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line).await {
                        Ok(0) => {
                            tracing::warn!(
                                "[LSP Pool] '{}' (PID {}) stdout closed (process exited)",
                                stdout_cmd,
                                stdout_pid
                            );
                            return;
                        }
                        Ok(_) => {
                            let trimmed = line.trim();
                            if trimmed.is_empty() {
                                break;
                            }
                            if let Some(len) = crate::protocol::parse_content_length(trimmed) {
                                content_length = len;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                "[LSP Pool] '{}' (PID {}) stdout read error: {}",
                                stdout_cmd,
                                stdout_pid,
                                e
                            );
                            return;
                        }
                    }
                }

                if content_length == 0 {
                    continue;
                }

                let mut body = vec![0u8; content_length];
                if reader.read_exact(&mut body).await.is_err() {
                    return;
                }

                if let Ok(msg) = String::from_utf8(body) {
                    let _ = stdout_tx_clone.send(msg);
                }
            }
        });

        // Background task: read LSP stderr line-by-line and log via tracing.
        let stderr_cmd = command.to_string();
        let stderr_pid = pid;
        tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        tracing::warn!(
                            "[LSP Pool] '{}' (PID {}) stderr: {}",
                            stderr_cmd,
                            stderr_pid,
                            line
                        );
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!(
                            "[LSP Pool] '{}' (PID {}) stderr read error: {}",
                            stderr_cmd,
                            stderr_pid,
                            e
                        );
                        break;
                    }
                }
            }
        });

        // Background task: wait for child exit (detect crash)
        let cmd_for_wait = command.to_string();
        tokio::spawn(async move {
            let status = child.wait().await;
            tracing::warn!(
                "[LSP Pool] '{}' (PID {}) exited: {:?}",
                cmd_for_wait,
                pid,
                status
            );
        });

        let entry = Arc::new(LspProcessEntry {
            stdin_tx,
            stdout_tx,
            active_clients: AtomicUsize::new(1),
            last_idle_since: Mutex::new(None),
            command: command.to_string(),
            workspace_root: workspace_root.to_string(),
            pid,
            init_result: Mutex::new(None),
        });

        Ok(entry)
    }

    /// Start a background reaper task that periodically evicts idle processes.
    pub fn start_reaper(pool: Arc<Self>) {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(REAPER_INTERVAL);
            loop {
                interval.tick().await;
                pool.reap_idle(DEFAULT_IDLE_TIMEOUT).await;
            }
        });
    }
}
