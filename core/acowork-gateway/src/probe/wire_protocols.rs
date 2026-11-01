//! ADR-103 D14 — probe wire shapes for the Chat-Completions and Anthropic
//! Messages protocols.
//!
//! The Responses probe ([`super::wire`]) answered one question: "does this
//! endpoint speak the OpenAI Responses API?". That was enough while the only
//! selectable override was `Responses`.
//!
//! It is not enough any more. ADR-103 D14 generalises the gate to **every**
//! model-level protocol selection, and a gate can only ask about a protocol
//! the probe can actually speak. This module supplies the two other shapes the
//! platform supports natively:
//!
//! | protocol | route | request shape | tool-call shape |
//! |---|---|---|---|
//! | Chat Completions | `POST {base}/chat/completions` | `messages[]` | `choices[].message.tool_calls[]` |
//! | Anthropic Messages | `POST {base}/messages` | `messages[]` + top-level `system` | `content[]` block `tool_use` |
//!
//! The tier contract is the same as the Responses probe (D10.2), so the
//! verdict semantics — and therefore the D1 gate — stay identical:
//!
//! - **L1** — the route exists and answers with the protocol's own envelope.
//! - **L2** — the envelope carries the usage counters the Runtime maps.
//! - **L3** — the model completes a tool round-trip (call → result → replay).
//!
//! Why L3 is the minimum for a positive verdict is unchanged from D10.2: the
//! provider exists to run the agent's tool loop, so an endpoint that answers
//! text but cannot complete a tool call is a green light that fails in
//! production.
//!
//! The bodies here are deliberately minimal and stateless (`store: false` /
//! no persistence) for the same reason as the Responses probe: a probe MUST
//! NOT leave state behind on the user's account (D10.4).

use serde_json::{Value, json};

use super::wire::{PROBE_TOOL_NAME, ProbeFunctionCall, ProbeUsage, truncate};

/// Token budget for the L1/L2 baseline call.
///
/// The Responses probe uses 64 because the OpenAI Responses API rejects
/// `max_output_tokens` below 16. Chat Completions has no such floor, and
/// Anthropic *requires* `max_tokens` (it is a required field there, and its
/// own floor is 1). 64 is comfortably above every known floor and still costs
/// a fraction of a cent.
const BASELINE_MAX_TOKENS: u32 = 64;

/// Token budget for the L3 call — it has to fit a tool call.
const ROUND_TRIP_MAX_TOKENS: u32 = 128;

/// The synthetic tool's parameter schema, shared by all three protocols.
fn probe_tool_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "value": { "type": "integer", "description": "Any integer." },
        },
        "required": ["value"],
        "additionalProperties": false,
    })
}

// ── Chat Completions ───────────────────────────────────────────────────

/// Chat-Completions L1/L2 baseline request.
pub fn chat_completions_baseline_request(model: &str) -> Value {
    json!({
        "model": model,
        "messages": [{ "role": "user", "content": "ping" }],
        "max_tokens": BASELINE_MAX_TOKENS,
        "stream": false,
    })
}

/// Chat-Completions L3 request: ask for one call to the synthetic tool.
pub fn chat_completions_round_trip_request(model: &str) -> Value {
    json!({
        "model": model,
        "messages": [{
            "role": "user",
            "content": format!("Call the {PROBE_TOOL_NAME} tool with {{\"value\": 1}}."),
        }],
        "tools": [{
            "type": "function",
            "function": {
                "name": PROBE_TOOL_NAME,
                "description": "Echo the given value back. Always call this tool when asked.",
                "parameters": probe_tool_parameters(),
            },
        }],
        "tool_choice": "auto",
        "max_tokens": ROUND_TRIP_MAX_TOKENS,
        "stream": false,
    })
}

/// Chat-Completions L3 replay: hand the tool result back.
///
/// Chat Completions replays a tool call as a `tool` role message carrying the
/// `tool_call_id` — the same contract the Runtime's own loop uses.
pub fn chat_completions_replay_request(
    model: &str,
    call: &ProbeFunctionCall,
) -> Value {
    json!({
        "model": model,
        "messages": [
            {
                "role": "user",
                "content": format!("Call the {PROBE_TOOL_NAME} tool with {{\"value\": 1}}."),
            },
            {
                "role": "assistant",
                "content": Value::Null,
                "tool_calls": [{
                    "id": call.call_id,
                    "type": "function",
                    "function": {
                        "name": call.name,
                        "arguments": call.arguments,
                    },
                }],
            },
            {
                "role": "tool",
                "tool_call_id": call.call_id,
                "content": "ok",
            },
        ],
        "tools": [{
            "type": "function",
            "function": {
                "name": PROBE_TOOL_NAME,
                "description": "Echo the given value back. Always call this tool when asked.",
                "parameters": probe_tool_parameters(),
            },
        }],
        "max_tokens": BASELINE_MAX_TOKENS,
        "stream": false,
    })
}

