//! Logging utilities: size-based rolling file appender, shared tracing timer,
//! and global panic hook.
//!
//! Used by both Gateway and Agent Runtime for consistent log file naming
//! (YYYYMMDD_HHMMSS.log) and auto-split behaviour.

use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A `FormatTime` implementation that uses `chrono::Local` to produce
/// RFC-3339 local-time timestamps (e.g. `2026-06-13T15:30:00.123+08:00`).
///
/// This avoids the `tracing-subscriber` `local-time` feature, which pulls
/// in the `time` crate and triggers an E0119 coherence conflict with
/// `EnvFilter`'s blanket `From<S: AsRef<str>>` impl.
#[derive(Default, Clone, Copy)]
pub struct ChronoLocalTimer;

impl std::fmt::Display for ChronoLocalTimer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", chrono::Local::now().to_rfc3339())
    }
}

impl tracing_subscriber::fmt::time::FormatTime for ChronoLocalTimer {
    fn format_time(&self, w: &mut tracing_subscriber::fmt::format::Writer<'_>) -> std::fmt::Result {
        write!(
            w,
            "{}",
            chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f%:z")
        )
    }
}

/// A file appender that auto-splits when the current log file exceeds a size limit.
/// Log files are named `YYYYMMDD_HHMMSS.log` using the creation timestamp.
pub struct SizeRollingFileAppender {
    dir: std::path::PathBuf,
    max_bytes: u64,
    max_file_count: AtomicUsize,
    inner: Mutex<AppenderInner>,
}

struct AppenderInner {
    file: std::fs::File,
    current_path: std::path::PathBuf,
    current_size: u64,
}

impl SizeRollingFileAppender {
    /// Create a new rolling file appender.
    ///
    /// `max_mb` — max file size in MB before rolling to a new file.
    /// `max_count` — maximum number of log files to keep (0 = unlimited).
    /// The initial file is named `YYYYMMDD_HHMMSS.log` based on current time.
    ///
    /// Creates `dir` (and any missing parents) if it does not already exist,
    /// so callers can point at a freshly-installed workspace without a prior
    /// bootstrap step.
    ///
    /// Returns `Err(io::Error)` if neither `OpenOptions::create+append` nor
    /// `File::create` can produce the initial log file. Callers are expected
    /// to fall back to a stderr-only subscriber rather than panic, so that
    /// transient filesystem failures (e.g. sandbox EPERM, full disk, missing
    /// parent directory AFTER the create_dir_all call) do not abort process
    /// startup.
    pub fn new(
        dir: std::path::PathBuf,
        max_mb: u64,
        max_count: usize,
    ) -> std::io::Result<Self> {
        let max_bytes = max_mb * 1024 * 1024;
        std::fs::create_dir_all(&dir)?;
        let now = chrono::Local::now();
        let filename = format!("{}.log", now.format("%Y%m%d_%H%M%S"));
        let path = dir.join(&filename);
        // First try create+append (preserves prior content); if the open fails
        // (e.g. file is exclusively locked or append-only flag set), fall back
        // to File::create which truncates. Both errors propagate so the caller
        // can switch to a stderr-only subscriber instead of crashing.
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .or_else(|_| std::fs::File::create(&path))?;
        let current_size = file.metadata().map(|m| m.len()).unwrap_or(0);

        let appender = Self {
            dir,
            max_bytes,
            max_file_count: AtomicUsize::new(max_count),
            inner: Mutex::new(AppenderInner {
                file,
                current_path: path,
                current_size,
            }),
        };
        appender.enforce_max_file_count();
        Ok(appender)
    }

    /// Create a new log file with a fresh timestamp name.
    fn roll(&self, inner: &mut AppenderInner) {
        let now = chrono::Local::now();
        let filename = format!("{}.log", now.format("%Y%m%d_%H%M%S"));
        let path = self.dir.join(&filename);
        match std::fs::File::create(&path) {
            Ok(file) => {
                inner.file = file;
                inner.current_path = path;
                inner.current_size = 0;
                // After rolling to a new file, enforce max file count
                let _ = inner;
                self.enforce_max_file_count();
            }
            Err(e) => {
                eprintln!("WARN: failed to create new log file {:?}: {}", path, e);
            }
        }
    }

