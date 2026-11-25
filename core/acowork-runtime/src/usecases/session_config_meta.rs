//! Session config for a session that has **no live `ConversationSession`**.
//!
//! # Why this module exists
//!
//! `RuntimeSessionConfigService` resolved a session through its
//! `session_configs` map — the table of *resident* (open, in-memory) sessions.
//! A session that had not been opened in this Runtime process (the normal
//! state of everything in the history list) was answered with
//! `Config error: Session not found: …` and, on the HTTP surface, a 500. A
//! rename of such a session therefore failed with an error that reads like a
//! permissions or routing fault, while the session was right there in the
//! list.
//!
//! Residency is an implementation detail of *this process*, not a property of
//! the session. Every field `SessionConfigDelta` can carry is a fact about the
//! session that lives in its `SessionMeta` row — the row is what a resumed
//! session is built from, so writing the row is a complete write, not a
//! partial one. This module is that projection:
//!
//! * [`snapshot_from_meta`] — the read direction (`GET …/config`);
//! * [`apply_to_meta`] — the write direction (`PUT …/config`).
//!
//! # What is deliberately NOT here
//!
//! The live-session path additionally
//!
//! * bumps `config_version` (the running loop's turn-boundary diff),
//! * notifies the config/state relays (MQTT pushes to the frontend),
//! * recomputes context usage (ADR-074 §11.1).
//!
//! None of those has a subject without a live session — there is no loop to
//! re-point and nobody subscribed to a session nobody has open. The next
//! `ConversationSession::open` seeds all of it from the row this module wrote.
//!
//! # Normalisation lives here, once
//!
//! Both directions go through the same helpers the live path uses
//! ([`crate::prompt::truncate_title_for_display`],
//! [`super::super::agent::session_config::is_valid_context_window`]) rather
//! than re-spelling the rules: a title must be truncated identically whether
//! it was typed into a resident or a non-resident session, or the two paths
//! would disagree about what "the title" is.

use std::path::Path;

use acowork_core::session_meta::{SessionMeta, TitleSource};

use crate::agent::session_config::{SessionConfigDelta, SessionConfigSnapshot};
use crate::conversation::{read_session_meta, write_session_meta};
use crate::error::{Result, RuntimeError};

/// Where the session rows live for a Runtime work dir.
pub(crate) fn conversations_dir(work_dir: &Path) -> std::path::PathBuf {
    work_dir.join("conversations")
}

/// Read a non-resident session's config out of its meta row.
///
/// The shape is the live `SessionConfigSnapshot`, so an HTTP client cannot
/// tell whether the session it just queried happened to be resident. Values
/// are the raw persisted ones — the effective-`reasoning_effort` resolution is
/// applied by the caller exactly as it is for the live path (it needs
/// `AgentCore` model capabilities, which this module has no business owning).
pub(crate) fn snapshot_from_meta(meta: &SessionMeta) -> SessionConfigSnapshot {
    SessionConfigSnapshot {
        model: meta.model.clone(),
        provider: meta.provider.clone(),
        account_id: meta.account_id.clone(),
        workspace_id: meta.workspace_id.clone(),
        reasoning_effort: meta.reasoning_effort.clone(),
        temperature: meta.temperature,
        context_window: meta.context_window,
        title: meta.title.clone(),
    }
}

/// Apply one field of a delta to a meta row.
///
/// This is the same projection as `ConversationSession::apply_config`,
/// including its normalisation, expressed against the row instead of the live
/// session's cells: the two may not disagree about what a delta means, because
/// a session's config does not change meaning when the process stops holding
/// it open.
///
/// That equivalence is **not** left to this comment. It is asserted by
/// [`tests::the_two_residency_paths_agree_on_every_field`], which applies one
/// all-fields delta through both paths and compares the whole
/// `SessionConfigSnapshot` — the delta's own mirror shape, so a field added to
/// the delta is compared there without anyone remembering to extend the test,
/// and a field implemented in only one path fails it.
///
/// Returns `true` when the delta asked for a change (whether or not the row
/// already held that value), which is what the caller needs to decide between
/// "write" and "nothing to do".
fn apply_field(meta: &mut SessionMeta, delta: &SessionConfigDelta) -> bool {
    let mut changed = false;
    if let Some(ref model) = delta.model {
        meta.model = Some(model.clone());
        changed = true;
    }
    if let Some(ref provider) = delta.provider {
        meta.provider = Some(provider.clone());
        changed = true;
    }
    if let Some(ref account_id) = delta.account_id {
        meta.account_id = Some(account_id.clone());
        changed = true;
    }
    if let Some(ref workspace_id) = delta.workspace_id {
        meta.workspace_id = Some(workspace_id.clone());
        changed = true;
    }
    if let Some(ref effort) = delta.reasoning_effort {
        meta.reasoning_effort = Some(effort.clone());
        changed = true;
    }
    if let Some(temp) = delta.temperature {
        meta.temperature = Some(temp);
        changed = true;
    }
    // ADR-074 D6: same normalisation as the live path — an invalid override
    // means "clear" and disappears from the row, rather than being persisted
    // as a value the resolution chain would have to defend against.
    if let Some(cw) = delta.context_window {
        meta.context_window = if crate::agent::session_config::is_valid_context_window(cw) {
            Some(cw)
        } else {
            None
        };
        changed = true;
    }
    if let Some(ref title) = delta.title {
        let truncated = crate::prompt::truncate_title_for_display(title);
        if meta.title.as_deref() != Some(truncated.as_str())
            || meta.effective_title_source() != TitleSource::User
        {
            meta.title = Some(truncated);
            // A `title` on the config path is always a human rename, so the
            // automatic title path is closed to this session from now on —
            // this is the fact that has to survive the restart, more than the
            // string itself.
            meta.title_source = Some(TitleSource::User);
            changed = true;
        }
    }
    changed
}

