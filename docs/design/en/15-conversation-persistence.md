# Conversation Persistence (Session Actor Architecture)

> Version: v3.1 | Last Updated: 2026-04-15

---

This document defines how Bot Runtime persists conversation state, manages multi-session lifecycle, and implements Session Actor architecture. Conversation persistence is the foundation of "bot has memory across conversations" — without reliable persistence, the system cannot deliver coherent multi-turn dialogue.

## 1. Session Actor Architecture

### 1.1 Why Session Actor

Bot Runtime may simultaneously serve multiple sessions (e.g. user opens multiple chat windows, or multiple users share one Bot). Each session has independent:
- Conversation history
- Loop detection state
- Token usage
- Model override (user selects different model for specific session)
- Compaction state

Sharing state across sessions introduces complexity (locking, race conditions, state pollution). Therefore ACowork adopts the **Session Actor** pattern: each session is an independent async task (tokio task), with its own state and message loop, communicating with the outside world through `mpsc::channel`.

```
┌─────────────────────────────────────────────────────────┐
│  AgentCore (Arc, cross-session shared)                  │
│  ├─ Provider (LLM client)                              │
│  ├─ Tool Registry                                       │
│  ├─ Manifest                                            │
│  ├─ Budget Guard                                        │
│  ├─ Model Caps                                          │
│  └─ Agent Config                                        │
└─────────────────────────────────────────────────────────┘
         │
         │ Arc::clone
         ▼
┌──────────────────┐  ┌──────────────────┐  ┌──────────────────┐
│ SessionHandle #1 │  │ SessionHandle #2 │  │ SessionHandle #3 │
│ ├─ inbound_tx    │  │ ├─ inbound_tx    │  │ ├─ inbound_tx    │
│ ├─ task handle   │  │ ├─ task handle   │  │ ├─ task handle   │
│ └─ on_chunk      │  │ └─ on_chunk      │  │ └─ on_chunk      │
└──────────────────┘  └──────────────────┘  └──────────────────┘
         │                     │                     │
         ▼                     ▼                     ▼
┌──────────────────┐  ┌──────────────────┐  ┌──────────────────┐
│ SessionTask #1   │  │ SessionTask #2   │  │ SessionTask #3   │
│ (independent     │  │ (independent     │  │ (independent     │
│  tokio task)     │  │  tokio task)     │  │  tokio task)     │
│                  │  │                  │  │                  │
│ SessionState:    │  │ SessionState:    │  │ SessionState:    │
│ ├─ History Mgr   │  │ ├─ History Mgr   │  │ ├─ History Mgr   │
│ ├─ Loop Detector │  │ ├─ Loop Detector │  │ ├─ Loop Detector │
│ ├─ Model Override│  │ ├─ Model Override│  │ ├─ Model Override│
│ ├─ Conversation  │  │ ├─ Conversation  │  │ ├─ Conversation  │
│ └─ Token Usage   │  │ └─ Token Usage   │  │ └─ Token Usage   │
└──────────────────┘  └──────────────────┘  └──────────────────┘
```

### 1.2 SessionManager

```rust
pub struct SessionManager {
    sessions: Arc<RwLock<HashMap<SessionId, SessionHandle>>>,
}

pub struct SessionHandle {
    pub inbound_tx: mpsc::Sender<InboundMessage>,
    pub task: JoinHandle<()>,
    pub on_chunk: Box<dyn Fn(ChunkEvent) + Send + Sync>,
}

impl SessionManager {
    pub async fn create_session(&self, session_id: SessionId) -> Result<SessionHandle>;
    pub async fn destroy_session(&self, session_id: &SessionId) -> Result<()>;
    pub async fn send_inbound(&self, session_id: &SessionId, msg: InboundMessage) -> Result<()>;
    pub async fn list_sessions(&self) -> Vec<SessionInfo>;
}
```

### 1.3 SessionTask Main Loop

