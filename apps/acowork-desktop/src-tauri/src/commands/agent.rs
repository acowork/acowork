//! Agent management commands

use std::io::Read;

use tauri::{Manager, State};

use crate::gateway_client::{
    AgentDetailResponse, AgentListEntry, CloneResponse, GatewayApiError, GenericMessageResponse,
    OperationAck,
};
use crate::state::{AppState, BootstrapStateView};

/// Maximum number of attempts for an `install_agent` POST to the Gateway
/// before giving up. The Gateway returns HTTP 409 with the structured
/// `dependency_not_ready` code while the Node control plane is still
/// bootstrapping (ADR-059 §6.3). Most retries recover within 1–2
/// iterations; we cap at 5 to keep the user-perceived onboarding
/// latency bounded.
const INSTALL_MAX_ATTEMPTS: usize = 5;

/// Time budget for waiting on the bootstrap phase to reach READY
/// between install retries (ADR-059 §5.1).
const INSTALL_BOOTSTRAP_WAIT_SECS: u64 = 30;

/// Per-call install wait budget. The Gateway inventory only flips after
/// the Node's retained `InstalledAgentInfo` aggregates (ADR-059 §6), so
/// the command should outlive typical Node-side install latency
/// (download + signature verify + extract + retained publish).
pub(crate) const INSTALL_WAIT_SECS: u64 = 180;

/// Extract the `agent_id` from a `.agent` package's embedded
/// `manifest.toml`. We do not trust the filename — OnboardingFlow §725
/// calls out the same hazard (the Gateway inventory and the Node's
/// install table key on the manifest id, not the filename), and the
/// generic AgentList installer hits the same bug if we silently use the
/// filename here. A non-MVP path: install the package from anywhere
/// (including a renamed `.agent`) and `wait_agent_installed` still
/// polls the right id.
pub(crate) fn read_agent_id_from_package(package_bytes: &[u8]) -> Result<String, String> {
    let reader = std::io::Cursor::new(package_bytes);
    let mut archive = zip::ZipArchive::new(reader)
        .map_err(|e| format!("Failed to read .agent package as ZIP: {}", e))?;
    let mut manifest_file = archive
        .by_name("manifest.toml")
        .map_err(|e| format!("manifest.toml not found in package: {}", e))?;
    let mut manifest_str = String::new();
    manifest_file
        .read_to_string(&mut manifest_str)
        .map_err(|e| format!("Failed to read manifest.toml: {}", e))?;
    let value: toml::Value = toml::from_str(&manifest_str)
        .map_err(|e| format!("manifest.toml is not valid TOML: {}", e))?;
    let agent_id = value
        .get("agent_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "manifest.toml has no `agent_id` field".to_string())?
        .trim()
        .to_string();
    if agent_id.is_empty() {
        return Err("manifest.toml `agent_id` is empty".to_string());
    }
    Ok(agent_id)
}

/// Decide whether an `install_agent` failure is retryable.
///
/// ADR-059 §6.3: only the structured `dependency_not_ready` code is
/// retried — the node control plane has not announced readiness yet, so
/// we wait for bootstrap phase READY and resubmit. The old
/// "503 / never enrolled" text matching is gone (Phase 5.5).
fn should_retry_install(err: &anyhow::Error) -> bool {
    err.downcast_ref::<GatewayApiError>()
        .map(|e| e.is_dependency_not_ready())
        .unwrap_or(false)
}

/// Poll `GET /api/bootstrap` until the aggregated phase is READY
/// (ADR-059 §5.1). The Gateway folds every required subsystem (vault /
/// mqtt / node.local / system_agent / publisher) into the phase, so a
/// single wait covers the whole dependency set — no per-subsystem
/// guessing. Returns the phase detail on terminal phases.
async fn wait_bootstrap_ready(
    state: &AppState,
    timeout_secs: u64,
) -> Result<BootstrapStateView, String> {
    let base_url = state.gateway.read().await.base_url().to_string();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {}", e))?;
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut last_detail = String::new();
    while std::time::Instant::now() < deadline {
        match client
            .get(format!("{}/api/bootstrap", base_url))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(view) = resp.json::<BootstrapStateView>().await {
                    match view.phase.as_str() {
                        "READY" => return Ok(view),
                        "FAILED" | "SHUTTING_DOWN" => {
                            return Err(format!(
                                "Gateway {}: {}",
                                view.phase, view.phase_detail
                            ));
                        }
                        _ => last_detail = view.phase_detail.clone(),
                    }
                }
            }
            Ok(_) | Err(_) => {}
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    Err(format!(
        "bootstrap did not reach READY within {}s ({})",
        timeout_secs, last_detail
    ))
}

/// Shared retry wrapper for a Gateway install / ensure POST. Returns the
/// first non-retryable error or the final result after
/// `INSTALL_MAX_ATTEMPTS` attempts. On `dependency_not_ready` the retry
/// waits for bootstrap phase READY first (ADR-059 §6.3) instead of a blind
/// sleep.
///
/// Generic over the ack type because the two submission endpoints answer
/// different bodies (`OperationAck` for `/install`, `EnsureAck` for
/// `/ensure`) while the retry semantics are identical.
pub(crate) async fn submit_with_retry<F, Fut, T>(
    state: &AppState,
    label: &str,
    mut op: F,
) -> Result<T, String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    let mut attempt: usize = 0;
    loop {
        attempt += 1;
        match op().await {
            Ok(ack) => {
                if attempt > 1 {
                    tracing::info!(
                        "[{}] Submit accepted on attempt {}/{}",
                        label,
                        attempt,
                        INSTALL_MAX_ATTEMPTS
                    );
                }
                return Ok(ack);
            }
            Err(e) => {
                let retryable = should_retry_install(&e);
                if !retryable || attempt >= INSTALL_MAX_ATTEMPTS {
                    return Err(e.to_string());
                }
                tracing::warn!(
                    "[{}] Install attempt {}/{} rejected (dependency_not_ready, \
                     waiting for bootstrap READY): {}",
                    label,
                    attempt,
                    INSTALL_MAX_ATTEMPTS,
                    e
                );
                wait_bootstrap_ready(state, INSTALL_BOOTSTRAP_WAIT_SECS).await?;
            }
        }
    }
}