/// L1 — the Chat-Completions envelope must carry a `choices` array.
pub fn assert_chat_completions_envelope(body: &Value) -> Result<(), String> {
    match body.get("choices").and_then(Value::as_array) {
        Some(choices) if !choices.is_empty() => Ok(()),
        Some(_) => Err("response has an empty `choices` array".to_string()),
        None => Err(format!(
            "response body has no `choices` array: {}",
            truncate(&body.to_string(), 200)
        )),
    }
}

/// L2 — Chat-Completions usage counters.
///
/// `prompt_tokens` / `completion_tokens` are the Chat-Completions names; the
/// Responses probe deliberately refuses to accept them (it wants
/// `input_tokens` / `output_tokens`), because tolerating the aliases would
/// hide exactly the shape drift the probe exists to catch. The same rule
/// applies here in the other direction.
pub fn assert_chat_completions_usage(body: &Value) -> Result<ProbeUsage, String> {
    let usage = body
        .get("usage")
        .ok_or_else(|| "response has no `usage` object".to_string())?;
    let input_tokens = usage
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .ok_or_else(|| "usage.prompt_tokens missing or not a number".to_string())?;
    let output_tokens = usage
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .ok_or_else(|| "usage.completion_tokens missing or not a number".to_string())?;
    Ok(ProbeUsage {
        input_tokens,
        output_tokens,
    })
}

/// L3 — find the tool call in `choices[0].message.tool_calls[]`.
///
/// Requires a non-empty `id`, not merely a name: the Runtime keys its tool
/// loop on the id, so an endpoint that emits an idless call fails this tier
/// on purpose.
pub fn find_chat_completions_function_call(body: &Value) -> Result<ProbeFunctionCall, String> {
    let choices = body
        .get("choices")
        .and_then(Value::as_array)
        .ok_or_else(|| "response has no `choices` array".to_string())?;
    let message = choices
        .first()
        .and_then(|c| c.get("message"))
        .ok_or_else(|| "response has no `choices[0].message`".to_string())?;
    let tool_calls = message.get("tool_calls").and_then(Value::as_array).ok_or(
        "message has no `tool_calls` array — the model did not call the no-op \
         tool, so the tool round-trip cannot be verified",
    )?;

    // The first tool call is the one the probe asked for: a provider that
    // answers with several calls has still answered the question.
    let call = tool_calls.first().ok_or_else(|| {
        format!(
            "no tool call in choices[0].message.tool_calls (got {} entries) — the model \
             did not call the no-op tool, so the tool round-trip cannot be verified",
            tool_calls.len()
        )
    })?;
    let call_id = call
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "tool call has no non-empty `id`".to_string())?;
    let function = call
        .get("function")
        .ok_or_else(|| "tool call has no `function` object".to_string())?;
    let name = function
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "tool call has no non-empty `function.name`".to_string())?;
    let arguments = match function.get("arguments") {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    };
    let probe_call = ProbeFunctionCall {
        call_id: call_id.to_string(),
        name: name.to_string(),
        arguments,
    };
    probe_call.arguments_json()?;
    Ok(probe_call)
}

// ── Anthropic Messages ────────────────────────────────────────────────

/// Anthropic L1/L2 baseline request.
///
/// Anthropic requires `max_tokens` on every request, and rejects an empty
/// `messages` array — hence the one user message.
pub fn anthropic_baseline_request(model: &str) -> Value {
    json!({
        "model": model,
        "messages": [{ "role": "user", "content": "ping" }],
        "max_tokens": BASELINE_MAX_TOKENS,
        "stream": false,
    })
}

/// Anthropic L3 request: ask for one call to the synthetic tool.
pub fn anthropic_round_trip_request(model: &str) -> Value {
    json!({
        "model": model,
        "messages": [{
            "role": "user",
            "content": format!("Call the {PROBE_TOOL_NAME} tool with {{\"value\": 1}}."),
        }],
        "tools": [{
            "name": PROBE_TOOL_NAME,
            "description": "Echo the given value back. Always call this tool when asked.",
            "input_schema": probe_tool_parameters(),
        }],
        "max_tokens": ROUND_TRIP_MAX_TOKENS,
        "stream": false,
    })
}

