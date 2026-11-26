# Communication Protocol Design

> Version: v3.2 | Last Updated: 2026-07-12
> Major revision in this version: §1 fully aligned with [ADR-033](./../adr/zh/ADR-033-mqtt-replace-grpc-websocket.md) — Gateway ↔ Runtime IPC replaced with **MQTT pub/sub + HTTP reverse proxy**. gRPC (v3.1) and historical Socket API (≤v3.0) are no longer the main paths. §2 cross-Bot Intent routing reuses the same MQTT topic tree. §3 Desktop App / CLI access layer changed from gRPC WebSocket to MQTT + HTTP API.

---

ACowork's communication protocol covers three communication paths:

1. **Gateway ↔ Bot Runtime IPC** — MQTT pub/sub (real-time events) + HTTP reverse proxy (large data queries)
2. **Cross-Bot Intent routing** — Reuses the same MQTT topic tree, with Gateway as router
3. **Desktop App / CLI ↔ Gateway access layer** — HTTP API (REST) + MQTT subscription (streaming events)

```
┌─────────────────────────────────────────────────────────────────────┐
│                                                                     │
│   ┌─────────────┐                                          ┌─────────────┐  │
│   │   Agent     │  ◀──── MQTT pub/sub (events) ──────▶    │             │  │
│   │   Runtime   │  ◀──── HTTP reverse proxy (queries) ──▶ │   Gateway   │  │
│   │   (per proc)│                                          │  (single)   │  │
│   └─────────────┘                                          └──────┬──────┘  │
│                                                                  │         │
│                                                                  │ MQTT pub/sub
│                                                                  │ + HTTP API
│                                                                  ▼         │
│                                                          ┌─────────────┐  │
│                                                          │ Desktop App │  │
│                                                          │   / CLI     │  │
│                                                          └─────────────┘  │
│                                                                     │
│   ┌─────────────┐         ┌─────────────┐                          │
│   │   Agent A   │ ──────▶│   Gateway   │ ──────▶ Agent B            │
│   │  (Intent)   │  MQTT  │  (router)   │  MQTT   (target)            │
│   └─────────────┘         └─────────────┘                          │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
```

## 1. Gateway ↔ Bot Runtime IPC (MQTT + HTTP Reverse Proxy)

Post-ADR-033 IPC is unified as **MQTT pub/sub + HTTP reverse proxy**:

| Channel | Use Case | Direction |
|---------|----------|-----------|
| MQTT | Real-time events (chat chunk, tool_call, done, identity_update, capability_update, device status Will+Retained), Key distribution (AgentHello), Intent triggering | Bidirectional |
| HTTP reverse proxy | Session history, message queries, config writeback, large payload forwarding | Gateway → Runtime |

Runtime itself does not expose external ports. Runtime's localhost HTTP (default `--http-port`, configurable) is only used by Gateway as reverse proxy target.

### 1.1 Why MQTT (vs Historical gRPC / Socket)

| Dimension | MQTT | gRPC (v3.1) | Socket (≤v3.0) |
|-----------|------|-------------|-----------------|
| Multiplexing | Topic pub/sub (many-to-many, natural multiplex) | Bidirectional stream (manual multiplexing) | One connection per message type |
| Push semantics | Native pub/sub (broker fanout) | Stream frames (manual routing) | None (pull only) |
| Client impl complexity | High-level client library | Bidirectional stream proto + manual ack | Manual message framing |
| Cross-language clients | Multiple mature clients (mqtt.js, paho, rumqtt) | gRPC ecosystem | None (each impl custom) |
| Broker | Embedded rumqttd (zero external deps) | Need tonic etc. | None |
| Browser support | mqtt.js via WebSocket | Need grpc-web proxy | None |
| Failure detection | LWT (Last Will and Testament) | TCP keepalive | TCP keepalive |

The key advantage is **multiplexing**: multiple Runtime instances connect to the same broker via the same TCP port, distinguished by topics. gRPC bidirectional streams require manual multiplexing, and Socket mode requires multiple connections per message type — both have higher operational complexity.