    /// Enforce the maximum number of log files.
    /// When the number of `*.log` files exceeds `max_file_count`, delete the
    /// oldest files (sorted by filename, which is timestamp-based) to maintain
    /// the limit. No-op when `max_file_count == 0`.
    fn enforce_max_file_count(&self) {
        let max = self.max_file_count.load(Ordering::Relaxed);
        if max == 0 {
            return;
        }

        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };

        let mut log_files: Vec<std::path::PathBuf> = entries
            .flatten()
            .filter_map(|e| {
                let path = e.path();
                if path.extension().is_some_and(|ext| ext == "log") {
                    Some(path)
                } else {
                    None
                }
            })
            .collect();

        if log_files.len() <= max {
            return;
        }

        // Sort by filename (YYYYMMDD_HHMMSS.log — lexicographic = chronological)
        log_files.sort();

        // Delete the oldest files, keeping the newest `max` files
        let to_remove = log_files.len() - max;
        for path in log_files.iter().take(to_remove) {
            if let Err(e) = std::fs::remove_file(path) {
                eprintln!("WARN: failed to delete old log file {:?}: {}", path, e);
            }
        }
    }

    /// Force immediate rotation: close current log file and open a new one.
    /// Called by the Runtime when Gateway requests log cleanup via gRPC.
    /// The caller should delete old *.log files before calling this.
    pub fn force_rotate(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        self.roll(&mut inner);
    }

    /// Dynamically update the maximum number of log files to keep.
    /// Immediately enforces the new limit by deleting the oldest files
    /// when the current count exceeds the new maximum.
    pub fn set_max_file_count(&self, count: usize) {
        self.max_file_count.store(count, Ordering::Relaxed);
        self.enforce_max_file_count();
    }
}

impl Write for &SizeRollingFileAppender {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.current_size >= self.max_bytes {
            self.roll(&mut inner);
        }
        let n = inner.file.write(buf)?;
        inner.current_size += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .file
            .flush()
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SizeRollingFileAppender {
    type Writer = &'a SizeRollingFileAppender;

    fn make_writer(&'a self) -> Self::Writer {
        self
    }

    fn make_writer_for(&'a self, _meta: &tracing::Metadata<'_>) -> Self::Writer {
        self
    }
}

/// Build an `EnvFilter` for a runtime/gateway process.
///
/// Starts from `RUST_LOG` if set, otherwise from `base_level`, then appends
/// targeted directives that QUIET noisy third-party crates. Without these,
/// `rumqttc` logs every MQTT PUBLISH frame at DEBUG and `rustls` logs every
/// handshake/alert frame — hundreds of lines per active LLM turn that drown
/// out our own `acowork_runtime::*` INFO lines (PERF-001 / LOG-001).
///
/// Directives are only added when the crate emits high-frequency, low-value
/// frames — the app's own crates are left untouched so their INFO/DEBUG
/// level follows `base_level`.
pub fn build_env_filter(base_level: &str) -> tracing_subscriber::EnvFilter {
    use tracing_subscriber::EnvFilter;
    let mut filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(base_level));
    // MQTT frame trace (rumqttc::state logs a `Publish.` line per frame).
    filter = filter.add_directive("rumqttc=warn".parse().expect("valid directive"));
    // Embedded broker (rumqttd) logs every route/ack/forward at DEBUG —
    // with 5 s node presence + agent status heartbeats that is hundreds
    // of lines per minute and drowns out Gateway WARN/ERROR (LOG-001).
    // off: even ERROR ("Duplicate client_id") floods when a remote node
    // MQTT-flaps; the Gateway's own dispatch logs cover the topology.
    filter = filter.add_directive("rumqttd=off".parse().expect("valid directive"));
    // TLS handshake/alert frame trace.
    filter = filter.add_directive("rustls=warn".parse().expect("valid directive"));
    // reqwest/hyper connection-level traces are per-request noise.
    filter = filter.add_directive("hyper=warn".parse().expect("valid directive"));
    filter = filter.add_directive("reqwest=warn".parse().expect("valid directive"));
    filter
}

