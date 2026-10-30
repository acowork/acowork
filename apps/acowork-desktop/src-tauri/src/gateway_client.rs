//! Gateway HTTP client
//!
//! Encapsulates all Gateway HTTP API calls. The Desktop App communicates
//! with the platform primarily through Gateway HTTP API and the Debug
//! Protocol (HTTP RPC via the same Gateway + MQTT push). It references
//! `acowork_core::defaults` for shared constants (host, port, URL) to
//! avoid hardcoded duplication.

use acowork_core::defaults;
use acowork_core::error_codes::{StructuredErrorBody, StructuredErrorCode};
use anyhow::Result;
use reqwest::Response;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tauri::Emitter;
use tokio::sync::watch;

/// Default Gateway base URL (from shared core constants)
const DEFAULT_BASE_URL: &str = defaults::GATEWAY_HTTP_URL;

/// Minimal RFC 3986 percent-encoder for query string values. Used to embed
/// user-supplied file paths in the upload endpoint URL without pulling in
/// the `urlencoding` crate just for this single use case.
fn urlencoded(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                out.push_str(&format!("%{:02X}", byte));
            }
        }
    }
    out
}

/// Gateway error response format (matches Gateway's `ApiError` struct)
#[derive(Deserialize)]
struct GatewayErrorResponse {
    error: String,
    #[allow(dead_code)]
    code: u16,
    /// ADR-059 §6.3: structured protocol error body, present on
    /// `dependency_not_ready` / `resource_version_conflict` etc.
    structured: Option<StructuredErrorBody>,
}

/// Structured Gateway API error (ADR-059 §6.3).
///
/// Carries the machine-readable `structured.code` so callers can branch
/// on protocol codes instead of string-matching the human-readable
/// `error` text (the old "503 / never enrolled" heuristic).
#[derive(Debug, Clone)]
pub struct GatewayApiError {
    pub status: u16,
    pub error: String,
    pub structured: Option<StructuredErrorBody>,
}

impl GatewayApiError {
    /// `true` when the error carries the protocol code `dependency_not_ready`
    /// (bootstrap not READY yet — retry once phase = READY, ADR-059 §6.3).
    pub fn is_dependency_not_ready(&self) -> bool {
        matches!(
            self.structured.as_ref().map(|s| &s.code),
            Some(StructuredErrorCode::DependencyNotReady)
        )
    }

    /// `true` when the client's `expected_version` was stale (Gateway
    /// restarted since the snapshot was read). Currently unused — the
    /// Desktop's install path sends no `expected_version` — but kept as
    /// the symmetric counterpart of `is_dependency_not_ready` for future
    /// mutation callers (ADR-059 §6.3).
    #[allow(dead_code)]
    pub fn is_resource_version_conflict(&self) -> bool {
        matches!(
            self.structured.as_ref().map(|s| &s.code),
            Some(StructuredErrorCode::ResourceVersionConflict)
        )
    }
}

impl std::fmt::Display for GatewayApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.structured {
            Some(s) => write!(f, "Gateway {}: {} ({:?})", self.status, self.error, s.code),
            None => write!(f, "Gateway {}: {}", self.status, self.error),
        }
    }
}

impl std::error::Error for GatewayApiError {}

// ── Desktop ↔ Gateway account session (ADR-076 §决策 3) ────────────────

/// Tauri event asking the webview to rotate the token pair.
///
/// Emitted when the Gateway answers 401 to a request that carried the
/// mirrored access token. The webview owns the refresh token — it is the
/// only party allowed to call `/api/auth/refresh`.
pub const AUTH_REQUIRED_EVENT: &str = "gateway-auth-required";

/// How long a 401 waits for the webview's renewed token before the
/// original response is surfaced to the caller. Bounds the worst case
/// (webview gone, session already dropped) to one visible failure
/// instead of a hang.
const AUTH_RENEW_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Desktop-side mirror of the webview's account session.
///
/// Under `AUTH_MODE=multi_user` every Gateway route except the public
/// probes requires `Authorization: Bearer <access_token>`. The webview
/// owns the token pair and is the only party that may rotate it: refresh
/// rotation has no grace window, so two concurrent rotations would revoke
/// the whole family (ADR-076 §10.1 #3). Rust therefore *mirrors* the token
/// instead of managing it:
///
/// 1. [`GatewayClient`] attaches the mirrored token to every request,
/// 2. a 401 emits [`AUTH_REQUIRED_EVENT`] and waits for the webview to push
///    a newer token (or to clear it, meaning the session is over),
/// 3. the request is replayed once with the fresh token.
///
/// The wait is keyed on [`AuthSnapshot::epoch`]: a 401 carries the epoch of
/// the token it used, and only a *newer* epoch ends the wait — so a rotation
/// that landed before the 401 was observed resolves immediately, and N
/// parallel 401s ask the webview at most once.
pub struct GatewayAuth {
    tx: watch::Sender<AuthSnapshot>,
    /// Webview handle used for the refresh event. Installed by the Tauri
    /// setup hook; absent only before setup completes (and in tests).
    app: std::sync::OnceLock<tauri::AppHandle>,
}

#[derive(Debug, Clone, Default)]
struct AuthSnapshot {
    /// Set by the first non-empty [`GatewayAuth::set_access_token`] push: the
    /// webview speaks the account protocol. `local` deployments never push a
    /// token (ADR-076 §决策 12 — no token exists there), so a stray 401 there
    /// fails fast instead of waiting for a rotation that cannot come. An
    /// initial `None` (the bridge's boot-time mirror, before the mode is even
    /// resolved) therefore must not arm this path either.
    armed: bool,
    access_token: Option<String>,
    /// Bumped on every push — the ordering key for the 401 wait.
    epoch: u64,
    /// Highest epoch the webview has already been asked to renew. Dedupes
    /// the event across concurrent 401s from the same token.
    renew_requested_epoch: u64,
}

impl GatewayAuth {
    fn new() -> Self {
        Self {
            tx: watch::channel(AuthSnapshot::default()).0,
            app: std::sync::OnceLock::new(),
        }
    }

    /// Hand the client the webview handle used for [`AUTH_REQUIRED_EVENT`].
    /// Called once from the Tauri setup hook.
    pub fn install_app_handle(&self, app: tauri::AppHandle) {
        let _ = self.app.set(app);
    }

    /// Mirror the webview's access token (ADR-076 §决策 3).
    ///
    /// `None` clears it — logout, or a refresh that ended the session. The
    /// epoch advances either way so a waiting 401 is released with "no
    /// session" instead of blocking until the timeout.
    pub fn set_access_token(&self, token: Option<String>) {
        let token = token.filter(|t| !t.trim().is_empty());
        self.tx.send_modify(|s| {
            // Only a *present* token arms the replay path: the webview mirrors
            // `None` at boot (before the auth mode is resolved) and on logout.
            s.armed |= token.is_some();
            s.access_token = token;
            s.epoch += 1;
        });
    }

    /// Current token, its epoch, and whether the webview ever pushed a state.
    fn snapshot(&self) -> (Option<String>, u64, bool) {
        let s = self.tx.borrow();
        (s.access_token.clone(), s.epoch, s.armed)
    }

    /// The mirrored access token, if a session is active.
    ///
    /// Read by `connect_mqtt` in relay mode: the strict remote MQTT
    /// listener requires the account access token as the CONNECT
    /// password (and the token's `sub` as the client_id name segment).
    /// Unverified parsing of the token's payload is done at the call
    /// site — the BROKER verifies the signature, so a locally-mis-read
    /// `sub` only fails the broker's cross-check, never authenticates
    /// anyone.
    pub fn current_access_token(&self) -> Option<String> {
        self.snapshot().0
    }

