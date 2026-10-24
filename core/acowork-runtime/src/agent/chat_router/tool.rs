//! Router-turn tools (ADR-099 D3).
//!
//! `dispatch` is the routing tool: one call covers both routable actions —
//! continue an existing execution session (`target = "S3"`) or start a new
//! one (`target = "new"`). **Not calling the tool** is the third action — a
//! plain chat answer. Alongside it the router exposes read-only information
//! tools (`memory_recall`, `http_request` narrowed to GET) so answering a
//! factual question does not require handing the message to an execution
//! session. The executor boundary is unchanged: no execution tool is ever
//! visible here, so a package cannot talk the router into doing work —
//! lookups read information, they never change anything.
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
/// MCP server name the official memory service registers under (ADR-100).
pub const MEMORY_SERVER_NAME: &str = "memory";
/// The recall tool the memory service exposes (ADR-100 §MCP).
pub const MEMORY_RECALL_SERVICE_TOOL: &str = "memory_recall";
/// HTTP lookup exposed on a router turn, **GET only** (see
/// [`http_request_router_spec`]). The router-side schema drops every
/// mutating field so a chat turn cannot issue a side-effecting request.
pub const HTTP_REQUEST_TOOL: &str = "http_request";

/// The `all_tools` name of the memory service's recall tool.
///
/// ADR-100: the Runtime no longer carries a builtin `memory_recall`, so the
/// router resolves the official service's MCP wrapper by name instead. The
/// shape is `<server>__<tool>` as built by `acowork_mcp::wrapper` (the
/// `mcp_` prefix marks MCP tools vs built-ins). A third-party memory MCP
/// with a different server name or tool name is deliberately **not** picked
/// up here — it stays an ordinary tool the execution session can call.
pub fn memory_recall_tool_name() -> String {
    format!("mcp_{MEMORY_SERVER_NAME}__{MEMORY_RECALL_SERVICE_TOOL}")
}

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

/// The `dispatch` entry of the router `tools` array. See
/// [`router_tools`] for the full request array.
///
/// Flat `name` / `description` / `parameters` at the top level — the same
/// `ChatRequest.tools` shape a serialized `ToolSpec` produces. The provider
/// adapters (`convert_tools` in `providers/openai.rs` and
/// `providers/anthropic.rs`) read these fields at the top level; handing
/// them the OpenAI nested form made every router tool arrive at the model
/// as `name = "unknown"` with an empty schema, so `dispatch` could never be
/// called and no execution session was ever created.
pub fn dispatch_tools() -> Vec<Value> {
    vec![json!({
        "name": DISPATCH_TOOL,
        "description": "Route this conversation turn to an execution session. Call at most once.",
        "parameters": dispatch_tool_spec(),
    })]
}

/// Router-facing memory-recall definition. A deliberate subset of the
/// service's full schema (ADR-099 D3): keyword/time query, read-only.
/// `tool_name` is the resolved MCP wrapper name so the model calls the tool
/// under the same name the executor resolves. Flat shape — see
/// [`dispatch_tools`].
pub fn memory_recall_router_spec(tool_name: &str) -> Value {
    json!({
        "name": tool_name,
        "description": "Search this agent's long-term memory for a fact or preference needed to answer. Returns scored results. Provide 'query' (keywords) and/or a time range. Look up at most what the answer needs.",
        "parameters": {
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Targeted keywords or phrase (optional if since/until provided)."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max results to return (default: 5)."
                },
                "since": {
                    "type": "string",
                    "description": "Only memories created at or after this time (RFC 3339)."
                },
                "until": {
                    "type": "string",
                    "description": "Only memories created at or before this time (RFC 3339)."
                }
            }
        }
    })
}

/// Router-facing `http_request` definition — GET only. No `method`, `body`,
/// or `content_type`: the router can read the web, never write to it. The
/// runner additionally forces `method = "GET"` at execution time so a
/// hallucinated mutating argument cannot reach the tool either. Flat shape —
/// see [`dispatch_tools`].
pub fn http_request_router_spec() -> Value {
    json!({
        "name": HTTP_REQUEST_TOOL,
        "description": "Fetch a URL with an HTTP GET to look up information needed to answer. Read-only: no request body, no mutating methods.",
        "parameters": {
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The http(s) URL to fetch."
                },
                "headers": {
                    "type": "object",
                    "description": "Optional request headers (key-value pairs).",
                    "additionalProperties": { "type": "string" }
                }
            },
            "required": ["url"]
        }
    })
}

/// Assemble the request's `tools` array from `dispatch` plus the enabled
/// information tools (order fixed: dispatch, memory recall, http_request).
///
/// `memory_tool` is `None` when the catalog has no official memory server —
/// the router then simply has no memory tool for that turn (ADR-100).
pub fn router_tools(memory_tool: Option<&str>, http_enabled: bool) -> Vec<Value> {
    let mut tools = dispatch_tools();
    if let Some(name) = memory_tool {
        tools.push(memory_recall_router_spec(name));
    }
    if http_enabled {
        tools.push(http_request_router_spec());
    }
    tools
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
    fn tool_schema_is_stable_and_dispatch_first() {
        // The schema is part of the cached prefix — pin the shape so an
        // accidental edit shows up as a failing test, not a cache miss.
        // Flat form (name/description/parameters at the top level) is the
        // ChatRequest.tools convention every provider adapter reads; the
        // OpenAI nested wrapper must NOT appear here (regression: it made
        // the adapters emit `name = "unknown"` and killed router dispatch).
        let tools = dispatch_tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], DISPATCH_TOOL);
        assert!(tools[0].get("function").is_none());
        let params = &tools[0]["parameters"];
        assert_eq!(params["required"], json!(["target"]));
        assert_eq!(params["properties"]["target"]["type"], "string");
    }

    #[test]
    fn router_tools_keeps_a_fixed_order_and_get_only_http() {
        let memory = memory_recall_tool_name();
        let all = router_tools(Some(&memory), true);
        let names: Vec<&str> = all
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec![DISPATCH_TOOL, memory.as_str(), HTTP_REQUEST_TOOL]);
        // The router-side http schema must not offer a way to mutate anything.
        let http_params = &all[2]["parameters"];
        assert!(!http_params["properties"].as_object().unwrap().contains_key("method"));
        assert!(!http_params["properties"].as_object().unwrap().contains_key("body"));
        assert_eq!(http_params["required"], json!(["url"]));
        // With nothing enabled the array is exactly the pre-tool-router one.
        assert_eq!(router_tools(None, false).len(), 1);
        // Deterministic bytes for a fixed enablement set (cached prefix).
        assert_eq!(
            serde_json::to_string(&all).unwrap(),
            serde_json::to_string(&router_tools(Some(&memory), true)).unwrap()
        );
    }

    /// ADR-100: the memory lookup is resolved from the catalog, so with no
    /// official memory server the router exposes no memory tool at all —
    /// a third-party memory MCP must not be silently half-wired.
    #[test]
    fn router_tools_omits_memory_when_catalog_has_no_memory_server() {
        let lister = router_tools(None, true);
        let names: Vec<&str> = lister
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec![DISPATCH_TOOL, HTTP_REQUEST_TOOL]);
    }
}