```rust
async fn session_task_loop(
    session_id: SessionId,
    mut inbound_rx: mpsc::Receiver<InboundMessage>,
    state: Arc<RwLock<SessionState>>,
    core: Arc<AgentCore>,
) {
    while let Some(msg) = inbound_rx.recv().await {
        match msg {
            InboundMessage::UserMessage(content) => {
                state.write().await.history.append_user_message(content);
                run_one_iteration(&state, &core, &session_id).await;
            }
            InboundMessage::SystemNotification(notify) => {
                state.write().await.history.append_system_message(notify);
            }
            InboundMessage::IntentMessage(intent) => {
                state.write().await.history.append_intent(intent);
                run_one_iteration(&state, &core, &session_id).await;
            }
            InboundMessage::Stop => break,
        }
    }
}
```

### 1.4 Session Lifecycle

```
Created (inactive)
  │
  ▼ First UserMessage / IntentMessage arrives
Active (running main loop)
  │
  ├─ Idle timeout (configurable) → Suspended (state persists to disk)
  │
  ▼ New message arrives
Active (resumed, state restored from disk)
  │
  ▼ User explicitly closes / Garbage collected
Destroyed (state deleted or archived)
```

### 1.5 Cross-Session Shared Resources

Although each Session has independent state, certain resources must be shared:

- **LLM Provider** (AgentCore.Provider): All sessions share one Provider instance (maintains connection pool, rate limit, etc.)
- **Tool Registry** (AgentCore.Tools): Tools are stateless; multiple sessions can concurrently invoke same tool
- **SQLite Memory Layer**: Each session's History/Conversation writes to same SQLite, distinguished by session_id
- **Vault Keys**: Shared, distributed by Gateway once, all sessions use same Key

**Concurrency model**: `Arc<AgentCore>` shared via `Arc::clone`, no locking needed (AgentCore itself is immutable after initialization). Session state uses `Arc<RwLock<SessionState>>`, with reads more than writes.

### 1.6 Session Identification

Session ID is generated by Gateway (UUID v4), passed to Runtime during handshake or when creating new session:

```
SessionId = UUID v4 (128-bit)

Example: "sess-7f3a9b2c-1234-5678-9abc-def012345678"
```

