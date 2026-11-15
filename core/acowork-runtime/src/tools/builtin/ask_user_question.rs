//! Ask User Question tool — LLM-initiated question with options for the user
//!
//! Per design doc (06-ask-user-question-tool.md):
//! - LLM calls this tool to present a question with pre-defined options
//! - Frontend renders options + "Other" textarea for free-text input
//! - Uses the same WaitingApproval state machine as tool_approval_needed
//! - Gateway dispatches via BridgeEvent::AskQuestion (WS push)
//! - User response flows back through HTTP question endpoint → gRPC push
//!
//! ## "Other" convention
//! The options array does NOT contain an explicit "Other" entry.
//! The frontend always shows an "Other" radio option that reveals a textarea
//! when selected. If the user selects "Other", the `answer` field contains
//! their free-text input rather than an option label.

use acowork_core::tools::traits::{Tool, ToolResult, ToolSpec};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Maximum number of options allowed (prevents LLM from generating huge lists)
const MAX_OPTIONS: usize = 10;

/// Maximum question length in characters
const MAX_QUESTION_LEN: usize = 2000;

/// Maximum option label length
const MAX_OPTION_LABEL_LEN: usize = 100;

/// Maximum free-text answer length
#[allow(dead_code)]
const MAX_ANSWER_LEN: usize = 4000;

/// A single option presented to the user
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionOption {
    /// Short label displayed as a choice (e.g. "函数式", "OOP")
    pub label: String,
    /// Optional longer description shown when the option is focused/hovered
    #[serde(default)]
    pub description: Option<String>,
}

/// Parameters for the ask_user_question tool (parsed from LLM tool call)
///
/// NOTE: The question wait timeout is intentionally NOT a tool parameter.
/// It is a scheduling decision owned by the agent runtime (driven by the
/// user's `approval_timeout_secs` preference), not the LLM. The LLM cannot
/// know how long the user will take to answer — making it a tool input
/// would let the LLM silently override the user's preferred wait time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskQuestionParams {
    /// The question to present to the user
    pub question: String,
    /// Pre-defined options for the user to choose from (1–10)
    pub options: Vec<QuestionOption>,
    /// Short title/header for the question card (optional)
    #[serde(default)]
    pub title: Option<String>,
}

/// Result returned from the user's response (received via gRPC from Gateway)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskQuestionAnswer {
    /// The request_id this answer corresponds to
    pub request_id: String,
    /// The user's selected answer:
    /// - If they chose a pre-defined option: the option's label
    /// - If they typed free text (via "Other"): their free-text input
    pub answer: String,
}

/// Ask User Question tool — LLM asks the user a question with options
///
/// The tool itself is stateless. The actual "wait for user response" logic
/// is handled by the AgentLoop (which sets WaitingApproval status, emits
/// ChunkEvent::AskQuestion, and awaits InboundMessage::QuestionAnswer).
/// This tool struct only validates params and constructs the event payload.
pub struct AskUserQuestionTool;

impl AskUserQuestionTool {
    pub fn new() -> Self {
        Self
    }

    fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "ask_user_question".to_string(),
            description: "Ask the user a question with pre-defined options. \
                Use this when you need the user to make a choice or provide input \
                before continuing. The user can also type a custom answer via 'Other'. \
                Do NOT use this for simple yes/no questions — just proceed with the best choice. \
                Only use when the decision genuinely requires user input."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "The question to ask the user. Be specific and concise."
                    },
                    "options": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "label": {
                                    "type": "string",
                                    "description": "Short label for this option (1-5 words)"
                                },
                                "description": {
                                    "type": "string",
                                    "description": "Optional longer description of what this option means"
                                }
                            },
                            "required": ["label"]
                        },
                        "minItems": 1,
                        "maxItems": 10,
                        "description": "Pre-defined options for the user to choose from. The frontend also shows an 'Other' option for free-text input."
                    },
                    "title": {
                        "type": "string",
                        "description": "Optional short title/header for the question card"
                    }
                },
                "required": ["question", "options"]
            }),
        }
    }

    /// Validate and parse tool params into AskQuestionParams
    pub fn validate_params(params: &Value) -> Result<AskQuestionParams, String> {
        let question = params
            .get("question")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();

        if question.is_empty() {
            return Err("Missing required parameter 'question'".to_string());
        }
        if question.len() > MAX_QUESTION_LEN {
            return Err(format!(
                "Question too long ({} chars, max {})",
                question.len(),
                MAX_QUESTION_LEN
            ));
        }

        let options_val = match params.get("options") {
            Some(v) if v.is_array() => v,
            _ => return Err("Missing required parameter 'options' (must be an array)".to_string()),
        };

        let options_arr = options_val.as_array().unwrap();
        if options_arr.is_empty() {
            return Err("At least 1 option is required".to_string());
        }
        if options_arr.len() > MAX_OPTIONS {
            return Err(format!(
                "Too many options ({}), max {}",
                options_arr.len(),
                MAX_OPTIONS
            ));
        }

        let mut options = Vec::with_capacity(options_arr.len());
        for (i, opt_val) in options_arr.iter().enumerate() {
            let label = opt_val
                .get("label")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();

            if label.is_empty() {
                return Err(format!("Option {} has empty label", i + 1));
            }
            if label.len() > MAX_OPTION_LABEL_LEN {
                return Err(format!(
                    "Option {} label too long ({} chars, max {})",
                    i + 1,
                    label.len(),
                    MAX_OPTION_LABEL_LEN
                ));
            }

            let description = opt_val
                .get("description")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());

            options.push(QuestionOption { label, description });
        }

        // Check for duplicate labels
        let mut seen_labels = std::collections::HashSet::new();
        for opt in &options {
            if !seen_labels.insert(opt.label.to_lowercase()) {
                return Err(format!("Duplicate option label: '{}'", opt.label));
            }
        }

        let title = params
            .get("title")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        Ok(AskQuestionParams {
            question,
            options,
            title,
        })
    }
}

impl Default for AskUserQuestionTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for AskUserQuestionTool {
    fn spec(&self) -> ToolSpec {
        Self::spec_value()
    }

