//! MCP tool dispatch for the memory service (ADR-101).
//!
//! Both tools are **instance-scoped**: the `X-MCP-Actor` value (the runtime
//! instance UUID, injected and validated by the Gateway `memory_proxy`) is the
//! namespace key, so there is no anonymous memory access at all — unlike doc,
//! where read-only tools may run without an actor. Missing or unusable actor →
//! [`ServiceError::Forbidden`] / [`ServiceError::InvalidInstanceId`], mapped to
//! JSON-RPC `-32002` / `-32602` in [`crate::service::mcp`].
//!
//! The returned text is the **exact** string the Runtime builtins produced
//! (`Stored episode: "…" (confidence: …, subtype: …)` / one
//! `- [label] (score=0.00) content` line per hit), so switching an Agent from
//! the builtin tool to the MCP tool does not change what the model reads.

use serde_json::Value;

use crate::MemoryManager;
use crate::MemoryManagerConfig;
use crate::MemoryQuery;
use crate::service::error::{Result, ServiceError};
use crate::service::state::MemoryState;
use crate::sqlite::SqliteStore;
use crate::types::{Episode, KnowledgeSubType, PrivacyLevel};

/// Default confidence when the LLM does not provide one.
const DEFAULT_CONFIDENCE: f32 = 0.7;

// ── Identity + argument helpers ───────────────────────────────────────────

/// Resolve the caller to a usable instance namespace key.
///
/// The actor header is the only identity the service has, and it doubles as
/// the storage directory name — so an absent actor is "no namespace" (403)
/// and a malformed one can never be turned into a path (400 → JSON-RPC
/// invalid-params).
fn require_instance(actor: Option<&str>) -> Result<String> {
    let actor = actor.ok_or_else(|| {
        ServiceError::Forbidden(
            "memory tools require an authenticated agent instance (X-MCP-Actor)".into(),
        )
    })?;
    crate::service::state::validate_instance_id(actor)?;
    Ok(actor.to_string())
}

/// Loose argument parsing: absent fields fall back, unknown fields ignored.
fn parse_args(name: &str, args: Value) -> Result<Value> {
    match args {
        Value::Object(map) => Ok(Value::Object(map)),
        Value::Null => Ok(Value::Object(serde_json::Map::new())),
        other => Err(ServiceError::BadRequest(format!(
            "invalid arguments for {name}: expected an object, got {other}"
        ))),
    }
}

/// Discriminated category for `memory_store` (ADR-068 §3.3).
///
/// Maps directly onto [`KnowledgeSubType`] — the LLM has no access to the
/// autobiographical or other distiller-only channels.
fn parse_category(s: &str) -> Option<KnowledgeSubType> {
    match s.to_lowercase().as_str() {
        "fact" => Some(KnowledgeSubType::Fact),
        "preference" => Some(KnowledgeSubType::Preference),
        "relation" => Some(KnowledgeSubType::Relation),
        "procedure" => Some(KnowledgeSubType::Procedure),
        _ => None,
    }
}

// ── memory_store ──────────────────────────────────────────────────────────

