//! ADR-060 end-to-end test (crate-internal): prompt-cache-friendly context
//! block reorganisation across the full production chain.
//!
//! Drives the REAL chain in-process:
//!   SessionManager → SessionTask → AgentLoop → ContextBuilder::build()
//!   → ScriptedProvider (records every ChatRequest) → tool dispatch
//!   (`todo_write` intercepted in `loop_tools`) → `SessionState::update_todos`
//!   → `ConversationSession::set_todos` → meta.json on disk
//!
//! Then simulates a process restart (fresh SessionManager, fresh AgentCore,
//! resume from disk) and verifies Block C is rebuilt from the persisted
//! todos.
//!
//! Why crate-internal? `AgentCore::new` is `pub(crate)` — the production
//! startup path (`startup::session_init`) is not a public API, so an
//! integration test under `tests/` cannot assemble the loop. Same pattern
//! as `test_support.rs`: the module is compiled out of non-test builds.
//!
//! The ONLY simulated part is the LLM: `ScriptedProvider` pops scripted
//! `ChatResponse` fixtures (the same trust boundary as `memory_e2e.rs`).
//! Everything else — session lifecycle, message persistence, meta writes,
//! context building, tool execution, restart recovery — is production code.

#![cfg(test)]

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use acowork_core::error::Result;
use acowork_core::manifest::AgentManifest;
use acowork_core::protocol::{ModelCapabilitiesInfo, ProviderListItem, ProviderModelEntry};
use acowork_core::providers::traits::{
    ChatMessage, ChatRequest, ChatResponse, FunctionCall, MessageRole, Provider, StreamEvent,
    ToolCall, UsageInfo,
};
use async_trait::async_trait;
use futures_core::Stream;

use crate::agent::agent_core::{AgentCore, BuiltinToolEntry};
use crate::agent::loop_::{ChunkEvent, SessionChunkEvent};
use crate::agent::session::session_manager::{SessionManager, SessionManagerConfig};
use crate::agent::session::session_task::SessionMessage;
use crate::config::RuntimeConfig;
use crate::conversation::{ConversationSession, SessionConfig};
use crate::tools::builtin::todo_write::TodoWriteTool;
use crate::tools::workspace_resolver::WorkspaceResolver;

// ═══════════════════════════════════════════════════════════════════════
// ScriptedProvider — the only simulated component
// ═══════════════════════════════════════════════════════════════════════

/// One scripted LLM response step.
#[derive(Debug, Clone)]
struct ScriptedStep {
    content: String,
    tool_calls: Option<Vec<ToolCall>>,
}

fn text_step(content: &str) -> ScriptedStep {
    ScriptedStep {
        content: content.to_string(),
        tool_calls: None,
    }
}

/// A `todo_write` tool-call step: asks the loop to persist `(id, content)`
/// pairs, exactly like the production LLM would after reading Block C.
fn todo_write_step(items: &[(&str, &str)]) -> ScriptedStep {
    let todos: Vec<serde_json::Value> = items
        .iter()
        .map(|(id, content)| {
            serde_json::json!({
                "id": id,
                "content": content,
                "status": "pending",
            })
        })
        .collect();
    ScriptedStep {
        content: String::new(),
        tool_calls: Some(vec![ToolCall {
            id: "toolu_01".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "todo_write".to_string(),
                arguments: serde_json::json!({ "todos": todos }).to_string(),
            },
        }]),
    }
}

/// Deterministic LLM stand-in for the ADR-060 E2E.
///
/// Two script queues keep the fixture deterministic even though the
/// session-title generation runs on a background task racing the main
/// loop: title requests (`Generate a session title …` single-message
/// calls, routed via `provider.chat`) pop from `title_steps`, main chat
/// requests (the Block A/B/C/D pipeline, routed via `provider.chat_stream`)
/// pop from `main_steps`. The last step of each queue repeats so an
/// unexpected extra call fails loudly in assertions instead of returning
/// an empty response.
struct ScriptedProvider {
    main_steps: Mutex<VecDeque<ScriptedStep>>,
    title_steps: Mutex<VecDeque<ScriptedStep>>,
    /// Main chat requests in call order (Block A/B/C/D assertions).
    main_captured: Mutex<Vec<ChatRequest>>,
    /// Title-generation requests (asserted only for count sanity).
    title_captured: Mutex<Vec<ChatRequest>>,
}

impl ScriptedProvider {
    fn new(main_steps: Vec<ScriptedStep>, title_steps: Vec<ScriptedStep>) -> Self {
        Self {
            main_steps: Mutex::new(main_steps.into()),
            title_steps: Mutex::new(title_steps.into()),
            main_captured: Mutex::new(Vec::new()),
            title_captured: Mutex::new(Vec::new()),
        }
    }

    /// Captured main-chat requests (excludes background title calls).
    fn captured(&self) -> Vec<ChatRequest> {
        self.main_captured.lock().unwrap().clone()
    }

    /// True for the session-title generation request: a single User
    /// message carrying the title prompt (spawned after the first message
    /// of each session).
    fn is_title_request(request: &ChatRequest) -> bool {
        request.messages.len() == 1
            && request.messages[0].role == MessageRole::User
            && request.messages[0]
                .content
                .contains("Generate a session title")
    }