    /// Ask the webview to rotate once and wait briefly for the new token.
    ///
    /// Exists for consumers that **cannot answer a 401**: the relay-mode
    /// MQTT CONNECT carries the access token as its password, and the
    /// strict remote listener drops a bad credential WITHOUT CONNACK
    /// (rumqttd `InvalidAuth`). The client therefore sees a bare
    /// "Connection closed by peer abruptly" and would retry the same
    /// expired password forever (see `MqttCredentials::refresher`).
    ///
    /// Same contract as the 401 replay path: Rust only *asks*; the
    /// webview performs the single-flight rotation (ADR-076 §决策 3) and
    /// pushes the result back through `set_access_token`.
    ///
    /// Returns the freshest token the mirror holds after the wait — the
    /// renewed one on success, the previous one when the session is not
    /// armed (local mode / no account) or the webview did not answer
    /// within [`AUTH_RENEW_TIMEOUT`].
    pub async fn renew_access_token(&self) -> Option<String> {
        let (token, epoch, armed) = self.snapshot();
        if !armed {
            // `local` deployments never push a token, so there is nobody
            // to ask — keep whatever we have (usually `None`).
            return token;
        }
        match self.renew(epoch).await {
            Some(fresh) => Some(fresh),
            None => self.snapshot().0,
        }
    }

    /// Wait for a token newer than `stale_epoch`, asking the webview to
    /// rotate once. Returns the renewed token, or `None` when the session is
    /// gone / the webview did not answer within [`AUTH_RENEW_TIMEOUT`].
    async fn renew(&self, stale_epoch: u64) -> Option<String> {
        // Fast path: something already rotated past the token we used —
        // either another request's renewal or the webview's own 401 path.
        let (token, epoch, _) = self.snapshot();
        if epoch > stale_epoch {
            return token;
        }

        // Ask the webview, at most once per observed epoch.
        let ask = {
            let mut ask = false;
            self.tx.send_modify(|s| {
                if s.renew_requested_epoch < stale_epoch {
                    s.renew_requested_epoch = stale_epoch;
                    ask = true;
                }
            });
            ask
        };
        if ask {
            match self.app.get() {
                Some(app) => {
                    if let Err(e) = app.emit(AUTH_REQUIRED_EVENT, ()) {
                        tracing::warn!(error = %e, "failed to emit {AUTH_REQUIRED_EVENT}");
                    }
                }
                None => tracing::warn!(
                    "account 401 but no webview handle installed — cannot renew the token"
                ),
            }
        }

        let mut rx = self.tx.subscribe();
        let wait = async {
            loop {
                if self.snapshot().1 > stale_epoch {
                    return;
                }
                if rx.changed().await.is_err() {
                    return;
                }
            }
        };
        if tokio::time::timeout(AUTH_RENEW_TIMEOUT, wait).await.is_err() {
            tracing::warn!(
                "account token renewal timed out after {}s — surfacing the 401",
                AUTH_RENEW_TIMEOUT.as_secs()
            );
            return None;
        }

        let (token, epoch, _) = self.snapshot();
        if epoch > stale_epoch { token } else { None }
    }
}

/// Unified response parser for all Gateway API calls.
///
/// - Success (2xx): deserializes the response body into `T`.
/// - Failure: extracts the Gateway `ApiError` JSON (human-readable
///   `error` + structured protocol body) into a [`GatewayApiError`];
///   falls back to raw text when the body is not JSON.
async fn parse_gateway_response<T: DeserializeOwned>(resp: Response) -> Result<T> {
    let status = resp.status();
    if status.is_success() {
        resp.json()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to parse Gateway response: {}", e))
    } else {
        let text = resp.text().await.unwrap_or_default();
        match serde_json::from_str::<GatewayErrorResponse>(&text) {
            Ok(err) => Err(anyhow::Error::new(GatewayApiError {
                status: status.as_u16(),
                error: err.error,
                structured: err.structured,
            })),
            Err(_) => anyhow::bail!("Gateway {}: {}", status, text),
        }
    }
}

/// Build the multipart body of `POST /api/agents/install`.
///
/// A free function instead of inline code inside
/// [`GatewayClient::install_agent`] because that request is rebuilt for the
/// transparent 401 replay ([`GatewayClient::send`]) and a multipart body
/// cannot be cloned.
fn install_form(
    package_bytes: &[u8],
    dev_mode: bool,
    node_id: Option<&str>,
) -> Result<reqwest::multipart::Form> {
    let mut form = reqwest::multipart::Form::new()
        .part(
            "package",
            reqwest::multipart::Part::bytes(package_bytes.to_vec())
                .file_name("package.agent")
                .mime_str("application/octet-stream")?,
        )
        .text("dev_mode", dev_mode.to_string());
    if let Some(node) = node_id {
        form = form.text("node_id", node.to_string());
    }
    Ok(form)
}

/// Gateway HTTP client
pub struct GatewayClient {
    client: reqwest::Client,
    base_url: String,
    /// Account-session mirror handed to the Tauri command layer — see
    /// [`GatewayAuth`]. Every request built through [`GatewayClient::send`]
    /// carries its token and replays once after a renewal.
    auth: Arc<GatewayAuth>,
}

/// Minimal mirror of Gateway's `SystemStatusResponse` — only the fields
/// the Desktop actually consumes (ADR-055 D3 §6.3: `mqtt_port` for
/// dynamic broker discovery; Phase 5a: MQTT CONNECT credentials when
/// `mqtt.auth_enabled` is on). Kept local so the Desktop never depends
/// on the `acowork-gateway` crate.
#[derive(Debug, Deserialize)]
pub struct SystemStatusInfo {
    pub mqtt_port: u16,
    /// ADR-055 Phase 5a: MQTT CONNECT username — present only when
    /// the Gateway has `mqtt.auth_enabled` on (None otherwise).
    #[serde(default)]
    pub mqtt_username: Option<String>,
    /// ADR-055 Phase 5a: MQTT CONNECT password (http_token) — present
    /// only when the Gateway has `mqtt.auth_enabled` on (None otherwise).
    #[serde(default)]
    pub mqtt_password: Option<String>,
}

impl GatewayClient {
    /// Create a new GatewayClient with the default base URL
    pub fn new() -> Self {
        Self::with_base_url(DEFAULT_BASE_URL.to_string())
    }

    /// Create a new GatewayClient with a custom base URL
    pub fn with_base_url(base_url: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("failed to build reqwest client");
        Self {
            client,
            base_url,
            auth: Arc::new(GatewayAuth::new()),
        }
    }

    /// The account-session mirror this client attaches to its requests.
    ///
    /// `AppState` holds the same handle, so the Tauri command layer can
    /// mirror the webview's token without reaching into the HTTP client.
    pub fn auth_handle(&self) -> Arc<GatewayAuth> {
        self.auth.clone()
    }

    /// Get the current base URL
    #[allow(dead_code)]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Update the base URL (e.g., from settings)
    #[allow(dead_code)]
    pub fn set_base_url(&mut self, url: String) {
        self.base_url = url;
    }

    /// Raw request builder against the Gateway, for the call sites that hit
    /// an endpoint this client has no method for (`/api/agents/ensure`,
    /// `/api/debug/mqtt/*`). Pair it with [`GatewayClient::send`] so the
    /// request still carries the account credentials and the transparent
    /// 401 replay.
    pub fn request(&self, method: reqwest::Method, url: &str) -> reqwest::RequestBuilder {
        self.client.request(method, url)
    }

