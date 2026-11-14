//! Embedding service lifecycle management.
//!
//! Manages the `acowork-embed` child process: auto-start at Gateway startup,
//! health-check, model switching, and graceful shutdown.

use std::fs;
use std::path::Path;
use std::process::Stdio;

use crate::error::GatewayError;

/// Shared HTTP client for all embed REST calls.
///
/// reqwest strongly recommends reusing a single client instance to
/// take advantage of connection pooling. Each embed operation
/// (`/health`, `/models/{id}/load`, `/v1/embeddings`, …) goes
/// through this client; if a particular call needs a different
/// per-request timeout it can override it with
/// [`reqwest::RequestBuilder::timeout`].
pub(crate) fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .connect_timeout(std::time::Duration::from_secs(5))
            // Loopback hop to a supervised sidecar. An ambient HTTP_PROXY
            // must not answer for it: a proxy's reply would make a dead
            // service look healthy (or a healthy one dead).
            .no_proxy()
            .build()
            .expect("Failed to build embed HTTP client")
    })
}

/// Default port for the embedding service.
#[allow(dead_code)]
const EMBED_DEFAULT_PORT: u16 = 18080;

/// State of the embedding service process.
#[derive(Debug, Clone)]
pub struct EmbedProcessState {
    /// PID of the running acowork-embed process. Zero means an already-running external embed service.
    pub pid: u32,
    /// Port the embedding service is listening on.
    pub port: u16,
    /// Currently loaded model ID (None if still downloading/starting).
    pub active_model_id: Option<String>,
    /// Currently loaded model dimension.
    pub active_dimension: Option<usize>,
    /// Whether the process has completed startup and health check.
    pub ready: bool,
}

/// Create gateway state for an already-running embed service.
pub fn attach_existing_embed_process(
    port: u16,
    health: Option<EmbedHealthStatus>,
) -> EmbedProcessState {
    EmbedProcessState {
        pid: 0,
        port,
        active_model_id: health
            .as_ref()
            .and_then(|h| h.model.as_ref().map(|m| m.id.clone())),
        active_dimension: health
            .as_ref()
            .and_then(|h| h.model.as_ref().map(|m| m.dimension)),
        ready: health.as_ref().map(|h| h.ready).unwrap_or(false),
    }
}

/// Choose the listener bind host for the embed sidecar from the
/// Gateway's advertised host (ADR-055 D3).
///
/// The Gateway publishes `http://{advertise_host}:{embed_port}/v1` on
/// the `acowork/global/embedding_models` topic and Runtimes on every
/// machine — including remote Node machines — dial that URL verbatim.
/// The embed listener must therefore accept connections arriving at
/// the advertised address:
///
/// - loopback advertise (desktop / local mode) → bind `127.0.0.1`
///   (keeps the model service private to this machine);
/// - any other value (remote-mode LAN IP, or a hostname, which by
///   advertise semantics implies cross-host reachability) → bind the
///   wildcard address so LAN peers can reach it.
pub fn bind_host_for_advertise(advertise_host: &str) -> &'static str {
    let host = advertise_host.trim();
    if host.eq_ignore_ascii_case("localhost") {
        return "127.0.0.1";
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) if ip.is_loopback() => "127.0.0.1",
        _ => "0.0.0.0",
    }
}