    async fn execute(
        &self,
        params: Value,
        _work_dir: Option<&str>,
    ) -> acowork_core::error::Result<ToolResult> {
        // NOTE: The actual execution (waiting for user response) is handled
        // by the AgentLoop, not here. This tool's execute() is a placeholder
        // that validates params. The real flow:
        //
        // 1. AgentLoop detects tool_call with name "ask_user_question"
        // 2. AgentLoop validates params (same as validate_params)
        // 3. AgentLoop sets status → WaitingApproval
        // 4. AgentLoop emits ChunkEvent::AskQuestion via chunk channel
        // 5. AgentLoop awaits InboundMessage::QuestionAnswer
        // 6. Answer is returned as the ToolResult
        //
        // This design matches the existing tool_approval_needed pattern where
        // the loop intercepts the tool call and handles the async flow.

        match Self::validate_params(&params) {
            Ok(parsed) => {
                // Return the parsed params as content — the AgentLoop will
                // intercept this and handle the question flow
                Ok(ToolResult {
                    content_parts: None,
                    ok: true,
                    content: serde_json::to_string(&parsed).unwrap_or_else(|_| params.to_string()),
                    error: None,
                    token_usage: None,
                })
            }
            Err(e) => Ok(ToolResult {
                content_parts: None,
                ok: false,
                content: String::new(),
                error: Some(e),
                token_usage: None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spec() {
        let spec = AskUserQuestionTool::spec_value();
        assert_eq!(spec.name, "ask_user_question");
        let schema = &spec.input_schema;
        assert!(schema["properties"]["question"].is_object());
        assert!(schema["properties"]["options"].is_object());
        assert!(schema["properties"]["title"].is_object());
    }

    /// `timeout_seconds` is intentionally NOT a tool input — it is a
    /// runtime scheduling concern driven by the user's `approval_timeout_secs`.
    /// Verify the schema does not expose it.
    #[test]
    fn test_spec_does_not_expose_timeout_seconds() {
        let spec = AskUserQuestionTool::spec_value();
        let schema = &spec.input_schema;
        assert!(
            schema["properties"]["timeout_seconds"].is_null(),
            "ask_user_question schema must not expose timeout_seconds to the LLM; \
             it is an agent scheduling concern driven by approval_timeout_secs"
        );
    }

    /// Verify validation tolerates an LLM that still emits `timeout_seconds`
    /// (e.g. from cached prompts or older model snapshots). The field is
    /// silently ignored — it never affects the wait timeout.
    #[test]
    fn test_validate_params_ignores_legacy_timeout_seconds() {
        let params = serde_json::json!({
            "question": "Which?",
            "options": [{ "label": "A" }],
            "timeout_seconds": 30
        });
        let result = AskUserQuestionTool::validate_params(&params).unwrap();
        assert_eq!(result.question, "Which?");
        // AskQuestionParams has no timeout_seconds field — the value is dropped.
    }

    #[test]
    fn test_validate_params_basic() {
        let params = serde_json::json!({
            "question": "Which approach?",
            "options": [
                { "label": "Option A", "description": "First approach" },
                { "label": "Option B", "description": "Second approach" }
            ]
        });
        let result = AskUserQuestionTool::validate_params(&params).unwrap();
        assert_eq!(result.question, "Which approach?");
        assert_eq!(result.options.len(), 2);
        assert_eq!(result.options[0].label, "Option A");
        assert!(result.title.is_none());
    }

    #[test]
    fn test_validate_params_with_title() {
        let params = serde_json::json!({
            "question": "Choose style",
            "options": [{ "label": "Functional" }],
            "title": "Refactoring Style"
        });
        let result = AskUserQuestionTool::validate_params(&params).unwrap();
        assert_eq!(result.title.as_deref(), Some("Refactoring Style"));
    }

    #[test]
    fn test_validate_params_missing_question() {
        let params = serde_json::json!({
            "options": [{ "label": "A" }]
        });
        let result = AskUserQuestionTool::validate_params(&params);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("Missing required parameter 'question'")
        );
    }

    #[test]
    fn test_validate_params_missing_options() {
        let params = serde_json::json!({
            "question": "Which?"
        });
        let result = AskUserQuestionTool::validate_params(&params);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("Missing required parameter 'options'")
        );
    }

    #[test]
    fn test_validate_params_empty_options() {
        let params = serde_json::json!({
            "question": "Which?",
            "options": []
        });
        let result = AskUserQuestionTool::validate_params(&params);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("At least 1 option"));
    }

    #[test]
    fn test_validate_params_duplicate_labels() {
        let params = serde_json::json!({
            "question": "Which?",
            "options": [
                { "label": "A" },
                { "label": "a" }
            ]
        });
        let result = AskUserQuestionTool::validate_params(&params);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Duplicate option label"));
    }

    #[test]
    fn test_validate_params_empty_label() {
        let params = serde_json::json!({
            "question": "Which?",
            "options": [
                { "label": "  " }
            ]
        });
        let result = AskUserQuestionTool::validate_params(&params);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("empty label"));
    }

    #[test]
    fn test_validate_params_option_without_description() {
        let params = serde_json::json!({
            "question": "Pick one",
            "options": [{ "label": "Go" }]
        });
        let result = AskUserQuestionTool::validate_params(&params).unwrap();
        assert_eq!(result.options[0].description, None);
    }

    #[tokio::test]
    async fn test_execute_valid_params() {
        let tool = AskUserQuestionTool::new();
        let result = tool
            .execute(
                serde_json::json!({
                    "question": "Style?",
                    "options": [{ "label": "Rust" }, { "label": "Go" }]
                }),
                None,
            )
            .await
            .unwrap();
        assert!(result.ok);
    }

    #[tokio::test]
    async fn test_execute_invalid_params() {
        let tool = AskUserQuestionTool::new();
        let result = tool
            .execute(serde_json::json!({ "question": "" }), None)
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(
            result
                .error
                .unwrap()
                .contains("Missing required parameter 'question'")
        );
    }
}