    /// Issue a Gateway request with the caller's account credentials
    /// attached, replaying it once after a transparent token renewal
    /// (ADR-076 §决策 3).
    ///
    /// `build` is called at most twice rather than taking a ready
    /// `RequestBuilder`: install / upload bodies are multipart streams,
    /// which reqwest cannot clone, so a replay has to reconstruct the
    /// request. Callers therefore pass a closure over owned data.
    pub async fn send<F>(&self, build: F) -> Result<Response>
    where
        F: Fn() -> Result<reqwest::RequestBuilder>,
    {
        let (token, epoch, armed) = self.auth.snapshot();

        let resp = match &token {
            Some(t) => build()?.bearer_auth(t).send().await?,
            None => build()?.send().await?,
        };

        // `local` deployments never push a token, so a 401 there is not an
        // expired session — fail fast instead of waiting for a rotation.
        if !armed || resp.status() != reqwest::StatusCode::UNAUTHORIZED {
            return Ok(resp);
        }

        // 401 on a request that carried the session: the access token
        // expired or was revoked. Ask the webview for a newer one and replay
        // exactly once — a second 401 is a real authorization failure
        // (disabled account, out-of-scope resource) and belongs to the
        // caller.
        let Some(fresh) = self.auth.renew(epoch).await else {
            return Ok(resp);
        };
        tracing::debug!("Gateway 401 — replaying the request with the renewed access token");
        build()?.bearer_auth(fresh).send().await.map_err(Into::into)
    }

    // ── Agent Management ───────────────────────────────────────────────

    /// `GET /api/status` — system status (ADR-055 D3 §6.3).
    ///
    /// Used by `connect_mqtt` to derive the MQTT broker port dynamically
    /// instead of assuming the default 19875 (L3-6 residual gap —
    /// ADR-058 W4 fixed the host derivation; this closes the port half).
    pub async fn system_status(&self) -> Result<SystemStatusInfo> {
        let resp = self
            .send(|| Ok(self.client.get(format!("{}/api/status", self.base_url))))
            .await?;
        parse_gateway_response(resp).await
    }

    /// `GET /api/agents`
    pub async fn list_agents(&self) -> Result<Vec<AgentListEntry>> {
        let resp = self
            .send(|| Ok(self.client.get(format!("{}/api/agents", self.base_url))))
            .await?;
        parse_gateway_response(resp).await
    }

    /// `GET /api/agents/:id`
    pub async fn get_agent_detail(&self, agent_id: &str) -> Result<AgentDetailResponse> {
        let resp = self
            .send(|| Ok(self.client.get(format!("{}/api/agents/{}", self.base_url, agent_id))))
            .await?;
        parse_gateway_response(resp).await
    }

    /// `POST /api/agents/install` — upload .agent package via multipart
    ///
    /// `node_id` is the target node (ADR-055 §6.13.3); `None` lets the
    /// Gateway default to `local`.
    ///
    /// ADR-059 §6: answers HTTP 202 with an [`OperationAck`]; the actual
    /// install runs asynchronously on the node and completes with a
    /// NodeEvent reply whose `request_id` equals the ack's
    /// `operation_id`. A structured `dependency_not_ready` error is
    /// returned (HTTP 409) while the node control plane has not
    /// announced readiness.
    pub async fn install_agent(
        &self,
        package_bytes: &[u8],
        dev_mode: bool,
        node_id: Option<&str>,
    ) -> Result<OperationAck> {
        let url = format!("{}/api/agents/install", self.base_url);
        let resp = self
            .send(|| {
                Ok(self
                    .client
                    .post(&url)
                    .multipart(install_form(package_bytes, dev_mode, node_id)?))
            })
            .await?;
        parse_gateway_response(resp).await
    }

    /// ADR-059 §6 — wait until the agent appears in the Gateway
    /// inventory (`GET /api/agents/{id}` returns 200).
    ///
    /// The Gateway aggregates the node's retained `InstalledAgentInfo`
    /// on install completion, so a 200 here is the authoritative
    /// "installed" signal — no fixed sleeps, no phase guessing. Used by
    /// the onboarding flow to wait out the async install operations
    /// submitted in parallel.
    pub async fn wait_for_agent_installed(
        &self,
        agent_id: &str,
        timeout: std::time::Duration,
    ) -> Result<AgentDetailResponse> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let resp = self
                .send(|| Ok(self.client.get(format!("{}/api/agents/{}", self.base_url, agent_id))))
                .await?;
            if resp.status().is_success() {
                return parse_gateway_response(resp).await;
            }
            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!(
                    "Agent '{}' did not appear in the Gateway inventory within {:.0}s",
                    agent_id,
                    timeout.as_secs_f64()
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
    }

    /// `POST /api/agents/:id/manifest/avatar` — write avatar / builtin_avatar
    /// fields into the agent's installed manifest.toml.
    ///
    /// Pass `Some("...")` to set, `Some("")` or `None` to clear (omit to leave
    /// unchanged). Used by the Publish wizard to bake the user's avatar
    /// selection into the package before build.
    pub async fn update_agent_manifest_avatar(
        &self,
        agent_id: &str,
        avatar: Option<&str>,
        builtin_avatar: Option<&str>,
    ) -> Result<serde_json::Value> {
        let mut body = serde_json::Map::new();
        if let Some(v) = avatar {
            body.insert("avatar".to_string(), serde_json::Value::String(v.to_string()));
        }
        if let Some(v) = builtin_avatar {
            body.insert(
                "builtin_avatar".to_string(),
                serde_json::Value::String(v.to_string()),
            );
        }
        let resp = self
            .send(|| {
                Ok(self
                    .client
                    .post(format!(
                        "{}/api/agents/{}/manifest/avatar",
                        self.base_url, agent_id
                    ))
                    .json(&body))
            })
            .await?;
        parse_gateway_response(resp).await
    }

    /// `POST /api/agents/:id/manifest/file?path=<relative>` — write a single
    /// file into the agent's install directory. Used by the Publish wizard to
    /// upload a custom avatar image. The path is restricted to image
    /// extensions server-side.
    pub async fn upload_agent_file(
        &self,
        agent_id: &str,
        relative_path: &str,
        bytes: &[u8],
    ) -> Result<serde_json::Value> {
        let url = format!(
            "{}/api/agents/{}/manifest/file?path={}",
            self.base_url,
            agent_id,
            urlencoded(relative_path)
        );
        let resp = self
            .send(|| {
                let form = reqwest::multipart::Form::new().part(
                    "file",
                    reqwest::multipart::Part::bytes(bytes.to_vec())
                        .file_name("upload")
                        .mime_str("application/octet-stream")?,
                );
                Ok(self.client.post(&url).multipart(form))
            })
            .await?;
        parse_gateway_response(resp).await
    }

    /// `POST /api/user/avatar-file` — upload a user avatar image.
    /// Returns `{ "relative_path": "assets/avatar-01.png" }`.
    pub async fn upload_user_avatar_file(
        &self,
        bytes: &[u8],
        file_name: &str,
    ) -> Result<serde_json::Value> {
        let url = format!("{}/api/user/avatar-file", self.base_url);
        let resp = self
            .send(|| {
                let form = reqwest::multipart::Form::new().part(
                    "file",
                    reqwest::multipart::Part::bytes(bytes.to_vec())
                        .file_name(file_name.to_string())
                        .mime_str("application/octet-stream")?,
                );
                Ok(self.client.post(&url).multipart(form))
            })
            .await?;
        parse_gateway_response(resp).await
    }

