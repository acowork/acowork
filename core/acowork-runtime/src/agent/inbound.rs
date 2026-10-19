//! Inbound message queue for external message injection
//!
//! Allows external sources (Gateway, cross-agent intents, system notifications)
//! to inject messages into the agent loop without blocking the main execution.

use serde::{Deserialize, Serialize};

use crate::agent::session::session_manager::RuntimeConfigOverrides;

/// Maximum payload size in bytes for inbound message text content.
/// Messages exceeding this limit will be truncated.
const MAX_INBOUND_PAYLOAD_SIZE: usize = 4096;

/// Truncate a string to at most `max_bytes` bytes, ensuring we don't split
/// a multi-byte UTF-8 character.
fn truncate_to_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    // Find the last valid char boundary within max_bytes
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Truncate a serde_json::Value's string representation to `max_bytes` bytes,
/// ensuring we don't split a multi-byte UTF-8 character.
fn truncate_json_to_bytes(val: &serde_json::Value, max_bytes: usize) -> serde_json::Value {
    let s = val.to_string();
    if s.len() <= max_bytes {
        return val.clone();
    }
    let truncated = truncate_to_bytes(&s, max_bytes);
    serde_json::Value::String(truncated)
}

/// User-initiated operations that take effect immediately via
/// the `send_inbound()` fast channel, bypassing SessionTask's
/// main message loop. This guarantees delivery even while the
/// agent loop is mid-execution (streaming or running tools).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UserOp {
    /// Stop the current agent loop iteration.
    StopLoop { reason: String },
    /// Continue execution after iteration limit was reached.
    /// Resets the iteration counter and resumes the agent loop.
    ContinueLoop { reason: String },
    /// Tool approval decision from user (shell command risk confirmation).
    ApprovalDecision {
        request_id: String,
        approved: bool,
        allow_all_session: bool,
        reason: Option<String>,
    },
    /// User's answer to an ask_user_question prompt.
    QuestionAnswer { request_id: String, answer: String },
    /// ADR-045: Cancel a single in-flight tool execution. The iteration
    /// continues normally; the cancelled tool returns a "Cancelled by user"
    /// result so the LLM can react. Unknown tool_call_id is a no-op
    /// (race vs. tool natural completion).
    CancelTool { tool_call_id: String },
    /// Apply runtime configuration changes immediately.
    ///
    /// Agent-scoped (not session-scoped):
    /// `gateway_loop::dispatch_inbound` routes this variant at BOTH
    /// routing levels (system-level `""` and per-session) through
    /// `SessionManager::apply_runtime_config_override`, which updates
    /// (1) the shared `AgentCore` template - so sessions created LATER
    /// inherit the new values, (2) the `runtime_overrides` cache - so
    /// every newly spawned `SessionTask::new` re-applies them on its
    /// per-session `core_mut`, (3) every active SessionTask's
    /// `ContextBuilder` (via the `SessionMessage::UpdateRuntimeConfig`
    /// broadcast - the LLM sees the new tool_definitions on the next
    /// `build_chat_request`), and (4) mid-execution AgentLoops via the
    /// inbound fast channel (`AgentLoop::apply_runtime_config` ->
    /// `core.apply_runtime_config`). ADR-052 §3.5.
    UpdateRuntimeConfig(Box<RuntimeConfigOverrides>),
}

/// Design doc 27 §3.2 intent envelope (tech-debt plan PR-D).
///
/// `message_id` is the Gateway-stable identity (mailbox key),
/// `conversation_id` / `source_chat_session_id` are empty until
/// Phase 2, and `repo` is a Gateway-validated source-side authorized
/// repo the target may work in. All optional (defaulted) so existing
/// serialized forms still deserialize.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentEnvelope {
    pub from: String,
    pub action: String,
    pub params: serde_json::Value,
    #[serde(default)]
    pub message_id: String,
    #[serde(default)]
    pub conversation_id: String,
    #[serde(default)]
    pub source_chat_session_id: String,
    #[serde(default)]
    pub repo: String,
    /// Initiator's `session_config` suggestion (design 27 §4.5); consumed
    /// at execution-session creation only, never rendered into the turn.
    #[serde(default)]
    pub session_config_json: String,
    /// "request" | "response" | "user_message" (design 27 §5.4.5 R2,
    /// §5.4.4: a human Chat-session message routed through the Intent
    /// transport); empty = request.
    #[serde(default)]
    pub kind: String,
    /// For a response: the request message id this answers.
    #[serde(default)]
    pub in_reply_to: String,
    /// For a response: outcome (§7.1); empty = completed.
    #[serde(default)]
    pub status: String,
    /// Design 27 §5.5.4 / plan 2b.3: whether the source Chat session has
    /// a human participant who can answer a decision card. `None` =
    /// unknown → human-facing tools stay available (fail open).
    #[serde(default)]
    pub has_human: Option<bool>,
}

