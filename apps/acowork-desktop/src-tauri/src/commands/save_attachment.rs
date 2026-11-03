//! Download an inbox chat attachment to a user-chosen path on disk.
//!
//! Tauri's WebView (WebView2 / WebKit) does not implement the browser
//! download protocol — `Content-Disposition: attachment` headers and
//! `<a download>.click()` are silently dropped, so the inbox's
//! "download attachment" button needs an explicit save path through the
//! OS save dialog (`tauri-plugin-dialog`).
//!
//! **Rust owns the transfer.** The bytes used to be fetched in the
//! webview and handed over as an `invoke` argument, which serialized the
//! file into a JSON *array of decimal numbers* (see Tauri's
//! `process-ipc-message-fn.js`: a named `Uint8Array` arg is not the raw
//! channel, only a top-level `ArrayBuffer` is). For a multi-MB
//! attachment that froze the webview main thread for seconds while it
//! built the string and then never came back — the `invoke` promise
//! never settled. So the command fetches the attachment itself through
//! [`GatewayClient`] (which carries the account token and replays once
//! after a transparent renewal) and streams it straight to the target
//! file, leaving only two short strings on the IPC wire.
//!
//! Byte counts are pushed back through a [`Channel`] as they reach the
//! disk, so the button can show real progress instead of a spinner that
//! tells the user nothing about a slow link.
//!
//! The path is treated as user-chosen and trusted: the dialog returns
//! whatever the OS file picker produced, and the user just confirmed it
//! by clicking Save. We still:
//!   - refuse empty paths (defensive — the dialog disallows it, but
//!     the IPC contract does not),
//!   - create missing parent directories so a freshly-picked folder
//!     works (rare but cheap to support),
//!   - reject directory paths (the dialog disallows selecting a
//!     folder, but a stale IPC call could still send one),
//!   - report I/O and HTTP errors as plain strings the frontend can toast.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use tauri::ipc::Channel;
use tauri::State;

use crate::gateway_client::GatewayClient;
use crate::state::AppState;

/// Total-request ceiling for one attachment download. The shared
/// [`GatewayClient`] is built with a 10 s timeout (fine for the small
/// JSON API calls it mostly serves); a file transfer needs far more, so
/// we override it per request. 10 min is a deliberately generous ceiling
/// for a remote Gateway on a thin link — a *stalled* socket still fails
/// instead of hanging the button forever.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);

#[tauri::command]
pub async fn download_attachment(
    state: State<'_, AppState>,
    url: String,
    path: String,
    on_progress: Channel<u64>,
) -> Result<(), String> {
    let client = state.gateway.read().await;
    download_to_file(&client, &url, &path, &on_progress).await
}

/// GET `url` with the account credentials attached and write the body to
/// `path` (creating missing parent directories), reporting bytes written
/// to `on_progress` as they land.
///
/// The body is streamed chunk by chunk straight to disk, so peak memory
/// is one chunk regardless of the attachment size.
///
/// Free function so the test below can drive it against a throwaway
/// local HTTP server instead of a live Gateway.
async fn download_to_file(
    client: &GatewayClient,
    url: &str,
    path: &str,
    on_progress: &Channel<u64>,
) -> Result<(), String> {
    let path_buf = prepare_target(path)?;

    let mut resp = client
        .send(|| {
            Ok(client
                .request(reqwest::Method::GET, url)
                .timeout(DOWNLOAD_TIMEOUT))
        })
        .await
        .map_err(|e| format!("download {}: {}", url, e))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("download {}: HTTP {} {}", url, status, body));
    }

    // Report roughly every 1% so a fast localhost download doesn't push one
    // channel message per reqwest chunk (8-64 KiB each).
    //
    // The Gateway serves attachments with `Body::from_stream` and therefore
    // *no* `Content-Length` (chunked on purpose — see
    // `chat_api.rs::download_attachment`), so the 128 KiB fallback below is
    // what actually runs: ~800 messages for a 100 MB attachment. That is
    // cheap (a small count goes through a direct `eval`, not a fetch round
    // trip) and keeps the bar moving on a slow link, which is the only case
    // where progress is worth showing at all.
    let step = resp
        .content_length()
        .map(|total| (total / 100).max(1))
        .unwrap_or(128 << 10);
    let mut written: u64 = 0;
    let mut reported: u64 = 0;

    let mut file = fs::File::create(&path_buf).map_err(|e| format!("create {}: {}", path, e))?;
    let streamed = async {
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| format!("download {}: {}", url, e))?
        {
            file.write_all(&chunk)
                .map_err(|e| format!("write {}: {}", path, e))?;
            written += chunk.len() as u64;
            if written - reported >= step {
                reported = written;
                // A closed channel (view unmounted mid-download) must not
                // abort the transfer.
                let _ = on_progress.send(written);
            }
        }
        Ok(())
    }
    .await;

    if let Err(e) = streamed {
        // Never leave a half-written file behind that looks like a
        // complete download.
        drop(file);
        let _ = fs::remove_file(&path_buf);
        return Err(e);
    }
    let _ = on_progress.send(written);
    Ok(())
}

