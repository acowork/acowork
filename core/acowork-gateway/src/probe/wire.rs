//! ADR-103 D10.2 — Responses-protocol wire shapes and assertions.
//!
//! Everything in this module is **pure**: request builders take plain data,
//! assertions take an already-parsed response body. The HTTP transport lives
//! in [`super::runner`], so the protocol contract (tiers L1/L2/L3) can be
//! tested without a socket, and the probe cannot accidentally acquire a
//! dependency on Gateway state.
//!
//! Tier definitions (D10.2):
//!
//! | tier | request | assertion |
//! |------|---------|-----------|
//! | L1 (existence) | minimal `POST {base}/responses` | 2xx + `object == "response"` |
//! | L2 (shape) | reuses the L1 response | `status`, `usage.input_tokens`, `usage.output_tokens` present |
//! | L3 (round-trip) | one no-op tool + `tool_choice` | `function_call` item produced, `call_id` replayable, `arguments` is valid JSON |

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Name of the synthetic no-op tool the L3 round-trip asks the model to call.
pub const PROBE_TOOL_NAME: &str = "acowork_probe_echo";

/// Token budget for the L1/L2 baseline call.
///
/// One output token is enough to prove the endpoint answers in the Responses
/// shape and to make `usage.output_tokens` a real number, and it keeps the
/// probe's cost negligible (D10.4).
const BASELINE_MAX_OUTPUT_TOKENS: u32 = 1;

/// Token budget for the L3 call — it has to fit a `function_call` item.
const ROUND_TRIP_MAX_OUTPUT_TOKENS: u32 = 128;

/// Truncation limit for upstream error bodies echoed into the verdict reason.
const MAX_ERROR_BODY_CHARS: usize = 400;

/// Usage counters read out of the L2 baseline response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// The `function_call` item found by the L3 round-trip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeFunctionCall {
    pub call_id: String,
    pub name: String,
    pub arguments: String,
}

impl ProbeFunctionCall {
    /// Parsed `arguments`. The probe only needs "is this valid JSON", so the
    /// caller decides what to do with the value.
    pub fn arguments_json(&self) -> Result<Value, String> {
        serde_json::from_str(&self.arguments).map_err(|e| {
            format!(
                "function_call.arguments is not valid JSON ({}): {}",
                e,
                truncate(&self.arguments, 200)
            )
        })
    }
}

/// L1/L2 baseline request: smallest possible Responses call.
///
/// `store: false` keeps the probe out of provider-side conversation storage —
/// a probe MUST NOT leave state behind on the user's account (D10.4).
pub fn baseline_request(model: &str) -> Value {
    json!({
        "model": model,
        "input": "ping",
        "max_output_tokens": BASELINE_MAX_OUTPUT_TOKENS,
        "stream": false,
        "store": false,
    })
}

/// L3 request: ask for one call to the synthetic no-op tool.
pub fn round_trip_request(model: &str) -> Value {
    json!({
        "model": model,
        "instructions": "You are a protocol probe. Call the provided tool exactly \
                         once with the requested arguments, then stop.",
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{
                "type": "input_text",
                "text": format!(
                    "Call the {PROBE_TOOL_NAME} tool with {{\"value\": 1}}."
                ),
            }],
        }],
        "tools": [{
            "type": "function",
            "name": PROBE_TOOL_NAME,
            "description": "Echo the given value back. Always call this tool when asked.",
            "strict": false,
            "parameters": {
                "type": "object",
                "properties": {
                    "value": { "type": "integer", "description": "Any integer." },
                },
                "required": ["value"],
                "additionalProperties": false,
            },
        }],
        "tool_choice": "auto",
        "max_output_tokens": ROUND_TRIP_MAX_OUTPUT_TOKENS,
        "stream": false,
        "store": false,
    })
}

