//! Instance identity routes: the avatar the user picked and the name this
//! agent is called by.
//!
//! # Where the data lives
//!
//! `{work_dir}/config/agent_config.json` — the instance's own settings
//! file, written by the Runtime and by nothing else. `{work_dir}` is the
//! Runtime's work dir (`{packages}/{agent_id}/{instance_id}/workspace`),
//! so the avatar and the name sit next to the temperature and the context
//! window they are configured beside, in a file an upgrade cannot touch
//! (upgrade replaces the package content and preserves `workspace/`).
//!
//! This module used to own a separate `{instance_id}.overrides.json`
//! **sibling of the package dir**. Two problems with that: a settings file
//! outside the directory of the thing it configures, and a second place a
//! name could come from — the "display name" override — which is exactly
//! how an agent ended up called one thing by humans and another by itself.
//! One settings file, one name.
//!
//! | Method | Path                          | Handler               |
//! |--------|-------------------------------|-----------------------|
//! | GET    | `/agents/{id}/avatar-config`  | [`get_avatar_config`] |
//! | PUT    | `/agents/{id}/avatar-config`  | [`put_avatar_config`] |
//!
//! Serving the image **bytes** is NOT here: the package (`assets/*`,
//! `manifest.avatar`) belongs to the node, which is the package manager
//! and is up whether or not this process is. A second copy of those reads
//! here would be a second code path to keep in sync — the mistake this
//! module's history is made of.

use acowork_core::avatar::{AVATAR_EXTENSIONS, has_avatar_extension};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};

use crate::agent_config::{load_agent_config, save_agent_config};
use crate::http::server::HttpState;

// ── Wire types ────────────────────────────────────────────────────────

/// `GET/PUT /agents/{id}/avatar-config` response — the **effective**
/// avatar and name (what the UI renders), plus where the avatar came from.
///
/// `name` is the instance's one name (`agent_config.json` `name`,
/// materialized from the package name on first boot) — the same string the
/// agent is told about itself and the same one every peer sees.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AvatarConfigResponse {
    pub agent_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub builtin_avatar: Option<String>,
    /// The instance's name. Always present: it is materialized on first
    /// boot, so an empty value here can only mean "the config file is
    /// missing", which this route reports rather than hides.
    pub name: String,
    /// `"config"` | `"manifest"` | `"fallback"` — where the avatar came from.
    pub source: String,
}

/// `PUT /agents/{id}/avatar-config` body.
///
/// Semantics per field: absent/`null` = leave unchanged; `""` = clear the
/// pick (fall back to the package default); any other value = set it.
/// Setting `avatar` clears `builtin_avatar` and vice versa.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UpdateAvatarConfigRequest {
    #[serde(default)]
    pub avatar: Option<String>,
    #[serde(default)]
    pub builtin_avatar: Option<String>,
    /// Replaces the instance's name. Absent = unchanged; `""` is rejected
    /// (an agent always has a name — there is no unnamed instance to fall
    /// back to).
    #[serde(default)]
    pub name: Option<String>,
}

// ── Router ────────────────────────────────────────────────────────────

/// The avatar routes (see module docs).
pub(crate) fn avatar_routes() -> Router<HttpState> {
    Router::new().route(
        "/agents/{id}/avatar-config",
        get(get_avatar_config).put(put_avatar_config),
    )
}

// ── Handlers ──────────────────────────────────────────────────────────

/// `GET /agents/{id}/avatar-config` — effective avatar + name.
async fn get_avatar_config(State(state): State<HttpState>, Path(id): Path<String>) -> Response {
    if !state.instance_matches(&id) {
        return instance_mismatch(&state, &id);
    }
    match effective_config(&state) {
        Ok(cfg) => Json(cfg).into_response(),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", e),
    }
}