/// Spawn the acowork-embed process.
///
/// The embedding service runs as a sibling process to the Gateway,
/// listening on `{bind_host}:{port}` — `bind_host` must be chosen with
/// [`bind_host_for_advertise`] so the listener matches the advertise
/// host published to Runtimes (ADR-055 D3). It downloads and loads the
/// recommended model on first startup.
///
/// Returns `(EmbedProcessState, Child)` — the caller is responsible for
/// reaping the child process (e.g. spawning a task that awaits `child.wait()`
/// and clears state on exit).
pub async fn spawn_embed_process(
    data_dir: &Path,
    models_dir: &Path,
    port: u16,
    bind_host: &str,
    hf_mirrors: &[String],
    onnx_variant: &str,
    model_id: Option<&str>,
) -> Result<(EmbedProcessState, tokio::process::Child), GatewayError> {
    // Locate the acowork-embed binary (sibling of current executable)
    let embed_bin = std::env::current_exe()
        .map_err(|e| GatewayError::Lifecycle(format!("Cannot find current executable: {}", e)))?
        .parent()
        .map(|p| {
            let bin_name = if cfg!(windows) {
                "acowork-embed.exe"
            } else {
                "acowork-embed"
            };
            p.join(bin_name)
        })
        .unwrap_or_else(|| {
            let bin_name = if cfg!(windows) {
                "acowork-embed.exe"
            } else {
                "acowork-embed"
            };
            std::path::PathBuf::from(bin_name)
        });

    if !embed_bin.exists() {
        return Err(GatewayError::Lifecycle(format!(
            "acowork-embed binary not found at {:?}",
            embed_bin
        )));
    }

    // Create log directory and open log file (truncate on each start)
    let log_dir = data_dir.join("logs");
    fs::create_dir_all(&log_dir).map_err(|e| {
        GatewayError::Lifecycle(format!("Failed to create log dir {:?}: {}", log_dir, e))
    })?;
    let log_path = log_dir.join("embed.log");
    let log_file = fs::File::create(&log_path).map_err(|e| {
        GatewayError::Lifecycle(format!(
            "Failed to create embed log file {:?}: {}",
            log_path, e
        ))
    })?;
    tracing::info!(path = %log_path.display(), "Embed process logging to file");

    // Probe for a local ONNX Runtime install so we can propagate the
    // library directory to the embed child process. The embed binary
    // is dynamically linked against libonnxruntime and the dynamic
    // linker (Linux/macOS) or PATH-based DLL search (Windows) needs
    // to know where to find it. Honor ORT_LIB_LOCATION first, then
    // fall back to scanning .ort/onnxruntime-*/lib/ relative to cwd.
    let ort_lib_dir = locate_ort_lib_dir();
    if let Some(ref dir) = ort_lib_dir {
        tracing::info!(ort_lib_dir = %dir, "Propagating ORT lib dir to embed child process");
    }

    let mut cmd = tokio::process::Command::new(&embed_bin);
    cmd.arg("--host")
        .arg(bind_host)
        .arg("--port")
        .arg(port.to_string())
        .arg("--models-dir")
        .arg(models_dir)
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--onnx-variant")
        .arg(onnx_variant)
        .arg("--log-level")
        .arg("info")
        .stdout(Stdio::null())
        .stderr(Stdio::from(log_file));

    // Inject the ORT lib directory into the dynamic linker's search
    // path for the embed child. Platform-specific env var:
    //   Linux   → LD_LIBRARY_PATH
    //   macOS   → DYLD_LIBRARY_PATH
    //   Windows → PATH (prepended; Windows DLL search includes PATH)
    if let Some(ref dir) = ort_lib_dir {
        #[cfg(target_os = "linux")]
        {
            let prev = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
            cmd.env("LD_LIBRARY_PATH", format!("{}:{}", dir, prev));
        }
        #[cfg(target_os = "macos")]
        {
            let prev = std::env::var("DYLD_LIBRARY_PATH").unwrap_or_default();
            cmd.env("DYLD_LIBRARY_PATH", format!("{}:{}", dir, prev));
        }
        #[cfg(windows)]
        {
            let prev = std::env::var("PATH").unwrap_or_default();
            cmd.env("PATH", format!("{};{}", dir, prev));
        }
    }

    if !hf_mirrors.is_empty() {
        let mirrors_arg = hf_mirrors.join(",");
        cmd.arg("--hf-mirrors").arg(&mirrors_arg);
    }

    if let Some(model_id) = model_id {
        cmd.arg("--model").arg(model_id);
    }

    // On Unix, create a new process group
    #[cfg(unix)]
    #[allow(unused_imports)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let child = cmd.spawn().map_err(|e| {
        GatewayError::Lifecycle(format!(
            "Failed to spawn acowork-embed (binary: {:?}): {}",
            embed_bin, e
        ))
    })?;

    let pid = child.id().ok_or_else(|| {
        GatewayError::Lifecycle(
            "Failed to get PID for acowork-embed (process may have exited immediately)".to_string(),
        )
    })?;

    tracing::info!(
        "Spawned acowork-embed process (PID: {}, port: {})",
        pid,
        port
    );

    Ok((
        EmbedProcessState {
            pid,
            port,
            active_model_id: None,
            active_dimension: None,
            ready: false,
        },
        child,
    ))
}

