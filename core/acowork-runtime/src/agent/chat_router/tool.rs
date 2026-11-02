//! Router-turn tools (ADR-099 D3, shrunk by ADR-106 D6).
//!
//! The router has two delivery acts and neither of them silently moves a human
//! message into an execution context:
//!
//! - `new_task(brief)` — open a fresh execution session. This is the only
//!   delivery a model may perform, because a new session belongs to nobody's
//!   prior context, so nothing can be contaminated by a wrong guess;
//! - `propose_existing(alias, why)` — say "this looks like a continuation of
//!   that one" and hand the human a confirmation card. It delivers nothing: a
//!   proposal is acted on only when the person who typed the message confirms it.
//!
//! **`dispatch(target = <existing alias>)` is deleted** (ADR-106 D6: "LLM 失去
//! 静默投递权"). Continuing a session is addressing, and addressing belongs to
//! the human — the transcript's reference rows (level 1), the second level of
//! the `@` (level 2), or the deterministic sole-candidate rule (level 3). A
//! model that happens to guess the session the human meant was still guessing.
//!
//! **Not calling either tool** is the third act — a plain chat answer, which is
//! also how the router asks a clarifying question (`ask_human` in the ADR's act
//! list needs no separate tool: a chat answer addressed to the human *is* the
//! question). Alongside the acts sit read-only information tools
//! (`memory_recall`, `http_request` narrowed to GET) so answering a factual
//! question does not require touching an execution session at all. The executor
//! boundary is unchanged: no execution tool is ever visible here, so a package
//! cannot talk the router into doing work — lookups read information, they never
//! change anything.
//!
//! The alias form (`S1`, `S2`, …) survives, with a narrowed purpose: it is the
//! proposal vocabulary. Candidate ids are runtime state; a model that only ever
//! sees short aliases cannot invent a plausible-looking session id, cannot leak
//! one into the chat log, and cannot aim a proposal at a session the permission
//! filter did not admit. The runtime owns the alias → session-id table for the
//! duration of one turn.

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::agent::session::intent_routing::SessionSeed;

/// Tool names, fixed across turns — they are part of the cached prefix.
pub const NEW_TASK_TOOL: &str = "new_task";
/// The proposal act: name a candidate, ask the human to confirm the continue.
pub const PROPOSE_TOOL: &str = "propose_existing";
/// MCP server name the official memory service registers under (ADR-101).
pub const MEMORY_SERVER_NAME: &str = "memory";
/// The recall tool the memory service exposes (ADR-101 §MCP).
pub const MEMORY_RECALL_SERVICE_TOOL: &str = "memory_recall";
/// HTTP lookup exposed on a router turn, **GET only** (see
/// [`http_request_router_spec`]). The router-side schema drops every
/// mutating field so a chat turn cannot issue a side-effecting request.
pub const HTTP_REQUEST_TOOL: &str = "http_request";

/// The `all_tools` name of the memory service's recall tool.
///
/// ADR-101: the Runtime no longer carries a builtin `memory_recall`, so the
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

/// The `new_task` schema: a brief, and an optional configuration for the
/// session it opens. There is no `target` — this tool cannot address anything,
/// it only ever starts something (ADR-106 D6).
///
/// Field order and wording are part of the cached prefix (design 29 §6.2):
/// never make them depend on turn state.
pub fn new_task_tool_spec() -> Value {
    json!({
        "type": "object",
        "properties": {
            "message": {
                "type": "string",
                "description": "REQUIRED, non-empty task brief. The new execution session starts with no history, so this IS its only context: write a self-contained instruction that states the user's goal plus the background from this conversation it needs."
            },
            "session_config": {
                "type": "object",
                "description": "Optional configuration for the new execution session. Only what the user explicitly asked for.",
                "properties": {
                    "model": { "type": "string" },
                    "provider": { "type": "string" },
                    "workspace": { "type": "string" },
                    "reasoning_effort": { "type": "string" }
                }
            }
        },
        "required": ["message"]
    })
}

