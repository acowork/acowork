//! Router-turn tools (ADR-099 D3, shrunk by ADR-106 D6).
//!
//! The router has two delivery acts and neither of them silently moves a human
//! message into an execution context:
//!
//! - `exec_session_create(message)` — open a fresh execution session. This is
//!   the only delivery a model may perform, because a new session belongs to
//!   nobody's prior context, so nothing can be contaminated by a wrong guess;
//! - `exec_session_propose_continue(target, why)` — say "this looks like a
//!   continuation of that one" and hand the human a confirmation card. It
//!   delivers nothing: a proposal is acted on only when the person who typed
//!   the message confirms it.
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
//! (`memory_recall`, `http_request` narrowed to GET, and
//! `exec_session_read_round`, which reads back the most recent rounds of one
//! candidate session over the same bounded projection read the panel uses,
//! ADR-106 D4) so answering a factual question — including "how did that task
//! end up?" — does not require touching an execution session at all. The
//! executor boundary is unchanged: no execution tool is ever visible here, so
//! a package cannot talk the router into doing work — lookups read
//! information, they never change anything.
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
pub const EXEC_SESSION_CREATE_TOOL: &str = "exec_session_create";
/// The proposal act: name a candidate, ask the human to confirm the continue.
pub const EXEC_SESSION_PROPOSE_CONTINUE_TOOL: &str = "exec_session_propose_continue";
/// The read-back act-shaped information tool: report what a candidate session
/// last said, over the bounded D4 round projection read. Read-only.
pub const EXEC_SESSION_READ_ROUND_TOOL: &str = "exec_session_read_round";
/// MCP server name the official memory service registers under (ADR-101).
pub const MEMORY_SERVER_NAME: &str = "memory";
/// The recall tool the memory service exposes (ADR-101 §MCP).
pub const MEMORY_RECALL_SERVICE_TOOL: &str = "memory_recall";
/// HTTP lookup exposed on a router turn, **GET only** (see
/// [`http_request_router_spec`]). The router-side schema drops every
/// mutating field so a chat turn cannot issue a side-effecting request.
pub const HTTP_REQUEST_TOOL: &str = "http_request";

/// Act names a previous prompt generation taught and the wire no longer
/// offers. ADR-106 D6 retired `dispatch`; the `exec_session_*` renaming
/// retired `new_task` and `propose_existing`. Any text still naming one
/// describes a tool the model cannot call, so the drift guard in this
/// module's tests fails when the kernel prompt teaches one.
pub const RETIRED_ACT_NAMES: &[&str] = &["dispatch", "new_task", "propose_existing"];

/// How many most recent rounds one `exec_session_read_round` call may read
/// back. The read is bounded so a router turn cannot pull a whole execution
/// transcript into the chat context.
pub const READ_ROUNDS_MAX: u32 = 3;

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

/// The `exec_session_create` schema: the **background** the new session needs,
/// and an optional configuration for it. There is no `target` — this tool
/// cannot address anything, it only ever starts something (ADR-106 D6).
///
/// **The human's own message is not an argument.** The runtime already holds
/// it verbatim and delivers it as the turn; asking the model to restate it was
/// what produced a persisted `user` row that began with the router's fixed
/// opening sentence instead of the person's words (ADR-107). `background` is
/// context only: what was decided, what is constrained, what was already
/// discussed — never the ask itself.
///
/// Field order and wording are part of the cached prefix (design 29 §6.2):
/// never make them depend on turn state.
pub fn exec_session_create_tool_spec() -> Value {
    json!({
        "type": "object",
        "properties": {
            "background": {
                "type": "string",
                "description": "REQUIRED, non-empty. The new execution session has no history, so this is the context it is missing: what was decided in this conversation, constraints and preferences already stated, files already looked at. Do NOT restate the user's request - the runtime delivers the user's own message itself. Keep it compact: a few lines, not a document."
            },
            "session_config": {
                "type": "object",
                "description": "Optional configuration for the new execution session. Only what the user explicitly asked for.",
                "properties": {
                    "model": { "type": "string" },
                    "provider": { "type": "string" },
                    "workspace": {
                        "type": "string",
                        "description": "The workspace id — the `id …` from Block C's `workspace=` entry — never its name: the runtime validates it against the workspace registry by id."
                    },
                    "reasoning_effort": { "type": "string" }
                }
            }
        },
        "required": ["background"]
    })
}