/// Messages that can be injected into the agent loop from external sources.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InboundMessage {
    /// Direct user message
    UserMessage(String),
    /// System notification (identity update, capability change, etc.)
    SystemNotification {
        notification_type: String,
        data: serde_json::Value,
    },
    /// Cross-agent intent message (design doc 27 §4.2.3 envelope).
    ///
    /// Boxed payload: the envelope carries ~11 string fields, which would
    /// otherwise dominate every `InboundMessage` slot (`clippy::large_enum_variant`).
    IntentMessage(Box<IntentEnvelope>),
    /// Stop signal to stop the current agent loop iteration
    Stop { reason: String },
    /// ADR-034: Continue execution after iteration limit was reached.
    /// Resets the iteration counter and resumes the agent loop.
    /// Carries `session_id` so MQTT dispatch can route via the
    /// same channel used by gRPC era (no change of semantics).
    ContinueExecution {
        /// ADR-034: session_id for routing (Phase 2 may deprecate).
        session_id: String,
        /// Reason for continuing (optional, for logging).
        reason: String,
    },
    /// ADR-034: Tool approval decision from user (shell command risk confirmation).
    /// Delivered by Gateway after user clicks Allow/Deny in the Desktop App.
    /// Carries `session_id` for parity with proto Message.
    ApprovalDecision {
        /// The approval request ID (matches ChunkEvent::ToolApprovalNeeded)
        request_id: String,
        /// Whether the user approved the tool execution
        approved: bool,
        /// Whether to always allow for this session
        allow_all_session: bool,
        /// Optional reason for denial
        reason: Option<String>,
        /// ADR-034: session_id for routing (Phase 2 may deprecate).
        session_id: String,
    },
    /// User's answer to an ask_user_question prompt.
    /// Delivered by Gateway after the user selects an option or types free text.
    /// Carries `session_id` for parity with proto Message.
    QuestionAnswer {
        /// The request ID (matches ChunkEvent::AskQuestion)
        request_id: String,
        /// The user's answer:
        /// - If they chose a pre-defined option: the option's label
        /// - If they typed free text (via "Other"): their free-text input
        answer: String,
        /// ADR-034: session_id for routing (Phase 2 may deprecate).
        session_id: String,
    },
    /// ADR-034 §8 Phase 1C placeholder (Phase 2 adds business logic):
    /// user-initiated compress action. `compress_type` is the prost-generated
    /// `CompressType` i32: 0=UNSPECIFIED, 1=SUMMARY, 2=TOOL_RESULTS.
    /// Phase 2 will dispatch to `SessionMessage::CompressAction(CompressionAction)`.
    CompressAction {
        session_id: String,
        compress_type: i32,
    },
    /// Unified user operation delivered via the fast `send_inbound()`
    /// channel.  These bypass SessionTask's message loop and are
    /// consumed by `poll_stop()` / `drain_inbound_queue()` for
    /// immediate in-flight effect.
    UserOperation(UserOp),
    /// ADR-034 Phase 7: User chat message from MQTT SendMessage.
    /// Carries content and message_id for SessionMessage::ChatMessage.
    /// Also carries raw params_json so the handler can extract
    /// attached_items / skill_instructions / content_parts, and the
    /// `command` (skill name for per-turn skill injection — resolved
    /// against the agent's SkillRegistry by the handler; the runtime is
    /// the authoritative source of skill instructions, never the client).
    ChatMessage {
        content: String,
        message_id: String,
        command: String,
        params_json: String,
    },
    /// ADR-029 + ADR-052: agent-level builtin-tool enabled-flag update.
    ///
    /// `gateway_loop::dispatch_inbound` routes this variant at BOTH
    /// routing levels (system-level `""` and per-session) through
    /// `SessionManager::apply_builtin_tools_enabled`, which updates
    /// (1) the shared `AgentCore` template - so sessions opened LATER
    /// inherit the new enabled flags via the same shared policy helper
    /// (`AgentCore::apply_builtin_enabled_entries`), (2) every active
    /// session's `builtin_tools` enabled flags (via the
    /// `SessionMessage::UpdateBuiltinTools` broadcast - which in turn
    /// triggers the dispatch-list + LLM `tool_definitions` atomic
    /// rebuild in `session_task::apply_builtin_tools_update`).
    UpdateBuiltinTools {
        entries: Vec<crate::agent_config::AgentToolEntry>,
    },
    /// ADR-063 §3.7.6: agent-level main-dialog system prompt update.
    ///
    /// Produced by the L2 reload endpoint (`POST /agents/{id}/prompts/reload`
    /// in `http::prompts`) after it rebuilds the main dialog system prompt
    /// from `prompts/*.md` (including the required `system.md`). Routed
    /// system-level (`session_id ""`) through
    /// `SessionManager::apply_system_prompt`, which updates the shared
    /// template (so sessions opened LATER inherit the new value) and
    /// broadcasts `SessionMessage::UpdateSystemPrompt` to every active
    /// session's `ContextBuilder` — so editing `system.md` takes effect
    /// immediately, without an agent restart.
    UpdateSystemPrompt { system_prompt: String },
    /// ADR-094: agent-level Progressive-mode skills summary update.
    /// `None` clears the section (Manual mode hot-toggle). Mirrors
    /// the `UpdateSystemPrompt` shape and lifecycle: agent-scoped
    /// template + per-session broadcast via
    /// `SessionManager::apply_skill_instructions`. Without this,
    /// `rebuild_and_dispatch_system_prompt` (HTTP hot-reload) would
    /// silently drop the skills section in Progressive mode because
    /// `SystemPromptParts.base` no longer embeds it.
    UpdateSkillInstructions { instructions: Option<String> },
    /// Replace the agent-wide immutable skill registry snapshot after package reload.
    UpdateSkillRegistry { registry: crate::skills::parser::SkillRegistry },
}