/// The `propose_existing` schema: which candidate, and the one sentence the
/// human reads before deciding. `why` is required because a card that names a
/// session without a reason asks the human to do the routing work the proposal
/// was supposed to save them.
pub fn propose_existing_tool_spec() -> Value {
    json!({
        "type": "object",
        "properties": {
            "target": {
                "type": "string",
                "description": "A candidate alias from the [candidates] block (S1, S2, …). This is a PROPOSAL only: nothing is delivered until the human confirms the card this call produces."
            },
            "why": {
                "type": "string",
                "description": "One short sentence for the human explaining why this session looks like the right one. Shown verbatim."
            }
        },
        "required": ["target", "why"]
    })
}

/// The two acts of the router `tools` array. See [`router_tools`] for the full
/// request array.
///
/// Flat `name` / `description` / `parameters` at the top level — the same
/// `ChatRequest.tools` shape a serialized `ToolSpec` produces. The provider
/// adapters (`convert_tools` in `providers/openai.rs` and
/// `providers/anthropic.rs`) read these fields at the top level; handing
/// them the OpenAI nested form made every router tool arrive at the model
/// as `name = "unknown"` with an empty schema, so no act could ever be
/// called and no execution session was ever created.
pub fn act_tools() -> Vec<Value> {
    vec![
        json!({
            "name": NEW_TASK_TOOL,
            "description": "Start a new execution session for this turn's work. Call at most once.",
            "parameters": new_task_tool_spec(),
        }),
        json!({
            "name": PROPOSE_TOOL,
            "description": "Ask the human whether this turn continues one of the listed sessions. Delivers nothing by itself - the human confirms or refuses a card.",
            "parameters": propose_existing_tool_spec(),
        }),
    ]
}

/// Router-facing memory-recall definition. A deliberate subset of the
/// service's full schema (ADR-099 D3): keyword/time query, read-only.
/// `tool_name` is the resolved MCP wrapper name so the model calls the tool
/// under the same name the executor resolves. Flat shape — see
/// [`act_tools`].
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
/// see [`act_tools`].
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

/// Assemble the request's `tools` array from the two acts plus the enabled
/// information tools (order fixed: new_task, propose_existing, memory recall,
/// http_request).
///
/// `memory_tool` is `None` when the catalog has no official memory server —
/// the router then simply has no memory tool for that turn (ADR-101).
pub fn router_tools(memory_tool: Option<&str>, http_enabled: bool) -> Vec<Value> {
    let mut tools = act_tools();
    if let Some(name) = memory_tool {
        tools.push(memory_recall_router_spec(name));
    }
    if http_enabled {
        tools.push(http_request_router_spec());
    }
    tools
}

/// The delivery vocabulary of `SessionManager::chat_dispatch`.
///
/// It outlives the router's own `dispatch` tool: the addressing ladder uses it
/// (levels 1–3 address an existing session from a durable fact), and the
/// dispatcher needs both arms to stamp the chat binding correctly. A router
/// model may only ever cause `New`; `Existing` arrives through an address a
/// human supplied, never through a tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Continue the execution session a human addressed.
    Existing(String),
    /// Start a fresh execution session.
    New,
}

/// The act a router turn decided on. `Not calling either tool` is the third
/// arm of the decision (`Answer`, in the runner's `Outcome`).
#[derive(Debug, Clone, PartialEq)]
pub enum Act {
    /// Open a new execution session and deliver the turn to it.
    NewTask(NewTaskArgs),
    /// Offer the human a continuation of one candidate session.
    Propose(ProposeArgs),
}

/// A validated `new_task` call.
#[derive(Debug, Clone, PartialEq)]
pub struct NewTaskArgs {
    /// The task brief. Required: the session this opens has no history, so a
    /// turn with no brief would receive nothing but the raw user text.
    pub message: String,
    /// Already clamped downstream.
    pub seed: Option<SessionSeed>,
}

