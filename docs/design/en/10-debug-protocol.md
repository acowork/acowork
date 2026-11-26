# Debug Protocol

> Version: v3.2 | Last Updated: 2026-04-16

---

The Debug Protocol is the communication specification between Desktop App's developer mode and Bot Runtime (DevMode). It enables step debugging, record/replay, message editing, and Skill hot-reload for Bot Runtime.

> **Post-ADR-033 Note**: The Debug Protocol previously used WebSocket as the transport. Following ADR-033 (MQTT replacing gRPC/WebSocket), the Debug Protocol has been refactored to use **HTTP RPC + MQTT events**: control plane commands (DebuggerAttach, Step, SetBreakpoint etc.) go through HTTP RPC, real-time events (iteration_started, tool_call, etc.) go through MQTT pub/sub.

```
┌──────────────────────────────────────────────────────────────┐
│  Desktop App (Developer Mode)                                 │
│                                                              │
│  ┌────────────────────────────────────────────────────┐     │
│  │  Debug Panel                                        │     │
│  │  ├─ Step / Resume / Stop controls                  │     │
│  │  ├─ Breakpoint list                                 │     │
│  │  ├─ Provider switcher                               │     │
│  │  └─ Recording control                               │     │
│  └────────────────────────────────────────────────────┘     │
│          │                                                    │
│          │ debug_rpc (HTTP POST, request/response)           │
│          │ debug-event (MQTT topic subscription)             │
└──────────┼──────────────────────────────────────────────────┘
           │
           ▼
┌──────────────────────────────────────────────────────────────┐
│  Agent Runtime (DevMode)                                      │
│                                                              │
│  ┌────────────────────────────────────────────────────┐     │
│  │  Debug Controller                                  │     │
│  │  ├─ Main loop under debugger control               │     │
│  │  ├─ Step / breakpoint / resume logic               │     │
│  │  ├─ Message edit + re-execute                      │     │
│  │  ├─ Skill hot-reload                               │     │
│  │  └─ Recording / replay                             │     │
│  └────────────────────────────────────────────────────┘     │
└──────────────────────────────────────────────────────────────┘
```

## 1. Activation Flow

### 1.1 Prerequisites

- Bot Runtime must be started with `--dev-mode` flag
- `--debug-port` (default 19878) opens HTTP RPC server
- Runtime subscribes to debug-related MQTT topics, publishes debug events

### 1.2 Attach Flow

```
1. User toggles Developer Mode in Desktop App
       │
       ▼
2. Desktop App checks if current Agent is running in DevMode
   ├─ Already DevMode → attach directly
   └─ Not DevMode → notify Gateway to restart in DevMode
       │
       ▼
3. Desktop App POSTs debug_rpc: DebuggerAttach { agent_id, dev_session_id }
       │
       ▼
4. Runtime receives, returns RuntimeInfo { supported_features, breakpoints, recording_status }
       │
       ▼
5. Desktop App subscribes debug-event topics:
   acowork/debug/{agent_id}/+/event
       │
       ▼
6. From now on, Runtime publishes iteration_started, chunk, tool_call, etc. via MQTT
   Desktop App renders debug panel UI
```

## 2. Transport Layer

### 2.1 HTTP RPC (Control Plane)

Synchronous request/response, used for debugger commands:

```
POST http://127.0.0.1:{debug-port}/debug/rpc
Content-Type: application/json
Body: { "method": "DebuggerStep", "params": { ... } }
```

Response:
```json
{
  "result": { ... },
  "error": null
}
```

Or:
```json
{
  "result": null,
  "error": { "code": "...", "message": "..." }
}
```

### 2.2 MQTT Events (Real-Time Push)

Runtime publishes debug events to MQTT topics:

```
acowork/debug/{agent_id}/iteration/event      → iteration lifecycle events
acowork/debug/{agent_id}/breakpoint/event     → breakpoint hit events
acowork/debug/{agent_id}/recording/event      → recording status events
acowork/debug/{agent_id}/variable/event       → variable inspection events
```

Event payload:
```json
{
  "seq": 1,
  "timestamp": "2026-07-12T10:30:00Z",
  "type": "iteration_started",
  "data": { ... }
}
```

## 3. RPC Method Definitions

### 3.1 DebuggerAttach

Attach debugger to Bot Runtime.

**Request**:
```json
{
  "method": "DebuggerAttach",
  "params": {
    "agent_id": "com.example.weather",
    "dev_session_id": "dev-uuid-001"
  }
}
```

**Response**:
```json
{
  "result": {
    "runtime_version": "0.1.0",
    "supported_features": [
      "step",
      "breakpoint",
      "message_edit",
      "skill_reload",
      "recording"
    ],
    "current_iteration": 5,
    "breakpoints": [],
    "recording_status": "idle"
  }
}
```