/// `PUT /agents/{id}/avatar-config` — persist the user's pick.
///
/// The single writer of the avatar/name fields in `agent_config.json`. The
/// Gateway reverse-proxies this route and does not choose the value: it
/// re-reads the instance's inventory (which the Node rebuilds from this
/// same file) for its list view. One file, one writer, one projection.
async fn put_avatar_config(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateAvatarConfigRequest>,
) -> Response {
    if !state.instance_matches(&id) {
        return instance_mismatch(&state, &id);
    }

    let mut cfg = load_agent_config(&state.work_dir)
        .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", e))
        .unwrap_or_default()
        .unwrap_or_default();

    // `avatar` and `builtin_avatar` are mutually exclusive — selecting one
    // clears the other, otherwise a stale lower-priority value would
    // resurrect as soon as the newer one is cleared.
    match normalize(&req.avatar) {
        Field::Unchanged => {}
        Field::Clear => cfg.avatar = None,
        Field::Set(value) => {
            if !has_avatar_extension(&value) {
                return error(
                    StatusCode::BAD_REQUEST,
                    "bad_request",
                    format!(
                        "Invalid avatar path '{value}': only {} are allowed",
                        AVATAR_EXTENSIONS.join(", ")
                    ),
                );
            }
            cfg.avatar = Some(value);
            cfg.builtin_avatar = None;
        }
    }
    match normalize(&req.builtin_avatar) {
        Field::Unchanged => {}
        Field::Clear => cfg.builtin_avatar = None,
        Field::Set(value) => {
            if !is_plausible_builtin_avatar_id(&value) {
                return error(
                    StatusCode::BAD_REQUEST,
                    "bad_request",
                    format!("Invalid builtin_avatar '{value}': expected 'icon-NN' or 1-99"),
                );
            }
            cfg.builtin_avatar = Some(value);
            cfg.avatar = None;
        }
    }
    match normalize(&req.name) {
        Field::Unchanged => {}
        Field::Clear => {
            return error(
                StatusCode::BAD_REQUEST,
                "bad_request",
                "name cannot be cleared: an agent always has a name".to_string(),
            );
        }
        Field::Set(value) => {
            if value.chars().count() > MAX_NAME_CHARS {
                return error(
                    StatusCode::BAD_REQUEST,
                    "bad_request",
                    format!(
                        "name too long ({} > {MAX_NAME_CHARS} chars)",
                        value.chars().count()
                    ),
                );
            }
            cfg.name = Some(value);
        }
    }

    if let Err(e) = save_agent_config(&state.work_dir, &cfg) {
        tracing::warn!(error = %e, "avatar-config: failed to persist agent_config.json");
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            format!("Failed to persist agent_config.json: {e}"),
        );
    }

    // Push the new name into the live core: the file is the durable record,
    // the core is what the next turn's `{{agent_name}}` reads. Without this
    // a rename would take effect in the sidebar immediately and in the
    // agent's own prompt only after a restart — the two-name drift again,
    // just spread over time.
    if let Some(name) = cfg.name.clone()
        && let Some(core) = state
            .agent_core
            .read()
            .ok()
            .and_then(|slot| slot.as_ref().map(std::sync::Arc::clone))
    {
        core.set_instance_name(name);
    }

    match effective_config(&state) {
        Ok(cfg) => Json(cfg).into_response(),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", e),
    }
}

// ── Resolution ────────────────────────────────────────────────────────

/// Reject absurd names at the trust boundary. Generous enough for any real
/// name; the point is to stop an unbounded string from reaching the file
/// and every list response.
const MAX_NAME_CHARS: usize = 100;

/// Tri-state for one incoming field: absent, explicitly cleared (`""`), or set.
enum Field {
    Unchanged,
    Clear,
    Set(String),
}

fn normalize(value: &Option<String>) -> Field {
    match value {
        None => Field::Unchanged,
        Some(v) if v.trim().is_empty() => Field::Clear,
        Some(v) => Field::Set(v.trim().to_string()),
    }
}