/// A validated `propose_existing` call.
#[derive(Debug, Clone, PartialEq)]
pub struct ProposeArgs {
    /// The alias the model used — kept alongside the resolved id so the
    /// decision log reads the way the model wrote it.
    pub alias: String,
    /// The session behind that alias. Resolved here, not in the runner: an
    /// alias that is not in this turn's table is a validation error the model
    /// gets one retry on, never a session id invented downstream.
    pub session_id: String,
    /// The one sentence shown on the confirmation card.
    pub why: String,
}

/// Why an act call was rejected. The message is fed back to the model for
/// exactly one retry (ADR-099 §5); a second failure degrades the turn to the
/// deterministic routing path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActError {
    MissingBrief,
    MissingTarget,
    UnknownAlias(String),
    MissingWhy,
}

impl ActError {
    /// Model-facing text. Deliberately names the valid forms so a retry can
    /// succeed without a second round of guessing.
    pub fn message(&self) -> String {
        match self {
            Self::MissingBrief => {
                "`new_task` requires a non-empty `message` — the execution agent starts with no \
                 history, so your brief IS its only starting context. Write a self-contained \
                 instruction summarising the user's goal and the background it needs."
                    .to_string()
            }
            Self::MissingTarget => format!(
                "`{PROPOSE_TOOL}` requires `target`: one of the aliases from the [candidates] block"
            ),
            Self::UnknownAlias(alias) => format!(
                "unknown candidate alias `{alias}` — use one of the aliases from the [candidates] block"
            ),
            Self::MissingWhy => format!(
                "`{PROPOSE_TOOL}` requires a non-empty `why` — the human is deciding on your \
                 reason, so say in one sentence what makes this session the right one"
            ),
        }
    }
}