impl InboundMessage {
    /// Apply payload size limits to this message, truncating if necessary.
    /// Returns the (possibly truncated) message and whether truncation occurred.
    pub fn enforce_size_limit(mut self) -> (Self, bool) {
        let mut truncated = false;
        match &mut self {
            InboundMessage::UserMessage(text) => {
                if text.len() > MAX_INBOUND_PAYLOAD_SIZE {
                    tracing::warn!(
                        original_len = text.len(),
                        max = MAX_INBOUND_PAYLOAD_SIZE,
                        "Truncating oversized inbound UserMessage"
                    );
                    *text = truncate_to_bytes(text, MAX_INBOUND_PAYLOAD_SIZE);
                    truncated = true;
                }
            }
            InboundMessage::SystemNotification {
                notification_type,
                data,
            } => {
                let data_str = data.to_string();
                let header_overhead = notification_type.len() + "[system:] ".len();
                let effective_limit = MAX_INBOUND_PAYLOAD_SIZE.saturating_sub(header_overhead);
                if data_str.len() > effective_limit {
                    tracing::warn!(
                        original_len = data_str.len(),
                        max = effective_limit,
                        notification_type = %notification_type,
                        "Truncating oversized inbound SystemNotification data"
                    );
                    *data = truncate_json_to_bytes(data, effective_limit);
                    truncated = true;
                }
            }
            InboundMessage::IntentMessage(env) => {
                let params_str = env.params.to_string();
                let header_overhead = env.from.len() + env.action.len() + "[intent::] ".len();
                let effective_limit = MAX_INBOUND_PAYLOAD_SIZE.saturating_sub(header_overhead);
                if params_str.len() > effective_limit {
                    tracing::warn!(
                        original_len = params_str.len(),
                        max = effective_limit,
                        from = %env.from,
                        action = %env.action,
                        "Truncating oversized inbound IntentMessage params"
                    );
                    env.params = truncate_json_to_bytes(&env.params, effective_limit);
                    truncated = true;
                }
            }
            InboundMessage::Stop { .. } => {
                // Stop messages don't need size limits
            }
            InboundMessage::ContinueExecution { .. } => {
                // Continue messages don't need size limits
            }
            InboundMessage::ApprovalDecision { .. } => {
                // Approval decisions don't need size limits
            }
            InboundMessage::QuestionAnswer { answer, .. } => {
                if answer.len() > MAX_INBOUND_PAYLOAD_SIZE {
                    tracing::warn!(
                        original_len = answer.len(),
                        max = MAX_INBOUND_PAYLOAD_SIZE,
                        "Truncating oversized inbound QuestionAnswer"
                    );
                    *answer = truncate_to_bytes(answer, MAX_INBOUND_PAYLOAD_SIZE);
                    truncated = true;
                }
            }
            InboundMessage::UserOperation(_) => {
                // UserOperation variants don't need size limits
            }
            // ADR-034 §8 Phase 1C placeholder: new control commands.
            // Phase 2 will route these through AgentLoop. For now,
            // they don't carry user-supplied bytes, so no size limit.
            InboundMessage::CompressAction { .. } => {}
            InboundMessage::ChatMessage { .. } => {}
            InboundMessage::UpdateBuiltinTools { .. } => {}
            InboundMessage::UpdateSystemPrompt { system_prompt } => {
                if system_prompt.len() > MAX_INBOUND_PAYLOAD_SIZE {
                    tracing::warn!(
                        original_len = system_prompt.len(),
                        max = MAX_INBOUND_PAYLOAD_SIZE,
                        "Truncating oversized inbound UpdateSystemPrompt"
                    );
                    *system_prompt = truncate_to_bytes(system_prompt, MAX_INBOUND_PAYLOAD_SIZE);
                    truncated = true;
                }
            }
            InboundMessage::UpdateSkillRegistry { .. } => {}
            InboundMessage::UpdateSkillInstructions { instructions } => {
                if let Some(s) = instructions.as_ref()
                    && s.len() > MAX_INBOUND_PAYLOAD_SIZE
                {
                    tracing::warn!(
                        original_len = s.len(),
                        max = MAX_INBOUND_PAYLOAD_SIZE,
                        "Truncating oversized inbound UpdateSkillInstructions"
                    );
                    *instructions = Some(truncate_to_bytes(s, MAX_INBOUND_PAYLOAD_SIZE));
                    truncated = true;
                }
            }
        }
        (self, truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inbound_message_serde_roundtrip() {
        let msg = InboundMessage::IntentMessage(Box::new(IntentEnvelope {
            from: "com.test.agent".to_string(),
            action: "update_identity".to_string(),
            params: serde_json::json!({"key": "value"}),
            message_id: "m-1".to_string(),
            conversation_id: String::new(),
            source_chat_session_id: String::new(),
            repo: String::new(),
            session_config_json: String::new(),
            kind: String::new(),
            in_reply_to: String::new(),
            status: String::new(),
            has_human: None,
        }));
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: InboundMessage = serde_json::from_str(&json).unwrap();
        if let InboundMessage::IntentMessage(env) = decoded {
            assert_eq!(env.from, "com.test.agent");
            assert_eq!(env.action, "update_identity");
            assert_eq!(env.params["key"], "value");
        } else {
            panic!("Expected IntentMessage");
        }
    }

    #[test]
    fn test_truncate_to_bytes_ascii() {
        let s = "Hello, world!";
        assert_eq!(truncate_to_bytes(s, 100), s);
        assert_eq!(truncate_to_bytes(s, 5), "Hello");
    }

    #[test]
    fn test_truncate_to_bytes_utf8_multibyte() {
        // "你好世界" — each CJK char is 3 bytes
        let s = "你好世界";
        assert_eq!(s.len(), 12); // 4 chars * 3 bytes
        // Truncate to 8 bytes — should land on char boundary (2 chars * 3 = 6 bytes)
        let result = truncate_to_bytes(s, 8);
        assert_eq!(result, "你好");
        // Truncate at exact char boundary
        let result = truncate_to_bytes(s, 6);
        assert_eq!(result, "你好");
        // Truncate at 7 bytes — should fall back to 6 (end of second char)
        let result = truncate_to_bytes(s, 7);
        assert_eq!(result, "你好");
    }

    #[test]
    fn test_truncate_to_bytes_emoji() {
        // "🎉🎊" — emoji are 4 bytes each in UTF-8
        let s = "🎉🎊";
        assert_eq!(s.len(), 8);
        // Truncate to 5 bytes — should fall back to 4 (first emoji only)
        let result = truncate_to_bytes(s, 5);
        assert_eq!(result, "🎉");
    }

    #[test]
    fn test_enforce_size_limit_small_message() {
        let msg = InboundMessage::UserMessage("Hello".to_string());
        let (msg, truncated) = msg.enforce_size_limit();
        assert!(!truncated);
        if let InboundMessage::UserMessage(text) = msg {
            assert_eq!(text, "Hello");
        } else {
            panic!("Expected UserMessage");
        }
    }

    #[test]
    fn test_enforce_size_limit_oversized_user_message() {
        let long_text = "A".repeat(MAX_INBOUND_PAYLOAD_SIZE + 1000);
        let msg = InboundMessage::UserMessage(long_text.clone());
        let (msg, truncated) = msg.enforce_size_limit();
        assert!(truncated);
        if let InboundMessage::UserMessage(text) = msg {
            assert!(text.len() <= MAX_INBOUND_PAYLOAD_SIZE);
            assert!(text.starts_with('A'));
        } else {
            panic!("Expected UserMessage");
        }
    }

    #[test]
    fn test_enforce_size_limit_utf8_safe_truncation() {
        // Create a string with multi-byte chars that exceeds the limit
        let long_text = "你好".repeat(2000); // Each 你好 is 6 bytes, total > 4096
        let msg = InboundMessage::UserMessage(long_text.clone());
        let (msg, truncated) = msg.enforce_size_limit();
        assert!(truncated);
        if let InboundMessage::UserMessage(text) = msg {
            // Verify the truncated string is valid UTF-8
            assert!(text.len() <= MAX_INBOUND_PAYLOAD_SIZE);
            assert!(std::str::from_utf8(text.as_bytes()).is_ok());
        } else {
            panic!("Expected UserMessage");
        }
    }

    #[test]
    fn test_enforce_size_limit_system_notification() {
        let big_data = serde_json::json!({"key": "A".repeat(MAX_INBOUND_PAYLOAD_SIZE + 500)});
        let msg = InboundMessage::SystemNotification {
            notification_type: "test".to_string(),
            data: big_data,
        };
        let (msg, truncated) = msg.enforce_size_limit();
        assert!(truncated);
        if let InboundMessage::SystemNotification { data, .. } = msg {
            // Data should have been truncated
            assert!(data.to_string().len() <= MAX_INBOUND_PAYLOAD_SIZE);
        } else {
            panic!("Expected SystemNotification");
        }
    }

    #[test]
    fn test_enforce_size_limit_intent_message() {
        let big_params = serde_json::json!({"data": "B".repeat(MAX_INBOUND_PAYLOAD_SIZE + 500)});
        let msg = InboundMessage::IntentMessage(Box::new(IntentEnvelope {
            from: "com.test.agent".to_string(),
            action: "ping".to_string(),
            params: big_params,
            message_id: String::new(),
            conversation_id: String::new(),
            source_chat_session_id: String::new(),
            repo: String::new(),
            session_config_json: String::new(),
            kind: String::new(),
            in_reply_to: String::new(),
            status: String::new(),
            has_human: None,
        }));
        let (msg, truncated) = msg.enforce_size_limit();
        assert!(truncated);
        if let InboundMessage::IntentMessage(env) = msg {
            assert!(env.params.to_string().len() <= MAX_INBOUND_PAYLOAD_SIZE);
        } else {
            panic!("Expected IntentMessage");
        }
    }
}