### 3.2 DebuggerDetach

Detach debugger, Bot Runtime resumes normal execution.

**Request**:
```json
{
  "method": "DebuggerDetach",
  "params": {
    "agent_id": "com.example.weather"
  }
}
```

**Response**:
```json
{
  "result": { "status": "detached" }
}
```

### 3.3 DebuggerStep

Single-step execution: execute one iteration step.

**Request**:
```json
{
  "method": "DebuggerStep",
  "params": {
    "agent_id": "com.example.weather",
    "step_type": "into"  // "into" | "over" | "out"
  }
}
```

**Response**:
```json
{
  "result": { "iteration": 6, "phase": "context_built" }
}
```

### 3.4 DebuggerResume

Resume normal execution until next breakpoint or end.

**Request**:
```json
{
  "method": "DebuggerResume",
  "params": {
    "agent_id": "com.example.weather"
  }
}
```

### 3.5 DebuggerPause

Pause currently executing Bot.

**Request**:
```json
{
  "method": "DebuggerPause",
  "params": {
    "agent_id": "com.example.weather"
  }
}
```

### 3.6 SetBreakpoint

Set a breakpoint.

**Request**:
```json
{
  "method": "SetBreakpoint",
  "params": {
    "agent_id": "com.example.weather",
    "breakpoint": {
      "type": "iteration",      // "iteration" | "tool_call" | "phase"
      "value": 10,              // iteration number, tool name, or phase name
      "condition": null         // optional condition expression
    }
  }
}
```

**Response**:
```json
{
  "result": { "breakpoint_id": "bp-001" }
}
```

### 3.7 RemoveBreakpoint

Remove a breakpoint.

**Request**:
```json
{
  "method": "RemoveBreakpoint",
  "params": {
    "agent_id": "com.example.weather",
    "breakpoint_id": "bp-001"
  }
}
```

### 3.8 EditMessage

Edit a historical message, then re-execute from that message.

**Request**:
```json
{
  "method": "EditMessage",
  "params": {
    "agent_id": "com.example.weather",
    "message_id": "msg-005",
    "new_content": "What's the weather in Shanghai tomorrow?"
  }
}
```

**Response**:
```json
{
  "result": { "re_execute_from": "msg-005" }
}
```

### 3.9 DebuggerReloadSkills

Hot-reload Skills (after editing SKILL.md).

**Request**:
```json
{
  "method": "DebuggerReloadSkills",
  "params": {
    "agent_id": "com.example.weather"
  }
}
```

**Response**:
```json
{
  "result": {
    "reloaded_skills": ["weather-query", "news-digest"],
    "failed_skills": []
  }
}
```

### 3.10 StartRecording

Start recording conversation (for later replay).

**Request**:
```json
{
  "method": "StartRecording",
  "params": {
    "agent_id": "com.example.weather",
    "session_id": "sess-001",
    "recording_name": "demo-recording-001"
  }
}
```

### 3.11 StopRecording

Stop recording.

**Request**:
```json
{
  "method": "StopRecording",
  "params": {
    "agent_id": "com.example.weather"
  }
}
```

### 3.12 LoadRecording

Load and replay a recording.

**Request**:
```json
{
  "method": "LoadRecording",
  "params": {
    "agent_id": "com.example.weather",
    "recording_name": "demo-recording-001"
  }
}
```

## 4. MQTT Event Types

### 4.1 Iteration Lifecycle Events

Published to `acowork/debug/{agent_id}/iteration/event`:

```json
{
  "type": "iteration_started",
  "data": {
    "iteration": 5,
    "phase": "pre_check"  // pre_check | context_built | llm_call | tool_dispatch | record
  }
}

{
  "type": "iteration_completed",
  "data": {
    "iteration": 5,
    "duration_ms": 1234,
    "tokens_used": 5678
  }
}

{
  "type": "iteration_paused",  // hit breakpoint
  "data": {
    "iteration": 5,
    "phase": "tool_dispatch",
    "reason": "breakpoint"
  }
}
```

### 4.2 Tool Call Events

```json
{
  "type": "tool_call_started",
  "data": {
    "iteration": 5,
    "call_id": "call-001",
    "tool_name": "http_request",
    "arguments": { ... }
  }
}

{
  "type": "tool_call_completed",
  "data": {
    "iteration": 5,
    "call_id": "call-001",
    "tool_name": "http_request",
    "result": { ... },
    "duration_ms": 234,
    "is_error": false
  }
}
```

### 4.3 LLM Call Events

