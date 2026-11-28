//! `.app` package install / uninstall / rollback (design doc 34 §6).
//!
//! Hard constraints:
//! - Any failure before the package is committed leaves no half-installed state.
//! - The Gateway never reads `app-data/{id}` contents.
//! - Builtin ids cannot be overwritten.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use uuid::Uuid;
use zip::ZipArchive;

use acowork_core::app::{
    APP_API_VERSION, APP_DATA_DIR_NAME, APP_STATE_FILE, APPS_DIR_NAME, AppInstallState,
    AppManifest, MAX_APP_PACKAGE_BYTES, current_platform_token, default_package_entry,
    is_valid_app_id, validate_package_spec,
};

/// Stable error codes returned to Desktop (design doc 34 §6.8).
#[derive(Debug, Clone, Serialize)]
pub struct InstallError {
    pub code: &'static str,
    pub message: String,
    #[serde(skip)]
    pub http: u16,
}

impl InstallError {
    pub(crate) fn new(code: &'static str, http: u16, message: impl Into<String>) -> Self {
        Self {
            code,
            http,
            message: message.into(),
        }
    }
}

/// Result of a successful install / upgrade.
#[derive(Debug, Clone, Serialize)]
pub struct InstallOutcome {
    pub app_id: String,
    pub installed_version: String,
    pub fingerprint: Option<String>,
    pub origin: String,
}

/// Options controlling trust checks at install time.
#[derive(Debug, Clone)]
pub struct InstallOptions {
    pub allow_unsigned: bool,
    pub origin: String,
}

impl Default for InstallOptions {
    fn default() -> Self {
        Self {
            allow_unsigned: false,
            origin: "upload".into(),
        }
    }
}