/// The `exec_session_propose_continue` schema: which candidate, and the one
/// sentence the human reads before deciding. `why` is required because a card
/// that names a session without a reason asks the human to do the routing work
/// the proposal was supposed to save them.
pub fn exec_session_propose_continue_tool_spec() -> Value {
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
            "name": EXEC_SESSION_CREATE_TOOL,
            "description": "Start a new execution session for this turn's work. Call at most once.",
            "parameters": exec_session_create_tool_spec(),
        }),
        json!({
            "name": EXEC_SESSION_PROPOSE_CONTINUE_TOOL,
            "description": "Ask the human whether this turn continues one of the listed sessions. Delivers nothing by itself - the human confirms or refuses a card.",
            "parameters": exec_session_propose_continue_tool_spec(),
        }),
    ]
}

/// Router-facing `exec_session_read_round` definition — a read-only
/// information tool, not a delivery act. It reads back the most recent rounds
/// of one candidate session (the human's question, the session's answer, its
/// terminal state) through the same bounded projection read the panel uses
/// (ADR-106 D4), so it works whether or not the session is resident. Flat
/// shape — see [`act_tools`].
pub fn exec_session_read_round_router_spec() -> Value {
    json!({
        "name": EXEC_SESSION_READ_ROUND_TOOL,
        "description": "Read back what one candidate execution session last reported: its recent rounds - the question, the answer, the terminal state. Read-only: delivers nothing, changes nothing. The only legitimate source for anything you say about what a session did.",
        "parameters": {
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "description": "A candidate alias from the [candidates] block (S1, S2, …)."
                },
                "rounds": {
                    "type": "integer",
                    "description": "How many most recent rounds to read back (1 to 3, default 1)."
                }
            },
            "required": ["target"]
        }
    })
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
/// information tools (order fixed: exec_session_create,
/// exec_session_propose_continue, memory recall, http_request,
/// exec_session_read_round).
///
/// `memory_tool` is `None` when the catalog has no official memory server —
/// the router then simply has no memory tool for that turn (ADR-101).
/// `exec_session_read_round` is always present: it can only read sessions the
/// permission filter already admitted to this turn's candidate set, so it
/// needs no separate enablement.
pub fn router_tools(memory_tool: Option<&str>, http_enabled: bool) -> Vec<Value> {
    let mut tools = act_tools();
    if let Some(name) = memory_tool {
        tools.push(memory_recall_router_spec(name));
    }
    if http_enabled {
        tools.push(http_request_router_spec());
    }
    tools.push(exec_session_read_round_router_spec());
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
    ExecSessionCreate(ExecSessionCreateArgs),
    /// Offer the human a continuation of one candidate session.
    ProposeContinue(ProposeContinueArgs),
}

/// A validated `exec_session_create` call.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecSessionCreateArgs {
    /// The background the new session needs (ADR-107). Required: the session
    /// this opens has no history, so a human sentence that refers to "the thing
    /// we discussed" would arrive with nothing to resolve it against. Delivered
    /// as a reference part AFTER the human's words — never as the message.
    pub background: String,
    /// Already clamped downstream.
    pub seed: Option<SessionSeed>,
}

/// A validated `exec_session_propose_continue` call.
#[derive(Debug, Clone, PartialEq)]
pub struct ProposeContinueArgs {
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

/// A validated `exec_session_read_round` call: the session behind a
/// candidate alias plus the bounded number of most recent rounds to read.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadRoundArgs {
    /// The alias the model used — kept so the decision trace reads the way
    /// the model wrote it.
    pub alias: String,
    /// The session behind that alias, resolved against this turn's table.
    pub session_id: String,
    /// Rounds to read back, already clamped to 1..=`READ_ROUNDS_MAX`.
    pub rounds: u32,
}

/// Why an act call was rejected. The message is fed back to the model for
/// exactly one retry (ADR-099 §5); a second failure degrades the turn to the
/// deterministic routing path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActError {
    MissingBackground,
    MissingTarget,
    UnknownAlias(String),
    MissingWhy,
}

impl ActError {
    /// Model-facing text. Deliberately names the valid forms so a retry can
    /// succeed without a second round of guessing.
    pub fn message(&self) -> String {
        match self {
            Self::MissingBackground => {
                "`exec_session_create` requires a non-empty `background`: the execution agent starts with no history, so this is the context it is missing. Give it what was decided, what constrains the work and what was already discussed. Do not restate the user's request - the runtime delivers the user's own message."
                    .to_string()
            }
            Self::MissingTarget => {
                "`target` is required: one of the aliases from the [candidates] block".to_string()
            }
            Self::UnknownAlias(alias) => format!(
                "unknown candidate alias `{alias}` — use one of the aliases from the [candidates] block"
            ),
            Self::MissingWhy => format!(
                "`{EXEC_SESSION_PROPOSE_CONTINUE_TOOL}` requires a non-empty `why` — the human is deciding on your \
                 reason, so say in one sentence what makes this session the right one"
            ),
        }
    }
}