/// Anthropic L3 replay: hand the tool result back.
///
/// Anthropic carries the tool result as a `user` message whose content is a
/// `tool_result` block — the same contract the Runtime's own loop uses.
pub fn anthropic_replay_request(model: &str, call: &ProbeFunctionCall) -> Value {
    json!({
        "model": model,
        "messages": [
            {
                "role": "user",
                "content": format!("Call the {PROBE_TOOL_NAME} tool with {{\"value\": 1}}."),
            },
            {
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": call.call_id,
                    "name": call.name,
                    "input": call.arguments_json().unwrap_or(Value::Null),
                }],
            },
            {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": call.call_id,
                    "content": "ok",
                }],
            },
        ],
        "tools": [{
            "name": PROBE_TOOL_NAME,
            "description": "Echo the given value back. Always call this tool when asked.",
            "input_schema": probe_tool_parameters(),
        }],
        "max_tokens": BASELINE_MAX_TOKENS,
        "stream": false,
    })
}

/// L1 — the Anthropic envelope must carry a `content` array.
pub fn assert_anthropic_envelope(body: &Value) -> Result<(), String> {
    match body.get("content").and_then(Value::as_array) {
        Some(content) if !content.is_empty() => Ok(()),
        Some(_) => Err("response has an empty `content` array".to_string()),
        None => Err(format!(
            "response body has no `content` array: {}",
            truncate(&body.to_string(), 200)
        )),
    }
}

/// L2 — Anthropic usage counters.
///
/// Anthropic names them `input_tokens` / `output_tokens` — the same names the
/// Responses API uses, which is why both probes read the same pair.
pub fn assert_anthropic_usage(body: &Value) -> Result<ProbeUsage, String> {
    let usage = body
        .get("usage")
        .ok_or_else(|| "response has no `usage` object".to_string())?;
    let input_tokens = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .ok_or_else(|| "usage.input_tokens missing or not a number".to_string())?;
    let output_tokens = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .ok_or_else(|| "usage.output_tokens missing or not a number".to_string())?;
    Ok(ProbeUsage {
        input_tokens,
        output_tokens,
    })
}