/// `memory_store` — thin episodic writer (ADR-068 §3.2).
///
/// Writes one Episode tagged with a `knowledge_subtype` routing hint; whether
/// it is ever promoted into the sediment layer is the offline distiller's
/// decision, not this tool's.
async fn memory_store(state: &MemoryState, instance_id: &str, args: Value) -> Result<String> {
    let args = parse_args("memory_store", args)?;

    let content = args
        .get("content")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .ok_or_else(|| ServiceError::BadRequest("Missing required parameter 'content'".into()))?
        .to_string();

    let category_str = args
        .get("category")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ServiceError::BadRequest(
                "Missing required parameter 'category'. Must be 'fact', 'preference', 'relation', or 'procedure'."
                    .into(),
            )
        })?;
    let knowledge_subtype = parse_category(category_str).ok_or_else(|| {
        ServiceError::BadRequest(format!(
            "Invalid category '{}'. Must be 'fact', 'preference', 'relation', or 'procedure'.",
            category_str
        ))
    })?;

    let confidence_explicit = args.get("confidence").is_some();
    let confidence = args
        .get("confidence")
        .and_then(Value::as_f64)
        .map(|c| c.clamp(0.0, 1.0) as f32)
        .unwrap_or(DEFAULT_CONFIDENCE);

    // ADR-062 §6.2.1 M5 step 2a: the LLM is the only keyword source, so the
    // quality gate runs at this boundary — garbage never reaches
    // metadata["keywords"] or the BM25 fold.
    let keywords: Option<Vec<String>> = args.get("keywords").and_then(|v| {
        v.as_array().map(|arr| {
            let raw: Vec<String> = arr
                .iter()
                .filter_map(|item| item.as_str().map(String::from))
                .collect();
            let (clean, stats) = crate::keyword::sanitize_with_stats(raw);
            tracing::debug!(
                target: "memory_write_keyword_gate",
                instance_id = %instance_id,
                input = stats.input_count,
                output = stats.output_count,
                dropped_empty_or_too_long = stats.dropped_empty_or_too_long,
                dropped_no_alpha_cjk = stats.dropped_no_alpha_cjk,
                dropped_duplicate = stats.dropped_duplicate,
                dropped_stopword = stats.dropped_stopword,
                dropped_over_cap = stats.dropped_over_cap,
                "keyword quality gate applied at memory_store boundary"
            );
            clean
        })
    });

    let privacy =
        args.get("privacy")
            .and_then(Value::as_str)
            .map(|s| match s.to_lowercase().as_str() {
                "public" => PrivacyLevel::Public,
                "sensitive" => PrivacyLevel::Sensitive,
                _ => PrivacyLevel::Personal,
            });

    let importance_explicit = args.get("importance").is_some();
    let importance = args
        .get("importance")
        .and_then(Value::as_f64)
        .map(|c| c.clamp(0.0, 1.0) as f32);

    // Blank strings from a hesitant model must not become a `normalized` the
    // distiller would prefer over the real content.
    let normalized = args
        .get("normalized")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string);

    let mut metadata = std::collections::HashMap::new();
    if let Some(imp) = importance {
        metadata.insert("importance".to_string(), Value::from(imp as f64));
    }
    metadata.insert("confidence".to_string(), Value::from(confidence as f64));
    metadata.insert(
        "privacy".to_string(),
        Value::from(match privacy {
            Some(PrivacyLevel::Public) => "public",
            Some(PrivacyLevel::Sensitive) => "sensitive",
            _ => "personal",
        }),
    );
    if let Some(kw) = keywords.as_ref()
        && !kw.is_empty()
    {
        metadata.insert(
            "keywords".to_string(),
            serde_json::to_value(kw).unwrap_or_default(),
        );
    }

    let mut episode = Episode {
        // The service has no Runtime session; the instance namespace is the
        // closest equivalent of the builtin's `agent_id` scoping.
        session_id: instance_id.to_string(),
        turn_index: 0,
        role: "assistant".to_string(),
        content: content.clone(),
        embedding: None,
        timestamp: chrono::Utc::now(),
        consolidated: false,
        metadata,
        importance: importance.unwrap_or(0.5),
        knowledge_subtype: Some(knowledge_subtype.clone()),
        normalized,
    };

    // Keyed on `statement()`, not `content`: the distiller recalls and merges
    // on the same string, so embedding anything else would make the write path
    // and the consolidation path disagree. Degrades to text-only when no
    // embedding backend is installed or the call fails.
    if let Some(embedder) = state.embedding() {
        match embedder.embed(episode.statement()).await {
            Ok(vec) => episode.embedding = Some(vec),
            Err(e) => {
                tracing::warn!(
                    instance_id = %instance_id,
                    error = %e,
                    "memory_store: failed to embed statement, storing text-only"
                );
            }
        }
    }

    let store = state.store(instance_id)?;
    let _episode_id = store.store_episode(&episode)?;

    // Feeds the distiller's idle gate: this process has no chat turn to observe,
    // so a memory write is the signal that the Agent is active (the Runtime
    // called `notify_active` on every turn).
    state.note_activity(instance_id);

    // ADR-062 M3.6: write-path distribution telemetry for threshold
    // re-calibration (ADR-062 §6.6).
    tracing::debug!(
        target: "memory_write_scores",
        instance_id = %instance_id,
        subtype = %knowledge_subtype.as_str(),
        confidence,
        confidence_explicit,
        importance = importance.unwrap_or(f32::NAN),
        importance_explicit,
        "memory write score distribution"
    );

    Ok(format!(
        "Stored episode: \"{content}\" (confidence: {conf:.2}, subtype: {sub})",
        conf = confidence,
        sub = knowledge_subtype.as_str()
    ))
}