```json
{
  "type": "llm_request",
  "data": {
    "iteration": 5,
    "provider": "openai",
    "model": "gpt-4o",
    "prompt_tokens": 1234,
    "messages_count": 8
  }
}

{
  "type": "llm_response",
  "data": {
    "iteration": 5,
    "finish_reason": "tool_calls",
    "completion_tokens": 567,
    "tool_calls_count": 1
  }
}
```

### 4.4 Breakpoint Events

```json
{
  "type": "breakpoint_hit",
  "data": {
    "breakpoint_id": "bp-001",
    "iteration": 10,
    "phase": "tool_dispatch"
  }
}
```

## 5. Main Loop Integration

When Debug Mode is active, the Bot Runtime main loop is intercepted at key phases:

```
Main loop step ⓪ (message merge)
  │
  ▼
[Pre-check] If breakpoint on iteration=N, pause here
  │
  ▼
Step ① budget pre-check
  │
  ▼
Step ② context build
  │
  ▼
[Pre-check] If breakpoint on phase="context_built", pause here
  │
  ▼
Step ②.5 context compression (if triggered)
  │
  ▼
Step ③ LLM call
  │
  ▼
[Pre-check] If breakpoint on phase="llm_call", pause here
  │
  ▼
Step ④ parse response
  │
  ▼
Step ④.5 tool call dedup
  │
  ▼
Step ⑤ tool dispatch
  │
  ▼
[Pre-check] If breakpoint on tool_call, pause before execution
  │
  ▼
Step ⑥ append to history
  │
  ▼
Step ⑦ usage report
  │
  ▼
Step ⑧ loop detection
  │
  ▼
Step ⑨ iteration count check
  │
  └─→ Back to ⓪
```

When paused:
- Runtime publishes `iteration_paused` event via MQTT
- Waits for `DebuggerResume` or `DebuggerStep` command
- After receiving command, continues execution

## 6. Message Edit and Re-execute

Developer Mode supports editing historical messages and re-executing from that point:

```
User: Original message "What's the weather in Beijing today?"
       │
       ▼ Runtime: processes, gets response
       │
User (Developer Mode): Edit message to "What's the weather in Shanghai tomorrow?"
       │
       ▼
1. Desktop App: EditMessage RPC { message_id: "msg-005", new_content: "..." }
       │
       ▼
2. Runtime: EditMessage handler
   ├─ Truncate History after msg-005
   ├─ Replace msg-005 with new content
   ├─ Reset subsequent state (tool results, token counts)
   └─ Trigger new iteration from msg-005
       │
       ▼
3. Runtime: continues main loop from msg-005, re-executes LLM call and tool calls
       │
       ▼
4. New response generated, written to History
```

**Use cases:**
- Test different prompts on same message
- Adjust user intent and observe Bot's response change
- A/B test different tool selection strategies

## 7. Skill Hot-Reload

After developer edits SKILL.md, they can hot-reload Skills without restarting Bot:

```
Developer: Edit skills/weather-query/SKILL.md
       │
       ▼
Desktop App: Click "Reload to Runtime"
       │
       ▼
Desktop App: DebuggerReloadSkills RPC
       │
       ▼
Runtime: ReloadSkills handler
   ├─ Re-read all SKILL.md files from disk
   ├─ Re-parse YAML frontmatter + Markdown body
   ├─ Update Skill Loader cache
   ├─ Invalidate Skill Experience association
   └─ Optionally: Re-load SkillExperience from SQLite memory layer
       │
       ▼
Runtime: Returns reloaded skills list
       │
       ▼
Developer: Click "Test in Chat"
       │
       ▼
Desktop App: Sends test trigger message via normal conversation flow
       │
       ▼
Runtime: Uses newly loaded Skills
```

## 8. Recording and Replay

### 8.1 Recording

When recording starts, Runtime captures all conversation events:

```
Recording file format (.arec):
{
    "recording_name": "demo-recording-001",
    "agent_id": "com.example.weather",
    "session_id": "sess-001",
    "started_at": "2026-07-12T10:00:00Z",
    "events": [
        {
            "seq": 1,
            "timestamp": "...",
            "type": "user_message",
            "data": { "content": "..." }
        },
        {
            "seq": 2,
            "timestamp": "...",
            "type": "iteration_started",
            "data": { "iteration": 1, ... }
        },
        ...
        {
            "seq": 100,
            "timestamp": "...",
            "type": "done",
            "data": { ... }
        }
    ]
}
```

Recording storage location: `~/.local/share/agent-gateway/agents/{agent_id}/recordings/{name}.arec`

### 8.2 Replay

When replaying, Runtime:
1. Loads recording file
2. Re-emits each event with original timing (or accelerated)
3. New session created, but LLM/tool calls use mocked responses from recording
4. Developer Mode can step through replay as if live