### 1.2 MQTT Topic Tree

All Runtime↔Gateway traffic goes through a unified topic tree:

```
acowork/
├── agents/{agent_id}/
│   ├── sessions/{session_id}/
│   │   ├── messages/+
│   │   │   ├── chunk             # LLM streaming chunk
│   │   │   ├── tool_call         # tool_call event
│   │   │   ├── tool_result       # tool_result event
│   │   │   ├── done              # session turn done
│   │   │   └── error             # error event
│   │   ├── inbound/+             # Gateway → Runtime inbound messages
│   │   │   ├── chat_message      # user message
│   │   │   ├── interrupt         # interrupt
│   │   │   └── system_notify     # system notification (identity_update etc.)
│   │   └── status                # session status (Retained)
│   ├── lifecycle/+
│   │   ├── hello                 # AgentHello
│   │   ├── ready                 # AgentHelloResult
│   │   ├── goodbye               # Agent exits
│   │   └── config_update         # RuntimeConfigUpdate
│   └── status                    # Agent status (Retained)
└── system/
    ├── intents/+                 # Intent routing (cross-Bot)
    │   ├── send
    │   └── received
    └── approvals/+               # Approval requests
        ├── request
        └── response
```

**Topic design rationale:**

- `agents/{agent_id}/sessions/{session_id}/messages/+` — Session-level message stream, Gateway pushes to Desktop App via fanout
- `agents/{agent_id}/sessions/{session_id}/inbound/+` — Gateway → Runtime reverse control channel (chat message, interrupt, system notification)
- `agents/{agent_id}/lifecycle/+` — Bot lifecycle events (hello, ready, goodbye, config_update)
- `agents/{agent_id}/status` — Bot status (Retained message, new subscribers get latest status immediately)
- `system/intents/+` — Cross-Bot Intent routing
- `system/approvals/+` — User approval flow (Gateway ↔ Runtime ↔ Desktop App)

### 1.3 MQTT Payload Format

Each MQTT message carries a typed payload. The `core/acowork-core/proto/mqtt_payload.proto` file defines all payload types (independent protobuf namespace from `gateway_ipc.proto`):