/// Read-modify-write a non-resident session's config into its meta row.
///
/// Returns the updated row (the caller logs / echoes through it). A session
/// with no row is a `Config` error, matching the previous message shape — the
/// HTTP layer has already established that the row exists, so this is a
/// create/delete race, not a normal outcome.
pub(crate) fn apply_to_meta(
    work_dir: &Path,
    session_id: &str,
    delta: &SessionConfigDelta,
) -> Result<SessionMeta> {
    let dir = conversations_dir(work_dir);
    let mut meta = read_session_meta(&dir, session_id).map_err(|e| {
        tracing::warn!(
            session_id = %session_id,
            error = %e,
            "non-resident config write: session meta unavailable"
        );
        RuntimeError::Config(format!("Session not found: {}", session_id))
    })?;

    if !apply_field(&mut meta, delta) {
        return Ok(meta);
    }

    // `ConversationSession::build_meta` stamps `last_active_at = now` on every
    // meta write, and the session list is ordered by that column. Mirroring it
    // keeps "rename" identical on both paths instead of reordering the list
    // only when the session happened to be resident.
    meta.last_active_at =
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

    write_session_meta(&dir, &meta)
        .map_err(|e| RuntimeError::Config(format!("Failed to persist session config: {}", e)))?;
    Ok(meta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn meta(title: Option<&str>, source: Option<TitleSource>) -> SessionMeta {
        SessionMeta {
            version: 3,
            session_id: "s-1".to_string(),
            agent_id: "com.test.agent".to_string(),
            created_at: "2026-01-01T00:00:00.000Z".to_string(),
            initiator: None,
            visibility: None,
            chat_session_id: None,
            aware_users: Vec::new(),
            interveners: Vec::new(),
            title: title.map(str::to_string),
            title_source: source,
            workspace_id: None,
            model: None,
            provider: None,
            account_id: None,
            reasoning_effort: None,
            temperature: None,
            context_window: None,
            todos: None,
            tool_policy: None,
            message_count: 0,
            last_active_at: "2026-01-01T00:00:00.000Z".to_string(),
            tokens: None,
            model_usage: None,
            llm_call_counter: None,
            perf: None,
            model_ratio: None,
            last_compaction_offset: None,
            corrupted: false,
        }
    }

    fn delta_title(title: &str) -> SessionConfigDelta {
        SessionConfigDelta {
            title: Some(title.to_string()),
            ..Default::default()
        }
    }

    /// The defect: a rename of a session this process does not hold open must
    /// still be a rename — and must claim the title for the human, so the
    /// automatic path cannot take it back after the next turn or restart.
    #[test]
    fn a_rename_of_a_generated_title_claims_it_for_the_user() {
        let mut m = meta(Some("first message"), Some(TitleSource::Generated));
        let changed = apply_field(&mut m, &delta_title("分支出重构"));
        assert!(changed);
        assert_eq!(m.title.as_deref(), Some("分支出重构"));
        assert_eq!(m.title_source, Some(TitleSource::User));
        assert!(m.title_is_user_owned());
    }

    /// A legacy row (written before `title_source` existed) has no author on
    /// disk; renaming it must still stamp the author.
    #[test]
    fn a_rename_of_a_legacy_row_stamps_the_author() {
        let mut m = meta(Some("auto"), None);
        assert!(apply_field(&mut m, &delta_title("人写的")));
        assert_eq!(m.title_source, Some(TitleSource::User));
    }

    /// Re-sending the identical title of an already user-owned session is a
    /// no-op write (no needless row churn / list reorder).
    #[test]
    fn an_identical_user_title_is_not_rewritten() {
        let mut m = meta(Some("人写的"), Some(TitleSource::User));
        assert!(!apply_field(&mut m, &delta_title("人写的")));
    }

    /// The config delta and the meta row must agree about every field they
    /// share: a non-resident write is only equivalent to a resident one if
    /// this mapping is total.
    #[test]
    fn every_config_field_reaches_the_row() {
        let mut m = meta(None, None);
        let delta = SessionConfigDelta {
            model: Some("m".into()),
            provider: Some("p".into()),
            account_id: Some("a".into()),
            workspace_id: Some("w".into()),
            reasoning_effort: Some("high".into()),
            temperature: Some(0.25),
            context_window: Some(200_000),
            title: Some("t".into()),
        };
        assert!(apply_field(&mut m, &delta));
        assert_eq!(m.model.as_deref(), Some("m"));
        assert_eq!(m.provider.as_deref(), Some("p"));
        assert_eq!(m.account_id.as_deref(), Some("a"));
        assert_eq!(m.workspace_id.as_deref(), Some("w"));
        assert_eq!(m.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(m.temperature, Some(0.25));
        assert_eq!(m.context_window, Some(200_000));
        assert_eq!(m.title.as_deref(), Some("t"));
    }

    /// `0` clears the override instead of persisting an unusable window.
    #[test]
    fn a_zero_context_window_clears_the_override() {
        let mut m = meta(None, None);
        m.context_window = Some(200_000);
        let delta = SessionConfigDelta {
            context_window: Some(0),
            ..Default::default()
        };
        assert!(apply_field(&mut m, &delta));
        assert_eq!(m.context_window, None);
    }

    #[test]
    fn an_empty_delta_changes_nothing() {
        let mut m = meta(Some("keep"), Some(TitleSource::Generated));
        assert!(!apply_field(&mut m, &SessionConfigDelta::default()));
        assert_eq!(m.title.as_deref(), Some("keep"));
        assert_eq!(m.title_source, Some(TitleSource::Generated));
    }

    #[test]
    fn the_snapshot_carries_the_persisted_values() {
        let mut m = meta(Some("标题"), Some(TitleSource::User));
        m.model = Some("m".into());
        m.context_window = Some(100_000);
        let snap = snapshot_from_meta(&m);
        assert_eq!(snap.title.as_deref(), Some("标题"));
        assert_eq!(snap.model.as_deref(), Some("m"));
        assert_eq!(snap.context_window, Some(100_000));
    }

    #[test]
    fn conversations_dir_is_the_work_dirs_conversations_child() {
        assert_eq!(
            conversations_dir(Path::new("/w")),
            PathBuf::from("/w").join("conversations")
        );
    }

    /// **The two write paths must agree on every field.**
    ///
    /// Residency is a property of *this process*, not of the session, so the
    /// same delta has to produce the same config whether the session happens
    /// to be open right now or only exists as a row. This is the enforcement
    /// of that rule, and the reason `apply_field` is allowed to look like a
    /// second copy of `ConversationSession::apply_config`: the comparison is
    /// the whole `SessionConfigSnapshot`, the delta's own mirror shape, so a
    /// field added to the delta (and therefore to the snapshot) is compared
    /// here automatically — and a field wired into only one of the two paths
    /// fails the assertion instead of silently disappearing for the sessions
    /// nobody has open.
    #[test]
    fn the_two_residency_paths_agree_on_every_field() {
        use crate::conversation::{ConversationSession, SessionConfig};
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let work_dir = dir.path();
        let sid = "s-agree";
        let (conv, _config_rx, _state_rx) = ConversationSession::new(
            work_dir,
            sid,
            SessionConfig {
                agent_id: "com.test.agree".to_string(),
                workspace_id: None,
                model: None,
                provider: None,
            },
            0,
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        )
        .unwrap();

        // One delta carrying every field the type has, so "forgot to teach
        // this path the new field" cannot hide behind a default.
        let delta = SessionConfigDelta {
            model: Some("gpt-4o".into()),
            provider: Some("openai".into()),
            account_id: Some("acct-2".into()),
            workspace_id: Some("ws-1".into()),
            reasoning_effort: Some("high".into()),
            temperature: Some(0.3),
            context_window: Some(200_000),
            title: Some("分支出重构".into()),
        };

        // Path 1: resident — the live session.
        conv.apply_config(&delta);
        let resident = conv.config_snapshot();

        // Path 2: non-resident — straight onto the row, exactly what
        // `apply_to_meta` does after its read-modify-write.
        let mut row = meta(None, None);
        assert!(apply_field(&mut row, &delta));
        let non_resident = snapshot_from_meta(&row);

        assert_eq!(
            resident, non_resident,
            "the same delta must mean the same thing on both paths"
        );
        // The author claim is part of "the same thing": a rename closes the
        // automatic title path on both sides.
        assert_eq!(row.title_source, Some(TitleSource::User));
        assert_eq!(conv.title_source(), TitleSource::User);
    }
}