/// Validate the dialog-provided path and make sure its parent exists.
fn prepare_target(path: &str) -> Result<PathBuf, String> {
    if path.trim().is_empty() {
        return Err("save path is empty".into());
    }

    let path_buf = PathBuf::from(path);

    // The dialog cannot pick a directory, but a parent that happens to
    // be a regular file would fail the write with a confusing OS error.
    // Surface it explicitly so the toast tells the user something
    // useful.
    if path_buf.exists() && !path_buf.is_file() {
        return Err(format!("not a regular file: {}", path));
    }

    if let Some(parent) = path_buf.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        fs::create_dir_all(parent)
            .map_err(|e| format!("create parent dir {}: {}", parent.display(), e))?;
    }

    Ok(path_buf)
}

// ── "Open externally" for workspace preview files ─────────────────────────
//
// The in-app preview renderers (PDF iframe, SheetJS, docx-preview, the
// pptx card fallback) cannot cover every file the user opens — legacy
// .doc/.ppt, encrypted documents, or simply "show me this in a real app".
// The workspace file lives on the Gateway/Node machine, not on this one,
// so there is no local path to hand the OS: the bytes must first come
// over via the same authenticated download as "save as", land in a temp
// folder, and only then open with the default application.

/// Folder holding the transient copies `open_file_externally` launches.
/// A fixed name under the OS temp dir so repeat opens of the same file
/// overwrite instead of piling up copies.
fn external_preview_dir() -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join("acowork-external-preview");
    fs::create_dir_all(&dir)
        .map_err(|e| format!("create preview temp dir {}: {}", dir.display(), e))?;
    Ok(dir)
}

/// Reduce a webview-supplied filename to one safe file name: last path
/// component only (kills any directory traversal), empty/`.`/`..` and
/// embedded NULs rejected, dots kept (extensions matter to the OS
/// launcher). Unicode names pass through untouched.
fn sanitize_external_name(filename: &str) -> Result<String, String> {
    let last = filename
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .trim()
        .replace('\0', "");
    if last.is_empty() || last == "." || last == ".." {
        return Err(format!("invalid file name: {filename:?}"));
    }
    Ok(last)
}

/// Launch `path` with the OS default application.
#[allow(unused_variables)] // platform-specific backends consume `path`
fn spawn_open(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    {
        // `explorer.exe <file>` opens the file with its associated app
        // (it is NOT the `/select,` reveal form from commands::reveal —
        // plain arg quoting is fine for this invocation).
        std::process::Command::new("explorer.exe").arg(path).spawn()?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(path).spawn()?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open").arg(path).spawn()?;
    }
    Ok(())
}