```protobuf
syntax = "proto3";
package acowork.mqtt;

// === Chat message events (Runtime → Gateway → Desktop App) ===
message ChatChunk {
    string message_id = 1;
    string session_id = 2;
    string delta = 3;               // streaming text delta
    uint64 seq = 4;                 // sequence number (for reordering)
}

message ToolCallEvent {
    string message_id = 1;
    string session_id = 2;
    string call_id = 3;
    string tool_name = 4;
    string arguments_json = 5;       // JSON serialized
}

message ToolResultEvent {
    string message_id = 1;
    string session_id = 2;
    string call_id = 3;
    string tool_name = 4;
    string result_json = 5;          // JSON serialized
    bool is_error = 6;
}

message DoneEvent {
    string message_id = 1;
    string session_id = 2;
    uint32 total_tokens = 3;
    uint32 prompt_tokens = 4;
    uint32 completion_tokens = 5;
    string finish_reason = 6;       // "stop" / "length" / "tool_calls"
    ModelUsage usage = 7;
}

// === Inbound (Gateway → Runtime) ===
message ChatMessageInbound {
    string session_id = 1;
    string user_message_id = 2;
    string content = 3;
    repeated ContentPart attachments = 4;
}

message InterruptInbound {
    string session_id = 1;
    string reason = 2;
}

message SystemNotifyInbound {
    string session_id = 1;
    oneof notify {
        IdentityUpdate identity_update = 10;
        CapabilityUpdate capability_update = 11;
        UserProfileUpdate user_profile_update = 12;
    }
}

// === Lifecycle (AgentHello, AgentHelloResult, etc.) ===
message AgentHelloPayload {
    string agent_id = 1;
    string package_path = 2;
    string work_dir = 3;
    string runtime_version = 4;
    uint64 provider_list_version = 5;
    uint64 mcp_list_version = 6;
    uint64 user_profile_version = 7;  // NEW: see 18-user-identity-simplified.md
}

message AgentHelloResultPayload {
    UserProfile user_identity = 1;     // NEW: see 18-user-identity-simplified.md
    uint64 user_profile_version = 2;
    repeated ProviderConfig providers = 10;
    uint64 provider_list_version = 11;
    repeated McpServerConfig mcp_servers = 20;
    uint64 mcp_list_version = 21;
    VaultKeyReleases keys = 40;       // encrypted key release
    CapabilityOverview capability_overview = 50;
}

message ConfigUpdatePayload {
    RuntimeConfig config = 1;
}

// === Intent routing (cross-Bot) ===
message IntentSendPayload {
    string from_agent = 1;
    string to_agent = 2;
    string action = 3;
    string payload_json = 4;          // arbitrary JSON
    string correlation_id = 5;
}

message IntentReceivedPayload {
    string from_agent = 1;
    string to_agent = 2;
    string action = 3;
    string payload_json = 4;
    string correlation_id = 5;
}

// === Approval flow ===
message ApprovalRequestPayload {
    string approval_id = 1;
    string agent_id = 2;
    string session_id = 3;
    string tool_name = 4;
    string arguments_json = 5;
    string reason = 6;
}

message ApprovalResponsePayload {
    string approval_id = 1;
    bool approved = 2;
    string reason = 3;                // user-supplied rejection reason
}

// === Shared types ===
message UserProfile {
    string user_id = 1;
    string display_name = 2;
    string language = 3;
    string timezone = 4;
    optional string city = 5;
    optional string occupation = 6;
    optional string communication_style = 7;
    bool is_active = 8;
}

message UserProfileUpdate {
    UserProfile user_identity = 1;
    uint64 version = 2;
}
```

### 1.4 Will and Retained Messages

MQTT's Will + Retained features are key to Gateway learning Runtime status:

**Will message** (configured at connect time, fires on abnormal disconnect):

```rust
// Runtime connects with Will topic acowork/agents/{agent_id}/lifecycle/goodbye
// Will payload:
{
    "agent_id": "com.example.weather",
    "reason": "abnormal_disconnect",
    "last_seen": "2026-07-12T10:30:00Z"
}
```

Gateway subscribes `acowork/agents/+/lifecycle/goodbye` — when a Runtime abnormally disconnects, broker fires the Will message, Gateway cleans up Bot state.

**Retained messages** (last value preserved by broker, new subscribers get immediately):

```rust
// Runtime periodically publishes its status to acowork/agents/{agent_id}/status (Retained)
// When Desktop App starts a new MQTT subscription, broker immediately pushes the latest status
{
    "agent_id": "com.example.weather",
    "status": "running",       // running | idle | error
    "pid": 12345,
    "active_sessions": 2,
    "last_heartbeat": "2026-07-12T10:30:00Z"
}
```

This solves the problem of "Desktop App just started, doesn't know current Bot status" — via Retained messages, new subscribers immediately know the latest state.

### 1.5 AgentHello Handshake (Identity, Key, Config Push)

After Runtime connects to MQTT broker, it publishes `AgentHello` to `acowork/agents/{agent_id}/lifecycle/hello`:

```
1. Runtime connects to MQTT broker (subscribes acowork/agents/{id}/lifecycle/+, acowork/agents/{id}/sessions/+/inbound/+)
2. Runtime publishes AgentHello to acowork/agents/{agent_id}/lifecycle/hello
3. Gateway receives AgentHello, checks version differences:
   ├─ provider_list_version differs → push provider_list
   ├─ mcp_list_version differs → push mcp_list
   ├─ user_profile_version differs → push user_identity
   └─ all match → only push CapabilityOverview
4. Gateway publishes AgentHelloResult to acowork/agents/{agent_id}/lifecycle/ready
   (contains user_identity, provider_list, mcp_list, encrypted keys, capability_overview)
5. Runtime receives AgentHelloResult, stores in AgentCore
```