    /// `DELETE /api/agents/:id`
    pub async fn uninstall_agent(&self, agent_id: &str) -> Result<GenericMessageResponse> {
        let resp = self
            .send(|| Ok(self.client.delete(format!("{}/api/agents/{}", self.base_url, agent_id))))
            .await?;
        parse_gateway_response(resp).await
    }

    /// `POST /api/agents/:id/start`
    pub async fn start_agent(
        &self,
        agent_id: &str,
        dev_mode: bool,
    ) -> Result<GenericMessageResponse> {
        let body = serde_json::json!({ "dev_mode": dev_mode });
        let resp = self
            .send(|| {
                Ok(self
                    .client
                    .post(format!("{}/api/agents/{}/start", self.base_url, agent_id))
                    .json(&body))
            })
            .await?;
        parse_gateway_response(resp).await
    }

    /// `POST /api/agents/:id/stop`
    pub async fn stop_agent(&self, agent_id: &str) -> Result<GenericMessageResponse> {
        let resp = self
            .send(|| {
                Ok(self
                    .client
                    .post(format!("{}/api/agents/{}/stop", self.base_url, agent_id)))
            })
            .await?;
        parse_gateway_response(resp).await
    }

    /// `POST /api/agents/:id/restart-debug`
    pub async fn restart_agent_in_debug(&self, agent_id: &str) -> Result<GenericMessageResponse> {
        let resp = self
            .send(|| {
                Ok(self.client.post(format!(
                    "{}/api/agents/{}/restart-debug",
                    self.base_url, agent_id
                )))
            })
            .await?;
        parse_gateway_response(resp).await
    }

    // ── Debug Protocol RPC (ADR-048 D6) ────────────────────────────────────

    /// ADR-048: generic Debug Protocol RPC relay.
    ///
    /// Sends `METHOD {base}/api/agents/{agent_id}/debug/{path}` to the
    /// Gateway, which reverse-proxies to the Runtime's `/api/debug/{path}`
    /// (Gateway `http/proxy.rs`, one wildcard route for every endpoint).
    /// Keeping the client generic mirrors that design: debug endpoints
    /// added on the Runtime need no Desktop Rust change, only a call
    /// site in `debugStore.ts`.
    ///
    /// The Runtime answers with the `{ ok, data?, error? }` envelope
    /// (`acowork-runtime/src/http/debug.rs`); this method unwraps it:
    /// `ok: true` -> `Ok(data)`, `ok: false` -> `Err(message)`. Errors
    /// from the Gateway itself (e.g. 502 when the Runtime is down) use
    /// its own `ApiError` shape and surface via the text fallback.
    pub async fn debug_rpc(
        &self,
        agent_id: &str,
        method: &str,
        path: &str,
        query: Option<&std::collections::HashMap<String, String>>,
        body: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value> {
        let mut url = format!("{}/api/agents/{}/debug/{}", self.base_url, agent_id, path);
        if let Some(params) = query {
            let qs = params
                .iter()
                .map(|(k, v)| format!("{}={}", urlencoded(k), urlencoded(v)))
                .collect::<Vec<_>>()
                .join("&");
            if !qs.is_empty() {
                url.push('?');
                url.push_str(&qs);
            }
        }

        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|e| anyhow::anyhow!("invalid HTTP method '{}': {}", method, e))?;
        let resp = self
            .send(|| {
                let mut req = self.client.request(method.clone(), &url);
                if let Some(json) = body {
                    req = req.json(json);
                }
                Ok(req)
            })
            .await?;
        let status = resp.status();
        let text = resp.text().await?;

        // Runtime debug envelope: `{ ok, data?, error? { code, message } }`
        #[derive(Deserialize)]
        struct DebugErrorBody {
            #[allow(dead_code)]
            code: i32,
            message: String,
        }
        #[derive(Deserialize)]
        struct DebugEnvelope {
            ok: bool,
            data: Option<serde_json::Value>,
            error: Option<DebugErrorBody>,
        }
        match serde_json::from_str::<DebugEnvelope>(&text) {
            Ok(env) if env.ok => Ok(env.data.unwrap_or(serde_json::Value::Null)),
            Ok(env) => {
                let message = env
                    .error
                    .map(|e| e.message)
                    .unwrap_or_else(|| "debug RPC failed".to_string());
                anyhow::bail!("Debug {}: {}", status, message)
            }
            // Not the debug envelope - Gateway-level error (ApiError or
            // raw body). Surface status + body verbatim.
            Err(_) => anyhow::bail!("Gateway {}: {}", status, text),
        }
    }

    // ── Clone ──────────────────────────────────────────────────────────

    /// `POST /api/agents/:id/clone`
    pub async fn clone_agent(
        &self,
        agent_id: &str,
        new_agent_id: &str,
        new_name: &str,
        mode: &str,
    ) -> Result<CloneResponse> {
        let body = serde_json::json!({
            "new_agent_id": new_agent_id,
            // Empty = keep the source name; the node suffixes "(clone)".
            "new_name": new_name,
            "mode": mode,
        });
        let resp = self
            .send(|| {
                Ok(self
                    .client
                    .post(format!("{}/api/agents/{}/clone", self.base_url, agent_id))
                    .json(&body))
            })
            .await?;
        parse_gateway_response(resp).await
    }

    // ── Publish ────────────────────────────────────────────────────────

    /// `POST /api/agents/:id/publish/prepare`
    pub async fn prepare_publish(
        &self,
        agent_id: &str,
        clean: bool,
    ) -> Result<PreparePublishResponse> {
        let body = serde_json::json!({ "clean": clean });
        let resp = self
            .send(|| {
                Ok(self
                    .client
                    .post(format!(
                        "{}/api/agents/{}/publish/prepare",
                        self.base_url, agent_id
                    ))
                    .json(&body))
            })
            .await?;
        parse_gateway_response(resp).await
    }

    /// `POST /api/agents/:id/publish/build`
    pub async fn build_publish(
        &self,
        agent_id: &str,
        sign: bool,
        key_dir: Option<&str>,
    ) -> Result<BuildPublishResponse> {
        let mut body = serde_json::json!({ "sign": sign });
        if let Some(dir) = key_dir {
            body["key_dir"] = serde_json::Value::String(dir.to_string());
        }
        let resp = self
            .send(|| {
                Ok(self
                    .client
                    .post(format!(
                        "{}/api/agents/{}/publish/build",
                        self.base_url, agent_id
                    ))
                    .json(&body))
            })
            .await?;
        parse_gateway_response(resp).await
    }

    /// `POST /api/agents/:id/publish/export`
    pub async fn export_package(&self, agent_id: &str) -> Result<ExportPackageResponse> {
        let resp = self
            .send(|| {
                Ok(self.client.post(format!(
                    "{}/api/agents/{}/publish/export",
                    self.base_url, agent_id
                )))
            })
            .await?;
        parse_gateway_response(resp).await
    }

    // ── Attachment upload (ADR-046) ─────────────────────────────────────