/// Install a `.app` package from a file on disk into `{data_dir}/apps/{id}`.
pub fn install_app_package(
    data_dir: &Path,
    package_path: &Path,
    opts: &InstallOptions,
) -> Result<InstallOutcome, InstallError> {
    let meta = fs::metadata(package_path).map_err(|e| {
        InstallError::new("package_malformed", 400, format!("cannot read package: {e}"))
    })?;
    if meta.len() > MAX_APP_PACKAGE_BYTES {
        return Err(InstallError::new(
            "package_too_large",
            413,
            format!(
                "package exceeds {} MB limit",
                MAX_APP_PACKAGE_BYTES / (1024 * 1024)
            ),
        ));
    }

    let apps_root = data_dir.join(APPS_DIR_NAME);
    let staging_root = apps_root.join(".staging");
    fs::create_dir_all(&staging_root).map_err(|e| {
        InstallError::new(
            "package_malformed",
            500,
            format!("cannot create staging: {e}"),
        )
    })?;
    let staging_id = Uuid::new_v4().to_string();
    let staging_pkg = staging_root.join(format!("{staging_id}.app"));
    let staging_dir = staging_root.join(&staging_id);

    // Copy into staging so a mid-flight failure never touches the live tree.
    fs::copy(package_path, &staging_pkg).map_err(|e| {
        InstallError::new("package_malformed", 500, format!("staging copy failed: {e}"))
    })?;

    let cleanup = |staging_pkg: &Path, staging_dir: &Path| {
        let _ = fs::remove_file(staging_pkg);
        let _ = fs::remove_dir_all(staging_dir);
    };

    let result = (|| {
        let (fingerprint, signer) = verify_trust(&staging_pkg, opts)?;
        extract_package(&staging_pkg, &staging_dir)?;
        let manifest = load_and_validate_manifest(&staging_dir)?;
        assert_binaries(&staging_dir, &manifest)?;

        if manifest.builtin || !is_valid_app_id(&manifest.id) {
            return Err(InstallError::new(
                "app_id_reserved",
                409,
                "builtin or invalid app id",
            ));
        }
        // Refuse overwriting a built-in id.
        if matches!(manifest.id.as_str(), "pm" | "doc") {
            return Err(InstallError::new(
                "app_id_reserved",
                409,
                format!("'{}' is a builtin app id", manifest.id),
            ));
        }

        let pkg = manifest.package.as_ref().ok_or_else(|| {
            InstallError::new("package_malformed", 400, "missing [package] segment")
        })?;
        if pkg.app_api > APP_API_VERSION {
            return Err(InstallError::new(
                "app_api_unsupported",
                400,
                format!(
                    "package.app_api {} unsupported (host={APP_API_VERSION})",
                    pkg.app_api
                ),
            ));
        }
        let host = current_platform_token();
        if !pkg.platforms.iter().any(|p| p == &host) {
            return Err(InstallError::new(
                "platform_unsupported",
                400,
                format!("package platforms {:?} do not include {host}", pkg.platforms),
            ));
        }
        if pkg.platform_app && signer.as_deref() != Some("platform") {
            return Err(InstallError::new(
                "platform_signature_required",
                400,
                "platform_app requires a Platform signature",
            ));
        }

        let app_dir = apps_root.join(&manifest.id);
        let existing = read_state(&app_dir);
        if let Some(ref state) = existing {
            if let (Some(prev), Some(next)) = (&state.fingerprint, &fingerprint)
                && prev != next
            {
                return Err(InstallError::new(
                    "signer_mismatch",
                    409,
                    "upgrade certificate fingerprint does not match the installed one",
                ));
            }
            let version_dir = app_dir.join("versions").join(&manifest.version);
            if version_dir.exists() {
                // Idempotent: the bits are already on disk. Returning Ok lets
                // the HTTP layer reload the registry and (re)start the app —
                // needed when a prior boot loaded AppRegistry from the wrong
                // directory and left `/api/apps` blind to an installed package.
                return Ok(InstallOutcome {
                    app_id: manifest.id,
                    installed_version: state.current.clone(),
                    fingerprint: state.fingerprint.clone().or(fingerprint),
                    origin: state.origin.clone(),
                });
            }
        }

        // Commit: move staging → versions/{version}/
        let version_dir = app_dir.join("versions").join(&manifest.version);
        fs::create_dir_all(version_dir.parent().unwrap()).map_err(|e| {
            InstallError::new("package_malformed", 500, format!("create versions: {e}"))
        })?;
        // On Windows, rename across dirs on same volume works for directories.
        if version_dir.exists() {
            let state = read_state(&app_dir);
            return Ok(InstallOutcome {
                app_id: manifest.id,
                installed_version: state
                    .as_ref()
                    .map(|s| s.current.clone())
                    .unwrap_or_else(|| manifest.version.clone()),
                fingerprint: state
                    .and_then(|s| s.fingerprint)
                    .or(fingerprint),
                origin: opts.origin.clone(),
            });
        }
        fs::rename(&staging_dir, &version_dir).map_err(|e| {
            InstallError::new(
                "package_malformed",
                500,
                format!("commit rename failed: {e}"),
            )
        })?;

        let state = AppInstallState {
            current: manifest.version.clone(),
            fingerprint: fingerprint.clone().or_else(|| {
                existing.and_then(|s| s.fingerprint)
            }),
            origin: opts.origin.clone(),
            installed_at: now_rfc3339(),
        };
        write_state_atomic(&app_dir, &state)?;

        // Ensure private data dir exists; never touch its contents.
        let _ = fs::create_dir_all(data_dir.join(APP_DATA_DIR_NAME).join(&manifest.id));

        Ok(InstallOutcome {
            app_id: manifest.id,
            installed_version: state.current,
            fingerprint: state.fingerprint,
            origin: state.origin,
        })
    })();

    match &result {
        Ok(_) => {
            let _ = fs::remove_file(&staging_pkg);
        }
        Err(_) => cleanup(&staging_pkg, &staging_dir),
    }
    result
}

/// Stream a request body into a staging `.app` file with a size cap.
pub fn write_upload_with_limit(
    data_dir: &Path,
    mut body: impl Read,
) -> Result<PathBuf, InstallError> {
    let staging = data_dir
        .join(APPS_DIR_NAME)
        .join(".staging")
        .join(format!("{}.app", Uuid::new_v4()));
    if let Some(parent) = staging.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            InstallError::new("package_malformed", 500, format!("staging: {e}"))
        })?;
    }
    let mut file = File::create(&staging).map_err(|e| {
        InstallError::new("package_malformed", 500, format!("create staging: {e}"))
    })?;
    let mut buf = [0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = body.read(&mut buf).map_err(|e| {
            InstallError::new("package_malformed", 400, format!("read body: {e}"))
        })?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > MAX_APP_PACKAGE_BYTES {
            drop(file);
            let _ = fs::remove_file(&staging);
            return Err(InstallError::new(
                "package_too_large",
                413,
                "package exceeds 200 MB limit",
            ));
        }
        file.write_all(&buf[..n]).map_err(|e| {
            InstallError::new("package_malformed", 500, format!("write staging: {e}"))
        })?;
    }
    Ok(staging)
}