Detailed handshake contract:

```protobuf
// AgentHelloRequest (Runtime → Gateway)
message AgentHelloRequest {
    string agent_id = 1;
    string package_path = 2;
    string work_dir = 3;
    string runtime_version = 4;
    uint64 provider_list_version = 5;   // Runtime's cached version
    uint64 mcp_list_version = 6;
    uint64 user_profile_version = 7;    // NEW: see 18-user-identity-simplified.md
}

// AgentHelloResult (Gateway → Runtime)
message AgentHelloResult {
    // Identity
    UserProfile user_identity = 31;     // NEW: only when user_profile_version differs
    uint64 user_profile_version = 32;

    // LLM providers (only when version differs)
    repeated ProviderConfig providers = 10;
    uint64 provider_list_version = 11;

    // MCP servers (only when version differs)
    repeated McpServerConfig mcp_servers = 20;
    uint64 mcp_list_version = 21;

    // Encrypted Key release (only when needed)
    VaultKeyReleases keys = 40;

    // Capability overview (always pushed)
    CapabilityOverview capability_overview = 50;
}
```

### 1.6 HTTP Reverse Proxy (Large Data Queries)

MQTT is not suitable for large data queries (session history, large config writes). For these scenarios, Gateway uses HTTP reverse proxy:

```
Desktop App (or other component)
   │
   ▼ HTTP GET /api/agents/:id/sessions/:sid/history
Gateway (HTTP server, 127.0.0.1:19876)
   │
   ▼ HTTP GET (localhost)
Runtime (HTTP server, --http-port, e.g. 127.0.0.1:19877)
   │
   ▼
Runtime's local History Manager queries Conversation JSONL file
```