/// Parse and validate one `exec_session_create` call.
///
/// Validation is model-facing and never panics. A blank background is rejected
/// rather than dropped: a session with no history would then be handed a
/// sentence that refers to "what we discussed" with nothing to resolve it
/// against, and the human would read the result as the agent ignoring them.
pub fn parse_exec_session_create(args: &Value) -> Result<ExecSessionCreateArgs, ActError> {
    let background = args
        .get("background")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(ActError::MissingBackground)?
        .to_string();
    let seed = match args.get("session_config") {
        Some(cfg) if !cfg.is_null() => SessionSeed::from_json(&cfg.to_string()),
        _ => None,
    };
    Ok(ExecSessionCreateArgs { background, seed })
}

/// Parse and validate one `exec_session_propose_continue` call against the
/// turn's alias table.
///
/// The alias must be a known candidate (case-insensitive, as before). The
/// table is built from the permission-filtered candidate list, so an invented
/// or out-of-scope alias is refused here and never reaches a card — the human
/// is only ever offered sessions the filter admitted.
pub fn parse_exec_session_propose_continue(
    args: &Value,
    aliases: &AliasTable,
) -> Result<ProposeContinueArgs, ActError> {
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
    Ok(ProposeContinueArgs {
        alias,
        session_id,
        why,
    })
}