/// List installed versions for one app id.
pub fn list_versions(data_dir: &Path, app_id: &str) -> Result<(String, Vec<VersionInfo>), InstallError> {
    if !is_valid_app_id(app_id) {
        return Err(InstallError::new("package_malformed", 400, "invalid app id"));
    }
    let app_dir = data_dir.join(APPS_DIR_NAME).join(app_id);
    let state = read_state(&app_dir).ok_or_else(|| {
        InstallError::new("package_malformed", 404, "app is not an installed package")
    })?;
    let versions_dir = app_dir.join("versions");
    let mut versions = Vec::new();
    if let Ok(entries) = fs::read_dir(&versions_dir) {
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let version = entry.file_name().to_string_lossy().to_string();
            versions.push(VersionInfo {
                version: version.clone(),
                installed_at: state.installed_at.clone(),
                fingerprint: state.fingerprint.clone(),
                current: version == state.current,
            });
        }
    }
    versions.sort_by(|a, b| a.version.cmp(&b.version));
    Ok((state.current, versions))
}

#[derive(Debug, Clone, Serialize)]
pub struct VersionInfo {
    pub version: String,
    pub installed_at: String,
    pub fingerprint: Option<String>,
    pub current: bool,
}

/// Switch `current` to an already-installed version.
pub fn rollback_app(
    data_dir: &Path,
    app_id: &str,
    version: &str,
) -> Result<AppInstallState, InstallError> {
    if !is_valid_app_id(app_id) || version.contains("..") || version.contains('/') {
        return Err(InstallError::new("package_malformed", 400, "invalid id/version"));
    }
    let app_dir = data_dir.join(APPS_DIR_NAME).join(app_id);
    let mut state = read_state(&app_dir).ok_or_else(|| {
        InstallError::new("package_malformed", 404, "app is not an installed package")
    })?;
    let version_dir = app_dir.join("versions").join(version);
    if !version_dir.join("app.toml").is_file() {
        return Err(InstallError::new(
            "package_malformed",
            404,
            format!("version {version} is not installed"),
        ));
    }
    state.current = version.to_string();
    write_state_atomic(&app_dir, &state)?;
    Ok(state)
}

/// Move `apps/{id}` into `.trash` and optionally purge `app-data/{id}`.
pub fn uninstall_app(
    data_dir: &Path,
    app_id: &str,
    purge_data: bool,
) -> Result<(), InstallError> {
    if !is_valid_app_id(app_id) {
        return Err(InstallError::new("invalid_app_id", 400, "invalid app id"));
    }
    if matches!(app_id, "pm" | "doc") {
        return Err(InstallError::new(
            "app_id_reserved",
            403,
            "cannot uninstall a builtin app",
        ));
    }
    let app_dir = data_dir.join(APPS_DIR_NAME).join(app_id);
    if !app_dir.exists() {
        return Err(InstallError::new("app_not_installed", 404, "app not installed"));
    }
    let trash = data_dir
        .join(APPS_DIR_NAME)
        .join(".trash")
        .join(format!("{app_id}.{}", now_unix()));
    if let Some(parent) = trash.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            InstallError::new("uninstall_failed", 500, format!("trash: {e}"))
        })?;
    }
    // Windows `std::fs::rename` uses MoveFileEx(MOVEFILE_REPLACE_EXISTING),
    // which MSDN forbids for directories — uninstall then 500s even after
    // the app process is gone. MoveFileW (or Unix rename) is the directory move.
    move_dir_retry(&app_dir, &trash).map_err(|e| {
        tracing::error!(app = %app_id, error = %e, "uninstall: move to trash failed");
        InstallError::new("uninstall_failed", 500, format!("move to trash: {e}"))
    })?;
    if purge_data {
        let data = data_dir.join(APP_DATA_DIR_NAME).join(app_id);
        let _ = fs::remove_dir_all(data);
    }
    Ok(())
}