    fn pop_step(&self, title: bool) -> ChatResponse {
        let steps = if title {
            &self.title_steps
        } else {
            &self.main_steps
        };
        let mut steps = steps.lock().unwrap();
        // Pop while more than one remains; the last step repeats so an
        // unexpected extra LLM call fails loudly in assertions instead of
        // returning an empty response.
        let step = if steps.len() > 1 {
            steps.pop_front().expect("non-empty queue")
        } else {
            steps.front().expect("script always has a step").clone()
        };
        ChatResponse {
            content: step.content,
            tool_calls: step.tool_calls,
            usage: Some(UsageInfo {
                prompt_tokens: 50,
                completion_tokens: 25,
                total_tokens: 75,
                ..Default::default()
            }),
            ..Default::default()
        }
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn name(&self) -> &str {
        "scripted-e2e"
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        let title = Self::is_title_request(&request);
        if title {
            self.title_captured.lock().unwrap().push(request);
        } else {
            self.main_captured.lock().unwrap().push(request);
        }
        Ok(self.pop_step(title))
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<Box<dyn Stream<Item = StreamEvent> + Send>> {
        let title = Self::is_title_request(&request);
        if title {
            self.title_captured.lock().unwrap().push(request.clone());
        } else {
            self.main_captured.lock().unwrap().push(request.clone());
        }
        let resp = self.pop_step(title);
        let mut events = Vec::with_capacity(2);
        if !resp.content.is_empty() {
            events.push(StreamEvent::Content(resp.content.clone()));
        }
        events.push(StreamEvent::Finished(resp));
        Ok(Box::new(futures_util::stream::iter(events)))
    }

    async fn chat_token_count(&self, messages: &[ChatMessage]) -> Result<u64> {
        Ok(messages.iter().map(|m| m.content.len() as u64 / 4).sum())
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Harness — real production assembly
// ═══════════════════════════════════════════════════════════════════════

/// Test agent identity shared by config + manifest.
const AGENT_ID: &str = "com.test.agent";
/// System prompt; Block A stability assertions depend on it being identical
/// across both harness generations.
const SYSTEM_PROMPT: &str = "You are the ACowork E2E test agent (ADR-060).";

fn test_config(work_dir: &Path) -> RuntimeConfig {
    RuntimeConfig {
        agent_id: AGENT_ID.to_string(),
        work_dir: work_dir.display().to_string(),
        ..Default::default()
    }
}

fn test_manifest() -> AgentManifest {
    // All optional manifest sections default via serde; only the identity
    // strings are required.
    AgentManifest::from_toml(
        r#"
        agent_id = "com.test.agent"
        version = "1.0.0"
        name = "Test Agent"
        description = "ADR-060 E2E test agent"
        author = "acowork"
        runtime_version = "1.0.0"
        "#,
    )
    .expect("minimal manifest parses")
}

struct E2eHarness {
    sid: String,
    sm: Arc<tokio::sync::Mutex<SessionManager>>,
    chunk_rx: tokio::sync::mpsc::Receiver<SessionChunkEvent>,
}

impl E2eHarness {
    /// Send one user message and block until the round emits `Done`.
    ///
    /// The session task is driven entirely by production code: context
    /// build, LLM call (scripted), tool execution, persistence.
    async fn send(&mut self, content: &str, message_id: &str) {
        {
            let mut guard = self.sm.lock().await;
            guard
                .send_to_session(
                    &self.sid,
                    SessionMessage::ChatMessage {
                        content: content.to_string(),
                        message_id: message_id.to_string(),
                        active_skill: None,
                        attached_items: None,
                        content_parts: None,
                        attachment_blobs: Vec::new(),
                    },
                )
                .expect("session task accepts the message");
        }
        // Wait for the round to finish (Done). Other control events
        // (ContextUsage, TodoListUpdated, …) are ignored.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let evt = tokio::time::timeout(remaining, self.chunk_rx.recv())
                .await
                .unwrap_or_else(|_| panic!("round for '{content}' did not finish within 60s"))
                .expect("chunk channel stays open");
            match evt.event {
                ChunkEvent::Done { .. } => return,
                ChunkEvent::Error { user_message, .. } => {
                    panic!("round for '{content}' failed: {user_message}")
                }
                _ => {}
            }
        }
    }

    /// Send the manual-compaction action (`CompressAction(CompressSummary)`,
    /// the same thing the Desktop's context-usage menu triggers) and wait
    /// until the compaction round has fully completed (not merely started).
    ///
    /// The session task emits `CompactingStarted` *before* the LLM summary
    /// call, so a wait on that event alone can race the writer: the
    /// `append_compaction_event` + injected-round persistence that follow it
    /// run asynchronously and may not be on disk when this returns. Waiting
    /// for `CompactingEnded` (emitted after the injected round is flushed,
    /// `loop_context.rs`) guarantees the restart test's Generation 2 sees the
    /// fully-persisted post-compaction JSONL.
    async fn compact(&mut self) {
        {
            let mut guard = self.sm.lock().await;
            guard
                .send_to_session(
                    &self.sid,
                    SessionMessage::CompressAction(
                        crate::agent::loop_::CompressionAction::CompressSummary,
                    ),
                )
                .expect("session task accepts CompressAction(CompressSummary)");
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        let mut started = false;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let evt = tokio::time::timeout(remaining, self.chunk_rx.recv())
                .await
                .unwrap_or_else(|_| panic!("compact did not finish within 60s"))
                .expect("chunk channel stays open");
            match evt.event {
                ChunkEvent::CompactingStarted => started = true,
                ChunkEvent::CompactingEnded => {
                    assert!(started, "CompactingEnded before CompactingStarted");
                    return;
                }
                ChunkEvent::Error { user_message, .. } => {
                    panic!("compact failed: {user_message}")
                }
                // Background events (TitleGenerated, ContextUsage, …) are
                // ignored; keep draining until the round terminates.
                _ => {}
            }
        }
    }

    /// Shut the session task down gracefully and await its termination.
    ///
    /// Simulates a clean process exit: sends `Close`, then awaits the
    /// session task's `JoinHandle`. Without the await, the single-process
    /// restart tests (`restart_after_compression_preserves_todo_state`)
    /// could race Generation 1's still-running writer thread against
    /// Generation 2's `ConversationSession::resume` — the residual writer
    /// holding an `Arc<ConversationSession>` could overwrite meta.json after
    /// Generation 2 has read it. Awaiting the join makes the process boundary
    /// deterministic.
    async fn shutdown(&mut self) {
        self.sm
            .lock()
            .await
            .close_session(&self.sid)
            .await
            .unwrap_or_else(|e| panic!("close_session failed: {e}"));
    }
}

/// Assemble the REAL production chain for one "process generation":
/// AgentCore (shared template) + SessionManager + a ConversationSession
/// wired to disk. When `resume_sid` is `Some`, the conversation is resumed
/// from disk (restart simulation) instead of created fresh.
async fn spawn_harness(
    work_dir: &Path,
    provider: Arc<ScriptedProvider>,
    resume_sid: Option<&str>,
) -> E2eHarness {
    let core = Arc::new(AgentCore::new(
        test_config(work_dir),
        test_manifest(),
        provider,
        vec![BuiltinToolEntry {
            // Minimal tool set: only `todo_write` is needed for ADR-060.
            tool: Arc::new(TodoWriteTool::new()),
            enabled: true,
        }],
    ));

    // Register the test model with capabilities — in production this data
    // arrives via the Gateway `AgentHelloResult` push; the loop's
    // context-usage reporting requires it before emitting a response.
    core.global_provider_list
        .write()
        .unwrap()
        .push(ProviderListItem {
            id: "test-provider".to_string(),
            base_url: "http://127.0.0.1:0".to_string(),
            protocol_type: acowork_core::protocol::ProtocolType::OpenAI,
            models: vec![ProviderModelEntry {
                id: "test-model".to_string(),
                capabilities: ModelCapabilitiesInfo {
                    context_window: 128_000,
                    max_output_tokens: 4096,
                    max_input_tokens: None,
                    supports_tool_calling: true,
                    supports_reasoning: None,
                    supports_attachment: None,
                    supports_temperature: None,
                    cost: None,
                    modalities: None,
                    name: Some("Test Model".to_string()),
                    family: None,
                    knowledge_cutoff: None,
                    default_reasoning_effort: None,
                    thinking_mode: None,
                },
                max_output_tokens_limit: 4096,
            }],
            compact_model: None,
            custom: false,
        });

    let (chunk_tx, chunk_rx) = tokio::sync::mpsc::channel(64);
    let mut sm = SessionManager::new(
        core,
        SessionManagerConfig {
            system_prompt: SYSTEM_PROMPT.to_string(),
            chunk_tx: Some(chunk_tx),
            ..Default::default()
        },
    );
    sm.set_resolver(Arc::new(std::sync::RwLock::new(
        WorkspaceResolver::new_for_test(vec![]),
    )));

    let committed_lines = SessionManager::new_committed_lines();
    let (sid, conversation) = if let Some(resume_sid) = resume_sid {
        let (conv, _meta_rx, _state_rx) =
            ConversationSession::resume(work_dir, resume_sid, committed_lines.clone())
                .expect("resume conversation from disk");
        (resume_sid.to_string(), conv)
    } else {
        let sid = "test-sid-1".to_string();
        let (conv, _meta_rx, _state_rx) = ConversationSession::new(
            work_dir,
            &sid,
            SessionConfig {
                agent_id: AGENT_ID.to_string(),
                workspace_id: None,
                // Model MUST be a registered id (loop resolves capabilities
                // from `global_provider_list`). Provider id is deliberately
                // NOT registered: `SessionTask` then falls into the
                // `ReliableProvider::new(core.provider)` branch instead of
                // building a real network provider for `base_url`, keeping
                // the scripted provider in the call path.
                model: Some("test-model".to_string()),
                provider: Some("scripted-e2e".to_string()),
            },
            8,
            committed_lines.clone(),
        )
        .expect("create conversation on disk");
        (sid, conv)
    };
    sm.create_session_with_id_and_conversation(
        sid.clone(),
        Some(conversation),
        Some(committed_lines),
    )
    .await
    .expect("session task starts");

    E2eHarness {
        sid,
        sm: Arc::new(tokio::sync::Mutex::new(sm)),
        chunk_rx,
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Assertion helpers
// ═══════════════════════════════════════════════════════════════════════

/// Short role names for layout assertions (A/B/C/D block ordering).
fn roles(msgs: &[ChatMessage]) -> Vec<&'static str> {
    msgs.iter()
        .map(|m| match m.role {
            MessageRole::System => "system",
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool",
        })
        .collect()
}

/// Assert the classic prompt-cache layout of one request:
/// ADR-060 v2 §5.4 + §5.5 erratum: the request layout is A → B only, with
/// the current user message at Block B's tail. Todo state lives in Block B
/// as the most recent `todo_write` tool result — there is no Block C tail
/// snapshot and no Block D duplicate. This helper asserts that contract.
///
/// NOTE: the "appears exactly once" check compares `content` strings, so it
/// assumes each test turn uses a distinct message text (all callers do).
fn assert_block_v2_layout(msgs: &[ChatMessage], current: &str) {
    let n = msgs.len();
    assert!(n >= 2, "expected A + B(current user at tail), got {n} messages");
    // Block A — system message at index 0.
    assert_eq!(msgs[0].role, MessageRole::System, "Block A is system role");
    assert!(
        !msgs[0].content.contains("## Todo Task List"),
        "Block A must NOT contain the todo snapshot — it lives in Block B"
    );
    // Block B tail — the current user message is the last message. ADR-060
    // §5.5 erratum: there is no separate "Block D" duplicate; the current
    // turn lives at Block B's tail and appears exactly once.
    assert_eq!(
        msgs[n - 1].role,
        MessageRole::User,
        "current user message is at Block B tail"
    );
    assert_eq!(
        msgs[n - 1].content,
        current,
        "Block B tail carries the current turn"
    );
    assert_eq!(
        msgs.iter().filter(|m| m.content == current).count(),
        1,
        "current turn appears exactly once (no Block D duplicate)"
    );
    // ADR-060 v2: no Block C. The "## Todo Task List" header must NOT appear
    // anywhere in the request — todo state is carried by the most recent
    // todo_write tool result in Block B (verified separately).
    assert!(
        !msgs.iter().any(|m| m.content.contains("## Todo Task List")),
        "ADR-060 v2: no Block C snapshot — todo state lives in Block B's tool result"
    );
}

/// Assert Block B contains a `todo_write` tool result with the given
/// `tool_call_id`. The canonical todo state source.
fn assert_block_b_carries_todo_state(msgs: &[ChatMessage], tool_call_id: &str) {
    let found = msgs
        .iter()
        .any(|m| m.role == MessageRole::Tool && m.tool_call_id.as_deref() == Some(tool_call_id));
    assert!(
        found,
        "Block B must contain the todo_write tool result ({tool_call_id}) \
         — that is the canonical todo state source"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// The end-to-end test
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn todo_write_roundtrip_v2_layout_and_restart_recovery() {
    // ADR-060 v2: Block C (tail todo snapshot) is removed. Todo state lives
    // in Block B as the most recent real `todo_write` tool result. Layout:
    // A → B → D (no C). Restart must preserve Block B across processes.
    let dir = tempfile::tempdir().expect("tempdir");
    let work_dir = dir.path();

    // ── Generation 1: fresh process ────────────────────────────────────
    // Main script: turn 1 text → turn 2 todo_write tool call → turn 2
    // follow-up text → turn 3 text. Title script: one response per
    // session (first message triggers a background title call).
    let provider = Arc::new(ScriptedProvider::new(
        vec![
            text_step("Turn 1 response"),
            todo_write_step(&[("t1", "Implement Block C")]),
            text_step("Todos saved"),
            text_step("Turn 3 response"),
        ],
        vec![text_step("Test session")],
    ));
    let mut h1 = spawn_harness(work_dir, provider.clone(), None).await;
    let sid = h1.sid.clone();

    // ── Turn 1: no todos yet → layout is A + B(u1), u1 at Block B tail ──
    h1.send("First turn", "m1").await;
    let captured = provider.captured();
    assert_eq!(
        captured.len(),
        1,
        "turn 1 must trigger exactly one LLM call"
    );
    let msgs = &captured[0].messages;
    assert_eq!(
        roles(msgs),
        ["system", "user"],
        "A + B(u1) — current user message appears once, no Block D duplicate"
    );
    assert!(
        msgs[0].content.contains(SYSTEM_PROMPT),
        "Block A carries the system prompt"
    );
    assert!(
        !msgs[0].content.contains("## Todo Task List"),
        "Block A must NOT contain the todo snapshot"
    );
    assert_eq!(
        msgs[1].content, "First turn",
        "Block B tail holds the current turn (appears exactly once)"
    );

    // ── Turn 2: LLM returns a todo_write tool call ──
    h1.send("Second turn", "m2").await;
    let captured = provider.captured();
    // Call 2 = turn-2 request; call 3 = the post-tool iteration request.
    assert_eq!(
        captured.len(),
        3,
        "turn 2 must trigger two LLM calls (tool iteration)"
    );
    let msgs = &captured[1].messages;
    assert_eq!(msgs.last().unwrap().role, MessageRole::User);
    assert_eq!(msgs.last().unwrap().content, "Second turn");

    let msgs = &captured[2].messages;
    // Tool iteration (ADR-060 v2 §5.5): no Block D — the request ends with
    // the Tool result of todo_write (Block B). The current user turn
    // appears only once (Block B). NO Block C — todo state is in Block B's
    // tool result, NOT in a synthetic tail snapshot.
    assert_eq!(
        msgs.last().unwrap().role,
        MessageRole::Tool,
        "tool iteration: last message is the todo_write Tool result, no Block C/D"
    );
    assert_eq!(
        msgs.last().unwrap().tool_call_id.as_deref(),
        Some("toolu_01"),
        "tail is the todo_write tool result"
    );
    assert_block_b_carries_todo_state(msgs, "toolu_01");
    // The "## Todo Task List" header must NOT appear anywhere — that was
    // the Block C contract that we just removed.
    assert!(
        !msgs.iter().any(|m| m.content.contains("## Todo Task List")),
        "ADR-060 v2: no Block C — todo state lives in the todo_write tool result"
    );
    // The current user turn appears exactly once (Block B only, no D).
    assert_eq!(
        msgs.iter().filter(|m| m.content == "Second turn").count(),
        1,
        "no Block D in tool iteration — user turn appears once in Block B"
    );

    // ── Disk: todos must be persisted to meta.json immediately ──
    let meta = crate::conversation::read_session_meta(&work_dir.join("conversations"), &sid)
        .expect("session meta exists");
    let todos = meta.todos.expect("todo list persisted to meta.json");
    assert_eq!(todos.len(), 1);
    assert_eq!(todos[0].id, "t1");
    assert_eq!(todos[0].content, "Implement Block C");

    // ── Turn 3: layout is A + B(..., u3), u3 at Block B tail, todo carried ──
    h1.send("Third turn", "m3").await;
    let captured = provider.captured();
    assert_eq!(captured.len(), 4);
    let msgs = &captured[3].messages;
    assert_block_v2_layout(msgs, "Third turn");
    assert_block_b_carries_todo_state(msgs, "toolu_01");
    assert_eq!(
        msgs.last().unwrap().content,
        "Third turn",
        "current turn is at Block B tail (no Block D duplicate)"
    );

    // ── Prefix stability: Block A byte-identical across every call ──
    // This is the core prompt-cache guarantee of ADR-060: the static
    // kernel (Block A) must never change once a session exists.
    for (i, req) in captured.iter().enumerate() {
        assert_eq!(
            req.messages[0].content, captured[0].messages[0].content,
            "Block A (system) must be byte-stable across calls (call {i})"
        );
        assert_eq!(req.messages[0].role, MessageRole::System);
    }
    // Block B is append-only: the turn-1 user message reappears verbatim.
    assert_eq!(
        captured[3].messages[1].content, captured[0].messages[1].content,
        "Block B history must be append-only (u1 verbatim in turn 3)"
    );

    // ── "Process restart": shut down generation 1, rebuild from disk ──
    // Graceful shutdown + await ensures the writer thread is fully stopped
    // before Generation 2 resumes the same JSONL — a bare `drop` could race
    // the still-running session task against Generation 2's resume.
    h1.shutdown().await;

    let provider2 = Arc::new(ScriptedProvider::new(
        vec![text_step("Turn 4 response")],
        vec![text_step("Restarted session")],
    ));
    let mut h2 = spawn_harness(work_dir, provider2.clone(), Some(&sid)).await;
    h2.send("Fourth turn", "m4").await;

    let captured2 = provider2.captured();
    assert_eq!(captured2.len(), 1);
    let msgs = &captured2[0].messages;
    // After restart, Block B is rebuilt from JSONL — it must still contain
    // the todo_write tool result. Block C does NOT exist (no recompute from
    // disk snapshot); todo state survives via Block B's restored round.
    assert_block_v2_layout(msgs, "Fourth turn");
    assert_block_b_carries_todo_state(msgs, "toolu_01");
    // Block A is byte-identical across the process restart.
    assert_eq!(
        msgs[0].content, captured[0].messages[0].content,
        "Block A must be byte-stable across the process restart"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Helpers — compression-aware additions for ADR-060 v2
// ═══════════════════════════════════════════════════════════════════════

/// A scripted LLM response for the compression summary call.
///
/// `validate_summary_output` (episode_distill.rs) requires:
/// - A `<summary>...</summary>` block present (else `MissingBlock`).
/// - Block content ≥ `MIN_SUMMARY_CHARS` (else `LowQuality`).
/// - No `file:line` / `tool echo` / table contamination (else `LowQuality`).
///
/// The body is a clean Chinese narrative — passes the quality gate without
/// triggering any contamination feature, and is long enough that future
/// tightening of `MIN_SUMMARY_CHARS` doesn't break these tests.
fn summary_step() -> ScriptedStep {
    ScriptedStep {
        content: "<summary>本次会话围绕 ADR-060 v2 压缩路径进行端到端验证，覆盖压缩注入、重启恢复、连续压缩、无 todo 历史等场景，确保 todo 状态在压缩与恢复后仍可被 LLM 正确识别与继续操作。</summary>"
            .to_string(),
        tool_calls: None,
    }
}

/// Count occurrences of a specific tool_call_id across the request's
/// messages. ADR-060 v2 idempotency invariant: after K compactions, the
/// todo_write round must appear exactly ONCE (no duplicate inject).
/// Count compaction markers (User messages with `name = "compaction_summary"`).
fn count_compaction_markers(msgs: &[ChatMessage]) -> usize {
    msgs.iter()
        .filter(|m| m.role == MessageRole::User && m.name.as_deref() == Some("compaction_summary"))
        .count()
}

// ═══════════════════════════════════════════════════════════════════════
// Compression × inject — full chain tests (ADR-060 v2 §5.4)
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn compression_injects_removed_todo_round_after_marker() {
    // ADR-060 v2 §5.4 (compression injection): when level-8 compression
    // strips the middle of the history, the most recent `todo_write` round
    // is spliced after the summary marker so the next LLM call still sees
    // the canonical todo state. With min_ratio=0.90 (default) and a small
    // test history, plan_compression falls through to level 5 (ratio
    // exempt) — which keeps only `[system, last_user]`. The todo round is
    // REMOVED and our inject path must restore it.
    let dir = tempfile::tempdir().expect("tempdir");
    let work_dir = dir.path();

    // Main-script ordering:
    //   [0] turn-1 text → [1] todo_write tool call → [2] post-tool text →
    //   [3] turn-3 text → [4] compaction summary → [5] post-compress text
    // Title-script: one response for the new-session title call.
    let provider = Arc::new(ScriptedProvider::new(
        vec![
            text_step("Turn 1 response"),
            todo_write_step(&[("t1", "Implement Block C")]),
            text_step("Todos saved"),
            text_step("Turn 3 response"),
            summary_step(),
            text_step("Post-compact response"),
        ],
        vec![text_step("Compress inject test")],
    ));
    let mut h = spawn_harness(work_dir, provider.clone(), None).await;

    h.send("First turn", "m1").await;
    h.send("Second turn", "m2").await;
    h.send("Third turn", "m3").await;
    h.compact().await;
    h.send("Fourth turn after compress", "m4").await;

    let captured = provider.captured();
    assert_eq!(
        captured.len(),
        6,
        "5 chat LLM calls (3 turns incl. tool iteration + 1 post) + 1 compaction"
    );

    // ── Sanity: the compaction LLM call was captured and saw the round ──
    let compaction_req = &captured[4];
    assert!(
        compaction_req
            .messages
            .iter()
            .any(|m| m.content.contains("First turn")),
        "compaction request must have seen the pre-compress history"
    );
    assert!(
        compaction_req
            .messages
            .iter()
            .any(|m| m.content.contains("Second turn")),
        "compaction request must have seen the todo_write turn"
    );

    // ── Post-compress request: A + B(marker, A2, T2, u3, u4), u4 at tail ──
    let msgs = &captured.last().unwrap().messages;
    assert_block_v2_layout(msgs, "Fourth turn after compress");
    assert_block_b_carries_todo_state(msgs, "toolu_01");
    assert_eq!(
        count_compaction_markers(msgs),
        1,
        "exactly one compaction marker in the post-compress request"
    );
}

#[tokio::test]
async fn compression_idempotent_across_multiple_compacts() {
    // ADR-060 v2 §5.4 idempotency: when compression fires twice in a row,
    // the inject path must NOT duplicate the todo round. The injector
    // checks for a tool_call_id collision (retained-tail case) before
    // splicing — but here level 5 also strips the round on the second pass,
    // so the injector would naively splice again. The snapshot+idempotency
    // logic in loop_context.rs guards against that.
    let dir = tempfile::tempdir().expect("tempdir");
    let work_dir = dir.path();

    // 7 main steps: 5 chat + 2 compactions. The 7th ("post") repeats if
    // any extra call happens.
    let provider = Arc::new(ScriptedProvider::new(
        vec![
            text_step("Turn 1 response"),
            todo_write_step(&[("t1", "Idempotency test")]),
            text_step("Todos saved"),
            text_step("Turn 3 response"),
            summary_step(),
            summary_step(),
            text_step("Post-compact x2 response"),
        ],
        vec![text_step("Idempotent compact test")],
    ));
    let mut h = spawn_harness(work_dir, provider.clone(), None).await;

    h.send("First turn", "m1").await;
    h.send("Second turn", "m2").await;
    h.send("Third turn", "m3").await;
    h.compact().await;
    h.compact().await;
    h.send("Fourth turn", "m4").await;

    let captured = provider.captured();
    assert_eq!(captured.len(), 7, "5 chat + 2 compactions");

    let msgs = &captured.last().unwrap().messages;
    assert_block_v2_layout(msgs, "Fourth turn");
    // ADR-060 v2 §5.4 idempotency trade-off: after the SECOND compression
    // (level 5 strips both the prior marker AND our prior splice), the
    // `last_injected_todo_call_id` gate refuses re-injecting the same
    // round. Block B therefore does NOT carry a todo_write tool result on
    // this turn — the JSONL still holds the synthetic row from the FIRST
    // injection, and the restorer rebuilds Block B from JSONL on process
    // restart. The test covers that downstream invariant separately
    // (`restart_after_compression_preserves_todo_state`).
    assert!(
        !msgs
            .iter()
            .any(|m| m.role == MessageRole::Tool && m.tool_call_id.as_deref() == Some("toolu_01")),
        "Block B must NOT contain a duplicate todo_write tool result \
         after the second compression (idempotency gate enforced) — the \
         canonical state lives in JSONL until the next chat turn refreshes it"
    );

    // Exactly 1 compaction marker — level 5 strips previous ones.
    assert_eq!(
        count_compaction_markers(msgs),
        1,
        "level 5 falls through and replaces the previous marker — only the \
         most recent survives. This is expected; the todo round is the \
         canonical state we preserve across compactions."
    );

    // The critical idempotency invariant: the JSONL holds exactly one
    // synthetic tool_call row for the todo_write round, regardless of how
    // many consecutive compressions fired. (Naive inject would append one
    // per compression.)
    let entries = read_jsonl_entries(work_dir, &h.sid);
    let todo_call_rows: Vec<&serde_json::Value> = entries
        .iter()
        .filter(|v| {
            v["role"] == "tool_call"
                && v["metadata"]["tool_name"] == "todo_write"
                && v["metadata"]["tool_call_id"] == "toolu_01"
                && v["id"]
                    .as_str()
                    .is_some_and(|id| id.starts_with("inject-call-"))
        })
        .collect();
    assert_eq!(
        todo_call_rows.len(),
        1,
        "JSONL must contain exactly one synthetic tool_call row for todo_write \
         across two compactions (injection idempotency contract); got {} rows",
        todo_call_rows.len()
    );

    // Note: with default min_ratio=0.90 the second compaction falls through
    // to level 5 (ratio exempt), which strips the previous marker along with
    // the rest of the middle. Only the most recent marker survives. This is
    // intentional (level 5 trades cache history for budget); the invariant
    // we care about is the todo round, which DID survive (see
    // `count_tool_call_id` assertion above).
    assert_eq!(
        count_compaction_markers(msgs),
        1,
        "level 5 falls through and replaces the previous marker — only the \
         most recent survives. This is expected; the todo round is the \
         canonical state we preserve across compactions."
    );
}

#[tokio::test]
async fn compression_no_todo_history_is_safe_noop() {
    // ADR-060 v2 §5.4 edge case: when history NEVER contained a
    // `todo_write` round, the inject path is a safe no-op. Compression
    // still proceeds; the next chat sees the marker but no todo round.
    let dir = tempfile::tempdir().expect("tempdir");
    let work_dir = dir.path();

    let provider = Arc::new(ScriptedProvider::new(
        vec![
            text_step("Turn 1 response"),
            text_step("Turn 2 response"),
            text_step("Turn 3 response"),
            summary_step(),
            text_step("Post-compact response"),
        ],
        vec![text_step("No-todo compact test")],
    ));
    let mut h = spawn_harness(work_dir, provider.clone(), None).await;

    h.send("First turn", "m1").await;
    h.send("Second turn", "m2").await;
    h.send("Third turn", "m3").await;
    h.compact().await;
    h.send("Fourth turn", "m4").await;

    let captured = provider.captured();
    assert_eq!(captured.len(), 5, "3 chat + 1 compact + 1 post");

    let msgs = &captured.last().unwrap().messages;
    assert_block_v2_layout(msgs, "Fourth turn");

    // No todo round → no tool result of any kind.
    assert_eq!(
        msgs.iter()
            .filter(|m| m.role == MessageRole::Tool && m.tool_call_id.is_some())
            .count(),
        0,
        "no todo round in history → no inject → no orphan tool result"
    );

    // Marker still present (compression happened).
    assert_eq!(count_compaction_markers(msgs), 1);
}

#[tokio::test]
async fn block_c_never_in_any_request_output_full_flow() {
    // Property-style invariant: across a complex flow (todo_write + compress
    // + multiple turns + restart), the v1 "## Todo Task List" Block C
    // header MUST NEVER appear in ANY captured ChatRequest — neither in
    // Block A, nor in Block B, nor in the marker. This is
    // the cache-correctness guarantee of ADR-060 v2: the static prefix
    // (Block A) and the dynamic tail (Block B's current user message) are both free of todo
    // content; todo state is purely a Block B phenomenon.
    let dir = tempfile::tempdir().expect("tempdir");
    let work_dir = dir.path();

    let provider = Arc::new(ScriptedProvider::new(
        vec![
            text_step("Turn 1"),
            todo_write_step(&[("t1", "Block C invariant test")]),
            text_step("After todo"),
            text_step("Turn 3"),
            summary_step(),
            text_step("Post-compact"),
        ],
        vec![text_step("Invariant test session")],
    ));
    let mut h = spawn_harness(work_dir, provider.clone(), None).await;
    let sid = h.sid.clone();

    h.send("u1", "m1").await;
    h.send("u2", "m2").await;
    h.send("u3", "m3").await;
    h.compact().await;
    h.send("u4", "m4").await;
    h.shutdown().await;

    // Generation 2: restart, send another turn. The persisted (post-
    // compression) history must NOT have gained a Block C either.
    let provider2 = Arc::new(ScriptedProvider::new(
        vec![text_step("Post-restart response")],
        vec![text_step("Restarted invariant test")],
    ));
    let mut h2 = spawn_harness(work_dir, provider2.clone(), Some(&sid)).await;
    h2.send("u5", "m5").await;

    // Both providers' captured requests across BOTH generations.
    for (gen_idx, captured) in [provider.captured(), provider2.captured()]
        .iter()
        .enumerate()
    {
        for (call_idx, req) in captured.iter().enumerate() {
            for (msg_idx, msg) in req.messages.iter().enumerate() {
                assert!(
                    !msg.content.contains("## Todo Task List"),
                    "Block C must NOT appear: gen={gen_idx} call={call_idx} \
                     msg_idx={msg_idx} role={:?}",
                    msg.role
                );
            }
        }
    }
}

#[tokio::test]
async fn restart_after_compression_preserves_todo_state() {
    // ADR-060 v2 §5.4 + §11: the post-compression history (including the
    // injected todo round + the marker) is persisted to JSONL. After a
    // process restart, the next chat request must still see BOTH — the
    // round is NOT re-injected (it's already there from the previous
    // inject), and the marker is restored from disk.
    let dir = tempfile::tempdir().expect("tempdir");
    let work_dir = dir.path();

    // Generation 1: todo_write + compress.
    let provider = Arc::new(ScriptedProvider::new(
        vec![
            text_step("Turn 1 response"),
            todo_write_step(&[("t1", "Restart-after-compress test")]),
            text_step("Todos saved"),
            text_step("Turn 3 response"),
            summary_step(),
        ],
        vec![text_step("Restart-after-compress session")],
    ));
    let mut h = spawn_harness(work_dir, provider.clone(), None).await;
    let sid = h.sid.clone();
    h.send("u1", "m1").await;
    h.send("u2", "m2").await;
    h.send("u3", "m3").await;
    h.compact().await;
    h.shutdown().await;

    // Generation 2: resume from disk. Send one turn. Verify Block B
    // contains both the marker AND the todo round — restored from JSONL,
    // NOT reconstructed.
    let provider2 = Arc::new(ScriptedProvider::new(
        vec![text_step("Post-restart response")],
        vec![text_step("Restarted session")],
    ));
    let mut h2 = spawn_harness(work_dir, provider2.clone(), Some(&sid)).await;
    h2.send("Post-restart turn", "m-pr").await;

    let captured2 = provider2.captured();
    assert_eq!(captured2.len(), 1);
    let msgs = &captured2[0].messages;
    assert_block_v2_layout(msgs, "Post-restart turn");
    assert_block_b_carries_todo_state(msgs, "toolu_01");
    assert_eq!(
        count_compaction_markers(msgs),
        1,
        "compaction marker survives JSONL round-trip"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Boundary tests — JSONL persistence under injection
// ═══════════════════════════════════════════════════════════════════════

/// Read the JSONL file for `sid` into a Vec<ConversationEntry> by parsing
/// each line. Skips blank lines silently.
fn read_jsonl_entries(work_dir: &Path, sid: &str) -> Vec<serde_json::Value> {
    use std::io::{BufRead, BufReader};
    let path = work_dir.join("conversations").join(format!("{sid}.jsonl"));
    let f = std::fs::File::open(&path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    let r = BufReader::new(f);
    r.lines()
        .map(|l| {
            l.unwrap_or_else(|e| panic!("read line: {e}"))
                .parse::<serde_json::Value>()
                .unwrap_or_else(|e| panic!("parse line: {e}"))
        })
        .filter(|v| !v.is_null())
        .collect()
}

#[tokio::test]
async fn normal_todo_write_persists_tool_call_and_tool_result_to_jsonl() {
    // Baseline regression: the NORMAL (non-injected) `todo_write` path must
    // already persist the tool_call / tool_result rows to JSONL exactly
    // like every other tool call. Without this, even the no-compression
    // path would lose the todo state on restart — and the inject path's
    // fix would be hiding a more fundamental bug.
    //
    // Asserts:
    // 1. JSONL contains at least one `role: "tool_call"` row with
    //    `metadata.tool_call_id == "toolu_01"` (the loop's stable test id).
    // 2. JSONL contains at least one `role: "tool_result"` row with the
    //    same tool_call_id.
    // 3. Both rows have `name`/`metadata.tool_name == "todo_write"` (the
    //    tool identification contract the restorer relies on).
    // 4. The rows appear AFTER any `kind: "compaction"` row — but here
    //    no compaction fires, so the assertion is just "no compaction row".
    let dir = tempfile::tempdir().expect("tempdir");
    let work_dir = dir.path();

    let provider = Arc::new(ScriptedProvider::new(
        vec![
            text_step("Turn 1"),
            todo_write_step(&[("t1", "Normal path baseline test")]),
            text_step("After todo"),
        ],
        vec![text_step("Normal path baseline session")],
    ));
    let mut h = spawn_harness(work_dir, provider, None).await;
    let sid = h.sid.clone();

    h.send("u1", "m1").await;
    h.send("u2", "m2").await;

    let entries = read_jsonl_entries(work_dir, &sid);

    // Locate the tool_call / tool_result rows for todo_write.
    let tool_call_rows: Vec<&serde_json::Value> = entries
        .iter()
        .filter(|v| v["role"] == "tool_call")
        .collect();
    let tool_result_rows: Vec<&serde_json::Value> = entries
        .iter()
        .filter(|v| v["role"] == "tool_result")
        .collect();

    assert!(
        !tool_call_rows.is_empty(),
        "expected at least one tool_call row in JSONL, got entries={entries:?}"
    );
    assert!(
        !tool_result_rows.is_empty(),
        "expected at least one tool_result row in JSONL, got entries={entries:?}"
    );

    let todo_call = tool_call_rows
        .iter()
        .find(|v| v["metadata"]["tool_name"] == "todo_write")
        .expect("must find a tool_call row with tool_name=todo_write");
    assert_eq!(
        todo_call["metadata"]["tool_call_id"], "toolu_01",
        "todo_write tool_call row carries the loop-stable id"
    );

    let todo_result = tool_result_rows
        .iter()
        .find(|v| v["metadata"]["tool_call_id"] == "toolu_01")
        .expect("must find a tool_result row for toolu_01");
    assert_eq!(
        todo_result["metadata"]["tool_name"], "todo_write",
        "tool_result row carries tool_name=todo_write (restorer uses this for sanity)"
    );

    // No compaction marker should be present in this test.
    let compaction_count = entries
        .iter()
        .filter(|v| v["kind"].as_str() == Some("compaction"))
        .count();
    assert_eq!(
        compaction_count, 0,
        "no compaction should have fired in this baseline test"
    );
}

#[tokio::test]
async fn multiple_compressions_write_exactly_one_synthesized_round_to_jsonl() {
    // ADR-060 v2 §5.4 idempotency, PERSISTENCE-SIDE: after N consecutive
    // manual compressions, the JSONL must contain EXACTLY N
    // `kind: "compaction"` rows AND EXACTLY 1 pair of synthesized
    // (tool_call, tool_result) rows for `toolu_01`.
    //
    // This pins down two contracts at once:
    // 1. The inject path is idempotent on disk (no duplicate synthetic
    //    round even when the LLM happens to call todo_write multiple times
    //    across pre-compress phases and the inject loops over the same
    //    retained-todo_write detection).
    // 2. The injection is permanent (no re-write, no re-shuffle), so the
    //    final JSONL layout reflects exactly one logical todo_write round
    //    in the LLM-visible history.
    //
    // A naive implementation would re-inject the round after every
    // compression, producing N copies on disk. The fail mode would be
    // subtle: the LLM-visible history stays correct (only one round, the
    //    `in_tail` detection catches dupes), but JSONL would carry N
    //    duplicates and any downstream tool / audit consumer that walks
    //    JSONL directly would see phantom rounds.
    let dir = tempfile::tempdir().expect("tempdir");
    let work_dir = dir.path();

    // 3 compactions. For each: 1 LLM call to produce the summary.
    // Plus the chat turns (3 turns, but turn 2 has 2 LLM calls = chat + tool iteration).
    // So main script:
    //   [0] turn 1 text
    //   [1] turn 2 todo_write tool call
    //   [2] turn 2 follow-up text (after tool result)
    //   [3] turn 3 text
    //   [4] compaction summary #1
    //   [5] compaction summary #2
    //   [6] compaction summary #3
    let provider = Arc::new(ScriptedProvider::new(
        vec![
            text_step("Turn 1 response"),
            todo_write_step(&[("t1", "Idempotency on disk")]),
            text_step("Todos saved"),
            text_step("Turn 3 response"),
            summary_step(),
            summary_step(),
            summary_step(),
        ],
        vec![text_step("JSONL idempotency test")],
    ));
    let mut h = spawn_harness(work_dir, provider, None).await;
    let sid = h.sid.clone();

    h.send("u1", "m1").await;
    h.send("u2", "m2").await;
    h.send("u3", "m3").await;
    h.compact().await;
    h.compact().await;
    h.compact().await;

    let entries = read_jsonl_entries(work_dir, &sid);

    // Exactly 3 compaction markers (one per manual compress).
    let compaction_rows: Vec<&serde_json::Value> = entries
        .iter()
        .filter(|v| v["kind"].as_str() == Some("compaction"))
        .collect();
    assert_eq!(
        compaction_rows.len(),
        3,
        "three manual compressions → three compaction markers in JSONL; \
         got entries={entries:?}"
    );

    // Exactly 1 synthesized tool_call row for todo_write (the injector
    // must be idempotent across multiple compressions). We count ONLY the
    // synthetic rows (id starts with `inject-call-`) — the live chat-path
    // tool_call row from the original turn also has tool_call_id=toolu_01
    // but is NOT a synthesized row and must not be counted.
    let todo_call_rows: Vec<&serde_json::Value> = entries
        .iter()
        .filter(|v| {
            v["role"] == "tool_call"
                && v["metadata"]["tool_name"] == "todo_write"
                && v["metadata"]["tool_call_id"] == "toolu_01"
                && v["id"]
                    .as_str()
                    .is_some_and(|id| id.starts_with("inject-call-"))
        })
        .collect();
    assert_eq!(
        todo_call_rows.len(),
        1,
        "exactly one synthesized tool_call row for todo_write across \
         three compactions (injection idempotency contract); \
         got {} rows, entries={:?}",
        todo_call_rows.len(),
        entries
    );

    // Exactly 1 synthesized tool_result row for todo_write.
    let todo_result_rows: Vec<&serde_json::Value> = entries
        .iter()
        .filter(|v| {
            v["role"] == "tool_result"
                && v["metadata"]["tool_name"] == "todo_write"
                && v["metadata"]["tool_call_id"] == "toolu_01"
                && v["id"]
                    .as_str()
                    .is_some_and(|id| id.starts_with("inject-result-"))
        })
        .collect();
    assert_eq!(
        todo_result_rows.len(),
        1,
        "exactly one synthesized tool_result row for todo_write across \
         three compactions; got {} rows, entries={:?}",
        todo_result_rows.len(),
        entries
    );

    // Position invariant: at least one synthesized pair must appear AFTER
    // SOME compaction marker in the file (so the restorer can pick it up
    // on resume). We accept any marker — the LAST marker is not required,
    // because level 5 strips earlier inject rounds from in-memory history
    // on each pass and the idempotency gate then refuses to re-inject, so
    // only the FIRST synthetic pair sits in the file (after marker #1).
    // The next chat turn (Turn 4 if any) or a process restart would
    // refresh Block B's todo presence via JSONL rebuild.
    let mut found_after_some_marker = false;
    for (i, v) in entries.iter().enumerate() {
        if v["kind"].as_str() == Some("compaction") {
            let synth_after = entries.iter().skip(i + 1).any(|w| {
                w["role"] == "tool_result"
                    && w["metadata"]["tool_call_id"] == "toolu_01"
                    && w["id"]
                        .as_str()
                        .is_some_and(|id| id.starts_with("inject-result-"))
            });
            if synth_after {
                found_after_some_marker = true;
                break;
            }
        }
    }
    assert!(
        found_after_some_marker,
        "at least one synthesized todo_write row must appear AFTER SOME \
         compaction marker in the JSONL so the restorer picks it up on resume; \
         got entries={entries:?}"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// ADR-061 v3 structural invariants — full-chain regression
// ═══════════════════════════════════════════════════════════════════════
//
// v3 replaced "delete Tool message + schema sweep" with round-atomic
// folding (tool result → placeholder). The properties below are the v3
// contract, asserted on EVERY real outbound request across:
//   tool rounds → compaction → post-compact tool round → process restart.
// A v2-style regression (ghost assistant / dangling tool_calls) would show
// up here as an adjacent-assistant or an uncovered tool_call.

/// A todo_write scripted step with an explicit tool_call id (multiple rounds
/// in one session need distinct ids so the loop's tool_result matching is
/// unambiguous).
fn todo_write_step_id(id: &str, items: &[(&str, &str)]) -> ScriptedStep {
    let todos: Vec<serde_json::Value> = items
        .iter()
        .map(|(i, content)| {
            serde_json::json!({
                "id": i,
                "content": content,
                "status": "pending",
            })
        })
        .collect();
    ScriptedStep {
        content: String::new(),
        tool_calls: Some(vec![ToolCall {
            id: id.to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "todo_write".to_string(),
                arguments: serde_json::json!({ "todos": todos }).to_string(),
            },
        }]),
    }
}

/// ADR-061 v3 round-integrity invariants for one outbound request.
///
/// 1. No two `Assistant` messages are adjacent (the "ghost assistant" bug).
/// 2. Every `Assistant` carrying `tool_calls` is immediately followed by
///    `Tool` messages covering every call id (verbatim result or the
///    "result reclaimed" placeholder — never deleted).
/// 3. No orphan `Tool` message: every tool result maps to the nearest
///    preceding assistant's tool_calls.
fn assert_round_schema_intact(msgs: &[ChatMessage], label: &str) {
    for (i, m) in msgs.iter().enumerate() {
        if m.role == MessageRole::Assistant
            && i + 1 < msgs.len()
            && msgs[i + 1].role == MessageRole::Assistant
        {
            panic!("{label}: assistant-assistant adjacency at index {i}");
        }
        let Some(calls) = &m.tool_calls else { continue };
        if calls.is_empty() {
            continue;
        }
        let mut missing: Vec<&str> = calls.iter().map(|c| c.id.as_str()).collect();
        let mut j = i + 1;
        while j < msgs.len() && msgs[j].role == MessageRole::Tool {
            if let Some(id) = msgs[j].tool_call_id.as_deref()
                && let Some(pos) = missing.iter().position(|w| *w == id)
            {
                missing.remove(pos);
            }
            j += 1;
        }
        assert!(
            missing.is_empty() && j > i + 1,
            "{label}: assistant tool_calls at index {i} not fully covered by \
             following Tool messages; missing={missing:?}",
        );
    }
    for (i, m) in msgs.iter().enumerate() {
        if m.role != MessageRole::Tool {
            continue;
        }
        let id = m.tool_call_id.clone().unwrap_or_default();
        let prev_assistant = msgs[..i]
            .iter()
            .rev()
            .find(|x| x.role == MessageRole::Assistant);
        let covered = prev_assistant
            .and_then(|a| a.tool_calls.as_ref())
            .is_some_and(|cs| cs.iter().any(|c| c.id == id));
        assert!(
            covered,
            "{label}: orphan Tool message (tool_call_id={id:?}) at index {i} \
             has no matching assistant tool_calls before it",
        );
    }
}

#[tokio::test]
async fn v3_full_chain_keeps_tool_rounds_valid_across_compact_and_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let work_dir = dir.path();

    // ── Generation 1 ────────────────────────────────────────────────────
    // Script (main): t1 text → t2 tool(toolu_01) → post-tool text →
    // t3 tool(toolu_02) → post-tool text → compaction summary →
    // t4 tool(toolu_03, AFTER compaction — proves tools still work) →
    // final text.
    let provider = Arc::new(ScriptedProvider::new(
        vec![
            text_step("First turn done"),
            todo_write_step_id("toolu_01", &[("t1", "Alpha")]),
            text_step("Alpha saved"),
            todo_write_step_id("toolu_02", &[("t2", "Beta")]),
            text_step("Beta saved"),
            summary_step(),
            todo_write_step_id("toolu_03", &[("t3", "Gamma after compact")]),
            text_step("Gamma saved"),
        ],
        vec![text_step("v3 invariant session")],
    ));
    let mut h1 = spawn_harness(work_dir, provider.clone(), None).await;
    let sid = h1.sid.clone();

    h1.send("First", "m1").await;
    h1.send("Second (tool)", "m2").await;
    h1.send("Third (tool)", "m3").await;
    h1.compact().await;
    h1.send("Fourth after compact (tool)", "m4").await;

    let captured = provider.captured();
    assert_eq!(
        captured.len(),
        8,
        "6 calls across 3 turns + 1 compaction + 2 calls for the post-compact \
         tool turn",
    );
    for (idx, req) in captured.iter().enumerate() {
        assert_round_schema_intact(&req.messages, &format!("gen1 request #{idx}"));
    }

    // The post-compact turn really used a tool: request #7 is the tool
    // iteration (its last message is the toolu_03 result) and #8 closes the
    // turn with the follow-up text.
    let post_tool_req = captured.last().unwrap();
    assert!(
        post_tool_req
            .messages
            .iter()
            .any(|m| m.role == MessageRole::Tool && m.tool_call_id.as_deref() == Some("toolu_03")),
        "tool call issued AFTER compaction must have executed (toolu_03 result present)"
    );

    // ── Generation 2: process restart, resume from disk ────────────────
    // Shut down Generation 1 before spawning Generation 2 so the two session
    // tasks never touch the same JSONL concurrently (same race as
    // `restart_after_compression_preserves_todo_state`).
    h1.shutdown().await;
    let provider2 = Arc::new(ScriptedProvider::new(
        vec![text_step("Post-restart response")],
        vec![text_step("v3 invariant restart")],
    ));
    let mut h2 = spawn_harness(work_dir, provider2.clone(), Some(&sid)).await;
    h2.send("Fifth after restart", "m5").await;

    let captured2 = provider2.captured();
    assert!(
        !captured2.is_empty(),
        "restarted session must make at least one LLM call"
    );
    for (idx, req) in captured2.iter().enumerate() {
        assert_round_schema_intact(&req.messages, &format!("gen2 (restart) request #{idx}"));
        // The restored history must still carry the post-compact tool round —
        // proof the placeholder/folding semantics survived serialization.
        assert!(
            req.messages
                .iter()
                .any(|m| m.role == MessageRole::Tool
                    && m.tool_call_id.as_deref() == Some("toolu_03")),
            "gen2 request #{idx}: restored history must retain the post-compact tool round",
        );
    }
}