// ── memory_recall ─────────────────────────────────────────────────────────

/// `memory_recall` — hybrid retrieval over one instance namespace.
///
/// The strategy (hybrid / BM25 / vector) stays an engine decision (ADR-062);
/// the tool only validates the filters and formats the hits.
async fn memory_recall(state: &MemoryState, instance_id: &str, args: Value) -> Result<String> {
    let args = parse_args("memory_recall", args)?;
    let query = args.get("query").and_then(Value::as_str).unwrap_or("");
    let since = args.get("since").and_then(Value::as_str);
    let until = args.get("until").and_then(Value::as_str);
    let exclude_session_id = args
        .get("exclude_session_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty());

    // At least one filter criterion is required (query or time range).
    if query.trim().is_empty() && since.is_none() && until.is_none() {
        return Err(ServiceError::BadRequest(
            "Provide at least 'query' (keywords) or time range ('since'/'until')".into(),
        ));
    }

    if let Some(s) = since
        && chrono::DateTime::parse_from_rfc3339(s).is_err()
    {
        return Err(ServiceError::BadRequest(format!(
            "Invalid 'since' date: {s}. Expected RFC 3339 format, e.g. 2025-03-01T00:00:00Z"
        )));
    }
    if let Some(u) = until
        && chrono::DateTime::parse_from_rfc3339(u).is_err()
    {
        return Err(ServiceError::BadRequest(format!(
            "Invalid 'until' date: {u}. Expected RFC 3339 format, e.g. 2025-03-01T00:00:00Z"
        )));
    }
    if let (Some(s), Some(u)) = (since, until)
        && let (Ok(s_dt), Ok(u_dt)) = (
            chrono::DateTime::parse_from_rfc3339(s),
            chrono::DateTime::parse_from_rfc3339(u),
        )
        && s_dt >= u_dt
    {
        return Err(ServiceError::BadRequest(
            "'since' must be before 'until'".into(),
        ));
    }

    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .map_or(5, |v| v.min(20)) as usize;

    let mut memory_query = MemoryQuery::deep_recall(query.to_string(), exclude_session_id);
    memory_query.limit = limit;

    // since/until → time_range, with a sane bound on single-sided ranges:
    // `since` only → [since, now]; `until` only → [epoch, until].
    if let Some(s) = since
        && let Ok(s_dt) = chrono::DateTime::parse_from_rfc3339(s)
    {
        let until_dt = until
            .and_then(|u| chrono::DateTime::parse_from_rfc3339(u).ok())
            .map(|u| u.with_timezone(&chrono::Utc))
            .unwrap_or_else(chrono::Utc::now);
        memory_query.filters.time_range = Some((s_dt.with_timezone(&chrono::Utc), until_dt));
    } else if let Some(u) = until
        && let Ok(u_dt) = chrono::DateTime::parse_from_rfc3339(u)
    {
        let epoch =
            chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0).unwrap_or_else(chrono::Utc::now);
        memory_query.filters.time_range = Some((epoch, u_dt.with_timezone(&chrono::Utc)));
    }

    // Retrieval quality parameters are service-wide now: the per-agent
    // `MemoryManagerConfig` lived inside the Runtime session handle and has no
    // counterpart on this side of the boundary. Defaults until the service
    // config grows a quality section.
    let manager = MemoryManager::new(MemoryManagerConfig::default());
    let store = state.store(instance_id)?;
    let embedder = state.embedding();

    let retrieval = manager
        .retrieve(store_ref(&store), &mut memory_query, embedder.as_deref())
        .await
        .map_err(|e| ServiceError::Internal(format!("Memory retrieval failed: {e}")))?;

    // Recall is the only read path since ADR-101, so it also carries the
    // "procedure was actually used" signal that the removed auto-injection
    // step used to feed into activation / confidence boosting.
    manager.activate_procedural_nodes(store_ref(&store), &retrieval.memories);

    if retrieval.memories.is_empty() {
        return Ok("No relevant memories found.".to_string());
    }

    let lines: Vec<String> = retrieval
        .memories
        .iter()
        .map(|m| format!("- [{}] (score={:.2}) {}", m.label, m.score, m.content))
        .collect();
    Ok(lines.join("\n"))
}