    /// `POST /api/agents/:agent_id/sessions/:session_id/files` — multipart
    /// upload of a single document or image. The runtime persists the blob
    /// at `<work_dir>/files/<document_id>` and returns the metadata envelope.
    ///
    /// `format` is the lowercase extension without dot (e.g. "pdf", "png").
    /// `width` / `height` are optional and should be supplied for image
    /// uploads (the desktop reads them via `new Image()`); the runtime
    /// accepts their absence for non-image blobs.
    ///
    /// Replaces the legacy `/api/sessions/:sid/documents` route (deleted as
    /// part of ADR-046 §Backend Storage).
    #[allow(clippy::too_many_arguments)]
    pub async fn upload_file(
        &self,
        agent_id: &str,
        session_id: &str,
        file_path: &str,
        format: &str,
        width: Option<u32>,
        height: Option<u32>,
    ) -> Result<FileUploadResponse> {
        let file_name = std::path::Path::new(file_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("attachment.bin");

        // Final existence pre-check before hitting `tokio::fs::read`.
        // `get_clipboard_file_paths` already filters non-existent paths,
        // but a file may be deleted between that call and now (race),
        // and the desktop dialog (`open`) could in theory hand back a
        // stale path on some platforms. We surface a clean error
        // message instead of letting `tokio::fs::read` produce the
        // raw `os error 53/161` text the user previously saw in the
        // console.
        let path = std::path::Path::new(file_path);
        if !path.exists() {
            return Err(anyhow::anyhow!("File not found: {}", file_path));
        }

        let file_bytes = tokio::fs::read(file_path).await?;

        let url = format!(
            "{}/api/agents/{}/sessions/{}/files",
            self.base_url,
            urlencoded(agent_id),
            urlencoded(session_id),
        );
        // Backend handler accepts `file` + optional `format` / `width` /
        // `height` as flat multipart fields. Unknown fields are ignored.
        //
        // The form is rebuilt on each attempt: `send` replays the request
        // once after a token renewal, and multipart bodies cannot be cloned.
        let resp = self
            .send(|| {
                let part = reqwest::multipart::Part::bytes(file_bytes.clone())
                    .file_name(file_name.to_string())
                    .mime_str("application/octet-stream")?;
                let mut form = reqwest::multipart::Form::new()
                    .part("file", part)
                    .text("format", format.to_string());
                if let Some(w) = width {
                    form = form.text("width", w.to_string());
                }
                if let Some(h) = height {
                    form = form.text("height", h.to_string());
                }
                Ok(self.client.post(&url).multipart(form))
            })
            .await?;
        parse_gateway_response(resp).await
    }

    // ── Vault ──────────────────────────────────────────────────────────

    /// `GET /api/providers`
    pub async fn list_keys(&self) -> Result<Vec<VaultKeyEntry>> {
        let resp = self
            .send(|| Ok(self.client.get(format!("{}/api/providers", self.base_url))))
            .await?;
        parse_gateway_response(resp).await
    }

    /// `POST /api/providers` (with optional base_url, default_model, models, model_capabilities, and custom flag)
    ///
    /// ADR-059 §7.3: the Gateway now answers with an [`OperationAck`]
    /// (operation_id / state / resource_version / terminal_error) on
    /// `POST /api/providers` — *not* the legacy `MessageResponse`. Older
    /// Desktop builds parsed this as `GenericMessageResponse` and the
    /// `reqwest::json` decoder silently rejected it as
    /// `"error decoding response body"`, leaving the user with the
    /// "Failed to parse Gateway response" alert even though the
    /// provider had been stored successfully. `PUT`/`DELETE`
    /// `/api/providers/:provider` still answer with `MessageResponse`.
    #[allow(clippy::too_many_arguments)]
    pub async fn add_key(
        &self,
        provider: &str,
        keys: &[crate::commands::vault::AddProviderKey],
        base_url: Option<&str>,
        default_model: Option<&str>,
        models: Option<&[String]>,
        model_capabilities: &HashMap<String, ModelCapabilities>,
        compact_model: Option<&str>,
        custom: bool,
        model_protocols: &ModelProtocolIntent,
        model_base_urls: &HashMap<String, String>,
    ) -> Result<ProviderAddResult> {
        let keys_json: Vec<serde_json::Value> = keys
            .iter()
            .map(|k| {
                serde_json::json!({
                    "alias": k.alias,
                    "key": k.key,
                })
            })
            .collect();
        let mut body = serde_json::json!({
            "provider": provider,
            "keys": keys_json,
        });
        // Preserve the legacy single-key field for old Gateway builds
        // that haven't been upgraded yet — first key only.
        if let Some(first) = keys.first() {
            body["key"] = serde_json::Value::String(first.key.clone());
        }
        if custom {
            body["custom"] = serde_json::Value::Bool(true);
        }
        if let Some(url) = base_url {
            body["base_url"] = serde_json::Value::String(url.to_string());
        }
        // Send models list if provided; otherwise fallback to default_model
        if let Some(models_list) = models {
            if !models_list.is_empty() {
                body["models"] = serde_json::Value::Array(
                    models_list
                        .iter()
                        .map(|m| serde_json::Value::String(m.clone()))
                        .collect(),
                );
            }
        } else if let Some(model) = default_model {
            body["default_model"] = serde_json::Value::String(model.to_string());
        }
        // Send model_capabilities if not empty
        if !model_capabilities.is_empty() {
            body["model_capabilities"] =
                serde_json::to_value(model_capabilities).unwrap_or_else(|e| {
                    eprintln!("serde_json::to_value failed for model_capabilities: {e}");
                    serde_json::to_value(model_capabilities)
                        .expect("model_capabilities serialization failed twice")
                });
        }
        // Send compact_model if provided
        if let Some(cm) = compact_model {
            body["compact_model"] = serde_json::Value::String(cm.to_string());
        }
        // ADR-103 D1/D10.3: per-model protocol intent + base URL override.
        // A `null` value is meaningful (clear the selection), so only an empty
        // map is skipped.
        if !model_protocols.is_empty() {
            body["model_protocols"] = serde_json::to_value(model_protocols)
                .expect("model_protocols serialization cannot fail");
        }
        if !model_base_urls.is_empty() {
            body["model_base_urls"] = serde_json::to_value(model_base_urls)
                .expect("model_base_urls serialization cannot fail");
        }
        let resp = self
            .send(|| {
                Ok(self
                    .client
                    .post(format!("{}/api/providers", self.base_url))
                    .json(&body))
            })
            .await?;
        parse_gateway_response(resp).await
    }

    /// `DELETE /api/providers/:provider` (legacy: drops every account for the
/// provider) or `DELETE /api/providers/:provider/keys/:account_id`
/// (drops a single account).
    pub async fn remove_key(
        &self,
        provider: &str,
        account_id: Option<&str>,
    ) -> Result<GenericMessageResponse> {
        let url = match account_id {
            Some(aid) => format!("{}/api/providers/{}/keys/{}", self.base_url, provider, aid),
            None => format!("{}/api/providers/{}", self.base_url, provider),
        };
        let resp = self.send(|| Ok(self.client.delete(&url))).await?;
        parse_gateway_response(resp).await
    }

    /// `PATCH /api/providers/:provider/keys/:account_id` — edit an existing
    /// account's alias and/or key. None on either field is a no-op (the
    /// Gateway returns 400 if both are missing — propagate that error).
    /// Used by the harness edit dialog so users can rename or rotate keys
    /// without losing the account's UUID.
    pub async fn update_account_key(
        &self,
        provider: &str,
        account_id: &str,
        alias: Option<&str>,
        key: Option<&str>,
    ) -> Result<GenericMessageResponse> {
        let mut body = serde_json::Map::new();
        if let Some(a) = alias {
            body.insert("alias".into(), serde_json::Value::String(a.to_string()));
        }
        if let Some(k) = key {
            body.insert("key".into(), serde_json::Value::String(k.to_string()));
        }
        let url = format!(
            "{}/api/providers/{}/keys/{}",
            self.base_url, provider, account_id
        );
        let body = serde_json::Value::Object(body);
        let resp = self
            .send(|| Ok(self.client.patch(&url).json(&body)))
            .await?;
        parse_gateway_response(resp).await
    }

    /// `PUT /api/providers/:provider` (supports partial updates — key is optional)
    ///
    /// If `key` is None, the existing API key is preserved on the Gateway side.
    /// This prevents the masked key_preview from overwriting the real key.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_key(
        &self,
        provider: &str,
        keys: &[crate::commands::vault::AddProviderKey],
        base_url: Option<&str>,
        default_model: Option<&str>,
        models: Option<&[String]>,
        model_capabilities: &HashMap<String, ModelCapabilities>,
        compact_model: Option<&str>,
        model_protocols: &ModelProtocolIntent,
        model_base_urls: &HashMap<String, String>,
    ) -> Result<ProviderUpdateResult> {
        let mut body = serde_json::Map::new();
        // Multi-account entries. The Gateway appends each non-empty
        // entry as a fresh vault account; existing accounts are
        // preserved.
        if !keys.is_empty() {
            let arr: Vec<serde_json::Value> = keys
                .iter()
                .map(|k| {
                    serde_json::json!({
                        "alias": k.alias,
                        "key": k.key,
                    })
                })
                .collect();
            body.insert("keys".to_string(), serde_json::Value::Array(arr));
        }
        if let Some(url) = base_url {
            body.insert(
                "base_url".to_string(),
                serde_json::Value::String(url.to_string()),
            );
        }
        // Send models list if provided; otherwise fallback to default_model
        if let Some(models_list) = models
            && !models_list.is_empty()
        {
            body.insert(
                "models".to_string(),
                serde_json::Value::Array(
                    models_list
                        .iter()
                        .map(|m| serde_json::Value::String(m.clone()))
                        .collect(),
                ),
            );
        } else if let Some(model) = default_model {
            body.insert(
                "default_model".to_string(),
                serde_json::Value::String(model.to_string()),
            );
        }
        // Send model_capabilities if not empty
        if !model_capabilities.is_empty() {
            body.insert(
                "model_capabilities".to_string(),
                serde_json::to_value(model_capabilities).unwrap_or_else(|e| {
                    eprintln!("serde_json::to_value failed for model_capabilities: {e}");
                    serde_json::to_value(model_capabilities)
                        .expect("model_capabilities serialization failed twice")
                }),
            );
        }
        // Send compact_model if provided
        if let Some(cm) = compact_model {
            body.insert(
                "compact_model".to_string(),
                serde_json::Value::String(cm.to_string()),
            );
        }
        // ADR-103 D1/D10.3: per-model protocol intent + base URL override.
        // An empty map means "leave every selection untouched"; an explicit
        // `null` value clears one model's selection.
        if !model_protocols.is_empty() {
            body.insert(
                "model_protocols".to_string(),
                serde_json::to_value(model_protocols)
                    .expect("model_protocols serialization cannot fail"),
            );
        }
        if !model_base_urls.is_empty() {
            body.insert(
                "model_base_urls".to_string(),
                serde_json::to_value(model_base_urls)
                    .expect("model_base_urls serialization cannot fail"),
            );
        }
        let resp = self
            .send(|| {
                Ok(self
                    .client
                    .put(format!("{}/api/providers/{}", self.base_url, provider))
                    .json(&body))
            })
            .await?;
        parse_gateway_response(resp).await
    }

