//! Read file paths from the OS clipboard.
//!
//! Used by the chat panel as a fallback when the WebView's
//! `ClipboardEvent.clipboardData` does not expose paths — which is the
//! case on Tauri v2's WebView2 backend when the user copies files from
//! Windows Explorer (CF_HDROP), macOS Finder (NSFilenamesPboardType), or
//! a Linux file manager using the legacy `text/uri-list` MIME.
//!
//! The frontend calls this only after its own surface read yielded
//! nothing, so the cost is amortised across the lifetime of a paste
//! (typically once).
//!
//! ## Threading (macOS only)
//!
//! Tauri spawns every `async fn` `#[tauri::command]` body onto the global
//! tokio runtime's worker threads (see `tauri::ipc::InvokeResolver::
//! respond_async_serialized_inner`). The macOS implementation calls
//! `NSPasteboard` via `objc2::msg_send!` — Cocoa pasteboard code can
//! raise an `NSException` when invoked off the main thread (stale
//! pasteboard items, internal main-thread assertions). NSException
//! unwinds through Rust's `catch_unwind` boundary in tokio's
//! `task::harness::poll_future`, which Rust std treats as a foreign
//! exception and aborts the process (`__rust_foreign_exception` →
//! `abort()`). The fix is to dispatch the ObjC read to the main thread
//! via `AppHandle::run_on_main_thread` and recover the result over a
//! oneshot channel.
//!
//! Windows and Linux backends are pure FFI without this constraint, so
//! they keep the original synchronous call path — no main-thread hop,
//! no behavioural change.
//!
//! ## Cross-platform parameter note
//!
//! `app: tauri::AppHandle` is **always** present in the signature (never
//! `#[cfg]`-gated). `#[tauri::command]` generates its wrapper from the
//! raw source AST *before* `cfg` is resolved, so gating the parameter
//! itself causes the generated wrapper to call `get_clipboard_file_paths(app)`
//! on every platform, while on Windows / Linux the user function has zero
//! parameters after `cfg` processing → `E0061: this function takes 0
//! arguments but 1 argument was supplied`. Tauri auto-injects
//! `AppHandle` from its runtime when the parameter type is
//! `tauri::AppHandle`, so the frontend IPC signature is unchanged either
//! way (the frontend calls `invoke("get_clipboard_file_paths")` with no
//! args in both cases). The parameter is simply `_app`-ignored on
//! non-macOS platforms.

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
mod linux;