/// Download `url` into the temp preview folder under `filename` (with
/// account credentials, streamed) and open it with the default app.
/// Powers the document viewers' "open externally" button.
#[tauri::command]
pub async fn open_file_externally(
    state: State<'_, AppState>,
    url: String,
    filename: String,
) -> Result<(), String> {
    let name = sanitize_external_name(&filename)?;
    let target = external_preview_dir()?.join(name);
    let client = state.gateway.read().await;
    // Progress goes nowhere: the download is a click-away step before the
    // OS launch, and a stalled socket still dies at DOWNLOAD_TIMEOUT.
    let quiet = Channel::new(|_| Ok(()));
    download_to_file(&client, &url, &target.to_string_lossy(), &quiet).await?;
    spawn_open(&target).map_err(|e| format!("open {}: {}", target.display(), e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use tauri::ipc::InvokeResponseBody;

    /// Serve one canned HTTP/1.1 response on a throwaway port and return
    /// its base URL. The connection is closed after the reply so
    /// `Connection: close` semantics hold without a keep-alive dance.
    fn spawn_one_shot_server(status_line: &'static str, body: &'static [u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            let mut scratch = [0u8; 1024];
            let _ = sock.read(&mut scratch);
            let head = format!(
                "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                status_line,
                body.len(),
            );
            let _ = sock.write_all(head.as_bytes());
            let _ = sock.write_all(body);
            let _ = sock.flush();
        });
        format!("http://{}", addr)
    }

    fn temp_target(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("acowork-download-attachment-test");
        fs::create_dir_all(&dir).expect("temp dir");
        dir.join(name)
    }

    /// A `Channel` that records every byte count the command reports, the
    /// same way the webview receives them (as JSON numbers).
    fn progress_recorder() -> (Channel<u64>, Arc<Mutex<Vec<u64>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let channel = Channel::new(move |body| {
            if let InvokeResponseBody::Json(json) = body {
                sink.lock().unwrap().push(json.parse().expect("byte count"));
            }
            Ok(())
        });
        (channel, seen)
    }

    #[tokio::test]
    async fn writes_served_bytes_to_the_chosen_path() {
        let payload: &[u8] = b"attachment-payload-0123456789";
        let base = spawn_one_shot_server("200 OK", payload);
        let target = temp_target("ok.bin");
        let _ = fs::remove_file(&target);
        let (progress, reported) = progress_recorder();

        download_to_file(
            &GatewayClient::new(),
            &format!("{base}/api/users/u/chats/c/files/f"),
            target.to_str().unwrap(),
            &progress,
        )
        .await
        .expect("download must succeed");

        assert_eq!(fs::read(&target).expect("file written"), payload);
        assert_eq!(
            *reported.lock().unwrap().last().expect("progress reported"),
            payload.len() as u64,
            "last reported byte count must be the file size"
        );
        let _ = fs::remove_file(&target);
    }

    #[tokio::test]
    async fn http_error_is_reported_and_writes_nothing() {
        let base = spawn_one_shot_server("404 Not Found", b"no such attachment");
        let target = temp_target("missing.bin");
        let _ = fs::remove_file(&target);
        let (progress, reported) = progress_recorder();

        let err = download_to_file(&GatewayClient::new(), &base, target.to_str().unwrap(), &progress)
            .await
            .expect_err("404 must fail");

        assert!(err.contains("404"), "error should name the status: {err}");
        assert!(!target.exists(), "nothing may be written on failure");
        assert!(reported.lock().unwrap().is_empty(), "no progress on failure");
    }

    #[test]
    fn empty_path_is_rejected() {
        assert!(prepare_target("   ").is_err());
    }

    #[test]
    fn sanitize_external_name_keeps_basenames_and_kills_traversal() {
        assert_eq!(sanitize_external_name("报告 Q1.pdf").unwrap(), "报告 Q1.pdf");
        assert_eq!(
            sanitize_external_name(r"C:\ws\sub\deck.pptx").unwrap(),
            "deck.pptx"
        );
        assert_eq!(sanitize_external_name("a/b/c.xlsx").unwrap(), "c.xlsx");
        assert!(sanitize_external_name("").is_err());
        assert!(sanitize_external_name("   ").is_err());
        assert!(sanitize_external_name("..").is_err());
        assert!(sanitize_external_name("a/../").is_err());
        // A trailing separator yields an empty last component — rejected,
        // never silently glued into the temp dir root.
        assert!(sanitize_external_name("x/").is_err());
    }
}
