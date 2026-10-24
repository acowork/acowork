//! MCP tool manifest for the memory service (ADR-100).
//!
//! The two Agent-facing tools are the service-side port of the Runtime
//! builtins `memory_store` / `memory_recall`. Parameter names, defaults and
//! validation messages are kept identical so an Agent that used to see the
//! builtins sees the same contract once it comes from MCP; only statements
//! about Runtime-internal context are corrected (see the notes below).
//!
//! Deviations from the builtin tools, both forced by the process boundary:
//! - the vector source is the tool's own `query` text — there is no
//!   `ToolContext.current_user_message` inside the service any more;
//! - `exclude_session_id` becomes an explicit parameter, because the loop
//!   used to inject the session id at dispatch time.

/// MCP tool manifest (compile-time constant, `tools/list` body).
pub const MEMORY_TOOL_MANIFEST: &str = r#"{
  "schema_version": "2025-06-18",
  "tools": [
    {
      "name": "memory_store",
      "description": "Store a memory in long-term memory for later recall. After a task is completed, when you are about to output a task completion summary, before outputting the summary, proactively call the `memory_store` tool to persist the task summary / key decisions / reusable experience into long-term memory. Category can be: - 'fact': objective truth about the user or world. - 'preference': user taste / habit. - 'relation': relationship between entities. - 'procedure': behavioural pattern — 'when X, do Y'. Autobiographical feedback about the agent itself should be written the same way as any other observation (it will be recognised offline by the distiller's server-side LLM). Describe what to remember in 'content' (natural language). Estimate your confidence (0.0-1.0). Optionally provide keywords. Also provide 'normalized': a one-sentence, de-contextualised restatement of the durable claim — this is what offline consolidation merges on, so a memory without it can only ever be carried forward verbatim.",
      "inputSchema": {
        "type": "object",
        "properties": {
          "content": {
            "type": "string",
            "description": "Natural language description of what to remember (e.g. 'User lives in Beijing', 'User prefers dark mode over light mode', 'When user asks for summary, reply in 3 sentences max', 'I tend to give conclusions first')"
          },
          "category": {
            "type": "string",
            "enum": ["fact", "preference", "relation", "procedure"],
            "description": "Knowledge subtype tag consumed by the offline EpisodicDistiller. Use 'fact' for objective truths, 'preference' for user taste, 'relation' for entity relationships, 'procedure' for behavioural patterns."
          },
          "confidence": {
            "type": "number",
            "description": "Your confidence in this observation (0.0-1.0), reflecting how certain you actually are. Anchor on evidence, not on a target value: base it on whether the statement is direct, explicit, recent, and from the user personally (higher), versus inferred, stale, or speculative (lower). Most routine observations are moderately certain — score them accordingly. Reserve very high scores for facts you would bet on; use very low scores for uncertain or contradicting signals. Do not inflate scores to make a memory seem more certain than it is."
          },
          "privacy": {
            "type": "string",
            "enum": ["public", "personal", "sensitive"],
            "description": "Privacy level: 'public' (shareable in agent packages), 'personal' (default, stripped on share), 'sensitive' (stripped on share)."
          },
          "importance": {
            "type": "number",
            "description": "How critical is this memory to long-term value (0.0-1.0)? Higher importance resists forgetting. Distinguish core identity facts (near 1.0) from transient preferences (~0.3-0.5) from trivia (~0.1)."
          },
          "keywords": {
            "type": "array",
            "items": { "type": "string" },
            "description": "Optional keywords to help retrieval. Provide short lowercase tokens (≤30 chars), avoid duplicates and common stopwords (e.g. ['beijing', 'location', 'home'])"
          },
          "normalized": {
            "type": "string",
            "description": "One sentence restating the durable claim in this memory, written so it still reads correctly years from now and outside the conversation that produced it. Third person, no deictic references (this/that/it/today/here), no task framing, no justification. Where 'content' may record what happened around the observation, 'normalized' states only what is now true. Example — content: 'User asked me to summarize and said 3 sentences was enough, so I kept it short' -> normalized: 'User wants summaries capped at three sentences.' Omit only when there is no durable claim to restate."
          }
        },
        "required": ["content", "category"]
      }
    },
    {
      "name": "memory_recall",
      "description": "Search long-term memory for relevant facts, preferences, or context. Returns scored results ranked by relevance. Supports keyword search, time-only query (since/until), or both. The 'query' text drives both lexical (BM25) and semantic matching, so prefer a phrase that reads like the memory you expect rather than a loose keyword bag. NOTE: use this tool for deeper or targeted recall (different keywords, or time-filtered) instead of repeating the same query, to avoid duplicating context already present.",
      "inputSchema": {
        "type": "object",
        "properties": {
          "query": {
            "type": "string",
            "description": "Keywords or phrase to match (optional if since/until provided)"
          },
          "limit": {
            "type": "integer",
            "description": "Max results to return (default: 5)"
          },
          "since": {
            "type": "string",
            "description": "Filter memories created at or after this time (RFC 3339, e.g. 2025-03-01T00:00:00Z)"
          },
          "until": {
            "type": "string",
            "description": "Filter memories created at or before this time (RFC 3339)"
          },
          "exclude_session_id": {
            "type": "string",
            "description": "Skip memories written by this session id — pass the current session so recall does not re-read material that is already in the conversation."
          }
        }
      }
    }
  ]
}"#;

/// Parse the manifest into the `tools/list` result array.
pub fn manifest_tools() -> Vec<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(MEMORY_TOOL_MANIFEST)
        .expect("MEMORY_TOOL_MANIFEST must be valid JSON");
    v["tools"].as_array().cloned().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parses_to_expected_tools() {
        let tools = manifest_tools();
        assert_eq!(tools.len(), 2, "the memory service exposes store + recall");
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["memory_store", "memory_recall"]);
        for t in &tools {
            assert!(t["description"].as_str().unwrap().len() > 40);
            assert_eq!(t["inputSchema"]["type"], "object");
        }
        // The write tool's two mandatory parameters must stay mandatory —
        // dropping them would let an Agent store an unlabelled memory.
        assert_eq!(
            tools[0]["inputSchema"]["required"],
            serde_json::json!(["content", "category"])
        );
    }
}