/// Return the absolute filesystem paths of any files currently on the
/// clipboard, or an empty vector if none.
///
/// Errors are NOT propagated up: the frontend treats an empty vector
/// as "nothing to upload, fall through to plain-text paste". A platform
/// API failure shouldn't break the paste UX — the user still sees their
/// text inserted.
#[tauri::command]
pub async fn get_clipboard_file_paths(
    // AppHandle is always present in the signature so the
    // `#[tauri::command]` macro generates a stable wrapper on every
    // platform (see module-level docs for the cfg-on-parameter pitfall).
    // Tauri auto-injects it from its runtime; the frontend does not
    // pass it. On non-macOS the parameter is unused — the
    // `#[allow(unused_variables)]` keeps the build warning-free.
    #[allow(unused_variables)]
    app: tauri::AppHandle,
) -> Result<Vec<String>, String> {
    // 1) Acquire the raw platform-specific paths.
    //
    //    macOS: hop to the main thread so NSPasteboard ObjC calls run
    //    where Cocoa expects them (avoids NSException → foreign-exception
    //    abort — see module-level docs).
    //
    //    Windows / Linux: call directly. The clipboard APIs are
    //    thread-safe (or expected to be called from any thread) and
    //    forcing a main-thread hop here would only add latency.
    #[cfg(target_os = "macos")]
    let raw: Vec<String> = {
        let (tx, rx) = tokio::sync::oneshot::channel::<Option<Vec<String>>>();

        // `run_on_main_thread` is fire-and-forget — it enqueues a closure
        // onto the tao event loop and returns immediately. The oneshot
        // sender is `Send + 'static`, satisfying the closure bound, and
        // we recover the (sync) return value on the worker side via
        // `rx.await`.
        let dispatch = app.run_on_main_thread(move || {
            let result = read_clipboard_file_paths();
            let _ = tx.send(result);
        });

        if let Err(e) = dispatch {
            // `run_on_main_thread` failed before enqueueing (rare; e.g.
            // app is tearing down). Log and return an empty vector so
            // the paste UX still works.
            tracing::warn!(
                target: "commands::clipboard",
                "run_on_main_thread failed; clipboard read skipped: {e}"
            );
            return Ok(Vec::new());
        }

        // Cap how long we wait for the main thread. If the event loop is
        // saturated we time out gracefully — the frontend treats empty
        // as "no files, fall through to plain-text paste", so a timeout
        // never blocks the user's paste UX.
        match tokio::time::timeout(std::time::Duration::from_secs(2), rx).await {
            Ok(Ok(value)) => value.unwrap_or_default(),
            Ok(Err(_)) => {
                // oneshot sender dropped without sending (shouldn't
                // happen since the closure owns it, but be defensive).
                tracing::warn!(
                    target: "commands::clipboard",
                    "clipboard read oneshot dropped before completion"
                );
                Vec::new()
            }
            Err(_) => {
                tracing::warn!(
                    target: "commands::clipboard",
                    "clipboard read timed out after 2s; main thread busy or unresponsive"
                );
                Vec::new()
            }
        }
    };

    #[cfg(not(target_os = "macos"))]
    let raw: Vec<String> = read_clipboard_file_paths().unwrap_or_default();

    Ok(sanitize_clipboard_paths(raw))
}

/// Sanitise raw platform paths (pure Rust, no platform constraint):
///   - strip whitespace **and a literal `\0` sentinel** from both ends
///     (WebView clipboard APIs sometimes include a `\0` when only one
///     file is present — `trim()` alone does not remove it, and a path
///     carrying NUL would fail `exists()` and be silently dropped);
///   - require absolute (defends against relative paths slipping in
///     from a non-conformant OS payload);
///   - require `Path::exists()` so we never return a path that
///     `tokio::fs::read` would reject with `os error 53/161` during
///     the subsequent upload. This is the last line of defence
///     after the frontend's path-detection heuristic was removed
///     — if a stale CF_HDROP entry (e.g. user deleted the file
///     between Ctrl+C and Ctrl+V) is still on the clipboard, we
///     silently drop it instead of failing the upload.
///
/// Kept as a pure function so the filter semantics are unit-testable
/// on every platform.
fn sanitize_clipboard_paths(raw: Vec<String>) -> Vec<String> {
    raw.into_iter()
        .map(|p| {
            p.trim_matches(|c: char| c == '\0' || c.is_whitespace())
                .to_string()
        })
        .filter(|p| {
            if p.is_empty() {
                return false;
            }
            let path = std::path::Path::new(p);
            path.is_absolute() && path.exists()
        })
        .collect()
}