/// Hand the store to [`MemoryManager`] as the provider trait object.
fn store_ref(store: &std::sync::Arc<SqliteStore>) -> &dyn crate::MemoryProvider {
    store.as_ref()
}

// ── dispatch ──────────────────────────────────────────────────────────────

/// Route a `tools/call` by name. Both tools require an actor (see the module
/// docs); unknown names are a bad request, not a silent no-op.
pub async fn dispatch(
    state: &MemoryState,
    actor: Option<&str>,
    name: &str,
    args: Value,
) -> Result<String> {
    let instance_id = require_instance(actor)?;
    match name {
        "memory_store" => memory_store(state, &instance_id, args).await,
        "memory_recall" => memory_recall(state, &instance_id, args).await,
        other => Err(ServiceError::BadRequest(format!("unknown tool: {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    const INSTANCE: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const OTHER: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";

    fn state_in(dir: &std::path::Path) -> Arc<MemoryState> {
        let config = crate::service::config::MemoryServiceConfig {
            data_dir: dir.to_path_buf(),
            // The tool contract (identity, validation, isolation) is what these
            // tests pin. The write path embeds when a backend is installed, and
            // leaving the default endpoint on would make every store call depend
            // on whether a sidecar happens to be listening on this machine.
            embedding: crate::service::config::EmbeddingSection {
                enabled: false,
                ..Default::default()
            },
            ..Default::default()
        };
        Arc::new(MemoryState::new(config).expect("state"))
    }

    async fn ok(state: &MemoryState, actor: Option<&str>, name: &str, args: Value) -> String {
        dispatch(state, actor, name, args)
            .await
            .unwrap_or_else(|e| panic!("tool {name} should succeed, got: {e:?}"))
    }

    async fn err(
        state: &MemoryState,
        actor: Option<&str>,
        name: &str,
        args: Value,
    ) -> ServiceError {
        dispatch(state, actor, name, args)
            .await
            .expect_err("expected an error")
    }

    #[tokio::test]
    async fn store_then_recall_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path());

        let text = ok(
            &state,
            Some(INSTANCE),
            "memory_store",
            json!({
                "content": "User lives in Beijing",
                "category": "fact",
                "confidence": 0.9,
                "keywords": ["beijing", "BEIJING", "location"],
                "normalized": "User lives in Beijing.",
            }),
        )
        .await;
        // Byte-compatible with the Runtime builtin output.
        assert_eq!(
            text,
            "Stored episode: \"User lives in Beijing\" (confidence: 0.90, subtype: Fact)"
        );

        let found = ok(
            &state,
            Some(INSTANCE),
            "memory_recall",
            json!({ "query": "Beijing" }),
        )
        .await;
        assert!(found.contains("User lives in Beijing"), "{found}");
        assert!(found.starts_with("- ["), "{found}");
    }

    #[tokio::test]
    async fn anonymous_callers_are_rejected_for_both_tools() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path());

        let e = err(
            &state,
            None,
            "memory_store",
            json!({ "content": "x", "category": "fact" }),
        )
        .await;
        assert!(matches!(e, ServiceError::Forbidden(_)), "got {e:?}");

        let e = err(&state, None, "memory_recall", json!({ "query": "x" })).await;
        assert!(matches!(e, ServiceError::Forbidden(_)), "got {e:?}");
    }

    #[tokio::test]
    async fn traversal_actor_never_becomes_a_path() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path());

        let e = err(
            &state,
            Some("../../escape"),
            "memory_store",
            json!({ "content": "x", "category": "fact" }),
        )
        .await;
        assert!(matches!(e, ServiceError::InvalidInstanceId(_)), "got {e:?}");
        assert!(
            !dir.path().parent().unwrap().join("escape").exists(),
            "must not create a store outside the instance root"
        );
    }

    #[tokio::test]
    async fn instances_cannot_read_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path());
        ok(
            &state,
            Some(INSTANCE),
            "memory_store",
            json!({ "content": "secret of instance A", "category": "fact" }),
        )
        .await;

        let other = ok(
            &state,
            Some(OTHER),
            "memory_recall",
            json!({ "query": "secret" }),
        )
        .await;
        assert_eq!(other, "No relevant memories found.");
    }

    #[tokio::test]
    async fn store_validation_errors_keep_the_builtin_messages() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path());

        let e = err(&state, Some(INSTANCE), "memory_store", json!({})).await;
        assert_eq!(
            e.to_string(),
            "bad request: Missing required parameter 'content'"
        );

        let e = err(
            &state,
            Some(INSTANCE),
            "memory_store",
            json!({ "content": "x" }),
        )
        .await;
        assert!(
            e.to_string()
                .contains("Missing required parameter 'category'"),
            "{e}"
        );

        let e = err(
            &state,
            Some(INSTANCE),
            "memory_store",
            json!({ "content": "x", "category": "gossip" }),
        )
        .await;
        assert!(e.to_string().contains("Invalid category 'gossip'"), "{e}");
    }

    #[tokio::test]
    async fn recall_requires_a_filter_and_valid_dates() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path());

        let e = err(&state, Some(INSTANCE), "memory_recall", json!({})).await;
        assert!(e.to_string().contains("Provide at least 'query'"), "{e}");

        let e = err(
            &state,
            Some(INSTANCE),
            "memory_recall",
            json!({ "query": "x", "since": "last tuesday" }),
        )
        .await;
        assert!(e.to_string().contains("Invalid 'since' date"), "{e}");

        let e = err(
            &state,
            Some(INSTANCE),
            "memory_recall",
            json!({
                "query": "x",
                "since": "2025-03-02T00:00:00Z",
                "until": "2025-03-01T00:00:00Z",
            }),
        )
        .await;
        assert!(
            e.to_string().contains("'since' must be before 'until'"),
            "{e}"
        );
    }

    #[tokio::test]
    async fn time_range_is_applied_and_a_time_only_call_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path());
        ok(
            &state,
            Some(INSTANCE),
            "memory_store",
            json!({ "content": "moved to Hangzhou", "category": "fact" }),
        )
        .await;

        // Window covering the write + matching text → hit.
        let found = ok(
            &state,
            Some(INSTANCE),
            "memory_recall",
            json!({ "query": "Hangzhou", "since": "2000-01-01T00:00:00Z" }),
        )
        .await;
        assert!(found.contains("moved to Hangzhou"), "{found}");

        // Window closed before the write → the filter really is wired in.
        let missed = ok(
            &state,
            Some(INSTANCE),
            "memory_recall",
            json!({ "query": "Hangzhou", "until": "2001-01-01T00:00:00Z" }),
        )
        .await;
        assert_eq!(missed, "No relevant memories found.");

        // A time-only call passes validation (the builtin accepted it too).
        // With no text there is no lexical term to match, so the engine
        // answers empty rather than listing the window.
        let time_only = ok(
            &state,
            Some(INSTANCE),
            "memory_recall",
            json!({ "since": "2000-01-01T00:00:00Z" }),
        )
        .await;
        assert_eq!(time_only, "No relevant memories found.");
    }

    #[tokio::test]
    async fn exclude_session_id_hides_a_memory() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path());
        // Write directly with a known session id (the tool stamps the instance
        // id, which is what `exclude_session_id` has to match).
        let store = state.store(INSTANCE).unwrap();
        let mut episode = Episode {
            session_id: "session-42".to_string(),
            turn_index: 0,
            role: "user".to_string(),
            content: "already in the conversation context".to_string(),
            embedding: None,
            timestamp: chrono::Utc::now(),
            consolidated: false,
            metadata: Default::default(),
            importance: 0.5,
            knowledge_subtype: None,
            normalized: None,
        };
        episode
            .metadata
            .insert("confidence".to_string(), Value::from(0.9f64));
        store.store_episode(&episode).unwrap();

        let kept = ok(
            &state,
            Some(INSTANCE),
            "memory_recall",
            json!({ "query": "conversation context" }),
        )
        .await;
        assert!(kept.contains("already in the conversation"), "{kept}");

        let hidden = ok(
            &state,
            Some(INSTANCE),
            "memory_recall",
            json!({
                "query": "conversation context",
                "exclude_session_id": "session-42",
            }),
        )
        .await;
        assert_eq!(hidden, "No relevant memories found.");
    }

    #[tokio::test]
    async fn unknown_tool_is_a_bad_request() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path());
        let e = err(&state, Some(INSTANCE), "memory_forget", json!({})).await;
        assert!(e.to_string().contains("unknown tool"), "{e}");
    }
}