/// Kill the embedding service process.
pub async fn kill_embed_process(pid: u32) -> Result<(), GatewayError> {
    crate::lifecycle::process::kill_agent_process(pid).await
}

/// Check if the embedding service is healthy by calling its /health endpoint.
pub async fn check_embed_health(port: u16) -> Option<EmbedHealthStatus> {
    let url = format!("http://127.0.0.1:{}/health", port);
    let client = http_client();

    let resp = client
        .get(&url)
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
        .ok()?;
    let body: serde_json::Value = resp.json().await.ok()?;

    let status = body.get("status")?.as_str()?.to_string();
    let model = body.get("model").and_then(|m| {
        let id = m.get("id")?.as_str()?.to_string();
        let dim = m.get("dimension")?.as_u64()? as usize;
        Some(EmbedModelInfo { id, dimension: dim })
    });

    Some(EmbedHealthStatus {
        ready: status == "ready",
        model,
    })
}

/// Health check result from acowork-embed.
#[derive(Debug, Clone)]
pub struct EmbedHealthStatus {
    pub ready: bool,
    pub model: Option<EmbedModelInfo>,
}

/// Model info from health check.
#[derive(Debug, Clone)]
pub struct EmbedModelInfo {
    pub id: String,
    pub dimension: usize,
}

/// Select an embedding model by triggering acowork-embed's load endpoint.
pub async fn select_embed_model(port: u16, model_id: &str) -> Result<(), GatewayError> {
    let url = format!("http://127.0.0.1:{}/models/{}/load", port, model_id);
    let client = http_client();

    let resp = client
        .post(&url)
        .timeout(std::time::Duration::from_secs(60)) // model loading can take a while
        .send()
        .await
        .map_err(|e| {
            GatewayError::Lifecycle(format!(
                "Failed to call load endpoint for model '{}': {}",
                model_id, e
            ))
        })?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(GatewayError::Lifecycle(format!(
            "Load model '{}' failed (HTTP {}): {}",
            model_id, status, body
        )));
    }

    tracing::info!(model_id, "Embedding model loaded successfully");
    Ok(())
}

/// Trigger a model download via acowork-embed's download endpoint.
///
/// The embed server runs downloads in the background (fire-and-forget)
/// and returns 202 Accepted immediately. Progress can be polled via
/// `get_embed_model_status`.
pub async fn download_embed_model(
    port: u16,
    model_id: &str,
    variant: Option<&str>,
) -> Result<(), GatewayError> {
    let url = format!("http://127.0.0.1:{}/models/{}/download", port, model_id);
    let client = http_client();

    let body = serde_json::json!({
        "variant": variant
    });

    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            GatewayError::Lifecycle(format!(
                "Failed to call download endpoint for model '{}': {}",
                model_id, e
            ))
        })?;

    // 202 Accepted (fire-and-forget) or 200 OK (already_downloaded)
    if !resp.status().is_success() && resp.status() != reqwest::StatusCode::ACCEPTED {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(GatewayError::Lifecycle(format!(
            "Download model '{}' failed (HTTP {}): {}",
            model_id, status, body
        )));
    }

    tracing::info!(model_id, "Embedding model download triggered");
    Ok(())
}

/// Get model status from acowork-embed.
pub async fn get_embed_model_status(
    port: u16,
    model_id: &str,
) -> Result<serde_json::Value, GatewayError> {
    let url = format!("http://127.0.0.1:{}/models/{}/status", port, model_id);
    let client = http_client();

    let resp = client
        .get(&url)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .map_err(|e| {
            GatewayError::Lifecycle(format!(
                "Failed to get status for model '{}': {}",
                model_id, e
            ))
        })?;

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| GatewayError::Lifecycle(format!("Failed to parse status response: {}", e)))?;

    Ok(body)
}

/// Delete a downloaded model via acowork-embed.
///
/// Calls `DELETE /models/{id}` on the embed service.
pub async fn delete_embed_model(
    port: u16,
    model_id: &str,
) -> Result<serde_json::Value, GatewayError> {
    let url = format!("http://127.0.0.1:{}/models/{}", port, model_id);
    let client = http_client();

    let resp = client
        .delete(&url)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| {
            GatewayError::Lifecycle(format!(
                "Failed to call delete endpoint for model '{}': {}",
                model_id, e
            ))
        })?;

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| GatewayError::Lifecycle(format!("Failed to parse delete response: {}", e)))?;

    Ok(body)
}