Gateway acts as reverse proxy; Desktop App doesn't directly know Runtime's port (Runtime doesn't expose external ports). Desktop App only talks to Gateway, and Gateway internally forwards to Runtime's localhost HTTP.

**Use cases:**
- `GET /api/agents/:id/sessions` — list session history (large list)
- `GET /api/agents/:id/sessions/:sid/messages` — paginate messages within session
- `PUT /api/agents/:id/sessions/:sid/messages/:mid` — edit message (debug mode re-execute)
- `GET /api/agents/:id/config` — get Bot config
- `PUT /api/agents/:id/config` — update Bot config

### 1.7 Stream Event Flow (End-to-End)

Complete flow of user sending message and receiving response:

```
1. User sends message via Desktop App
   │
   ▼ Desktop App: POST /api/agents/:id/message
       { content: "What's the weather in Beijing today?" }
       │
       ▼ Gateway: receives HTTP request
       │
       ├─ Intent Router determines target Agent = com.example.weather
       ├─ If Agent not running → Lifecycle Manager starts it
       │
       ▼ Gateway: PUB acowork/agents/:id/sessions/:sid/inbound/chat_message
       payload: { session_id, user_message_id, content, attachments }
       │
       ▼ Runtime: receives chat_message via MQTT subscription
       │
       ├─ Append to InboundQueue
       ├─ Main loop step ⓪ drains, appends UserMessage to History
       │
       ▼ Runtime: enters main loop, calls LLM
       │
       ▼ Runtime: PUB acowork/agents/:id/sessions/:sid/messages/chunk
       payload: { message_id, session_id, delta: "Beijing ", seq: 1 }
       │
       ▼ Runtime: PUB acowork/agents/:id/sessions/:sid/messages/chunk
       payload: { message_id, session_id, delta: "today is ", seq: 2 }
       │
       ▼ ... multiple chunks ...
       │
       ▼ Runtime: PUB acowork/agents/:id/sessions/:sid/messages/tool_call
       payload: { message_id, session_id, call_id, tool_name: "http_request", arguments_json }
       │
       ▼ Runtime: executes tool, gets result
       │
       ▼ Runtime: PUB acowork/agents/:id/sessions/:sid/messages/tool_result
       payload: { message_id, session_id, call_id, tool_name, result_json, is_error: false }
       │
       ▼ Runtime: continues iteration, more chunks
       │
       ▼ Runtime: PUB acowork/agents/:id/sessions/:sid/messages/done
       payload: { message_id, session_id, total_tokens, finish_reason: "stop" }
       │
       ▼ Desktop App (subscribed to acowork/agents/:id/sessions/+/messages/+)
       receives all chunks, tool_call, tool_result, done via MQTT
       │
       └─ UI renders streaming output, displays tool calls
```

## 2. Cross-Bot Intent Routing

Intent is the mechanism for Bots to communicate. When Bot A needs to call Bot B's capability, it sends an Intent message via Gateway.

### 2.1 Intent Message Format

```protobuf
// Intent request (from agent's intent_send tool call)
message IntentSendPayload {
    string from_agent = 1;          // sender agent_id
    string to_agent = 2;            // target agent_id (or "*" for broadcast)
    string action = 3;              // capability name (must match target Agent's manifest.capabilities)
    string payload_json = 4;        // JSON parameters
    string correlation_id = 5;      // for matching response
}

// Intent response (target Agent's reply)
message IntentReceivedPayload {
    string from_agent = 1;          // actual processing agent
    string to_agent = 2;            // original requester
    string action = 3;              // echo of capability name
    string payload_json = 4;        // JSON result
    string correlation_id = 5;      // echo for correlation
}
```

### 2.2 Routing Flow

```
Agent A's LLM calls intent_send tool:
  arguments: { target: "com.example.calendar", action: "create_event", payload: {...} }

Runtime A (Tool Dispatcher)
       │
       ▼ PUB acowork/system/intents/send
       payload: IntentSendPayload { from: "com.example.weather", to: "com.example.calendar", ... }
       │
       ▼ Gateway (Intent Router subscribes acowork/system/intents/+)
       │
       ├─ Check target_agent exists and is running
       ├─ If not running → Lifecycle Manager starts it
       │
       ▼ Gateway: PUB acowork/agents/com.example.calendar/sessions/:sid/inbound/chat_message
       payload: { session_id, user_message_id, content: "[Intent from com.example.weather] create_event(...)" }
       │
       ▼ Runtime B (subscribed to acowork/agents/:id/sessions/+/inbound/+)
       receives, processes Intent
       │
       ▼ Runtime B: PUB acowork/system/intents/received
       payload: IntentReceivedPayload { from: "com.example.calendar", to: "com.example.weather", correlation_id, payload_json: result }
       │
       ▼ Gateway: PUB acowork/agents/com.example.weather/sessions/:sid/inbound/chat_message
       payload: { session_id, user_message_id, content: "[Intent response from com.example.calendar] {...}" }
       │
       ▼ Runtime A: receives response, continues main loop
```

### 2.3 Authorization

Intent send requires the sender Bot to declare permission `intent:send:<target_agent_id>`:

```toml
[[permissions]]
type = "IntentSend"
value = "com.example.calendar"
```

Gateway checks the permission table; if Bot A doesn't declare `intent:send:com.example.calendar`, the Intent is rejected and an error is returned.

## 3. Desktop App / CLI ↔ Gateway Access Layer

Desktop App and CLI access Gateway via HTTP API (REST), and subscribe to streaming events via MQTT:

| Channel | Use Case |
|---------|----------|
| HTTP API | All management operations (install, uninstall, start, stop, config, vault, etc.) |
| MQTT subscription | Streaming conversation events (chat chunk, tool_call, done) |

This is a "MQTT + HTTP API" dual-channel architecture — HTTP for management, MQTT for streaming.

### 3.1 HTTP API (REST)

See [04-gateway.md §9](./04-gateway.md) for complete HTTP API definitions.

### 3.2 MQTT Subscription (Streaming Events)

Desktop App subscribes to `acowork/agents/+/sessions/+/messages/+` to receive streaming conversation events of all Bots:

```javascript
// Desktop App frontend
const client = mqtt.connect('ws://127.0.0.1:19875');  // MQTT over WebSocket
client.subscribe('acowork/agents/+/sessions/+/messages/+');

client.on('message', (topic, payload) => {
    const event = JSON.parse(payload);
    const sessionId = topic.split('/')[3];
    const messageId = event.message_id;

    switch (event.type) {
        case 'chunk':
            appendToMessage(messageId, event.delta);
            break;
        case 'tool_call':
            displayToolCall(event.tool_name, JSON.parse(event.arguments_json));
            break;
        case 'tool_result':
            displayToolResult(event.tool_name, JSON.parse(event.result_json));
            break;
        case 'done':
            markMessageComplete(messageId, event.usage);
            break;
    }
});
```

### 3.3 Why Two Channels

| Scenario | HTTP API | MQTT |
|----------|----------|------|
| Install/uninstall Bot | ✅ One-shot request | ❌ Unsuitable |
| Send message | ✅ One-shot, returns immediately | ❌ Unsuitable |
| Receive streaming response | ❌ Unsuitable (HTTP request/response) | ✅ Pub/sub natural fit |
| Query session history | ✅ Pagination | ❌ Unsuitable |
| Update config | ✅ One-shot write | ❌ Unsuitable |
| Real-time status updates | ❌ Requires polling | ✅ Retained messages |

Two channels split by scenario characteristics: HTTP for one-shot operations, MQTT for streaming events. Both share Gateway internal state.

## 4. Protocol Version Compatibility

Protocol changes follow semver:

| Change Type | Compatibility | Handling |
|-------------|---------------|----------|
| Add new field | Backward compatible | Old version ignores unknown fields, new version reads with default |
| Modify field semantics | Incompatible | Requires version bump |
| Delete field | Backward compatible | Old version ignores missing fields, new version tolerates absence |
| Add new message type | Backward compatible | Old version ignores unknown message types |
| Modify topic structure | Incompatible | Requires version bump |

Each AgentHello carries Runtime's protocol version; Gateway refuses to connect Runtimes with incompatible versions.

## 5. Historical Protocols

| Version | Protocol | Status | Notes |
|---------|----------|--------|-------|
| ≤ v3.0 | Socket API (Unix Socket / Named Pipe / Local TCP) | Deprecated | See historical git history |
| v3.1 | gRPC bidirectional stream | Deprecated | See [16-ipc-grpc-migration.md](./16-ipc-grpc-migration.md) |
| v3.2 (current) | MQTT pub/sub + HTTP reverse proxy | **Current** | Per ADR-033 |

Migration path:

```
Socket API (≤v3.0)
   │
   ▼
gRPC bidirectional stream (v3.1, per 16-ipc-grpc-migration.md)
   │
   ▼
MQTT pub/sub + HTTP reverse proxy (v3.2, per ADR-033)
```

The current design (v3.2) is the target architecture; historical protocols are no longer the main path.

## 6. Cross-references

| Document | Relationship |
|----------|-------------|
| [03-agent-runtime.md](./03-agent-runtime.md) | Runtime internal structure and main loop |
| [04-gateway.md](./04-gateway.md) | Gateway HTTP API definitions (§9) |
| [15-conversation-persistence.md](./15-conversation-persistence.md) | Session persistence and Actor architecture |
| [18-user-identity-simplified.md](./18-user-identity-simplified.md) | User identity handshake (AgentHello.user_profile_version) |
| [ADR-033](../adr/zh/ADR-033-mqtt-replace-grpc-websocket.md) | Decision record for MQTT replacing gRPC |
| [ADR-034](../adr/zh/ADR-034-mqtt-http-boundary.md) | MQTT/HTTP boundary decision |