/// L3 replay request: hand the produced `function_call` back with its output.
///
/// This is the "call_id 可回环" half of the tier — a provider that emits a
/// `function_call` but rejects the matching `function_call_output` cannot run
/// a tool loop, and therefore cannot serve the Runtime (D5).
pub fn round_trip_replay_request(model: &str, call: &ProbeFunctionCall) -> Value {
    json!({
        "model": model,
        "input": [
            {
                "type": "function_call",
                "call_id": call.call_id,
                "name": call.name,
                "arguments": call.arguments,
            },
            {
                "type": "function_call_output",
                "call_id": call.call_id,
                "output": "ok",
            },
        ],
        "max_output_tokens": BASELINE_MAX_OUTPUT_TOKENS,
        "stream": false,
        "store": false,
    })
}

/// L1 — the response envelope must be a Responses object.
pub fn assert_envelope(body: &Value) -> Result<(), String> {
    match body.get("object").and_then(Value::as_str) {
        Some("response") => Ok(()),
        Some(other) => Err(format!(
            "expected object=\"response\", got object={:?}",
            other
        )),
        None => Err(format!(
            "response body has no `object` field: {}",
            truncate(&body.to_string(), 200)
        )),
    }
}

/// L2 — `status` plus both usage counters must be present and numeric.
///
/// Deliberately strict about the Responses names (`input_tokens` /
/// `output_tokens`): the Runtime maps usage from these, and tolerating the
/// Chat-Completions aliases here would hide exactly the shape drift the probe
/// exists to catch.
pub fn assert_usage(body: &Value) -> Result<ProbeUsage, String> {
    let status = body.get("status").and_then(Value::as_str).ok_or_else(|| {
        format!(
            "response has no `status` field (D5 finish_reason mapping needs it): {}",
            truncate(&body.to_string(), 200)
        )
    })?;
    let usage = body.get("usage").ok_or_else(|| {
        format!(
            "response (status={}) has no `usage` object",
            status
        )
    })?;
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

/// L3 — find the `function_call` item in `output[]` and validate it.
///
/// Requires a non-empty `call_id`, not merely `id`: the Runtime keys its tool
/// loop on `call_id` (D5), so a provider that only emits `id` fails this tier
/// on purpose.
pub fn find_function_call(body: &Value) -> Result<ProbeFunctionCall, String> {
    let output = body
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            format!(
                "response has no `output` array: {}",
                truncate(&body.to_string(), 200)
            )
        })?;

    for item in output {
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            continue;
        }
        let call_id = item
            .get("call_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "function_call item has no non-empty `call_id`".to_string())?;
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "function_call item has no non-empty `name`".to_string())?;
        let call = ProbeFunctionCall {
            call_id: call_id.to_string(),
            name: name.to_string(),
            arguments: match item.get("arguments") {
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => String::new(),
            },
        };
        call.arguments_json()?;
        return Ok(call);
    }

    let kinds: Vec<&str> = output
        .iter()
        .filter_map(|i| i.get("type").and_then(Value::as_str))
        .collect();
    Err(format!(
        "no function_call item in output[] (got {:?}) — the model did not call the \
         no-op tool, so the tool round-trip cannot be verified",
        kinds
    ))
}

/// Truncate a string for an error message / verdict reason.
pub fn truncate(s: &str, limit: usize) -> String {
    if s.chars().count() <= limit {
        return s.to_string();
    }
    let cut: String = s.chars().take(limit).collect();
    format!("{cut}…")
}