/// The level the `log` facade should be opened to for a process whose
/// tracing filter comes from `base_level` (or from `RUST_LOG`, if given).
///
/// Split out from [`sync_log_facade`] so the mapping is testable without
/// touching `log`'s global state.
fn log_facade_level(base_level: &str, rust_log: Option<&str>) -> log::LevelFilter {
    // A target-scoped `RUST_LOG` (`acowork_relay=info,yamux=trace`) has no
    // single equivalent level, so open the facade fully and let the
    // `EnvFilter` decide per target.
    match rust_log {
        Some(_) => log::LevelFilter::Trace,
        None => base_level
            .parse()
            .unwrap_or(log::LevelFilter::Info),
    }
}

/// Raise the `log` crate's runtime max level to match the tracing filter
/// being installed. Call next to every subscriber init.
///
/// `log`'s gate starts at OFF and is only opened by `log` backends
/// (env_logger) or by `tracing_subscriber::reload` when a filter is
/// reloaded. A process that installs a plain subscriber therefore loses
/// every record from `log`-only dependencies before tracing ever sees them:
/// the relay logged nothing at all from yamux — no stream `eof`, no
/// `can no longer write`, no ping/pong RTT — which is precisely the
/// evidence a wedged tunnel stream leaves behind. `rumqttc` is `log`-based
/// too, so this is not relay-specific.
pub fn sync_log_facade(base_level: &str) {
    let rust_log = std::env::var_os("RUST_LOG");
    let level = log_facade_level(base_level, rust_log.as_deref().and_then(|s| s.to_str()));
    log::set_max_level(level);
}

/// Initialize tracing to write to stderr.
///
/// This is intended for subprocesses (embed, lsp-relay) whose stdout is
/// consumed by the parent (Gateway) as protocol data, while stderr is
/// redirected to a log file by the Gateway's spawn logic.
///
/// Usage in subprocess `main()`:
/// ```ignore
/// acowork_core::logging::init_subprocess_logging(&cli.log_level);
/// ```
pub fn init_subprocess_logging(level: &str) {
    let filter = build_env_filter(level);
    sync_log_facade(level);

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_thread_ids(false)
        .init();
}

/// Install a global panic hook that routes panic information into the tracing
/// log system (and thus into both stderr and the rolling log file).
///
/// This MUST be called **after** the tracing subscriber has been initialized
/// (i.e. after `init_tracing` / `init_logging`), otherwise the panic message
/// will be lost because the subscriber isn't ready yet.
///
/// With this hook installed, every panic — including those inside
/// `tokio::spawn` tasks that would otherwise be silently swallowed — produces
/// an `ERROR`-level tracing event with the panic payload and location.
pub fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Extract a human-readable payload string.
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(|s| s.as_str()))
            .unwrap_or("<non-string panic payload>");

        // Emit to tracing so it appears in both stderr and the log file.
        tracing::error!(
            panic.payload = %payload,
            panic.location = %info
                .location()
                .map(|loc| format!("{}:{}:{}", loc.file(), loc.line(), loc.column()))
                .unwrap_or_else(|| "<unknown location>".to_string()),
            "PANIC"
        );

        // Also call the default hook so the panic still prints to stderr
        // (useful for terminal visibility and for the OS crash reporter).
        default_hook(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::log_facade_level;

    #[test]
    fn facade_follows_the_base_level() {
        assert_eq!(log_facade_level("debug", None), log::LevelFilter::Debug);
        assert_eq!(log_facade_level("info", None), log::LevelFilter::Info);
        assert_eq!(log_facade_level("trace", None), log::LevelFilter::Trace);
        // An unparsable level must never land on OFF — that is the exact
        // silence this function exists to remove.
        assert_eq!(log_facade_level("verbose", None), log::LevelFilter::Info);
        assert_eq!(log_facade_level("", None), log::LevelFilter::Info);
    }

    #[test]
    fn rust_log_opens_the_facade_fully() {
        // A target-scoped filter has no single equivalent level; the
        // EnvFilter installed next to this call does the per-target work.
        assert_eq!(
            log_facade_level("info", Some("acowork_relay=info,yamux=trace")),
            log::LevelFilter::Trace
        );
    }
}