fn move_dir_retry(src: &Path, dst: &Path) -> io::Result<()> {
    const ATTEMPTS: u32 = 20;
    let mut last = None;
    for attempt in 0..ATTEMPTS {
        match move_dir(src, dst) {
            Ok(()) => return Ok(()),
            Err(e) if is_sharing_violation(&e) && attempt + 1 < ATTEMPTS => {
                std::thread::sleep(Duration::from_millis(50));
                last = Some(e);
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("move_dir retry exhausted")))
}

fn is_sharing_violation(err: &io::Error) -> bool {
    matches!(
        err.raw_os_error(),
        Some(5) | Some(32) | Some(33) // ACCESS_DENIED / SHARING_VIOLATION / LOCK_VIOLATION
    )
}

fn move_dir(src: &Path, dst: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        move_dir_windows(src, dst)
    }
    #[cfg(not(windows))]
    {
        fs::rename(src, dst)
    }
}

/// `MoveFileW` without `MOVEFILE_REPLACE_EXISTING` — that flag cannot move a directory.
#[cfg(windows)]
fn move_dir_windows(src: &Path, dst: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    fn wide(p: &Path) -> Vec<u16> {
        p.as_os_str().encode_wide().chain(Some(0)).collect()
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileW(lp_existing_file_name: *const u16, lp_new_file_name: *const u16) -> i32;
    }
    let src_w = wide(src);
    let dst_w = wide(dst);
    let ok = unsafe { MoveFileW(src_w.as_ptr(), dst_w.as_ptr()) };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Drop trash directories older than 7 days.
pub fn cleanup_trash(data_dir: &Path) {
    let trash_root = data_dir.join(APPS_DIR_NAME).join(".trash");
    let Ok(entries) = fs::read_dir(&trash_root) else {
        return;
    };
    let cutoff = now_unix().saturating_sub(7 * 24 * 3600);
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(ts) = name.rsplit('.').next().and_then(|s| s.parse::<u64>().ok())
            && ts < cutoff
        {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

fn verify_trust(
    package_path: &Path,
    opts: &InstallOptions,
) -> Result<(Option<String>, Option<String>), InstallError> {
    match acowork_sign::verify::verify_package(package_path) {
        Ok(result) => Ok((
            Some(result.certificate_fingerprint),
            Some(result.signer),
        )),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("No signers")
                || msg.contains("Signing block")
                || msg.contains("not found")
                || msg.contains("No signing")
            {
                if opts.allow_unsigned {
                    tracing::warn!(
                        path = %package_path.display(),
                        "installing unsigned .app (apps.allow_unsigned=true)"
                    );
                    return Ok((None, None));
                }
                return Err(InstallError::new(
                    "unsigned_package",
                    400,
                    "unsigned packages are rejected by default",
                ));
            }
            if msg.contains("Digest") || msg.contains("Signature") || msg.contains("tamper") {
                return Err(InstallError::new(
                    "package_tampered",
                    400,
                    msg,
                ));
            }
            // Unsigned ZIPs typically fail while extracting the signing block.
            if opts.allow_unsigned {
                tracing::warn!(
                    path = %package_path.display(),
                    error = %msg,
                    "treating package as unsigned (apps.allow_unsigned=true)"
                );
                return Ok((None, None));
            }
            Err(InstallError::new("unsigned_package", 400, msg))
        }
    }
}

fn extract_package(package_path: &Path, dest: &Path) -> Result<(), InstallError> {
    fs::create_dir_all(dest).map_err(|e| {
        InstallError::new("package_malformed", 500, format!("create dest: {e}"))
    })?;
    let file = File::open(package_path).map_err(|e| {
        InstallError::new("package_malformed", 400, format!("open package: {e}"))
    })?;
    let mut archive = ZipArchive::new(file).map_err(|e| {
        InstallError::new("package_malformed", 400, format!("invalid zip: {e}"))
    })?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| {
            InstallError::new("package_malformed", 400, format!("zip entry: {e}"))
        })?;
        let name = entry.name().to_string();
        if name.contains("..")
            || name.starts_with('/')
            || name.starts_with('\\')
            || Path::new(&name).is_absolute()
        {
            return Err(InstallError::new(
                "package_malformed",
                400,
                format!("illegal zip entry path: {name}"),
            ));
        }
        // Nested zip: refuse entries that look like nested archives.
        if name.to_ascii_lowercase().ends_with(".zip")
            || name.to_ascii_lowercase().ends_with(".app")
        {
            return Err(InstallError::new(
                "package_malformed",
                400,
                "nested archives are not allowed",
            ));
        }
        let out_path = dest.join(&name);
        if entry.is_dir() {
            fs::create_dir_all(&out_path).map_err(|e| {
                InstallError::new("package_malformed", 500, format!("mkdir: {e}"))
            })?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                InstallError::new("package_malformed", 500, format!("mkdir: {e}"))
            })?;
        }
        let mut outfile = File::create(&out_path).map_err(|e| {
            InstallError::new("package_malformed", 500, format!("create file: {e}"))
        })?;
        io::copy(&mut entry, &mut outfile).map_err(|e| {
            InstallError::new("package_malformed", 500, format!("extract: {e}"))
        })?;
    }
    Ok(())
}