Properties:
- Globally unique (collision probability < 10^-12)
- No semantic info (doesn't contain user_id, agent_id)
- Persistent (saved in Conversation JSONL file name)

### 1.7 Session State vs Bot State

| Dimension | Session State (per session) | Bot State (cross-session) |
|-----------|----------------------------|------------------------------|
| Conversation history | ✓ | ✗ |
| Loop detection counter | ✓ | ✗ |
| Model override | ✓ | ✗ |
| Token usage | ✓ | Aggregated to Bot |
| Compaction state | ✓ | ✗ |
| Tool definitions | ✗ | ✓ |
| Provider config | ✗ | ✓ |
| Manifest | ✗ | ✓ |
| Budget Guard | ✗ | ✓ |
| UserProfile | ✗ | ✓ |

**Boundary principle**: Session state is "private conversation context", Bot state is "Bot capability configuration". Two are strictly separated, avoiding cross-session state leakage.

### 1.8 Budget Allocation

Each session has independent token budget, but total Bot budget is constrained:

```rust
pub struct BudgetGuard {
    agent_total_limit: u64,           // Bot-level total limit
    session_default_limit: u64,       // default per session
    session_overrides: HashMap<SessionId, u64>,  // per-session override
}

impl BudgetGuard {
    pub fn check_session(&self, session_id: &SessionId, tokens_needed: u64) -> BudgetDecision {
        // 1. Check session limit
        let session_limit = self.session_overrides
            .get(session_id)
            .copied()
            .unwrap_or(self.session_default_limit);

        let session_used = self.get_session_used(session_id);
        if session_used + tokens_needed > session_limit {
            return BudgetDecision::Stop;
        }

        // 2. Check Agent total limit
        let agent_used: u64 = self.get_all_sessions_used();
        if agent_used + tokens_needed > self.agent_total_limit {
            return BudgetDecision::Stop;
        }

        BudgetDecision::Allow
    }
}
```

## 2. Conversation Persistence

### 2.1 JSONL Storage Format

Each session's conversation is persisted in JSONL (JSON Lines) format, one JSON object per line:

```
~/.local/share/agent-gateway/agents/{agent_id}/workspace/conversations/{session_id}.jsonl
```

Each line is a complete JSON object representing one message or event:

```jsonl
{"seq":1,"timestamp":"2026-07-12T10:00:00Z","type":"user_message","content":"What's the weather in Beijing today?"}
{"seq":2,"timestamp":"2026-07-12T10:00:01Z","type":"iteration_started","iteration":1}
{"seq":3,"timestamp":"2026-07-12T10:00:02Z","type":"llm_request","iteration":1,"provider":"openai","model":"gpt-4o","prompt_tokens":234}
{"seq":4,"timestamp":"2026-07-12T10:00:03Z","type":"chunk","message_id":"msg-001","delta":"Beijing "}
{"seq":5,"timestamp":"2026-07-12T10:00:03Z","type":"chunk","message_id":"msg-001","delta":"today is "}
{"seq":6,"timestamp":"2026-07-12T10:00:04Z","type":"tool_call","message_id":"msg-001","call_id":"call-001","tool_name":"http_request","arguments":{"url":"https://api.weather.com/..."}}
{"seq":7,"timestamp":"2026-07-12T10:00:05Z","type":"tool_result","message_id":"msg-001","call_id":"call-001","result":{"temp":25,"humidity":60}}
{"seq":8,"timestamp":"2026-07-12T10:00:06Z","type":"chunk","message_id":"msg-001","delta":"sunny, 25°C"}
{"seq":9,"timestamp":"2026-07-12T10:00:07Z","type":"done","message_id":"msg-001","finish_reason":"stop","usage":{"total_tokens":567,"prompt_tokens":234,"completion_tokens":333}}
{"seq":10,"timestamp":"2026-07-12T10:00:30Z","type":"user_message","content":"What about tomorrow?"}
...
```

### 2.2 Why JSONL

| Format | Pros | Cons |
|--------|------|------|
| JSONL | Append-only, atomic write per line, easy tail/replay | Need to parse full file for filtering |
| SQLite | Query-friendly, transactional | Overkill for simple log |
| Binary | Compact, fast | Not human-readable, hard to debug |
| Parquet | Columnar analytics | Overkill for single-session data |

JSONL chosen for:
- **Append-only**: New messages append to file end, no rewrite of historical content
- **Atomicity**: Single line write is atomic, no half-written messages
- **Debuggable**: `tail -f` to view real-time events, `cat` to view full history
- **Replayable**: Recording format is essentially JSONL superset (see [10-debug-protocol.md](./10-debug-protocol.md))

### 2.3 Write Strategy

```rust
impl ConversationWriter {
    pub async fn append(&self, event: ConversationEvent) -> Result<()> {
        let line = serde_json::to_string(&event)?;
        let mut file = self.file.lock().await;
        writeln!(file, "{}", line)?;
        file.flush().await?;  // Immediately flush, don't rely on OS buffer
        Ok(())
    }
}
```

**Flush strategy**: Each event immediately flushes to disk (fsync), avoiding Bot crash losing recent conversation. Trade-off is performance, but conversation persistence is correctness-critical.

**File rotation**: When single file exceeds 100 MB, auto-rotate to `{session_id}.{n}.jsonl`. Old files compressed and archived.

### 2.4 Read Strategy

When session resumes, read JSONL file and replay to reconstruct History:

```rust
impl ConversationReader {
    pub async fn load(&self, session_id: &SessionId) -> Result<Vec<ConversationEvent>> {
        let path = self.conversation_path(session_id);
        let file = tokio::fs::File::open(&path).await?;
        let reader = BufReader::new(file);
        let mut events = Vec::new();

        let mut lines = reader.lines();
        while let Some(line) = lines.next_line().await? {
            let event: ConversationEvent = serde_json::from_str(&line)?;
            events.push(event);
        }

        Ok(events)
    }
}
```

Reconstruct History from events:
- `user_message` → `History.push(UserMessage)`
- `chunk` → merge into current `AssistantMessage.content`
- `tool_call` → `History.push(ToolCall)`
- `tool_result` → `History.push(ToolResult)`
- `done` → mark `AssistantMessage.complete = true`

## 3. Episode Extraction

> **v3.9 Simplification (ADR-011)**: Episode extraction no longer per-round triggered. Only triggers when Compaction (80% context usage) or Session close.

### 3.1 Trigger Conditions

```
Episode extraction trigger:
├─ Compaction triggered (80% context usage) → Compact Model outputs summary
│   ├─ Summary writes to SQLite memory layer distilled Episode
│   ├─ Summary includes entities and triples (auto-extracted by Compact Model)
│   └─ is_compacted = true; reset on new user message
│
└─ Session closes (user closes / idle timeout / Agent shutdown) → Session close distillation
    ├─ If is_compacted = true: skip (already distilled by Compaction)
    └─ If is_compacted = false: trigger final distillation
        ├─ Compact Model outputs summary
        └─ Write to SQLite memory layer distilled Episode
```

### 3.2 Distillation vs Original Episode

```rust
enum EpisodeType {
    /// Original conversation fragment (raw record)
    Original,
    /// Compaction-distilled summary
    Distilled,
}

struct Episode {
    episode_id: String,
    episode_type: EpisodeType,
    content: String,
    source_session: String,
    source_iterations: Vec<u32>,  // original iteration range (Distilled type)
    entities: Vec<String>,        // Distilled type only
    triples: Vec<(String, String, String)>,  // Distilled type only
    created_at: DateTime,
    importance: f32,
    consolidated: bool,
}
```

Distilled Episode is the "experiential memory" of Bot, used for:
- Future retrieval (when user asks related questions)
- Cross-session memory continuity
- Long-term memory basis

### 3.3 Distillation Flow

```
Compaction trigger / Session close
       │
       ▼
Compact Model invocation
       │
       ├─ Input: Full conversation context (system + history + tool calls + tool results)
       │
       ├─ Output: structured result
       │   <summary>Natural language summary text...</summary>
       │   <entities>Entity1, Entity2, Entity3</entities>
       │   <triples>
       │   subject | predicate | object
       │   subject | predicate | object
       │   </triples>
       │
       ▼
Parse output, construct Distilled Episode
       │
       ├─ content = summary
       ├─ entities = [entity strings]
       ├─ triples = [(s, p, o)]
       ├─ episode_type = Distilled
       ├─ importance = LLM-scored (0.0-1.0)
       └─ source_session = current session_id
       │
       ▼
Generate embedding via EmbeddingProvider
       │
       ▼
Write Episode to SQLite memory layer (experiential layer)
       │
       ▼
Set is_compacted = true
```

## 4. Compaction vs Distillation

Per ADR-011, Compaction and Distillation are unified as single Compact Model call:

| Operation | Purpose | Output |
|-----------|---------|--------|
| Compaction | Replace memory middle section, free up context window | Summary text replaces Conversation middle section |
| Distillation | Write to experiential layer, persistent storage | Summary text writes to SQLite memory layer Episode |

Unified call's output simultaneously serves two needs:
- Compaction: `replace_middle_with_summary` (preserve system prompt + last 3 rounds, replace middle with summary)
- Distillation: write to SQLite memory layer

This avoids "Compaction call + Distillation call" duplicate LLM invocation, halves cost.

## 5. State Recovery

When Bot Runtime restarts (e.g. user explicitly restarts, configuration update triggers restart), Session state is recovered from disk:

```
Agent Runtime startup
       │
       ▼
1. Load AgentCore (Manifest, Tools, Provider config etc.) from package
       │
       ▼
2. Scan workspace/conversations/ directory, list all JSONL files
       │
       ▼
3. For each session_id.jsonl:
   ├─ Load file, parse all events
   ├─ Reconstruct History
   ├─ Create SessionHandle, register with SessionManager
   ├─ Session state = Suspended (waiting for new message)
   └─ Don't auto-resume, wait for explicit activation
       │
       ▼
4. Load SQLite memory layer (SkillExperience, KnowledgeNode, ProceduralNode, AutobiographicalNode etc.)
       │
       ▼
5. Agent Runtime ready
```

## 6. Cross-references

| Document | Relationship |
|----------|-------------|
| [03-agent-runtime.md](./03-agent-runtime.md) | Main loop structure (which Session Actor wraps) |
| [05-memory.md](./05-memory.md) | Memory layers (Session writes to experiential layer via Compaction) |
| [06-communication.md](./06-communication.md) | IPC layer (Session receives InboundMessage via MQTT) |
| [10-debug-protocol.md](./10-debug-protocol.md) | Recording format uses JSONL superset |
| [ADR-011](../adr/zh/ADR-011-compaction-as-distillation.md) | Compaction = Distillation unified call |