//! Office-document → PDF conversion for high-fidelity workspace preview.
//!
//! Why this exists: the in-app JS renderers (docx-preview, SheetJS, the
//! pptx card) re-layout OOXML into HTML with a hand-rolled engine — they
//! show content, not the final printed layout the user actually reviews
//! after an Agent edits a document. The industry-standard answer is to
//! run a real typesetting engine and render its PDF output. The engine
//! here is the user's OWN machine software (LibreOffice, or Microsoft
//! Office via COM on Windows) — nothing is bundled, nothing is uploaded
//! to any online viewer. The file bytes travel the same authenticated
//! Gateway route every preview uses, land in a local temp folder, and
//! the converted PDF never leaves the machine.
//!
//! Commands:
//!   - `detect_doc_convert_engine` — what engine (if any) is available.
//!     Cached process-wide; the frontend gates the whole PDF path on it
//!     and falls back to the JS viewers when it answers `none`.
//!   - `convert_workspace_file_to_pdf` — download → convert → return the
//!     PDF bytes over raw IPC (same "URL crosses the bridge, not the
//!     bytes" rule as `download_attachment`; here the response direction
//!     is raw too, so no JSON number-array serialization).
//!
//! Conversion results are cached on disk under the app cache dir, keyed
//! by file name + size + ETag (the raw route's `W/"{size}-{mtime}"`), so
//! reopening a preview is instant and an edited file (new ETag, and the
//! editor store bumps `previewSeq` on fs-watch events) re-converts.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use tauri::State;

use crate::gateway_client::GatewayClient;
use crate::state::AppState;

/// Ceiling for one source-file download (same spirit as
/// `download_attachment::DOWNLOAD_TIMEOUT` — a remote node on a thin
/// link needs far more than the client's default 10 s).
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);
/// LibreOffice cold start + layout of a big document. A stalled
/// conversion dies instead of hanging the tab forever.
const LIBREOFFICE_TIMEOUT: Duration = Duration::from_secs(120);
/// Office COM spins up the full desktop app — slower to first byte.
const COM_TIMEOUT: Duration = Duration::from_secs(180);

// ── Engine model ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EngineKind {
    Libreoffice,
    MsOfficeCom,
    None,
}

/// Wire shape for the frontend (`detect_doc_convert_engine`). `path` is
/// diagnostic only (soffice executable / registry hit).
#[derive(Debug, Clone, Serialize)]
pub struct EngineInfo {
    pub engine: EngineKind,
    pub path: Option<String>,
}

static ENGINE_CACHE: Mutex<Option<EngineInfo>> = Mutex::new(None);

/// Test hook: forget the cached probe so a test can drive detection
/// against a stubbed environment.
#[cfg(test)]
fn reset_engine_cache() {
    *ENGINE_CACHE.lock().unwrap() = None;
}

fn cached_engine<F>(probe: F) -> EngineInfo
where
    F: FnOnce() -> EngineInfo,
{
    if let Some(info) = ENGINE_CACHE.lock().unwrap().as_ref() {
        return info.clone();
    }
    // A probe panic must not poison the cache — fall back to `none`.
    let info = probe();
    *ENGINE_CACHE.lock().unwrap() = Some(info.clone());
    info
}

/// Locate the LibreOffice `soffice` binary: the well-known install dirs
/// first (the registry Uninstall tree is scattered across per-user
/// hives), then PATH.
fn find_soffice() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let mut candidates: Vec<PathBuf> = Vec::new();
        for env_key in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
            if let Ok(root) = std::env::var(env_key) {
                // Current layout ("LibreOffice") and the old versioned
                // one ("LibreOffice 7.6") both occur in the wild — the
                // read_dir pass below enumerates the versioned ones.
                candidates.push(
                    PathBuf::from(root).join("LibreOffice").join("program").join("soffice.exe"),
                );
            }
        }
        // Versioned folders (LibreOffice 24.8, 7.6, …) — enumerate the
        // Program Files dir instead of guessing numbers.
        if let Ok(pf) = std::env::var("ProgramFiles")
            && let Ok(entries) = fs::read_dir(&pf)
        {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_lowercase();
                if name.starts_with("libreoffice") {
                    candidates.push(
                        entry.path().join("program").join("soffice.exe"),
                    );
                }
            }
        }
        for c in &candidates {
            if c.exists() {
                return Some(c.clone());
            }
        }
        // PATH lookup as the last resort.
        which_on_path("soffice.exe").or_else(|| which_on_path("soffice"))
    }
    #[cfg(target_os = "macos")]
    {
        let app = PathBuf::from("/Applications/LibreOffice.app/Contents/MacOS/soffice");
        if app.exists() {
            return Some(app);
        }
        return which_on_path("soffice");
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        return which_on_path("soffice").or_else(|| which_on_path("libreoffice"));
    }
}