/// List all installed agents
#[tauri::command]
pub async fn list_agents(state: State<'_, AppState>) -> Result<Vec<AgentListEntry>, String> {
    let client = state.gateway.read().await;
    client.list_agents().await.map_err(|e| e.to_string())
}

/// Get agent detail
#[tauri::command]
pub async fn get_agent_detail(
    state: State<'_, AppState>,
    agent_id: String,
) -> Result<AgentDetailResponse, String> {
    let client = state.gateway.read().await;
    client
        .get_agent_detail(&agent_id)
        .await
        .map_err(|e| e.to_string())
}

/// Install an agent from a .agent package
///
/// Reads the package file locally (Desktop App side) and uploads its contents
/// to the Gateway via multipart/form-data. This works across platform boundaries
/// (e.g. Windows client → WSL Gateway) because the file content is transmitted
/// over HTTP rather than relying on shared filesystem paths.
///
/// ADR-059 §6: the Gateway answers HTTP 202 with an [`OperationAck`]; the
/// actual install runs asynchronously on the node (completion is correlated
/// via the ack's `operation_id`). While the node control plane is still
/// bootstrapping it answers HTTP 409 `dependency_not_ready` instead — we
/// wait for bootstrap phase READY and resubmit up to `INSTALL_MAX_ATTEMPTS`
/// times.
// Signature strictness is NOT a client-side knob: the Gateway drops the
// multipart `dev_mode` field and reads its own `dev_mode` config instead
// (PRD §9 F3 / §14.5 F17). The parameter was therefore always either ignored
// or — worse — read as if it worked, so it is gone from this command's surface.
#[tauri::command]
pub async fn install_agent(
    state: State<'_, AppState>,
    package_path: String,
    node_id: Option<String>,
) -> Result<OperationAck, String> {
    // Read the .agent file into memory on the Desktop App side
    let package_bytes = std::fs::read(&package_path)
        .map_err(|e| format!("Failed to read package file '{}': {}", package_path, e))?;

    if package_bytes.is_empty() {
        return Err("Package file is empty".to_string());
    }

    // ADR-059 §6: install is asynchronous (202 + OperationAck). Resolve +
    // validate the manifest up front: a broken package fails here with a
    // clear local error instead of a Gateway round-trip.
    read_agent_id_from_package(&package_bytes)?;

    // Clone the captured Tauri `State` outside the closure so the retry
    // wrapper can borrow `state` while the `move` closure owns its copy.
    let state_for_op = state.clone();
    let ack = submit_with_retry(&state, "INSTALL_AGENT", move || {
        let state = state_for_op.clone();
        let package_bytes = package_bytes.clone();
        let node_id = node_id.clone();
        async move {
            let client = state.gateway.read().await;
            // Historical default; see the command's doc comment.
            client
                .install_agent(&package_bytes, false, node_id.as_deref())
                .await
        }
    })
    .await?;

    // Wait until the install actually lands in the Gateway inventory.
    // ADR-073: the inventory is keyed by the gateway-generated INSTANCE
    // identity (the install ack carries it) — polling the package id 404s
    // forever and hung this call for the full timeout, leaving the
    // caller's `installing` flag stuck. Any error here surfaces as a Tauri
    // error → invoke() throws on the frontend → AgentList's catch shows
    // the toast.
    let wait_key = ack.instance_id.as_deref().ok_or_else(|| {
        format!(
            "Install accepted (operation {}) but the Gateway reported no instance id",
            ack.operation_id
        )
    })?;
    let client = state.gateway.read().await;
    client
        .wait_for_agent_installed(
            wait_key,
            std::time::Duration::from_secs(INSTALL_WAIT_SECS),
        )
        .await
        .map_err(|e| {
            format!(
                "Install submitted (operation {}) but did not complete within {}s: {}",
                ack.operation_id, INSTALL_WAIT_SECS, e
            )
        })?;

    Ok(ack)
}