/// Loose syntactic check for builtin_avatar values: `icon-NN` (1-99) or a
/// bare number 1-99. The Desktop's bundled icon set remains the source of
/// truth for which ids exist; this only rejects obvious typos.
fn is_plausible_builtin_avatar_id(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    match lower.strip_prefix("icon-") {
        Some(num) => num.parse::<u32>().is_ok_and(|n| (1..=99).contains(&n)),
        None => lower.parse::<u32>().is_ok_and(|n| (1..=99).contains(&n)),
    }
}

/// The effective avatar / name for this instance.
fn effective_config(state: &HttpState) -> Result<AvatarConfigResponse, String> {
    let manifest = crate::package::loader::load_manifest(&state.package_dir)
        .map_err(|e| format!("package manifest unavailable: {e}"))?;
    let cfg = load_agent_config(&state.work_dir)
        .map_err(|e| format!("agent_config.json unreadable: {e}"))?
        .unwrap_or_default();
    let (avatar, builtin_avatar, source) = resolve_effective_avatar(&cfg, &manifest);
    Ok(AvatarConfigResponse {
        agent_id: state.instance_id.clone(),
        avatar,
        builtin_avatar,
        // An unmaterialized name (a config file that exists but predates the
        // field) reports the package name — the same value first boot would
        // have written, so the answer is identical either side of that write.
        name: cfg
            .name
            .clone()
            .unwrap_or_else(|| manifest.name.clone()),
        source: source.to_string(),
    })
}

/// User pick first, packaged default second, deterministic client-side
/// fallback last. Returns `(avatar, builtin_avatar, source)`.
fn resolve_effective_avatar(
    cfg: &crate::agent_config::AgentConfig,
    manifest: &acowork_core::AgentManifest,
) -> (Option<String>, Option<String>, &'static str) {
    if cfg.avatar.is_some() || cfg.builtin_avatar.is_some() {
        return (cfg.avatar.clone(), cfg.builtin_avatar.clone(), "config");
    }
    if manifest.avatar.is_some() || manifest.builtin_avatar.is_some() {
        return (
            manifest.avatar.clone(),
            manifest.builtin_avatar.clone(),
            "manifest",
        );
    }
    (None, None, "fallback")
}

// ── Helpers ───────────────────────────────────────────────────────────

fn instance_mismatch(state: &HttpState, id: &str) -> Response {
    error(
        StatusCode::NOT_FOUND,
        "instance_id_mismatch",
        format!(
            "path '{id}' is not this runtime's instance id '{}'",
            state.instance_id
        ),
    )
}

fn error(status: StatusCode, code: &str, message: String) -> Response {
    (
        status,
        Json(serde_json::json!({ "error": code, "message": message })),
    )
        .into_response()
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_whitelist_is_case_insensitive() {
        assert!(has_avatar_extension("assets/avatar.PNG"));
        assert!(has_avatar_extension("avatar-01.webp"));
        assert!(!has_avatar_extension("assets/evil.sh"));
        assert!(!has_avatar_extension("assets/noext"));
    }

    #[test]
    fn avatar_pick_beats_manifest_and_reports_its_source() {
        let mut cfg = crate::agent_config::AgentConfig::default();
        let mut manifest = acowork_core::AgentManifest::from_toml(
            r#"
            agent_id = "com.test.weather"
            version = "1.0.0"
            name = "Weather"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"
            avatar = "assets/avatar.jpg"
            [llm]
            provider = "openai"
            model = "gpt-4"
        "#,
        )
        .unwrap();

        assert_eq!(
            resolve_effective_avatar(&cfg, &manifest),
            (
                Some("assets/avatar.jpg".to_string()),
                None,
                "manifest"
            )
        );

        cfg.avatar = Some("assets/avatar-02.png".to_string());
        assert_eq!(
            resolve_effective_avatar(&cfg, &manifest),
            (
                Some("assets/avatar-02.png".to_string()),
                None,
                "config"
            )
        );

        // No packaged default and no pick: the client's own fallback applies.
        manifest.avatar = None;
        cfg.avatar = None;
        assert_eq!(resolve_effective_avatar(&cfg, &manifest), (None, None, "fallback"));
    }
}