/// Parse and validate one `new_task` call.
///
/// Validation is model-facing and never panics. A blank brief is rejected
/// rather than dropped: an empty instruction would open a session with nothing
/// to work on, and the human would read that as the agent ignoring them.
pub fn parse_new_task(args: &Value) -> Result<NewTaskArgs, ActError> {
    let message = args
        .get("message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(ActError::MissingBrief)?
        .to_string();
    let seed = match args.get("session_config") {
        Some(cfg) if !cfg.is_null() => SessionSeed::from_json(&cfg.to_string()),
        _ => None,
    };
    Ok(NewTaskArgs { message, seed })
}

/// Parse and validate one `propose_existing` call against the turn's alias
/// table.
///
/// The alias must be a known candidate (case-insensitive, as before). The
/// table is built from the permission-filtered candidate list, so an invented
/// or out-of-scope alias is refused here and never reaches a card — the human
/// is only ever offered sessions the filter admitted.
pub fn parse_propose(args: &Value, aliases: &AliasTable) -> Result<ProposeArgs, ActError> {
    let target = args
        .get("target")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(ActError::MissingTarget)?;
    let alias = target.to_ascii_uppercase();
    let session_id = aliases
        .get(&alias)
        .ok_or_else(|| ActError::UnknownAlias(target.to_string()))?
        .clone();
    let why = args
        .get("why")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(ActError::MissingWhy)?
        .to_string();
    Ok(ProposeArgs {
        alias,
        session_id,
        why,
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
    fn new_task_carries_the_brief_and_a_seed() {
        let args = json!({
            "message": "  pick this up  ",
            "session_config": { "model": "m1", "provider": "p1" }
        });
        let parsed = parse_new_task(&args).unwrap();
        assert_eq!(parsed.message, "pick this up");
        let seed = parsed.seed.expect("seed parsed");
        assert_eq!(seed.model.as_deref(), Some("m1"));
        assert_eq!(seed.provider.as_deref(), Some("p1"));
    }

    /// The regression this pins: a brief-less `new_task` used to be legal for an
    /// `Existing` target (`message` was optional because the user's own words
    /// were forwarded). Now the only act that delivers must state the task, or a
    /// fresh session opens on raw text with no context.
    #[test]
    fn a_new_task_without_a_brief_is_rejected() {
        for args in [json!({}), json!({ "message": "   " }), json!({ "message": null })] {
            assert_eq!(parse_new_task(&args), Err(ActError::MissingBrief));
        }
    }

    #[test]
    fn a_proposal_resolves_its_alias_and_keeps_the_reason() {
        // Case-insensitive alias match is unchanged: the model has no reason to
        // know the table is uppercase-keyed.
        let parsed = parse_propose(&json!({ "target": "s1", "why": "same login bug" }), &aliases())
            .unwrap();
        assert_eq!(parsed.session_id, "sess-1");
        assert_eq!(parsed.alias, "S1");
        assert_eq!(parsed.why, "same login bug");
    }

    /// An alias that is not a candidate of this turn is refused rather than
    /// fuzzy-matched to a title: a card that names the wrong session is the
    /// silent-misdelivery this whole change exists to remove.
    #[test]
    fn an_unknown_alias_or_a_missing_reason_is_rejected() {
        assert_eq!(
            parse_propose(&json!({ "target": "S9", "why": "x" }), &aliases()),
            Err(ActError::UnknownAlias("S9".into()))
        );
        assert_eq!(
            parse_propose(&json!({ "why": "x" }), &aliases()),
            Err(ActError::MissingTarget)
        );
        assert_eq!(
            parse_propose(&json!({ "target": "S1" }), &aliases()),
            Err(ActError::MissingWhy)
        );
        assert_eq!(
            parse_propose(&json!({ "target": "S1", "why": "  " }), &aliases()),
            Err(ActError::MissingWhy)
        );
    }

    /// The act schemas are part of the cached prefix — pin the shape so an
    /// accidental edit shows up as a failing test, not a cache miss. Flat form
    /// (name/description/parameters at the top level) is the `ChatRequest.tools`
    /// convention every provider adapter reads; the OpenAI nested wrapper must
    /// NOT appear here (regression: it made the adapters emit `name = "unknown"`
    /// and killed router dispatch). And `new_task` must never grow a `target`
    /// again: naming a session is the delivery right ADR-106 D6 took away.
    #[test]
    fn the_act_schemas_are_stable_and_carry_no_delivery_address() {
        let tools = act_tools();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], NEW_TASK_TOOL);
        assert_eq!(tools[1]["name"], PROPOSE_TOOL);
        assert!(tools[0].get("function").is_none());
        let new_params = &tools[0]["parameters"];
        assert_eq!(new_params["required"], json!(["message"]));
        assert!(
            new_params["properties"].get("target").is_none(),
            "`new_task` must not be able to name a session"
        );
        let propose_params = &tools[1]["parameters"];
        assert_eq!(propose_params["required"], json!(["target", "why"]));
    }

    #[test]
    fn router_tools_keeps_a_fixed_order_and_get_only_http() {
        let memory = memory_recall_tool_name();
        let all = router_tools(Some(&memory), true);
        let names: Vec<&str> = all
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec![NEW_TASK_TOOL, PROPOSE_TOOL, memory.as_str(), HTTP_REQUEST_TOOL]
        );
        // The router-side http schema must not offer a way to mutate anything.
        let http_params = &all[3]["parameters"];
        assert!(!http_params["properties"].as_object().unwrap().contains_key("method"));
        assert!(!http_params["properties"].as_object().unwrap().contains_key("body"));
        assert_eq!(http_params["required"], json!(["url"]));
        // With nothing enabled the array is exactly the two acts.
        assert_eq!(router_tools(None, false).len(), 2);
        // Deterministic bytes for a fixed enablement set (cached prefix).
        assert_eq!(
            serde_json::to_string(&all).unwrap(),
            serde_json::to_string(&router_tools(Some(&memory), true)).unwrap()
        );
    }

    /// ADR-101: the memory lookup is resolved from the catalog, so with no
    /// official memory server the router exposes no memory tool at all —
    /// a third-party memory MCP must not be silently half-wired.
    #[test]
    fn router_tools_omits_memory_when_catalog_has_no_memory_server() {
        let lister = router_tools(None, true);
        let names: Vec<&str> = lister
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec![NEW_TASK_TOOL, PROPOSE_TOOL, HTTP_REQUEST_TOOL]);
    }
}