#[tauri::command]
pub async fn install_bundled_agent(
    state: State<'_, AppState>,
    app_handle: tauri::AppHandle,
    resource_name: String,
    dev_mode: Option<bool>,
) -> Result<OperationAck, String> {
    if resource_name.contains('/') || resource_name.contains('\\') || resource_name.contains("..") {
        return Err("Invalid bundled agent name".to_string());
    }

    let resource_dir = app_handle
        .path()
        .resource_dir()
        .map_err(|e| format!("Failed to get resource dir: {}", e))?;
    let package_file = bundled_agent_package_path(&resource_dir, &resource_name);
    tracing::info!(
        "[INSTALL_BUNDLED] Reading bundled package for {} from {:?}",
        resource_name,
        package_file
    );
    let package_bytes = std::fs::read(&package_file).map_err(|e| {
        format!(
            "Failed to read bundled agent package '{}': {}",
            package_file.display(),
            e
        )
    })?;
    tracing::info!(
        "[INSTALL_BUNDLED] Read {} bytes for {}, submitting install",
        package_bytes.len(),
        resource_name
    );

    // Resolve the agent_id up front so we can both submit the install
    // and verify the post-install inventory appearance against the same
    // canonical identifier (filename-independent — see
    // `read_agent_id_from_package`).
    let agent_id = read_agent_id_from_package(&package_bytes)?;
    tracing::info!(
        "[INSTALL_BUNDLED] Resolved agent_id={} for {}",
        agent_id,
        resource_name
    );

    let state_for_op = state.clone();
    let ack = submit_with_retry(&state, "INSTALL_BUNDLED", move || {
        // See INSTALL_AGENT — the retry wrapper borrows `state` while this
        // closure owns its pre-cloned copy. `submit_with_retry` is `FnMut`,
        // so we re-clone the bytes on every attempt instead of moving.
        let state = state_for_op.clone();
        let package_bytes = package_bytes.clone();
        let dev_mode = dev_mode.unwrap_or(true);
        async move {
            let client = state.gateway.read().await;
            client.install_agent(&package_bytes, dev_mode, None).await
        }
    })
    .await?;

    // See INSTALL_AGENT: wait for the install to land in the Gateway
    // inventory, keyed by the ack's instance id (ADR-073) — the package
    // id resolved from the manifest is only for logging / early validation.
    let wait_key = ack.instance_id.as_deref().ok_or_else(|| {
        format!(
            "Install of {} accepted (operation {}) but the Gateway reported no instance id",
            resource_name, ack.operation_id
        )
    })?;
    let client = state.gateway.read().await;
    client
        .wait_for_agent_installed(
            wait_key,
            std::time::Duration::from_secs(INSTALL_WAIT_SECS),
        )
        .await
        .map_err(|e| {
            format!(
                "Install of {} submitted (operation {}) but did not complete within {}s: {}",
                resource_name, ack.operation_id, INSTALL_WAIT_SECS, e
            )
        })?;

    Ok(ack)
}

/// Wait until the agent appears in the Gateway inventory.
///
/// ADR-059 §6: install is asynchronous — `install_bundled_agent`
/// returns the [`OperationAck`] immediately (HTTP 202) and the node
/// installs in the background; the Gateway aggregates the node's
/// retained `InstalledAgentInfo` into the inventory on completion. This
/// command polls `GET /api/agents/{id}` until the agent is visible —
/// the authoritative completion signal (no fixed sleeps, no phase
/// guessing).
#[tauri::command]
pub async fn wait_agent_installed(
    state: State<'_, AppState>,
    agent_id: String,
    timeout_secs: Option<u64>,
) -> Result<AgentDetailResponse, String> {
    let timeout = std::time::Duration::from_secs(timeout_secs.unwrap_or(120));
    let client = state.gateway.read().await;
    client
        .wait_for_agent_installed(&agent_id, timeout)
        .await
        .map_err(|e| e.to_string())
}