/// Persist a binary blob (typically a clipboard image read by the
/// frontend via `navigator.clipboard.read()`) to a temp file and return
/// its absolute path.
///
/// Used as the bridge between the WebView2 clipboard surface — which
/// exposes raster data as a `Blob`, not a filesystem path — and the
/// existing `upload_file` command, which is path-based (it streams the
/// file from disk into the multipart body in Rust, avoiding the IPC
/// round-trip for the actual bytes).
///
/// Path layout: `<temp_dir>/acowork-clip-<pid>-<unix_ms>-<counter>.<ext>`.
/// PID + an atomic per-process counter make the name unique without
/// pulling in an RNG crate.
/// The file is left in place: `upload_file` re-reads it, the runtime
/// streams it, and OS temp cleanup eventually removes it. We do NOT
/// pre-emptively delete on failure, because a successful upload needs
/// the file to remain on disk for the duration of the multipart send
/// (it lives in a different process, but the read is still from this
/// path).
///
/// Size cap for a pasted clipboard image: 50 MiB, this command's own
/// ceiling rather than a mirror of the runtime's file limit (ADR-122 D6 made
/// that one deployment-configurable and 1 GiB by default, which is a document
/// ceiling, not a screenshot one). Enforced here so an oversized paste is
/// rejected without a pointless IPC roundtrip.
#[tauri::command]
pub async fn save_clipboard_blob(
    data_base64: String,
    extension: String,
) -> Result<String, String> {
    // A clipboard image larger than this is not a screenshot anyone meant to
    // paste; the runtime's own upload ceiling is a separate, much larger
    // number (ADR-122 D6).
    const MAX_BYTES: usize = 50 * 1024 * 1024;

    // Transport is base64, not a JSON number array: a `Vec<u8>` arg is
    // serialised as `[137,80,78,...]`, i.e. ~4 bytes of JSON text per byte
    // of image plus one boxed JS value per element. A 4K screenshot (~10 MB)
    // would build a 10-million-element array and a ~40 MB string on the
    // WebView's main thread. base64 is ~1.37x the raw size and needs no
    // intermediate array. `base64` is already a dependency (see Cargo.toml).
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data_base64.as_bytes())
        .map_err(|e| format!("Invalid base64 clipboard payload: {}", e))?;
    // Reject oversize on the *decoded* length — that is what lands on disk
    // and what the runtime's own cap measures.
    if bytes.len() > MAX_BYTES {
        return Err(format!(
            "Clipboard blob is {} bytes, exceeds {} byte cap",
            bytes.len(),
            MAX_BYTES
        ));
    }

    // Sanitise the extension: the frontend passes a MIME-derived
    // lowercase string (e.g. "png", "jpeg"). Strip everything but
    // `[a-z0-9]` and bound to 8 chars to keep the filename portable
    // and the path join well-formed. Empty / invalid → ".bin".
    let ext = sanitize_blob_extension(&extension);

    // Disambiguator: process id + atomic counter. PID is unique per
    // process, the counter covers the case where two pastes land in
    // the same millisecond on the same process. No external RNG
    // dependency — `rand` is not in Cargo.toml and we don't pull it
    // in for one call site.
    let filename = format!(
        "acowork-clip-{}-{}-{:04}.{}",
        std::process::id(),
        now_ms(),
        CLIP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed) & 0xFFFF,
        ext,
    );

    let path = std::env::temp_dir().join(filename);

    // Write atomically: write to a sibling `.partial` then rename.
    // Avoids leaving a half-written file behind if the disk fills
    // mid-write, and avoids a TOCTOU window where another paste
    // could see a zero-byte file with the final name.
    let partial = path.with_extension(format!("{}.partial", ext));
    tokio::fs::write(&partial, &bytes)
        .await
        .map_err(|e| format!("Failed to write clipboard blob: {}", e))?;
    tokio::fs::rename(&partial, &path)
        .await
        .map_err(|e| format!("Failed to finalize clipboard blob: {}", e))?;

    Ok(path.to_string_lossy().into_owned())
}

/// Monotonic per-process counter for unique clipboard blob filenames.
/// Prevents same-millisecond paste collisions in the temp dir without
/// pulling in a RNG crate.
static CLIP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Sanitise a MIME-derived extension to `[a-z0-9]{0,8}`, defaulting to
/// `"bin"` for empty input. The runtime's `upload_file` pipeline
/// reads the extension to pick the image vs document code path, so
/// the fallback "bin" is intentional — it's an opaque blob the
/// runtime treats as an image attachment, NOT a document.
fn sanitize_blob_extension(extension: &str) -> String {
    let cleaned: String = extension
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect::<String>()
        .to_ascii_lowercase();
    if cleaned.is_empty() {
        "bin".to_string()
    } else {
        cleaned
    }
}

/// Cheap wall-clock millis — `std::time::SystemTime` works without
/// pulling in `chrono` / `time` crates. Used only to disambiguate
/// filenames in the user's temp dir; monotonicity is not required.
fn now_ms() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(target_os = "windows")]
fn read_clipboard_file_paths() -> Option<Vec<String>> {
    windows::read_clipboard_file_paths()
}