/// L3 — find the `tool_use` block in `content[]`.
///
/// Anthropic's tool call carries an `id` (not `call_id`) and its arguments as
/// a structured `input` object rather than a JSON string.
pub fn find_anthropic_function_call(body: &Value) -> Result<ProbeFunctionCall, String> {
    let content = body
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| "response has no `content` array".to_string())?;

    for block in content {
        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
            continue;
        }
        let call_id = block
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "tool_use block has no non-empty `id`".to_string())?;
        let name = block
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "tool_use block has no non-empty `name`".to_string())?;
        // Anthropic's `input` is already structured; re-serialize it so the
        // shared `arguments_json` contract holds for every protocol.
        let arguments = match block.get("input") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => String::new(),
        };
        let probe_call = ProbeFunctionCall {
            call_id: call_id.to_string(),
            name: name.to_string(),
            arguments,
        };
        probe_call.arguments_json()?;
        return Ok(probe_call);
    }

    let kinds: Vec<&str> = content
        .iter()
        .filter_map(|b| b.get("type").and_then(Value::as_str))
        .collect();
    Err(format!(
        "no tool_use block in content[] (got {:?}) — the model did not call the \
         no-op tool, so the tool round-trip cannot be verified",
        kinds
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Chat Completions ───────────────────────────────────────────────

    #[test]
    fn chat_completions_baseline_is_minimal() {
        let req = chat_completions_baseline_request("gpt-4o");
        assert_eq!(req["model"], "gpt-4o");
        assert_eq!(req["stream"], false);
        assert_eq!(req["messages"].as_array().unwrap().len(), 1);
        assert!(req["tools"].is_null(), "baseline must not declare tools");
    }

    #[test]
    fn chat_completions_round_trip_declares_a_flat_tool() {
        let req = chat_completions_round_trip_request("gpt-4o");
        let tool = &req["tools"][0];
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["function"]["name"], PROBE_TOOL_NAME);
        assert!(tool["function"]["parameters"]["properties"]["value"].is_object());
    }

    #[test]
    fn chat_completions_replay_echoes_the_tool_call_id() {
        let call = ProbeFunctionCall {
            call_id: "call_1".into(),
            name: PROBE_TOOL_NAME.into(),
            arguments: "{\"value\":1}".into(),
        };
        let req = chat_completions_replay_request("gpt-4o", &call);
        let tool_msg = req["messages"].as_array().unwrap().last().unwrap();
        assert_eq!(tool_msg["role"], "tool");
        assert_eq!(tool_msg["tool_call_id"], "call_1");
    }

    #[test]
    fn chat_completions_envelope_requires_choices() {
        assert!(assert_chat_completions_envelope(&json!({"choices": [{"x": 1}]})).is_ok());
        assert!(assert_chat_completions_envelope(&json!({"choices": []})).is_err());
        assert!(assert_chat_completions_envelope(&json!({"object": "response"})).is_err());
    }

    #[test]
    fn chat_completions_usage_uses_its_own_counter_names() {
        let body = json!({"usage": {"prompt_tokens": 7, "completion_tokens": 1}});
        let usage = assert_chat_completions_usage(&body).unwrap();
        assert_eq!(usage.input_tokens, 7);
        assert_eq!(usage.output_tokens, 1);
        // The Responses names are NOT accepted here — accepting them would
        // hide the shape drift the probe exists to catch.
        assert!(
            assert_chat_completions_usage(&json!({"usage": {"input_tokens": 7, "output_tokens": 1}}))
                .is_err()
        );
    }

    #[test]
    fn chat_completions_finds_the_tool_call() {
        let body = json!({
            "choices": [{
                "message": {
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": PROBE_TOOL_NAME, "arguments": "{\"value\":1}"},
                    }],
                },
            }],
        });
        let call = find_chat_completions_function_call(&body).unwrap();
        assert_eq!(call.call_id, "call_1");
        assert_eq!(call.name, PROBE_TOOL_NAME);
        assert_eq!(call.arguments_json().unwrap()["value"], 1);
    }

    #[test]
    fn chat_completions_without_a_tool_call_is_an_error() {
        let body = json!({"choices": [{"message": {"content": "hello"}}]});
        let err = find_chat_completions_function_call(&body).unwrap_err();
        assert!(err.contains("tool_calls"), "{err}");
    }

    // ── Anthropic ──────────────────────────────────────────────────────

    #[test]
    fn anthropic_baseline_carries_the_required_max_tokens() {
        let req = anthropic_baseline_request("claude-3-5");
        assert_eq!(req["model"], "claude-3-5");
        // Anthropic rejects a request without max_tokens.
        assert!(req["max_tokens"].is_number());
        assert_eq!(req["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn anthropic_round_trip_declares_a_flat_tool() {
        let req = anthropic_round_trip_request("claude-3-5");
        let tool = &req["tools"][0];
        assert_eq!(tool["name"], PROBE_TOOL_NAME);
        assert!(tool["input_schema"]["properties"]["value"].is_object());
    }

    #[test]
    fn anthropic_replay_echoes_the_tool_use_id() {
        let call = ProbeFunctionCall {
            call_id: "toolu_1".into(),
            name: PROBE_TOOL_NAME.into(),
            arguments: "{\"value\":1}".into(),
        };
        let req = anthropic_replay_request("claude-3-5", &call);
        let last = req["messages"].as_array().unwrap().last().unwrap();
        assert_eq!(last["role"], "user");
        assert_eq!(last["content"][0]["type"], "tool_result");
        assert_eq!(last["content"][0]["tool_use_id"], "toolu_1");
    }

    #[test]
    fn anthropic_envelope_requires_content() {
        assert!(assert_anthropic_envelope(&json!({"content": [{"type": "text"}]})).is_ok());
        assert!(assert_anthropic_envelope(&json!({"content": []})).is_err());
        assert!(assert_anthropic_envelope(&json!({"choices": []})).is_err());
    }

    #[test]
    fn anthropic_usage_reads_the_responses_style_names() {
        let body = json!({"usage": {"input_tokens": 7, "output_tokens": 1}});
        let usage = assert_anthropic_usage(&body).unwrap();
        assert_eq!(usage.input_tokens, 7);
        assert_eq!(usage.output_tokens, 1);
    }

    #[test]
    fn anthropic_finds_the_tool_use_block() {
        let body = json!({
            "content": [{
                "type": "tool_use",
                "id": "toolu_1",
                "name": PROBE_TOOL_NAME,
                "input": {"value": 1},
            }],
        });
        let call = find_anthropic_function_call(&body).unwrap();
        assert_eq!(call.call_id, "toolu_1");
        assert_eq!(call.arguments_json().unwrap()["value"], 1);
    }

    #[test]
    fn anthropic_without_a_tool_use_block_is_an_error() {
        let body = json!({"content": [{"type": "text", "text": "hello"}]});
        let err = find_anthropic_function_call(&body).unwrap_err();
        assert!(err.contains("tool_use"), "{err}");
    }
}