fn load_and_validate_manifest(staging_dir: &Path) -> Result<AppManifest, InstallError> {
    let path = staging_dir.join("app.toml");
    let text = fs::read_to_string(&path).map_err(|e| {
        InstallError::new("package_malformed", 400, format!("missing app.toml: {e}"))
    })?;
    let manifest = AppManifest::from_toml(&text)
        .map_err(|e| InstallError::new("package_malformed", 400, e))?;
    let pkg = manifest.package.as_ref().ok_or_else(|| {
        InstallError::new("package_malformed", 400, "missing [package] segment")
    })?;
    validate_package_spec(pkg, &manifest.process.binary)
        .map_err(|e| InstallError::new("package_malformed", 400, e))?;
    Ok(manifest)
}

fn assert_binaries(staging_dir: &Path, manifest: &AppManifest) -> Result<(), InstallError> {
    let entry = manifest
        .package
        .as_ref()
        .and_then(|p| p.entry.clone())
        .unwrap_or_else(|| default_package_entry(&manifest.process.binary));
    let path = staging_dir.join(&entry);
    let alt = staging_dir
        .join("bin")
        .join(if cfg!(windows) {
            format!("{}.exe", manifest.process.binary)
        } else {
            manifest.process.binary.clone()
        });
    let bin = if path.is_file() {
        path
    } else if alt.is_file() {
        alt
    } else {
        return Err(InstallError::new(
            "binary_missing",
            400,
            format!("bin entry '{entry}' missing or not executable"),
        ));
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&bin)
            .map(|m| m.permissions().mode())
            .unwrap_or(0);
        if mode & 0o111 == 0 {
            // Make newly extracted binaries executable — zip often drops mode bits.
            let mut perms = fs::metadata(&bin)
                .map_err(|e| InstallError::new("binary_missing", 400, e.to_string()))?
                .permissions();
            perms.set_mode(mode | 0o755);
            let _ = fs::set_permissions(&bin, perms);
        }
    }
    let _ = bin;
    Ok(())
}

fn read_state(app_dir: &Path) -> Option<AppInstallState> {
    fs::read_to_string(app_dir.join(APP_STATE_FILE))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
}