/// Parse and validate one `exec_session_read_round` call against the turn's
/// alias table.
///
/// Same alias discipline as a proposal: the read can only name a session this
/// turn's permission-filtered candidate set admitted. `rounds` is clamped, not
/// rejected — an oversized request is a bounded read of the newest rounds, so
/// the right repair is "fewer", not a wasted retry.
pub fn parse_exec_session_read_round(
    args: &Value,
    aliases: &AliasTable,
) -> Result<ReadRoundArgs, ActError> {
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
    let rounds = args
        .get("rounds")
        .and_then(Value::as_u64)
        .map(|r| (r as u32).clamp(1, READ_ROUNDS_MAX))
        .unwrap_or(1);
    Ok(ReadRoundArgs {
        alias,
        session_id,
        rounds,
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
    fn exec_session_create_carries_the_background_and_a_seed() {
        let args = json!({
            "background": "  decided: keep the tests untouched  ",
            "session_config": { "model": "m1", "provider": "p1" }
        });
        let parsed = parse_exec_session_create(&args).unwrap();
        assert_eq!(parsed.background, "decided: keep the tests untouched");
        let seed = parsed.seed.expect("seed parsed");
        assert_eq!(seed.model.as_deref(), Some("m1"));
        assert_eq!(seed.provider.as_deref(), Some("p1"));
    }

    /// The regression this pins: a background-less create call used to be legal
    /// for an `Existing` target (the field was optional because the user's own
    /// words were forwarded). Now the only act that delivers must state the
    /// context the fresh session is missing, or a message like "continue the
    /// thing we discussed" opens a session with nothing to resolve it against.
    ///
    /// ADR-107 also pins the OLD field name: `message` is not accepted at all,
    /// because a schema that still takes it invites the model to re-author the
    /// human's message — the failure this change removes.
    #[test]
    fn a_create_without_a_background_is_rejected() {
        for args in [
            json!({}),
            json!({ "background": "   " }),
            json!({ "background": null }),
            json!({ "message": "do the thing" }),
        ] {
            assert_eq!(
                parse_exec_session_create(&args),
                Err(ActError::MissingBackground)
            );
        }
    }

    #[test]
    fn a_proposal_resolves_its_alias_and_keeps_the_reason() {
        // Case-insensitive alias match is unchanged: the model has no reason to
        // know the table is uppercase-keyed.
        let parsed = parse_exec_session_propose_continue(
            &json!({ "target": "s1", "why": "same login bug" }),
            &aliases(),
        )
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
            parse_exec_session_propose_continue(&json!({ "target": "S9", "why": "x" }), &aliases()),
            Err(ActError::UnknownAlias("S9".into()))
        );
        assert_eq!(
            parse_exec_session_propose_continue(&json!({ "why": "x" }), &aliases()),
            Err(ActError::MissingTarget)
        );
        assert_eq!(
            parse_exec_session_propose_continue(&json!({ "target": "S1" }), &aliases()),
            Err(ActError::MissingWhy)
        );
        assert_eq!(
            parse_exec_session_propose_continue(&json!({ "target": "S1", "why": "  " }), &aliases()),
            Err(ActError::MissingWhy)
        );
    }

    /// ADR-106 D6's "new task" is a fact the ladder must honour at every level,
    /// not only at the deterministic sole-candidate shortcut.
    ///
    /// The measured defect this pins: the human answered an addressing card with
    /// "new task", the turn reached the router with `force_new`, and the router
    /// was given the ordinary candidate list anyway — so the model proposed the
    /// very continuation that had just been refused, six turns in a row, each
    /// write landing on the same idempotency key (`confirm_continue:<message>`).
    /// Nothing was appended, nothing was delivered, and the click looked dead.
    ///
    /// With no candidates there is no alias, so a proposal cannot name a session
    /// at all — the model's only remaining acts are `exec_session_create` and a
    /// plain reply.
    #[test]
    fn a_refused_continuation_offers_no_candidate_to_name() {
        // The alias table of a forced-new turn is empty because the turn was
        // offered no candidates, so every target is an unknown alias.
        for target in ["S1", "S2", "s3"] {
            assert_eq!(
                parse_exec_session_propose_continue(
                    &json!({ "target": target, "why": "same login bug" }),
                    &AliasTable::new(),
                ),
                Err(ActError::UnknownAlias(target.into())),
                "a forced-new turn must not be able to produce a continuation card"
            );
        }
        // The read tool is refused the same way: there is no session this turn
        // may look at, so there is nothing to read back either.
        assert!(parse_exec_session_read_round(
            &json!({ "target": "S1" }),
            &AliasTable::new()
        )
        .is_err());
    }

    /// The read tool shares the proposal's alias discipline — it can name no
    /// session the candidate filter did not admit — and clamps `rounds` rather
    /// than rejecting it: the honest repair for an oversized read is fewer
    /// rounds, not a retry.
    #[test]
    fn a_read_resolves_its_alias_and_clamps_the_round_count() {
        let parsed = parse_exec_session_read_round(&json!({ "target": "s2", "rounds": 2 }), &aliases())
            .unwrap();
        assert_eq!(parsed.session_id, "sess-2");
        assert_eq!(parsed.alias, "S2");
        assert_eq!(parsed.rounds, 2);
        // Default and both clamp bounds.
        assert_eq!(
            parse_exec_session_read_round(&json!({ "target": "S1" }), &aliases())
                .unwrap()
                .rounds,
            1
        );
        assert_eq!(
            parse_exec_session_read_round(&json!({ "target": "S1", "rounds": 0 }), &aliases())
                .unwrap()
                .rounds,
            1
        );
        assert_eq!(
            parse_exec_session_read_round(
                &json!({ "target": "S1", "rounds": READ_ROUNDS_MAX + 10 }),
                &aliases()
            )
            .unwrap()
            .rounds,
            READ_ROUNDS_MAX
        );
        // Same alias refusals as a proposal.
        assert_eq!(
            parse_exec_session_read_round(&json!({ "target": "S7" }), &aliases()),
            Err(ActError::UnknownAlias("S7".into()))
        );
        assert_eq!(
            parse_exec_session_read_round(&json!({}), &aliases()),
            Err(ActError::MissingTarget)
        );
    }

    /// The act schemas are part of the cached prefix — pin the shape so an
    /// accidental edit shows up as a failing test, not a cache miss. Flat form
    /// (name/description/parameters at the top level) is the `ChatRequest.tools`
    /// convention every provider adapter reads; the OpenAI nested wrapper must
    /// NOT appear here (regression: it made the adapters emit `name = "unknown"`
    /// and killed router dispatch). And `exec_session_create` must never grow a
    /// `target` again: naming a session is the delivery right ADR-106 D6 took
    /// away.
    #[test]
    fn the_act_schemas_are_stable_and_carry_no_delivery_address() {
        let tools = act_tools();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], EXEC_SESSION_CREATE_TOOL);
        assert_eq!(tools[1]["name"], EXEC_SESSION_PROPOSE_CONTINUE_TOOL);
        assert!(tools[0].get("function").is_none());
        let new_params = &tools[0]["parameters"];
        assert_eq!(new_params["required"], json!(["background"]));
        assert!(
            new_params["properties"].get("message").is_none(),
            "the human's message is the runtime's to deliver, never the model's to restate (ADR-107)"
        );
        assert!(
            new_params["properties"].get("target").is_none(),
            "`exec_session_create` must not be able to name a session"
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
            vec![
                EXEC_SESSION_CREATE_TOOL,
                EXEC_SESSION_PROPOSE_CONTINUE_TOOL,
                memory.as_str(),
                HTTP_REQUEST_TOOL,
                EXEC_SESSION_READ_ROUND_TOOL
            ]
        );
        // The router-side http schema must not offer a way to mutate anything.
        let http_params = &all[3]["parameters"];
        assert!(!http_params["properties"].as_object().unwrap().contains_key("method"));
        assert!(!http_params["properties"].as_object().unwrap().contains_key("body"));
        assert_eq!(http_params["required"], json!(["url"]));
        // The read tool is always present (it can only name admitted
        // candidates), so with nothing else enabled the array is the two acts
        // plus the read.
        assert_eq!(router_tools(None, false).len(), 3);
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
        assert_eq!(
            names,
            vec![
                EXEC_SESSION_CREATE_TOOL,
                EXEC_SESSION_PROPOSE_CONTINUE_TOOL,
                HTTP_REQUEST_TOOL,
                EXEC_SESSION_READ_ROUND_TOOL
            ]
        );
    }

    /// The prompt and the wire schema are two halves of one contract, and they
    /// have drifted twice with the same outcome — the model could not call any
    /// act, so no execution session was ever created:
    ///
    /// 1. ADR-106 D6 replaced `dispatch` with `new_task` / `propose_existing`
    ///    but left the kernel prompt teaching `dispatch` (2026-11); the
    ///    `exec_session_*` renaming then retired those two names in turn.
    /// 2. Before that, the OpenAI-nested tool shape made every router tool
    ///    arrive as `name = "unknown"` with an empty schema.
    ///
    /// Neither was caught by review; both were caught from a production log.
    /// This pins the two halves to each other instead of trusting a reader to
    /// notice the mismatch. It covers parameter names as well as tool names: a
    /// schema that renames `message` while the prompt still says `brief` is
    /// just as broken and just as invisible.
    #[test]
    fn the_prompt_teaches_exactly_the_tools_the_wire_offers() {
        use acowork_core::chat_prompt::CHAT_ROUTER_BASE_PROMPT as prompt;

        // The acts and the HTTP lookup carry names fixed across turns, so the
        // prompt can and must name them exactly. The memory tool cannot: its
        // wire name is the MCP-resolved `mcp_<server>__<tool>`, which a
        // byte-stable prompt may not hardcode — the prompt names the capability
        // and the model reads the real name off the tools array. That asymmetry
        // is deliberate, so it is asserted rather than left implicit.
        for name in [
            EXEC_SESSION_CREATE_TOOL,
            EXEC_SESSION_PROPOSE_CONTINUE_TOOL,
            HTTP_REQUEST_TOOL,
            EXEC_SESSION_READ_ROUND_TOOL,
        ] {
            assert!(
                prompt.contains(name),
                "`{name}` is offered on the wire but never named in \
                 CHAT_ROUTER_BASE_PROMPT — the model cannot call what the prompt \
                 does not teach"
            );
        }
        assert!(
            prompt.contains("memory"),
            "the router may carry a memory-recall tool, but the prompt never \
             mentions memory at all"
        );

        // Each act's parameters, not only its name.
        for (tool, spec) in [
            (EXEC_SESSION_CREATE_TOOL, exec_session_create_tool_spec()),
            (
                EXEC_SESSION_PROPOSE_CONTINUE_TOOL,
                exec_session_propose_continue_tool_spec(),
            ),
            (
                EXEC_SESSION_READ_ROUND_TOOL,
                exec_session_read_round_router_spec()["parameters"].clone(),
            ),
        ] {
            for param in spec["properties"].as_object().unwrap().keys() {
                assert!(
                    prompt.contains(param.as_str()),
                    "`{tool}` takes `{param}`, but CHAT_ROUTER_BASE_PROMPT never \
                     mentions it — the prompt is documenting a different schema"
                );
            }
        }

        // Reverse direction: nothing the prompt calls an act may be absent
        // from the wire.
        for retired in RETIRED_ACT_NAMES {
            assert!(
                !prompt.contains(retired),
                "CHAT_ROUTER_BASE_PROMPT still names `{retired}`, which is not on \
                 the wire any more; the model would call it and `act_call()` would \
                 find no act, so the turn would fall back to a plain reply"
            );
        }
    }
}