    // ── Config ─────────────────────────────────────────────
    //
    // Config and log management are now handled by the frontend directly
    // via fetch() to the Gateway HTTP API (getGatewayUrl()).
}

// ── API response types ──────────────────────────────────────────────

/// Agent list entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentListEntry {
    /// ADR-073: instance identity (UUID v4, immutable). `None` when the
    /// Gateway predates the field (legacy single-instance shape) — the
    /// frontend falls back to `agent_id`.
    pub instance_id: Option<String>,
    pub agent_id: String,
    /// ADR-073: current location (node hosting this instance).
    pub node_id: Option<String>,
    pub name: String,
    pub display_name: Option<String>,
    pub role: Option<String>,
    pub avatar: Option<String>,
    /// Builtin avatar index declared in the manifest (e.g. "icon-05").
    pub builtin_avatar: Option<String>,
    pub version: String,
    /// Distributed liveness verdict: whether the Runtime's MQTT session is
    /// reachable at the broker level (protobuf `AgentStatus::online`).
    /// Topology independent — never a process/PID probe. Mirrors the
    /// Gateway's `AgentListResponse.alive`; the only field the UI gates
    /// "agent is alive" on.
    pub alive: bool,
    /// ADR-085: lifecycle capability state ("unspecified" | "offline" |
    /// "starting" | "http_ready" | "sessions_ready" | "failed") —
    /// replaces the retired `ready` bool.
    pub lifecycle: String,
    /// ADR-085: failure reason when lifecycle == "failed".
    #[serde(default)]
    pub lifecycle_detail: Option<String>,
    pub dev_mode: bool,
    /// Whether DevMode is live right now (ADR-048 follow-up; can be enabled
    /// at runtime via POST /api/agents/{id}/debug/enable without restarting
    /// the agent). The Gateway always sends it ("enabled"/"disabled");
    /// `None` only if an older Gateway omits the field — the frontend treats
    /// that exactly like "disabled".
    pub debug_state: Option<String>,
    pub debug_port: Option<u16>,
    /// Last user interaction time (RFC 3339).  Drives the frontend auto-select
    /// logic: on webview reload the agent with the largest value is selected.
    pub last_interaction_at: Option<String>,
}

/// Agent detail response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDetailResponse {
    /// ADR-073: instance identity (UUID v4, immutable).
    pub instance_id: Option<String>,
    pub agent_id: String,
    /// ADR-073: current location (mutable on migration).
    pub node_id: Option<String>,
    pub name: String,
    pub display_name: Option<String>,
    pub role: Option<String>,
    pub avatar: Option<String>,
    /// Builtin avatar index declared in the manifest (e.g. "icon-05").
    pub builtin_avatar: Option<String>,
    pub version: String,
    pub description: String,
    pub author: String,
    pub install_path: String,
    /// Distributed liveness verdict — same semantics as
    /// [`AgentListEntry::alive`] (MQTT network signal, never a PID probe).
    pub alive: bool,
    /// ADR-085: lifecycle capability state — see [`AgentListEntry::lifecycle`].
    pub lifecycle: String,
    #[serde(default)]
    pub lifecycle_detail: Option<String>,
    pub pid: Option<u32>,
    pub started_at: Option<String>,
    /// Whether DevMode is live right now (ADR-048 follow-up; can be enabled
    /// at runtime via POST /api/agents/{id}/debug/enable without restarting
    /// the agent). `None` only if an older Gateway omits the field — the
    /// frontend treats that exactly like "disabled".
    pub debug_state: Option<String>,
    pub debug_port: Option<u16>,
}

/// Generic message response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenericMessageResponse {
    pub message: String,
}

/// ADR-103 D12.1 — one model's save-time probe verdict, as the Gateway
/// returns it on `POST /api/providers` and `PUT /api/providers/{provider}`.
///
/// The shell is a transport; this mirrors the wire shape of
/// `acowork_gateway::probe::ProbeReport` so the verdicts reach the UI instead
/// of being dropped. Dropping them was a real defect: the user was told
/// "save to verify", the save did probe, and the answer was thrown away —
/// a selection that failed to verify looked like one that silently succeeded.
///
/// `usage` is deliberately absent: it is billing telemetry the UI does not
/// render, and an unused field here would be dead weight.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderProbeReport {
    /// Model id the verdict is about.
    pub model: String,
    /// Protocol label (`"responses"` in v1).
    #[serde(default)]
    pub protocol: String,
    /// Tier the save paid for (`"standard"` = L1+L2, `"deep"` = +L3).
    #[serde(default)]
    pub tier: String,
    /// `true` when the verdict came from the 24 h cache, not a live call.
    #[serde(default)]
    pub cached: bool,
    pub outcome: ProviderProbeOutcome,
}