fn bundled_agent_package_path(
    resource_dir: &std::path::Path,
    resource_name: &str,
) -> std::path::PathBuf {
    let package_name = match resource_name {
        "software-architect-agent" => "com.acowork.software-architect.agent",
        "senior-engineer-agent" => "com.acowork.senior-engineer.agent",
        "quality-assurance-agent" => "com.acowork.quality-assurance.agent",
        "project-manager-agent" => "com.acowork.project-manager.agent",
        "product-manager-agent" => "com.acowork.product-manager.agent",
        "document-manager-agent" => "com.acowork.document-manager.agent",
        other => {
            return resource_dir
                .join("agent-packages")
                .join(format!("{}.agent", other));
        }
    };
    resource_dir.join("agent-packages").join(package_name)
}

/// Uninstall an agent
#[tauri::command]
pub async fn uninstall_agent(
    state: State<'_, AppState>,
    agent_id: String,
) -> Result<GenericMessageResponse, String> {
    let client = state.gateway.read().await;
    client
        .uninstall_agent(&agent_id)
        .await
        .map_err(|e| e.to_string())
}

/// Start an agent
#[tauri::command]
pub async fn start_agent(
    state: State<'_, AppState>,
    agent_id: String,
    dev_mode: Option<bool>,
) -> Result<GenericMessageResponse, String> {
    let client = state.gateway.read().await;
    client
        .start_agent(&agent_id, dev_mode.unwrap_or(false))
        .await
        .map_err(|e| e.to_string())
}

/// Stop an agent
#[tauri::command]
pub async fn stop_agent(
    state: State<'_, AppState>,
    agent_id: String,
) -> Result<GenericMessageResponse, String> {
    let client = state.gateway.read().await;
    client
        .stop_agent(&agent_id)
        .await
        .map_err(|e| e.to_string())
}

/// Restart an agent in debug mode (atomic in-Runtime switch, no process restart)
#[tauri::command]
pub async fn restart_agent_in_debug(
    state: State<'_, AppState>,
    agent_id: String,
) -> Result<GenericMessageResponse, String> {
    let client = state.gateway.read().await;
    client
        .restart_agent_in_debug(&agent_id)
        .await
        .map_err(|e| e.to_string())
}

/// Clone an agent (skeleton or full mode)
#[tauri::command]
pub async fn clone_agent(
    state: State<'_, AppState>,
    agent_id: String,
    new_agent_id: String,
    new_name: Option<String>,
    mode: Option<String>,
) -> Result<CloneResponse, String> {
    let client = state.gateway.read().await;
    client
        .clone_agent(
            &agent_id,
            &new_agent_id,
            new_name.as_deref().unwrap_or(""),
            &mode.unwrap_or_else(|| "skeleton".to_string()),
        )
        .await
        .map_err(|e| e.to_string())
}

/// Update the avatar / builtin_avatar fields in the agent's installed
/// `manifest.toml`. Used by the Publish wizard to bake the user's avatar
/// selection into the package before build.
///
/// Pass `Some("...")` to set, `Some("")` or omit to leave the field unchanged.
#[tauri::command]
pub async fn update_agent_manifest_avatar(
    state: State<'_, AppState>,
    agent_id: String,
    avatar: Option<String>,
    builtin_avatar: Option<String>,
) -> Result<serde_json::Value, String> {
    let client = state.gateway.read().await;
    client
        .update_agent_manifest_avatar(
            &agent_id,
            avatar.as_deref(),
            builtin_avatar.as_deref(),
        )
        .await
        .map_err(|e| e.to_string())
}

/// Upload a file into the agent's install directory. Used by the Publish
/// wizard to attach a custom avatar image before the manifest is updated to
/// reference it.
///
/// `relative_path` is the destination path inside the install dir
/// (e.g. "assets/avatar.png"). The server restricts accepted extensions to
/// image formats.
#[tauri::command]
pub async fn upload_agent_file(
    state: State<'_, AppState>,
    agent_id: String,
    relative_path: String,
    file_path: String,
) -> Result<serde_json::Value, String> {
    let bytes = std::fs::read(&file_path)
        .map_err(|e| format!("Failed to read file '{}': {}", file_path, e))?;
    let client = state.gateway.read().await;
    client
        .upload_agent_file(&agent_id, &relative_path, &bytes)
        .await
        .map_err(|e| e.to_string())
}

/// Upload a user avatar image file to the Gateway's `{data_dir}/assets/`.
///
/// The Gateway auto-generates the filename (avatar-01.png, avatar-02.png, etc.)
/// and returns the relative path.
#[tauri::command]
pub async fn upload_user_avatar_file(
    state: State<'_, AppState>,
    file_path: String,
) -> Result<serde_json::Value, String> {
    let bytes = std::fs::read(&file_path)
        .map_err(|e| format!("Failed to read file '{}': {}", file_path, e))?;
    let file_name = std::path::Path::new(&file_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("avatar.png");
    let client = state.gateway.read().await;
    client
        .upload_user_avatar_file(&bytes, file_name)
        .await
        .map_err(|e| e.to_string())
}