**Use cases:**
- Reproduce reported bugs
- A/B test different Bot behavior on same input
- Demo / training material generation

## 9. Bot Clone

Clone runs through the Gateway HTTP API (invoked by Desktop App), not through the Debug Protocol.

### 9.1 Clone Request

```http
POST /api/agents/{instance_id}/clone
Content-Type: application/json

{
  "mode": "skeleton" | "full",
  "new_agent_id": "com.example.weather-dev"
}
```

`{instance_id}` is the source **instance** (ADR-073); `new_agent_id` is a **package** id (reverse-domain). The two identities are distinct and are never compared against each other.

### 9.2 Clone Flow

```
Desktop App → Gateway POST /api/agents/{instance_id}/clone
       │
       ▼
Gateway → Node control plane (clone is node-local, ADR-055 §6.6 L2-5):
  ├─ Read source install dir {packages_dir}/{agent_id}/{instance_id}/
  ├─ Copy per mode (see §9.3 for the boundary definition):
  │   ├─ skeleton: every package-level file
  │   │           (manifest.toml with agent_id rewritten, prompts/,
  │   │            skills/, assets/, …)
  │   │
  │   └─ full: skeleton + the instance's workspace state:
  │       conversations/ (current session JSONL snapshot)
  │       memory/private.sqlite (snapshot, incl. -wal/-shm sidecars)
  │
  ├─ Write to {packages_dir}/{new_agent_id}/{new_instance_id}/
  ├─ Mark the clone dev: true
  └─ Return the clone result
```

### 9.3 Skeleton / Full Boundary

One criterion decides: **package static content vs. per-instance runtime state.**

| Mode | Contents | Rationale |
|---|---|---|
| **skeleton** | Every **package-level file** — `manifest.toml` (with `agent_id` rewritten), `prompts/`, `skills/`, `assets/`, `tools/`, `resources/`… | Present at install time; independent of the user |
| **full** | skeleton **+ instance state under `workspace/`**: `workspace/config/` (user-curated model picks, MCP servers, custom workspace dirs in `agent_workspaces.json`), `workspace/conversations/`, `workspace/memory/` (incl. `-wal`/`-shm`) | Produced by running; the trace of what this bot has seen |

Two constraints:

1. **Neither list is hard-coded.** Package-level exclusion is driven by `PACKAGE_ALWAYS_EXCLUDE_DIRS` (`memory`/`workspace`/`runtime`); workspace-level exclusion is driven by `WORKSPACE_LOCAL_DIRS` (`files`/`logs`). Every other entry is copied. An allowlist rots as the format evolves — an earlier `["prompts","config","tools","resources"]` package list dropped `skills/`/`assets/` from skeleton, and an earlier `(conversations, memory)` workspace list dropped `config/` from full.
2. **Instance state lives under `workspace/`, not at package level.** They are siblings, not parent/child: `{instance}/prompts/` is package content while `{instance}/workspace/conversations/` is state. Conflating them makes full mode copy from a path that never exists — silently shipping a bot with no history.

`workspace/files/` (host-specific attachments) and `workspace/logs/` (runtime artefacts) stay behind even in `full` — they are machine-local. `workspace/config/` ships with `full` because the user spent the afternoon setting it up and a clone should not force them to redo it.

### 9.4 Clone Constraints

- The clone is independent of its source; later source updates do not sync.
- A full clone's SQLite snapshot is a point-in-time copy; afterwards the two evolve separately.

## 10. Publish Wizard Integration

Developer Mode's Publish Wizard uses Debug Protocol to validate Bot before publishing:

```
Step 1: Check — manifest integrity, SKILL.md format, prompts presence
  └─ RPC: ValidatePackage { package_path }

Step 2: Clean — remove dev tag, clear recordings/, reset config/
  └─ RPC: PreparePublish { remove_dev: true, clear_recordings: true, reset_config: true }

Step 3: Package — generate .agent ZIP
  └─ Local file operation

Step 4: Sign — call acowork-sign to sign
  └─ Local command invocation

Step 5: Distribute — local install / export / upload
  ├─ Local install: POST /api/agents/install
  ├─ Export: save to file
  └─ Upload: POST to repository
```

## 11. Cross-references

| Document | Relationship |
|----------|-------------|
| [03-agent-runtime.md](./03-agent-runtime.md) | Runtime main loop structure (which DevMode intercepts) |
| [06-communication.md](./06-communication.md) | IPC layer (DevMode uses HTTP + MQTT) |
| [14-desktop-app.md](./14-desktop-app.md) | Desktop App developer mode UI (calls these RPCs) |
| [13-skill-system.md](./13-skill-system.md) | Skill hot-reload targets |
| [02-agent-package.md](./02-agent-package.md) | Publish wizard output format |