/// Mirror of the Gateway's `ProbeOutcome`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderProbeOutcome {
    /// The D10 verdict: `true` means the selection passed the D1 gate (see
    /// [`Self::stripped_fields`] for the "passes, but only without a field"
    /// case).
    pub success: bool,
    /// Tiers that passed, in order (`["L1","L2","L3"]`).
    #[serde(default)]
    pub tier_passed: Vec<String>,
    #[serde(default)]
    pub latency_ms: u64,
    /// Base URL that actually answered, when it differs from the configured
    /// one (the `/v1` recovery path).
    #[serde(default)]
    pub observed_base_url: Option<String>,
    #[serde(default)]
    pub error: Option<ProviderProbeFailure>,
    /// Request fields the probe had to drop to get a 2xx (D8 trim). Non-empty
    /// with `success: true` is the degraded verdict.
    #[serde(default)]
    pub stripped_fields: Vec<String>,
}

/// Mirror of the Gateway's `ProbeFailure`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderProbeFailure {
    /// Machine-readable error class (`TransportError`, `UnsupportedField`, …).
    #[serde(default)]
    pub class: String,
    /// Human-readable reason — this is what the UI shows the user.
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub http_status: Option<u16>,
    /// The request field a 400 blamed (`UnsupportedField` only).
    #[serde(default)]
    pub unsupported_field: Option<String>,
}

/// `POST /api/providers` — the ADR-059 ack plus this save's probe verdicts.
///
/// Mirrors `acowork_gateway::http::provider_api::ProviderSaveResponse`. The
/// ack is flattened so the IPC shape is byte-identical to the old
/// `OperationAck` return; `probes` is empty for every save that probed nothing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderAddResult {
    #[serde(flatten)]
    pub ack: OperationAck,
    #[serde(default)]
    pub probes: Vec<ProviderProbeReport>,
}

/// `PUT /api/providers/{provider}` — mirrors the Gateway's
/// `ProviderUpdateResponse`: a plain message plus this save's probe verdicts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderUpdateResult {
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub probes: Vec<ProviderProbeReport>,
}

/// ADR-059 §7.3 — mirror of the Gateway's `OperationAck`.
///
/// The full `acowork_gateway::http::routes::OperationAck` is not
/// re-exported from `acowork-core` (the Desktop doesn't depend on the
/// Gateway crate), so the wire shape is mirrored here so `reqwest::json`
/// can decode the response on `POST /api/providers`. Wire fields must
/// stay in sync with the Gateway's `OperationAck` (operation_id,
/// state, resource_version, terminal_error).
///
/// ADR-059 §7.3/§7.4 — unified mutation ack returned by the Gateway's
/// operation-bearing write APIs (`POST /api/agents/install` answers
/// HTTP 202 with this body). The ack only carries protocol-level fields
/// (OCP §5.4.4): the operation id, its current state, the post-commit
/// resource version and the structured terminal error when the
/// operation failed. The client correlates the asynchronous outcome
/// via `operation_id` (the node's NodeEvent reply echoes it back as
/// `request_id`, ADR-059 §6.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationAck {
    pub operation_id: String,
    /// Operation state in snake_case (`accepted` / `committed` /
    /// `running` / `completed` / `failed`).
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_version: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_error: Option<StructuredErrorBody>,
    /// ADR-073: instance identity created by this operation
    /// (install / clone only). `None` for other operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
}

/// Clone response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloneResponse {
    pub agent_id: String,
    pub install_path: String,
}

/// A single check item from publish prepare (ADR-055 Phase 3b).
///
/// Wire shape mirrors `acowork_node::package::publish::CheckItem`
/// (Node is the source of truth — Gateway's `PrepareResponse` forwards
/// the node's result JSON verbatim via `Vec<serde_json::Value>`, then
/// serde re-parses each element into this struct). Field names MUST
/// stay in sync with the Node side: `name` and `detail` — not `field`
/// / `message`. The earlier `field`/`message` shape was stale from a
/// pre-ADR-055 revision where the Gateway itself implemented the
/// checks; PublishWizard then read `item.field`/`item.message` and
/// silently got `undefined`, painting every check item with the
/// "error" red `XCircle` regardless of real status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckItem {
    /// Check name (e.g. "manifest.agent_id", "manifest.version").
    pub name: String,
    /// "ok" / "warning" / "error" (lowercase, snake_case-ish).
    pub status: String,
    /// Optional human-readable detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Publish prepare response.
///
/// Wire shape mirrors `acowork_gateway::http::publish_api::PrepareResponse`
/// which itself forwards the Node's `PrepareResult` JSON. Field names
/// MUST stay in sync with `acowork_node::package::publish::PrepareResult`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreparePublishResponse {
    pub checks: Vec<CheckItem>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
    pub cleaned: bool,
}

/// Publish build response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildPublishResponse {
    pub output_path: String,
    pub signed: bool,
    pub file_size: u64,
}

/// Export package response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportPackageResponse {
    pub status: String,
    pub output_path: String,
}

/// Vault key entry (masked, with optional base_url, default_model, models list, and model capabilities)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultKeyEntry {
    pub provider: String,
    /// Stable account UUID — use this for subsequent add/update/remove.
    /// Empty for legacy single-key rows.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub account_id: String,
    /// User-facing label for this account. Empty for legacy single-key rows.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub alias: String,
    pub key_preview: String,
    /// Configured base URL (if any)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Configured default model (if any)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// Selected models list (may be empty)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    /// Per-model capabilities
    ///
    /// ADR-059 §7.3 mirror — the Gateway's `ProviderEntryResponse` types
    /// this field as `Option<HashMap<...>>` (the field is absent on the
    /// wire until the provider has at least one user-configured model
    /// capability override; the earlier `HashMap<...>` shape always
    /// serialized an empty map, which produced spurious `model_capabilities: {}`
    /// entries on every round-trip). `None` and `Some(empty)` are
    /// normalised to an empty map on read; both representations show up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_capabilities: Option<HashMap<String, ModelCapabilities>>,
    /// Compact model for LLM summarization (ADR-010). None = use current model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_model: Option<String>,
    /// Whether this is a local (self-hosted) provider (no API key required)
    #[serde(default)]
    pub local: bool,
    /// Whether this is a user-defined custom provider (not listed in models.dev)
    #[serde(default)]
    pub custom: bool,
}

/// ADR-103 D1: per-model protocol intent (`model id -> protocol`).
///
/// The value is the wire protocol name (`"responses"` / `"openai"` /
/// `"anthropic"` / `"google"` / `"ollama"`); `None` clears the selection for
/// that model ("follow the data"), which restores the npm-derived default and
/// drops the stored probe verdict. The shell is a transport here — the Gateway
/// owns validation and the D1 gate, so the value stays a plain wire string.
pub type ModelProtocolIntent = HashMap<String, Option<String>>;