fn write_state_atomic(app_dir: &Path, state: &AppInstallState) -> Result<(), InstallError> {
    fs::create_dir_all(app_dir).map_err(|e| {
        InstallError::new("package_malformed", 500, format!("mkdir app: {e}"))
    })?;
    let path = app_dir.join(APP_STATE_FILE);
    let tmp = app_dir.join(format!("{APP_STATE_FILE}.tmp"));
    let text = serde_json::to_string_pretty(state).map_err(|e| {
        InstallError::new("package_malformed", 500, format!("serialize state: {e}"))
    })?;
    fs::write(&tmp, text).map_err(|e| {
        InstallError::new("package_malformed", 500, format!("write state: {e}"))
    })?;
    fs::rename(&tmp, &path).map_err(|e| {
        InstallError::new("package_malformed", 500, format!("commit state: {e}"))
    })?;
    Ok(())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_rfc3339() -> String {
    // Keep the ledger free of chrono so acowork-core stays light; a unix-based
    // RFC3339-ish stamp is enough for display.
    let secs = now_unix();
    format!("{secs}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use acowork_sign::packager::build_app_package;
    use std::io::Write;

    fn sample_app_dir(root: &Path, id: &str, version: &str) -> PathBuf {
        let dir = root.join(format!("{id}-src"));
        fs::create_dir_all(dir.join("bin")).unwrap();
        let bin_name = if cfg!(windows) {
            format!("{id}.exe")
        } else {
            id.to_string()
        };
        {
            let mut f = File::create(dir.join("bin").join(&bin_name)).unwrap();
            f.write_all(b"#!/bin/sh\necho ok\n").unwrap();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let p = dir.join("bin").join(&bin_name);
            let mut perms = fs::metadata(&p).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&p, perms).unwrap();
        }
        let platform = current_platform_token();
        let entry = if cfg!(windows) {
            format!("bin/{id}.exe")
        } else {
            format!("bin/{id}")
        };
        let toml = format!(
            r#"
id = "{id}"
version = "{version}"
name = "Demo {id}"
icon = "box"
builtin = false

[process]
runtime = "generic"
binary = "{id}"
port_hint = 18200
health_path = "/health"

[mcp]
path = "/mcp"
gateway_path = "/api/{id}/mcp"

[ui]
path = "/ui/"

[ui.nav]
label = "Demo"

[package]
app_api = 1
platforms = ["{platform}"]
entry = "{entry}"
"#
        );
        fs::write(dir.join("app.toml"), toml).unwrap();
        dir
    }

    #[test]
    fn install_unsigned_when_allowed() {
        let root = std::env::temp_dir().join(format!("acowork-app-install-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let src = sample_app_dir(&root, "demoapp", "1.0.0");
        let pkg = root.join("demoapp-1.0.0.unsigned.app");
        build_app_package(&src, &pkg).unwrap();
        let data = root.join("data");
        fs::create_dir_all(&data).unwrap();
        let out = install_app_package(
            &data,
            &pkg,
            &InstallOptions {
                allow_unsigned: true,
                origin: "upload".into(),
            },
        )
        .unwrap();
        assert_eq!(out.app_id, "demoapp");
        assert_eq!(out.installed_version, "1.0.0");
        assert!(
            data.join(APPS_DIR_NAME)
                .join("demoapp")
                .join("versions")
                .join("1.0.0")
                .join("app.toml")
                .is_file()
        );
        assert!(data.join(APP_DATA_DIR_NAME).join("demoapp").is_dir());
        // Reinstall same version → idempotent Ok (registry can reload/start).
        let again = install_app_package(
            &data,
            &pkg,
            &InstallOptions {
                allow_unsigned: true,
                origin: "upload".into(),
            },
        )
        .unwrap();
        assert_eq!(again.app_id, "demoapp");
        assert_eq!(again.installed_version, "1.0.0");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn reject_unsigned_by_default() {
        let root = std::env::temp_dir().join(format!("acowork-app-unsigned-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let src = sample_app_dir(&root, "denyapp", "1.0.0");
        let pkg = root.join("deny.app");
        build_app_package(&src, &pkg).unwrap();
        let data = root.join("data");
        fs::create_dir_all(&data).unwrap();
        let err = install_app_package(&data, &pkg, &InstallOptions::default()).unwrap_err();
        assert_eq!(err.code, "unsigned_package");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn uninstall_moves_a_nonempty_tree_to_trash() {
        let root = std::env::temp_dir().join(format!(
            "acowork-app-uninstall-{}-{}",
            std::process::id(),
            now_unix()
        ));
        let _ = fs::remove_dir_all(&root);
        let app_dir = root.join(APPS_DIR_NAME).join("demoapp");
        fs::create_dir_all(app_dir.join("versions").join("1.0.0").join("bin")).unwrap();
        fs::write(
            app_dir.join("versions").join("1.0.0").join("bin").join("demoapp.exe"),
            b"not-a-real-binary",
        )
        .unwrap();
        fs::write(app_dir.join("state.json"), "{}").unwrap();
        uninstall_app(&root, "demoapp", false).unwrap();
        assert!(!app_dir.exists());
        let trash = root.join(APPS_DIR_NAME).join(".trash");
        let found = fs::read_dir(&trash)
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with("demoapp."));
        assert!(found, "expected demoapp.* in {:?}", trash);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn uninstall_missing_app_is_not_package_malformed() {
        let root = std::env::temp_dir().join(format!(
            "acowork-app-uninstall-missing-{}-{}",
            std::process::id(),
            now_unix()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let err = uninstall_app(&root, "demoapp", false).unwrap_err();
        assert_eq!(err.code, "app_not_installed");
        assert_eq!(err.http, 404);
        let bad = uninstall_app(&root, "Not Valid", false).unwrap_err();
        assert_eq!(bad.code, "invalid_app_id");
        let _ = fs::remove_dir_all(&root);
    }
}