/// Result of an embedding test.
#[derive(Debug, Clone)]
pub struct EmbedTestResult {
    /// Whether the test succeeded.
    pub success: bool,
    /// Model ID that was tested.
    pub model_id: Option<String>,
    /// Embedding dimension returned.
    pub dimension: Option<usize>,
    /// Inference latency in milliseconds.
    pub latency_ms: Option<u64>,
    /// Error message if test failed.
    pub error: Option<String>,
}

/// Test the currently loaded embedding model by sending a sample sentence.
///
/// Calls `POST /v1/embeddings` on the embed server with a short test input
/// and verifies a valid embedding vector is returned.
pub async fn test_embed_model(port: u16) -> Result<EmbedTestResult, GatewayError> {
    let url = format!("http://127.0.0.1:{}/v1/embeddings", port);
    let client = http_client();

    let body = serde_json::json!({
        "input": "ACowork embedding test sentence."
    });

    let start = std::time::Instant::now();
    let resp = client
        .post(&url)
        .json(&body)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| {
            GatewayError::Lifecycle(format!("Failed to call embeddings endpoint: {}", e))
        })?;
    let latency_ms = start.elapsed().as_millis() as u64;

    if !resp.status().is_success() {
        let status = resp.status();
        let body_text = resp.text().await.unwrap_or_default();
        return Ok(EmbedTestResult {
            success: false,
            model_id: None,
            dimension: None,
            latency_ms: Some(latency_ms),
            error: Some(format!("HTTP {}: {}", status, body_text)),
        });
    }

    let resp_body: serde_json::Value = resp.json().await.map_err(|e| {
        GatewayError::Lifecycle(format!("Failed to parse embedding response: {}", e))
    })?;

    // Extract model name and embedding dimension from response
    let model_id = resp_body
        .get("model")
        .and_then(|m| m.as_str())
        .map(|s| s.to_string());
    let dimension = resp_body
        .get("data")
        .and_then(|d| d.as_array())
        .and_then(|arr| arr.first())
        .and_then(|item| item.get("embedding"))
        .and_then(|emb| emb.as_array())
        .map(|arr| arr.len());

    Ok(EmbedTestResult {
        success: true,
        model_id,
        dimension,
        latency_ms: Some(latency_ms),
        error: None,
    })
}

/// Locate the local ONNX Runtime install directory.
///
/// Resolution order:
///   1. `ORT_LIB_LOCATION` environment variable (explicit override).
///   2. First matching `.ort/onnxruntime-<platform>-<arch>-<ver>/lib/`
///      directory under the current working directory.
///
/// Returns `None` if neither yields a valid path. The caller decides
/// what to do with the result (typically: inject into the embed
/// child's dynamic-linker search path).
fn locate_ort_lib_dir() -> Option<String> {
    if let Ok(dir) = std::env::var("ORT_LIB_LOCATION")
        && !dir.is_empty() {
            return Some(dir);
        }
    let cwd = std::env::current_dir().ok()?;
    let ort_base = cwd.join(".ort");
    let entries = std::fs::read_dir(&ort_base).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with("onnxruntime-") {
            let lib_dir = entry.path().join("lib");
            if lib_dir.is_dir() {
                return lib_dir.to_str().map(String::from);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::bind_host_for_advertise;

    #[test]
    fn embed_bind_follows_advertise_reachability() {
        // Loopback advertise (desktop / local modes) → private to host.
        assert_eq!(bind_host_for_advertise("127.0.0.1"), "127.0.0.1");
        assert_eq!(bind_host_for_advertise("::1"), "127.0.0.1");
        assert_eq!(bind_host_for_advertise("localhost"), "127.0.0.1");
        assert_eq!(bind_host_for_advertise("LOCALHOST"), "127.0.0.1");
        // Non-loopback advertise (remote mode) → LAN-reachable wildcard.
        assert_eq!(bind_host_for_advertise("192.168.3.61"), "0.0.0.0");
        assert_eq!(bind_host_for_advertise(" 192.168.3.61 "), "0.0.0.0");
        // Hostname advertise implies cross-host reachability.
        assert_eq!(bind_host_for_advertise("gw-host.lan"), "0.0.0.0");
    }
}
