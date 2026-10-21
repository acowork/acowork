//! `dispatch` — the only tool visible on a Chat Router turn (ADR-099 D15).
//!
//! One tool covers both routable actions: continue an existing execution
//! session (`target = "S3"`) or start a new one (`target = "new"`). **Not
//! calling the tool** is the third action — a plain chat answer. Keeping the
//! action set this small is the point: a router model that has no execution
//! tools cannot be talked into doing the work instead of routing it.
//!
//! The alias form (`S1`, `S2`, …) is deliberate. Candidate ids are runtime
//! state; a model that only ever sees short aliases cannot invent a
//! plausible-looking session id and cannot leak one into the chat log. The
//! runtime owns the alias → session-id table for the duration of one turn.

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::agent::session::intent_routing::SessionSeed;

/// Tool name, fixed across turns — it is part of the cached prefix.
pub const DISPATCH_TOOL: &str = "dispatch";

/// Alias → real session id for one router turn.
pub type AliasTable = HashMap<String, String>;

/// The fixed tool schema. Field order and wording are part of the cached
/// prefix (design 29 §6.2): never make them depend on turn state.
pub fn dispatch_tool_spec() -> Value {
    json!({
        "type": "object",
        "properties": {
            "target": {
                "type": "string",
                "description": "Either a candidate alias from the [candidates] block (S1, S2, …) to continue that execution session, or \"new\" to start a new one."
            },
            "message": {
                "type": "string",
                "description": "Optional instruction added for the execution session. Omit to forward the user's message verbatim."
            },
            "session_config": {
                "type": "object",
                "description": "Only meaningful with target=\"new\": optional session configuration for the new execution session.",
                "properties": {
                    "model": { "type": "string" },
                    "provider": { "type": "string" },
                    "workspace": { "type": "string" },
                    "reasoning_effort": { "type": "string" }
                }
            }
        },
        "required": ["target"]
    })
}

/// The whole `tools` array of a router request.
pub fn dispatch_tools() -> Vec<Value> {
    vec![json!({
        "type": "function",
        "function": {
            "name": DISPATCH_TOOL,
            "description": "Route this conversation turn to an execution session. Call at most once.",
            "parameters": dispatch_tool_spec(),
        }
    })]
}

/// The action a router turn decided on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Continue the execution session behind this alias.
    Existing(String),
    /// Start a fresh execution session.
    New,
}

/// A validated `dispatch` call.
#[derive(Debug, Clone, PartialEq)]
pub struct DispatchArgs {
    pub target: Target,
    /// `None` = forward the user's message verbatim.
    pub message: Option<String>,
    /// Applied only with `Target::New`; already clamped downstream.
    pub seed: Option<SessionSeed>,
}

/// Why a `dispatch` call was rejected. The message is fed back to the model
/// for exactly one retry (ADR-099 §5); a second failure degrades the turn to
/// the legacy routing path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchError {
    MissingTarget,
    UnknownAlias(String),
    EmptyMessage,
}

impl DispatchError {
    /// Model-facing text. Deliberately names the valid forms so a retry can
    /// succeed without a second round of guessing.
    pub fn message(&self) -> String {
        match self {
            Self::MissingTarget => {
                "dispatch requires `target`: a candidate alias (S1, S2, …) or \"new\"".to_string()
            }
            Self::UnknownAlias(alias) => format!(
                "unknown candidate alias `{alias}` — use one of the aliases from the [candidates] block, or \"new\""
            ),
            Self::EmptyMessage => {
                "`message` was empty — omit it to forward the user's message verbatim".to_string()
            }
        }
    }
}