/// Model capabilities info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCapabilities {
    /// Context window size (total tokens: input + output)
    pub context_window: u64,
    /// Maximum output tokens the model can generate
    pub max_output_tokens: u64,
    /// Maximum input tokens (from models.dev limit.input)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u64>,
    /// Whether the model supports tool/function calling
    #[serde(default = "default_true")]
    pub supports_tool_calling: bool,
    /// Whether the model supports reasoning/thinking
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_reasoning: Option<bool>,
    /// Whether the model supports file attachments
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_attachment: Option<bool>,
    /// Whether the model supports temperature parameter
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_temperature: Option<bool>,
    /// Pricing information (USD per 1M tokens)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<ModelCostInfo>,
    /// Supported modalities
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modalities: Option<ModelModalities>,
    /// Model display name
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Model family
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    /// Knowledge cutoff date
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub knowledge_cutoff: Option<String>,
    /// Default reasoning effort level (user-configured). Values: "off", "low", "medium", "high", "max".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_reasoning_effort: Option<String>,
    /// Anthropic thinking mode: "extended" or "adaptive".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_mode: Option<String>,
}

/// Cost information for a model
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCostInfo {
    /// Input cost per million tokens (USD)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_per_million: Option<f64>,
    /// Output cost per million tokens (USD)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_per_million: Option<f64>,
}

/// Modality information for a model
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelModalities {
    /// Input modalities
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input: Vec<String>,
    /// Output modalities
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output: Vec<String>,
}

fn default_true() -> bool {
    true
}

// ── Attachment upload (ADR-046) ────────────────────────────────────────

/// Response from `POST /api/agents/{agent_id}/sessions/{sid}/files`.
///
/// Mirrors backend `UploadedFileResponse` (camelCase via serde). `width` /
/// `height` are only set for `image_upload` payloads where the desktop
/// frontend had dimensions available at upload time. Fields are camelCase
/// to match the wire JSON shape — the `#[allow(non_snake_case)]` below
/// suppresses the lint without changing the wire contract.
#[allow(non_snake_case)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileUploadResponse {
    pub documentId: String,
    pub filename: String,
    pub format: String,
    pub sizeBytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

// ── Wire-shape regression tests ────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{
        AgentDetailResponse, AgentListEntry, ProviderAddResult, ProviderUpdateResult,
    };

    /// Real `GET /api/agents` sample (single entry) — locks the Tauri mirror
    /// to the Gateway's current wire shape after the `running`/`connected`
    /// → `alive` rename and the Sept 2026 protobuf/`sleeping` removal. A
    /// dropped or renamed Gateway field must fail this test instead of
    /// silently emptying the agent list at runtime.
    ///
    /// NOTE: this fixture is hand-written, so it only catches a field the
    /// Gateway *renames or drops while the fixture keeps it*. When you
    /// change `AgentListResponse` (acowork-gateway/src/http/agents.rs),
    /// update this JSON in the same commit.
    #[test]
    fn agent_list_entry_decodes_gateway_wire_shape() {
        let json = r#"{
            "instance_id": "59901a82-f991-46b9-aaee-0c914250e0ea",
            "agent_id": "com.test.agent",
            "node_id": "94688c78-e36f-4355-8540-d4da4c442cbf",
            "name": "ACowork",
            "display_name": "ACowork",
            "role": "assistant",
            "avatar": null,
            "builtin_avatar": null,
            "version": "1.0.0",
            "alive": true,
            "lifecycle": "sessions_ready",
            "dev_mode": false,
            "debug_state": "disabled",
            "debug_port": null
        }"#;
        let entry: AgentListEntry =
            serde_json::from_str(json).expect("Gateway list entry must decode");
        assert!(entry.alive);
        assert_eq!(entry.lifecycle, "sessions_ready");
    }

    /// Detail endpoint shares the rename — `pid` stays as a diagnostic-only
    /// field and must never be treated as the liveness signal.
    #[test]
    fn agent_detail_decodes_gateway_wire_shape() {
        let json = r#"{
            "instance_id": "59901a82-f991-46b9-aaee-0c914250e0ea",
            "agent_id": "com.test.agent",
            "node_id": "94688c78-e36f-4355-8540-d4da4c442cbf",
            "name": "ACowork",
            "display_name": "ACowork",
            "role": "assistant",
            "avatar": null,
            "builtin_avatar": null,
            "version": "1.0.0",
            "description": "Test agent",
            "author": "ACowork",
            "install_path": "/tmp/agents/com.test.agent",
            "alive": true,
            "lifecycle": "sessions_ready",
            "ready": true,
            "pid": null,
            "started_at": null,
            "dev_mode": false,
            "debug_state": "disabled",
            "debug_port": null
        }"#;
        let detail: AgentDetailResponse =
            serde_json::from_str(json).expect("Gateway detail must decode");
        assert!(detail.alive);
        assert_eq!(detail.pid, None);
    }

    /// ADR-103 D12.1 — the save responses MUST deliver the probe verdicts.
    ///
    /// `ProviderUpdateResponse` carries `{message, probes}`; decoding it as
    /// `GenericMessageResponse` (the pre-fix code) silently dropped `probes`,
    /// which is why a failed verification produced no feedback at all. This
    /// fixture is a real `PUT /api/providers/{provider}` body with one failed
    /// and one degraded verdict.
    #[test]
    fn provider_update_result_keeps_the_probe_verdicts() {
        let json = r#"{
            "message": "Provider 'acme' updated",
            "probes": [
                {
                    "model": "acme-3.1-flash",
                    "protocol": "responses",
                    "tier": "deep",
                    "cached": false,
                    "outcome": {
                        "success": false,
                        "tier_passed": ["L1"],
                        "latency_ms": 412,
                        "error": {
                            "class": "UnsupportedField",
                            "message": "endpoint rejected 'reasoning'",
                            "http_status": 400,
                            "unsupported_field": "reasoning"
                        }
                    }
                },
                {
                    "model": "acme-4",
                    "protocol": "responses",
                    "tier": "standard",
                    "cached": true,
                    "outcome": {
                        "success": true,
                        "tier_passed": ["L1", "L2"],
                        "latency_ms": 180,
                        "observed_base_url": "https://api.acme.test/v1",
                        "stripped_fields": ["reasoning"]
                    }
                }
            ]
        }"#;
        let body: ProviderUpdateResult =
            serde_json::from_str(json).expect("PUT /api/providers must decode");
        assert_eq!(body.message, "Provider 'acme' updated");
        assert_eq!(body.probes.len(), 2);
        let failed = &body.probes[0];
        assert!(!failed.outcome.success);
        assert_eq!(failed.outcome.tier_passed, vec!["L1".to_string()]);
        let err = failed.outcome.error.as_ref().expect("failure reason");
        assert_eq!(err.class, "UnsupportedField");
        assert_eq!(err.http_status, Some(400));
        let degraded = &body.probes[1];
        assert!(degraded.outcome.success);
        assert!(degraded.cached);
        assert_eq!(degraded.outcome.stripped_fields, vec!["reasoning".to_string()]);
    }

    /// The `POST` shape flattens the ADR-059 ack, so the legacy `OperationAck`
    /// fields must survive alongside `probes`. A save that probed nothing omits
    /// the key entirely and must still decode.
    #[test]
    fn provider_add_result_flattens_the_ack_and_tolerates_no_probes() {
        let json = r#"{
            "operation_id": "b1f0e2c4-0000-4000-8000-000000000001",
            "state": "committed"
        }"#;
        let body: ProviderAddResult =
            serde_json::from_str(json).expect("POST /api/providers must decode");
        assert_eq!(body.ack.state, "committed");
        assert!(body.probes.is_empty());
    }
}