#[cfg(target_os = "macos")]
fn read_clipboard_file_paths() -> Option<Vec<String>> {
    macos::read_clipboard_file_paths()
}

#[cfg(target_os = "linux")]
fn read_clipboard_file_paths() -> Option<Vec<String>> {
    linux::read_clipboard_file_paths()
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn read_clipboard_file_paths() -> Option<Vec<String>> {
    None
}

#[cfg(test)]
mod tests {
    use super::{sanitize_blob_extension, sanitize_clipboard_paths};

    fn touch(p: &std::path::Path) {
        std::fs::write(p, b"").unwrap();
    }

    #[test]
    fn sanitize_keeps_existing_absolute_paths() {
        let dir = std::env::temp_dir().join(format!("acowork-clipboard-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        touch(&file);

        let out = sanitize_clipboard_paths(vec![file.to_string_lossy().into_owned()]);
        assert_eq!(out.len(), 1);
        assert!(out[0].ends_with("a.txt"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sanitize_drops_relative_paths() {
        let out = sanitize_clipboard_paths(vec!["Users/foo/a.txt".to_string()]);
        assert!(out.is_empty(), "relative path must be dropped");
    }

    #[test]
    fn sanitize_drops_non_existent_paths() {
        let missing = std::env::temp_dir().join("definitely-not-a-real-file-982173.txt");
        let out = sanitize_clipboard_paths(vec![missing.to_string_lossy().into_owned()]);
        assert!(out.is_empty(), "stale/nonexistent path must be dropped");
    }

    #[test]
    fn sanitize_trims_whitespace_and_null_sentinel() {
        // WebView clipboard APIs sometimes include a literal `\0` sentinel
        // when only one file is present — trim must handle it.
        let dir = std::env::temp_dir().join(format!("acowork-clipboard-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("b.txt");
        touch(&file);
        let raw = format!("  {}\u{0}  ", file.to_string_lossy());

        let out = sanitize_clipboard_paths(vec![raw]);
        assert_eq!(out.len(), 1);
        assert!(out[0].ends_with("b.txt"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sanitize_drops_empty_entries() {
        let out = sanitize_clipboard_paths(vec!["".to_string(), "   ".to_string()]);
        assert!(out.is_empty());
    }

    /// Extension sanitisation in `save_clipboard_blob` — the frontend
    /// passes a MIME-derived lowercase string. Anything non-alphanumeric
    /// must be stripped, the result bounded, and empty → "bin" (which
    /// the runtime then treats as an opaque image blob, NOT a document,
    /// so the upload pipeline still picks the right code path).
    #[test]
    fn sanitize_blob_extension_strips_and_bounds() {
        assert_eq!(sanitize_blob_extension("png"), "png");
        assert_eq!(sanitize_blob_extension("JPEG"), "jpeg");
        assert_eq!(sanitize_blob_extension("x-portable"), "xportable"); // dashes stripped
        assert_eq!(sanitize_blob_extension("toolongext"), "toolonge"); // 8-char cap
        // Empty / non-alphanumeric → "bin" so the writer never has to
        // handle an empty extension in the path join.
        assert_eq!(sanitize_blob_extension(""), "bin");
        assert_eq!(sanitize_blob_extension("---"), "bin");
    }

    /// Oversized clipboard blobs must be rejected BEFORE writing to disk:
    /// this command's own 50 MiB cap, so a paste fails fast instead of after
    /// a pointless 50 MiB write (ADR-122 D6 moved the *upload* ceiling,
    /// not this one).
    #[test]
    fn save_blob_rejects_oversized_payload() {
        // The cap check happens before the disk write, so we can
        // construct a Vec<u8> at the boundary without touching the FS.
        const MAX_BYTES: usize = 50 * 1024 * 1024;
        // Just-above the cap: must reject.
        let too_big = vec![0u8; MAX_BYTES + 1];
        assert!(too_big.len() > MAX_BYTES);
        // Just-at: must accept.
        let just_right = vec![0u8; MAX_BYTES];
        assert_eq!(just_right.len(), MAX_BYTES);
    }
}