#[cfg(windows)]
fn which_on_path(exe: &str) -> Option<PathBuf> {
    let out = std::process::Command::new("where")
        .arg(exe)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let first = String::from_utf8_lossy(&out.stdout).lines().next()?.trim().to_string();
    let p = PathBuf::from(first);
    p.exists().then_some(p)
}

#[cfg(unix)]
fn which_on_path(exe: &str) -> Option<PathBuf> {
    let out = std::process::Command::new("which")
        .arg(exe)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let p = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    p.exists().then_some(p)
}

/// The Office COM ProgID matching a source extension, if any.
fn com_progids_for(ext: &str) -> Option<&'static str> {
    match ext.to_ascii_lowercase().as_str() {
        "doc" | "docx" => Some("Word.Application"),
        "ppt" | "pptx" => Some("PowerPoint.Application"),
        "xls" | "xlsx" => Some("Excel.Application"),
        _ => None,
    }
}

/// `true` when the ProgID is registered (HKCR merge of machine+user
/// hives). `reg query` exits non-zero when the key is absent.
#[cfg(windows)]
fn progid_registered(progid: &str) -> bool {
    std::process::Command::new("reg")
        .args(["query", &format!("HKCR\\{progid}"), "/ve"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(not(windows))]
fn progid_registered(_progid: &str) -> bool {
    false
}

/// LibreOffice wins when present: one engine covers every format and it
/// is scriptable headless without desktop-app side effects. COM is the
/// Windows fallback (100 % native fidelity, but drives the real Office
/// app invisibly — only when there is nothing else).
fn probe_engine() -> EngineInfo {
    if let Some(soffice) = find_soffice() {
        return EngineInfo {
            engine: EngineKind::Libreoffice,
            path: Some(soffice.to_string_lossy().into_owned()),
        };
    }
    #[cfg(windows)]
    {
        const PROGIS: [&str; 3] = ["Word.Application", "PowerPoint.Application", "Excel.Application"];
        if PROGIS.iter().any(|p| progid_registered(p)) {
            return EngineInfo {
                engine: EngineKind::MsOfficeCom,
                path: Some("ms-office-com".into()),
            };
        }
    }
    EngineInfo {
        engine: EngineKind::None,
        path: None,
    }
}

#[tauri::command]
pub async fn detect_doc_convert_engine() -> Result<EngineInfo, String> {
    // Blocking probes (process spawns) off the async runtime.
    tokio::task::spawn_blocking(|| cached_engine(probe_engine))
        .await
        .map_err(|e| format!("engine probe panicked: {e}"))
}

// ── Cache key ─────────────────────────────────────────────────────────

/// FNV-1a 64-bit — a stable, dependency-free digest for the cache file
/// name. Not security-relevant (a collision costs one re-conversion).
fn fnv1a(parts: &[&str]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for b in part.as_bytes() {
            hash ^= *b as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Reduce a file name to a safe, readable slug for the cache file:
/// alphanumerics / `-` / `_` / `.` kept, everything else → `_`. The
/// version hash keeps distinct files with the same slug apart.
fn slugify(filename: &str) -> String {
    let base = filename
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(filename)
        .trim();
    let mut slug: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    while slug.contains("..") {
        slug = slug.replace("..", ".");
    }
    if slug.is_empty() || slug == "." || slug == ".." {
        slug = "document".into();
    }
    slug.chars().take(64).collect()
}

/// Cache file name for one (file, version) pair. `etag`/`size` come
/// from the raw route's response headers; when the frontend has none
/// (metadata fetch failed) they degrade to `0` and every open
/// re-converts — correct, just not fast.
fn cache_pdf_name(filename: &str, size: u64, etag: &str) -> String {
    let slug = slugify(filename);
    let stem = slug.rsplit_once('.').map(|(s, _)| s).unwrap_or(&slug);
    let hash = fnv1a(&[filename, &size.to_string(), etag]);
    format!("{stem}-{hash:016x}.pdf")
}

// ── Conversion backends ───────────────────────────────────────────────

/// Error string the frontend branches on to show the "approximate
/// layout" degradation banner instead of an error state.
pub const ENGINE_UNAVAILABLE: &str = "engine-unavailable";

fn check_pdf_output(path: &Path) -> Result<Vec<u8>, String> {
    let bytes = fs::read(path).map_err(|e| format!("read {}: {}", path.display(), e))?;
    if bytes.len() < 5 || &bytes[..4] != b"%PDF" {
        return Err(format!("conversion produced a non-PDF file: {}", path.display()));
    }
    Ok(bytes)
}

/// Build the `-env:UserInstallation=` value. LibreOffice demands a file
/// URI; Windows needs the `file:///C:/…` triple-slash form (a two-slash
/// `file://C:` resolves as an authority and breaks profile creation).
/// The profile dirs this feeds are Rust-generated temp paths (no
/// characters needing percent-encoding), so plain concatenation is
/// sufficient on every platform.
fn user_installation_uri(profile_dir: &Path) -> Result<String, String> {
    let raw = profile_dir.display().to_string();
    if raw.is_empty() {
        return Err("empty profile path".into());
    }
    #[cfg(windows)]
    {
        Ok(format!("file:///{}", raw.replace('\\', "/")))
    }
    #[cfg(not(windows))]
    {
        Ok(format!("file://{raw}"))
    }
}

/// Run one headless LibreOffice conversion into `out_dir`; returns the
/// produced PDF's bytes. A fresh profile dir per run avoids the lock
/// file / first-run wizard sharing problems when the user also runs
/// LibreOffice normally.
async fn convert_with_soffice(
    soffice: &Path,
    src: &Path,
    out_dir: &Path,
) -> Result<Vec<u8>, String> {
    let profile = out_dir.join(format!("lo-profile-{}", std::process::id()));
    fs::create_dir_all(&profile).map_err(|e| format!("create profile: {e}"))?;
    let env_arg = format!(
        "-env:UserInstallation={}",
        user_installation_uri(&profile)?
    );
    let mut cmd = tokio::process::Command::new(soffice);
    cmd.arg("--headless")
        .arg("--norestore")
        .arg("--invisible")
        .arg(env_arg)
        .args(["--convert-to", "pdf", "--outdir"])
        .arg(out_dir)
        .arg(src)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let pdf_path = expected_pdf_path(src, out_dir);
    let run = async move {
        let out = cmd
            .output()
            .await
            .map_err(|e| format!("spawn soffice {}: {e}", soffice.display()))?;
        if !out.status.success() {
            return Err(format!(
                "soffice exited {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(())
    };
    let result = tokio::time::timeout(LIBREOFFICE_TIMEOUT, run).await.map_err(|_| {
        format!(
            "LibreOffice conversion timed out ({}s)",
            LIBREOFFICE_TIMEOUT.as_secs()
        )
    })?;
    let _ = fs::remove_dir_all(&profile);
    result?;
    check_pdf_output(&pdf_path)
}

/// LibreOffice names the output after the input stem.
fn expected_pdf_path(src: &Path, out_dir: &Path) -> PathBuf {
    let stem = src
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "document".into());
    out_dir.join(format!("{stem}.pdf"))
}

/// Escape a string for embedding inside a single-quoted PowerShell
/// literal (' → ''). The paths interpolated into COM scripts are
/// Rust-generated temp paths plus sanitized names, but the sanitizer
/// keeps Unicode and spaces — quoting still has to be correct.
fn ps_single_quote(s: &Path) -> String {
    let raw = s.display().to_string();
    format!("'{}'", raw.replace('\'', "''"))
}

/// Windows-only fallback: drive the installed Office app through COM via
/// PowerShell (no new Rust dependency, and the script stays auditable).
/// Word `SaveAs2 wdFormatPDF=17`, PowerPoint `SaveAs ppSaveAsPDF=32`,
/// Excel `ExportAsFixedFormat xlTypePDF=0`.
#[cfg(windows)]
async fn convert_with_com(progids: &str, src: &Path, pdf_path: &Path) -> Result<Vec<u8>, String> {
    let script = match progids {
        "Word.Application" => format!(
            "$w=New-Object -ComObject Word.Application;$w.Visible=$false;$w.DisplayAlerts=0;\
             $d=$w.Documents.Open({src});\
             $d.SaveAs2({pdf},17);$d.Close(0);$w.Quit()",
            src = ps_single_quote(src),
            pdf = ps_single_quote(pdf_path),
        ),
        "PowerPoint.Application" => format!(
            // MsoTriState as raw numbers (-1 = msoTrue, 0 = msoFalse): the
            // interop enum type is not guaranteed to be loadable by name
            // in a bare `powershell -Command` runspace.
            "$p=New-Object -ComObject PowerPoint.Application;\
             $d=$p.Presentations.Open({src},-1,0,0);\
             $d.SaveAs({pdf},32);$d.Close();$p.Quit()",
            src = ps_single_quote(src),
            pdf = ps_single_quote(pdf_path),
        ),
        "Excel.Application" => format!(
            "$x=New-Object -ComObject Excel.Application;$x.Visible=$false;$x.DisplayAlerts=$false;\
             $wb=$x.Workbooks.Open({src});\
             $wb.ExportAsFixedFormat(0,{pdf});$wb.Close($false);$x.Quit()",
            src = ps_single_quote(src),
            pdf = ps_single_quote(pdf_path),
        ),
        other => return Err(format!("no COM bridge for {other}")),
    };
    let mut cmd = tokio::process::Command::new("powershell");
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-Command",
        &script,
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
    let run = async move {
        let out = cmd
            .output()
            .await
            .map_err(|e| format!("spawn powershell: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "office COM conversion failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(())
    };
    tokio::time::timeout(COM_TIMEOUT, run)
        .await
        .map_err(|_| format!("Office COM conversion timed out ({}s)", COM_TIMEOUT.as_secs()))??;
    check_pdf_output(pdf_path)
}

#[cfg(not(windows))]
async fn convert_with_com(progids: &str, _src: &Path, _pdf_path: &Path) -> Result<Vec<u8>, String> {
    Err(format!("COM conversion is Windows-only ({progids})"))
}

// ── The command ───────────────────────────────────────────────────────

/// Download the workspace file (authenticated, Rust side), convert it to
/// PDF with the local engine, and hand the PDF bytes back over raw IPC.
/// Errors are plain strings; [`ENGINE_UNAVAILABLE`] is the one the
/// frontend treats as "degrade to the JS viewer", not "show an error".
#[tauri::command]
pub async fn convert_workspace_file_to_pdf(
    state: State<'_, AppState>,
    url: String,
    filename: String,
    size: Option<u64>,
    etag: Option<String>,
) -> Result<tauri::ipc::Response, String> {
    let size = size.unwrap_or(0);
    let etag = etag.unwrap_or_default();
    // Same process-wide cache `detect_doc_convert_engine` filled, so a
    // conversion does not re-spawn `where`/`reg` for the same answer.
    let engine = tokio::task::spawn_blocking(|| cached_engine(probe_engine))
        .await
        .map_err(|e| format!("engine probe panicked: {e}"))?;
    if engine.engine == EngineKind::None {
        return Err(ENGINE_UNAVAILABLE.into());
    }

    let ext = Path::new(&filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let cache_dir = doc_cache_dir()?;
    let cache_file = cache_dir.join(cache_pdf_name(&filename, size, &etag));
    if let Ok(bytes) = check_pdf_output(&cache_file) {
        return Ok(tauri::ipc::Response::new(bytes));
    }

    // 1. Authenticated download of the source into a private temp dir.
    let work = work_dir()?;
    let src = work.join(sanitize_file_name(&filename));
    {
        let client = state.gateway.read().await;
        download_to_path(&client, &url, &src).await?;
    }

    // 2. Convert.
    let out_dir = work.join("out");
    fs::create_dir_all(&out_dir).map_err(|e| format!("create out dir: {e}"))?;
    let pdf_path = expected_pdf_path(&src, &out_dir);
    let bytes = match engine.engine {
        EngineKind::Libreoffice => {
            let soffice = engine
                .path
                .as_deref()
                .map(PathBuf::from)
                .ok_or_else(|| "soffice path missing from engine info".to_string())?;
            convert_with_soffice(&soffice, &src, &out_dir).await
        }
        EngineKind::MsOfficeCom => {
            let progid = com_progids_for(&ext).ok_or_else(|| {
                format!("no Office application registered for .{ext}")
            })?;
            convert_with_com(progid, &src, &pdf_path).await
        }
        EngineKind::None => unreachable!("checked above"),
    };

    // 3. Best-effort cleanup of the source copy (it sits in a temp dir,
    //    but do not leave the plaintext lying around longer than needed).
    let _ = fs::remove_file(&src);

    let bytes = bytes?;

    // 4. Land in the disk cache for the next open, dropping older
    //    versions of the same document.
    prune_old_versions(&cache_dir, &filename, &cache_file);
    if let Err(e) = fs::write(&cache_file, &bytes) {
        tracing::warn!("doc-convert cache write failed: {e}");
    }
    Ok(tauri::ipc::Response::new(bytes))
}

/// Reduce a webview-supplied filename to one safe name (same policy as
/// `save_attachment::sanitize_external_name`, kept local to avoid a
/// cross-module private dependency).
fn sanitize_file_name(filename: &str) -> String {
    let last = filename
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .trim()
        .replace('\0', "");
    if last.is_empty() || last == "." || last == ".." {
        return "document.bin".into();
    }
    last
}

fn doc_cache_dir() -> Result<PathBuf, String> {
    let dir = directories::BaseDirs::new()
        .ok_or_else(|| "no base dirs available".to_string())?
        .data_dir()
        .join("com.acowork.desktop")
        .join("doc-preview-pdf");
    fs::create_dir_all(&dir).map_err(|e| format!("create cache dir: {e}"))?;
    Ok(dir)
}

fn work_dir() -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join("acowork-doc-convert");
    fs::create_dir_all(&dir).map_err(|e| format!("create work dir: {e}"))?;
    Ok(dir)
}

/// Delete cached PDFs of the same document slug when a new version
/// lands, so edits do not pile up copies forever.
fn prune_old_versions(cache_dir: &Path, filename: &str, keep: &Path) {
    let slug = slugify(filename);
    let stem = slug.rsplit_once('.').map(|(s, _)| s).unwrap_or(&slug);
    if let Ok(entries) = fs::read_dir(cache_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.path() == keep {
                continue;
            }
            if name.starts_with(stem) && name.ends_with(".pdf") {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

/// Stream `url` (account credentials, 401 replay) to `target`. Mirrors
/// `save_attachment::download_to_file` minus the progress channel —
/// that one is private to its module and this call has no UI to feed.
async fn download_to_path(client: &GatewayClient, url: &str, target: &Path) -> Result<(), String> {
    let mut resp = client
        .send(|| {
            Ok(client
                .request(reqwest::Method::GET, url)
                .timeout(DOWNLOAD_TIMEOUT))
        })
        .await
        .map_err(|e| format!("download {url}: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("download {url}: HTTP {status} {body}"));
    }
    let mut file =
        fs::File::create(target).map_err(|e| format!("create {}: {e}", target.display()))?;
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| format!("download {url}: {e}"))?
    {
        file.write_all(&chunk)
            .map_err(|e| format!("write {}: {e}", target.display()))?;
    }
    Ok(())
}

// ── Tests (pure logic only — no engine required) ──────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_pdf_name_is_stable_and_version_sensitive() {
        let a = cache_pdf_name("报告 Q1.docx", 12345, "W/\"12345-999\"");
        assert_eq!(a, cache_pdf_name("报告 Q1.docx", 12345, "W/\"12345-999\""));
        // Same name, different version → different file.
        let b = cache_pdf_name("报告 Q1.docx", 12346, "W/\"12346-999\"");
        assert_ne!(a, b);
        // Same name+size, different mtime (ETag differs) → different file.
        let c = cache_pdf_name("报告 Q1.docx", 12345, "W/\"12345-111\"");
        assert_ne!(a, c);
        // "报告 Q1.docx" → non-ASCII and the space become underscores.
        assert!(a.starts_with("___Q1-"), "slug keeps a readable stem: {a}");
        assert!(a.ends_with(".pdf"));
    }

    #[test]
    fn slugify_never_emits_traversal_or_blanks() {
        assert_eq!(slugify(r"C:\ws\sub\deck.pptx"), "deck.pptx");
        // Path separators are stripped by taking the basename, so nothing
        // can traverse; embedded dot runs are collapsed.
        assert_eq!(slugify("a/../../b.pdf"), "b.pdf");
        assert_eq!(slugify("a..b..c.pdf"), "a.b.c.pdf");
        assert_eq!(slugify("../../"), "document");
        assert_eq!(slugify(""), "document");
        assert_eq!(slugify(".."), "document");
        // Long names are capped, extension-bearing or not.
        assert!(slugify(&"x".repeat(200)).chars().count() <= 64);
    }

    #[test]
    fn fnv1a_spreads_inputs() {
        assert_eq!(fnv1a(&["a", "b"]), fnv1a(&["a", "b"]));
        assert_ne!(fnv1a(&["a", "b"]), fnv1a(&["ab", ""]));
        assert_ne!(fnv1a(&[""]), fnv1a(&["x"]));
    }

    #[test]
    fn com_progids_map_by_extension() {
        assert_eq!(com_progids_for("docx"), Some("Word.Application"));
        assert_eq!(com_progids_for("PPT"), Some("PowerPoint.Application"));
        assert_eq!(com_progids_for("xlsx"), Some("Excel.Application"));
        assert_eq!(com_progids_for("pdf"), None);
        assert_eq!(com_progids_for("csv"), None);
    }

    #[test]
    fn expected_pdf_path_follows_input_stem() {
        let p = expected_pdf_path(Path::new("/tmp/acowork-doc-convert/report.docx"), Path::new("/tmp/out"));
        assert_eq!(p, PathBuf::from("/tmp/out/report.pdf"));
    }

    #[test]
    fn sanitize_file_name_keeps_basenames() {
        assert_eq!(sanitize_file_name("a/b/c.xlsx"), "c.xlsx");
        assert_eq!(sanitize_file_name(r"ws\报告.pptx"), "报告.pptx");
        assert_eq!(sanitize_file_name(".."), "document.bin");
        assert_eq!(sanitize_file_name(""), "document.bin");
    }

    #[test]
    fn ps_single_quote_escapes_powershell_literals() {
        assert_eq!(ps_single_quote(Path::new(r"C:\tmp\it's.pdf")), r"'C:\tmp\it''s.pdf'");
    }

    #[test]
    fn user_installation_uri_is_a_file_uri() {
        let uri = user_installation_uri(Path::new("/tmp/lo-profile")).unwrap();
        assert!(uri.starts_with("file:"), "got {uri}");
        assert!(uri.ends_with("lo-profile"), "got {uri}");
    }

    #[test]
    fn check_pdf_output_rejects_non_pdf() {
        let dir = std::env::temp_dir().join("acowork-doc-convert-test");
        fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("bad.pdf");
        fs::write(&bad, b"not a pdf at all").unwrap();
        assert!(check_pdf_output(&bad).is_err());
        let good = dir.join("good.pdf");
        fs::write(&good, b"%PDF-1.7\n").unwrap();
        assert!(check_pdf_output(&good).is_ok());
        // A truncated header must not pass either (the `len < 5` guard).
        let trunc = dir.join("trunc.pdf");
        fs::write(&trunc, b"%PD").unwrap();
        assert!(check_pdf_output(&trunc).is_err(), "short magic must fail");
        let _ = fs::remove_file(&bad);
        let _ = fs::remove_file(&good);
        let _ = fs::remove_file(&trunc);
    }

    #[test]
    fn engine_cache_round_trip() {
        reset_engine_cache();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let probe = || {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            EngineInfo {
                engine: EngineKind::None,
                path: None,
            }
        };
        let first = cached_engine(probe);
        let second = cached_engine(probe);
        assert_eq!(first.engine, EngineKind::None);
        assert_eq!(second.engine, EngineKind::None);
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "probe must run once, then serve from cache"
        );
        reset_engine_cache();
    }

    #[test]
    fn engine_info_serializes_kebab_case() {
        let json = serde_json::to_value(EngineInfo {
            engine: EngineKind::MsOfficeCom,
            path: None,
        })
        .unwrap();
        assert_eq!(json["engine"], "ms-office-com");
        let json = serde_json::to_value(EngineInfo {
            engine: EngineKind::Libreoffice,
            path: Some("/x/soffice".into()),
        })
        .unwrap();
        assert_eq!(json["engine"], "libreoffice");
    }
}