/// Truncate an upstream error body for the verdict reason.
pub fn error_reason(body: &str) -> String {
    truncate(body.trim(), MAX_ERROR_BODY_CHARS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_request_is_minimal_and_stateless() {
        let req = baseline_request("gpt-5");
        assert_eq!(req["model"], "gpt-5");
        assert_eq!(req["input"], "ping");
        assert_eq!(req["max_output_tokens"], 1);
        assert_eq!(req["stream"], false);
        assert_eq!(req["store"], false);
        // A probe must not leave a stored response behind (D10.4).
        assert!(req.get("previous_response_id").is_none());
        assert!(req.get("conversation").is_none());
        assert!(req.get("background").is_none());
    }

    #[test]
    fn round_trip_request_declares_flat_tool_and_auto_choice() {
        let req = round_trip_request("gpt-5");
        let tool = &req["tools"][0];
        // Responses tools are flat — no nested `function` wrapper (D5).
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["name"], PROBE_TOOL_NAME);
        assert!(tool.get("function").is_none());
        assert_eq!(tool["parameters"]["type"], "object");
        assert_eq!(req["tool_choice"], "auto");
        assert!(req["max_output_tokens"].as_u64().unwrap() >= 64);
    }

    #[test]
    fn replay_request_echoes_call_id() {
        let call = ProbeFunctionCall {
            call_id: "call_1".into(),
            name: PROBE_TOOL_NAME.into(),
            arguments: r#"{"value":1}"#.into(),
        };
        let req = round_trip_replay_request("gpt-5", &call);
        assert_eq!(req["input"][0]["type"], "function_call");
        assert_eq!(req["input"][0]["call_id"], "call_1");
        assert_eq!(req["input"][1]["type"], "function_call_output");
        assert_eq!(req["input"][1]["call_id"], "call_1");
    }

    #[test]
    fn envelope_accepts_response_object_only() {
        assert!(assert_envelope(&json!({"object": "response"})).is_ok());
        let err = assert_envelope(&json!({"object": "chat.completion"})).unwrap_err();
        assert!(err.contains("chat.completion"), "{err}");
        let err = assert_envelope(&json!({})).unwrap_err();
        assert!(err.contains("no `object` field"), "{err}");
    }

    #[test]
    fn usage_requires_status_and_both_counters() {
        let ok = assert_usage(&json!({
            "status": "completed",
            "usage": {"input_tokens": 8, "output_tokens": 1},
        }))
        .unwrap();
        assert_eq!(
            ok,
            ProbeUsage {
                input_tokens: 8,
                output_tokens: 1
            }
        );

        let err = assert_usage(&json!({"usage": {"input_tokens": 1, "output_tokens": 1}}))
            .unwrap_err();
        assert!(err.contains("`status`"), "{err}");

        let err = assert_usage(&json!({"status": "completed"})).unwrap_err();
        assert!(err.contains("no `usage`"), "{err}");

        // Chat-Completions aliases must NOT satisfy L2 — that is the drift
        // this tier exists to catch.
        let err = assert_usage(&json!({
            "status": "completed",
            "usage": {"prompt_tokens": 8, "completion_tokens": 1},
        }))
        .unwrap_err();
        assert!(err.contains("input_tokens"), "{err}");
    }

    #[test]
    fn find_function_call_requires_call_id_and_json_arguments() {
        let call = find_function_call(&json!({
            "output": [
                {"type": "reasoning", "summary": []},
                {
                    "type": "function_call",
                    "call_id": "call_abc",
                    "name": PROBE_TOOL_NAME,
                    "arguments": "{\"value\": 1}",
                },
            ],
        }))
        .unwrap();
        assert_eq!(call.call_id, "call_abc");
        assert_eq!(call.arguments_json().unwrap()["value"], 1);

        // `id` alone is not enough: the Runtime's tool loop keys on `call_id`.
        let err = find_function_call(&json!({
            "output": [{"type": "function_call", "id": "fc_1", "name": "x", "arguments": "{}"}],
        }))
        .unwrap_err();
        assert!(err.contains("call_id"), "{err}");

        // Non-JSON arguments break the tool loop downstream.
        let err = find_function_call(&json!({
            "output": [{
                "type": "function_call",
                "call_id": "c",
                "name": "x",
                "arguments": "{value:1}",
            }],
        }))
        .unwrap_err();
        assert!(err.contains("not valid JSON"), "{err}");

        // No tool call at all — the tier's headline failure mode.
        let err = find_function_call(&json!({
            "output": [{"type": "message", "role": "assistant", "content": []}],
        }))
        .unwrap_err();
        assert!(err.contains("no function_call item"), "{err}");
        assert!(err.contains("message"), "{err}");
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("abcdef", 3), "abc…");
        assert_eq!(truncate("中文测试", 2), "中文…");
        assert_eq!(truncate("ok", 10), "ok");
    }
}