/// Parse and validate one `dispatch` call against the turn's alias table.
///
/// Validation rules (all model-facing, never panics):
/// - `target` is required; `"new"` (case-insensitive) is the new-session
///   sentinel, anything else must be a known alias;
/// - `message` is optional, but a present-and-blank value is rejected — an
///   empty instruction reads as "replace the user's message with nothing";
/// - `session_config` is parsed with the same `SessionSeed` parser as the
///   inbound envelope (unknown keys ignored, malformed values dropped) and
///   is ignored for `Existing` targets: an existing session keeps its own
///   configuration (ADR-099: a config change alone is not a new task).
pub fn parse_dispatch(args: &Value, aliases: &AliasTable) -> Result<DispatchArgs, DispatchError> {
    let target = args
        .get("target")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(DispatchError::MissingTarget)?;

    let target = if target.eq_ignore_ascii_case("new") {
        Target::New
    } else {
        let alias = target.to_ascii_uppercase();
        match aliases.get(&alias) {
            Some(sid) => Target::Existing(sid.clone()),
            None => return Err(DispatchError::UnknownAlias(target.to_string())),
        }
    };

    let message = match args.get("message").and_then(Value::as_str) {
        Some(m) => {
            let trimmed = m.trim();
            if trimmed.is_empty() {
                return Err(DispatchError::EmptyMessage);
            }
            Some(trimmed.to_string())
        }
        None => None,
    };

    let seed = match (&target, args.get("session_config")) {
        (Target::New, Some(cfg)) if !cfg.is_null() => SessionSeed::from_json(&cfg.to_string()),
        _ => None,
    };

    Ok(DispatchArgs {
        target,
        message,
        seed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aliases() -> AliasTable {
        AliasTable::from([
            ("S1".to_string(), "sess-1".to_string()),
            ("S2".to_string(), "sess-2".to_string()),
        ])
    }

    #[test]
    fn existing_alias_maps_to_the_real_session_id() {
        let args = json!({ "target": "S2" });
        let parsed = parse_dispatch(&args, &aliases()).expect("valid alias");
        assert_eq!(parsed.target, Target::Existing("sess-2".into()));
        assert_eq!(parsed.message, None);
        assert!(parsed.seed.is_none());
    }

    #[test]
    fn alias_match_is_case_insensitive() {
        let parsed = parse_dispatch(&json!({ "target": "s1" }), &aliases()).unwrap();
        assert_eq!(parsed.target, Target::Existing("sess-1".into()));
    }

    #[test]
    fn new_is_the_sentinel_and_carries_a_seed() {
        let args = json!({
            "target": "NEW",
            "message": "  pick this up  ",
            "session_config": { "model": "m1", "provider": "p1" }
        });
        let parsed = parse_dispatch(&args, &aliases()).unwrap();
        assert_eq!(parsed.target, Target::New);
        assert_eq!(parsed.message.as_deref(), Some("pick this up"));
        let seed = parsed.seed.expect("seed parsed");
        assert_eq!(seed.model.as_deref(), Some("m1"));
        assert_eq!(seed.provider.as_deref(), Some("p1"));
    }

    #[test]
    fn session_config_is_ignored_for_existing_targets() {
        // ADR-099: an existing session keeps its configuration; a config
        // suggestion must not silently rotate it (T3 removed).
        let args = json!({ "target": "S1", "session_config": { "model": "m1" } });
        let parsed = parse_dispatch(&args, &aliases()).unwrap();
        assert!(parsed.seed.is_none());
    }

    #[test]
    fn unknown_alias_and_blank_message_are_rejected() {
        assert_eq!(
            parse_dispatch(&json!({ "target": "S9" }), &aliases()),
            Err(DispatchError::UnknownAlias("S9".into()))
        );
        assert_eq!(
            parse_dispatch(&json!({ "target": "   " }), &aliases()),
            Err(DispatchError::MissingTarget)
        );
        assert_eq!(
            parse_dispatch(&json!({ "target": "S1", "message": "   " }), &aliases()),
            Err(DispatchError::EmptyMessage)
        );
    }

    #[test]
    fn tool_schema_is_stable_and_single_function() {
        // The schema is part of the cached prefix — pin the shape so an
        // accidental edit shows up as a failing test, not a cache miss.
        let tools = dispatch_tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], DISPATCH_TOOL);
        let params = &tools[0]["function"]["parameters"];
        assert_eq!(params["required"], json!(["target"]));
        assert_eq!(params["properties"]["target"]["type"], "string");
    }
}
