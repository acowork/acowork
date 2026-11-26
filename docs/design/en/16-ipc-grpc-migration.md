# IPC gRPC Migration (Historical)

> Version: v1.0 | Last Updated: 2026-04-15
> **Status: Historical Document** — Records v3.1's gRPC migration journey. v3.2 has further migrated to MQTT + HTTP reverse proxy per [ADR-033](../adr/zh/ADR-033-mqtt-replace-grpc-websocket.md); see [06-communication.md](./06-communication.md).

---

## 0. Background

This document records the v3.0 → v3.1 IPC migration, from Socket API (Unix Socket / Named Pipe / Local TCP) to gRPC bidirectional stream. Subsequent v3.1 → v3.2 migration (gRPC → MQTT + HTTP reverse proxy) is recorded in [ADR-033](../adr/zh/ADR-033-mqtt-replace-grpc-websocket.md) and reflected in [06-communication.md](./06-communication.md).

This document is retained as historical reference, explaining why Socket API was abandoned and what problems gRPC tried to solve.

## 1. v3.0 Socket API Issues

### 1.1 Original Design

In v3.0 and earlier, Gateway ↔ Bot Runtime IPC used Socket API:
- **Linux**: Unix Domain Socket (e.g. `/tmp/gateway-{agent_id}.sock`)
- **Windows**: Named Pipe (e.g. `\\.\pipe\acowork-{agent_id}`)
- **macOS**: Unix Domain Socket (same as Linux)

Each message type required a separate Socket connection:
- AgentHello → hello.sock
- UserMessage → user.sock
- ToolCall → tool.sock
- ... (10+ message types, 10+ Sockets)

### 1.2 Problems Exposed

After v3.0 running for some time, we found:

| Problem | Impact | Severity |
|---------|--------|----------|
| Too many Sockets | Resource consumption, fd limit risk | Medium |
| No multiplexing | Each message type needs separate connection, low efficiency | Medium |
| Manual message framing | Each message type needs custom serialization/deserialization | High |
| No schema validation | Message structure errors only discovered at runtime | High |
| No automatic reconnection | Bot Runtime restart requires Gateway actively re-establish all Sockets | Medium |
| Cross-platform inconsistency | Linux/Windows/macOS implementations differ, code complexity | Medium |
| Cross-language clients | CLI / Desktop App (TypeScript) cannot directly call, need HTTP bridge | High |
| No streaming | Large data transmission needs custom chunking | Medium |
| No backpressure | Sender doesn't know receiver's processing speed, easy to OOM | Medium |
| Poor debugging | tcpdump can see bytes but can't parse semantics | Medium |

Most critical issues:
- **No schema validation**: New message field added, old code may silently ignore, leading to subtle bugs
- **Cross-language clients**: CLI / Desktop App needs HTTP bridge, increasing architecture complexity

## 2. Why gRPC Was Chosen (v3.1)

### 2.1 gRPC Advantages

| Dimension | gRPC | Socket API | Improvement |
|-----------|------|------------|-------------|
| Schema definition | .proto file, strong type | No schema, runtime errors | Strong typing |
| Multiplexing | Single HTTP/2 connection, multiple streams | One Socket per message type | Single connection |
| Code generation | protoc auto-generates Rust/TS/Python etc. | Handwritten serialization | Auto-generated |
| Streaming | Bidirectional stream native | Custom chunking | Native support |
| Backpressure | Flow control via HTTP/2 | Manual | Native |
| Cross-language | gRPC ecosystem mature | None | Desktop App direct call |
| Reconnection | gRPC auto-reconnect | Manual | Automatic |
| Debugging | grpcurl, envoy | tcpdump | Semantic tool support |

### 2.2 Why Not Other Options

| Option | Pros | Cons | Why Not |
|--------|------|------|---------|
| HTTP REST | Simple, ubiquitous | Request/response only, no bidirectional push | Can't push real-time events |
| WebSocket | Bidirectional, browser-friendly | Need custom protocol design | gRPC has stronger schema |
| ZeroMQ | High performance | No schema, no codegen | Same problem as Socket |
| MessagePack-RPC | Compact | Small ecosystem | gRPC ecosystem more mature |
| Cap'n Proto RPC | Zero-copy | Smaller community | gRPC is industry standard |

gRPC chosen for: schema validation + cross-language codegen + bidirectional stream + mature ecosystem, these four points together solve Socket API's core issues.

## 3. v3.1 gRPC Architecture

### 3.1 Proto Definition

Gateway ↔ Runtime IPC proto:

```protobuf
syntax = "proto3";
package acowork.ipc.v1;

// === Lifecycle ===
message AgentHelloRequest {
    string agent_id = 1;
    string package_path = 2;
    string work_dir = 3;
    string runtime_version = 4;
}

message AgentHelloResult {
    repeated ProviderConfig providers = 1;
    repeated McpServerConfig mcp_servers = 2;
    VaultKeyReleases keys = 4;
    IdentityDelivery identity_delivery = 5;       // v3.1
    CapabilityOverview capability_overview = 6;
}

// === Identity delivery (v3.1) ===
message IdentityDelivery {
    repeated IdentityEntry entries = 1;
}

message IdentityEntry {
    string key = 1;        // "display_name" / "language" / "city"
    string value = 2;
    float confidence = 3;
    string source = 4;     // "user_input" / "conversation_inferred"
}

// === Conversation ===
message ChatMessage {
    string session_id = 1;
    string message_id = 2;
    string content = 3;
}

// === Streaming events ===
message StreamEvent {
    oneof event {
        ChunkEvent chunk = 1;
        ToolCallEvent tool_call = 2;
        ToolResultEvent tool_result = 3;
        DoneEvent done = 4;
        ErrorEvent error = 5;
    }
}

message ChunkEvent {
    string message_id = 1;
    string session_id = 2;
    string delta = 3;
}

// === gRPC service definition ===
service AgentRuntimeService {
    // Lifecycle
    rpc AgentHello(AgentHelloRequest) returns (AgentHelloResult);

    // Conversation (bidirectional stream)
    rpc Conversation(stream ClientMessage) returns (stream ServerMessage);

    // Streaming events
    rpc SubscribeEvents(SubscribeRequest) returns (stream StreamEvent);
}
```

### 3.2 Connection Flow

```
1. Agent Runtime starts, connects to Gateway gRPC endpoint (127.0.0.1:19877)
       │
       ▼
2. Runtime sends AgentHelloRequest
       │
       ▼
3. Gateway returns AgentHelloResult
   (providers, mcp_servers, search_providers, keys, identity_delivery, capability_overview)
       │
       ▼
4. Runtime establishes bidirectional stream: Conversation()
       │
       ├─ Client → Server: ChatMessage (user messages)
       ├─ Server → Client: StreamEvent (chunks, tool calls, done)
       │
       ▼
5. Agent Runtime is online, can serve conversations
```

### 3.3 Identity Delivery

v3.1 introduced IdentityDelivery in AgentHelloResult, used to deliver user identity (display name, language, city etc.):

```rust
// Gateway side
fn build_identity_delivery(agent: &RunningAgent) -> IdentityDelivery {
    let deps = agent.manifest.identity_deps.as_ref().cloned().unwrap_or_default();
    let profile = load_user_profile();

    let entries: Vec<IdentityEntry> = deps.iter()
        .filter_map(|dep| {
            profile.get_field(dep).map(|value| IdentityEntry {
                key: dep.clone(),
                value,
                confidence: 1.0,
                source: "user_input".to_string(),
            })
        })
        .collect();

    IdentityDelivery { entries }
}
```

**Issue**: This mechanism requires Gateway to know each Bot's `identity_deps`, then deliver corresponding fields. Adds per-Agent configuration burden.

## 4. Problems Found in v3.1 (Leading to v3.2)

After v3.1 running for some time, new issues emerged:

### 4.1 Bidirectional Stream Complexity

gRPC bidirectional stream theoretically supports multiplexing, but in practice:
- Client/Server must manually track stream state
- Reconnection logic complex (need to re-establish stream, re-deliver state)
- When multiple Runtime instances connect to same Gateway, each needs independent stream

### 4.2 HTTP/2 Connection Limit

Browser and some HTTP/2 clients have connection limit (usually 6 per origin). Desktop App connecting to Gateway and Runtime simultaneously is fine, but when Runtime scales, HTTP/2 connection management becomes complex.

### 4.3 Identity Delivery Coupling

`identity_deps` requires Bot to declare required fields in manifest, increasing manifest complexity. Many Bots declare same fields, manifest maintainability drops.

### 4.4 No Native Pub/Sub

gRPC streams are point-to-point; cross-Agent broadcast needs custom implementation. MQTT's native pub/sub is more natural.

### 4.5 Code Generation Overhead

gRPC requires protoc + plugin + cargo build each proto change, iteration speed affected.

## 5. v3.2 Migration Decision (Pointer to ADR-033)

In v3.2, based on v3.1 issues, decided to migrate to **MQTT pub/sub + HTTP reverse proxy**:

| Dimension | gRPC (v3.1) | MQTT + HTTP (v3.2) |
|-----------|-------------|---------------------|
| Multiplexing | Manual | Topic-based natural multiplex |
| Pub/Sub | None | Native |
| Browser support | Need grpc-web | MQTT.js via WebSocket |
| Reconnection | Manual | Auto (broker handles) |
| State tracking | Complex | Will + Retained auto-handle |
| Code generation | Required | Optional (JSON or protobuf) |

Detailed migration rationale see [ADR-033](../adr/zh/ADR-033-mqtt-replace-grpc-websocket.md); new architecture details see [06-communication.md](./06-communication.md).

## 6. v3.1 → v3.2 Compatibility

v3.1 gRPC API and v3.2 MQTT API are not directly compatible. Migration strategy:

1. v3.2 introduces MQTT + HTTP architecture
2. v3.1 gRPC API marked deprecated, retains 6-month compatibility window
3. Runtime compiled with v3.2 supports both gRPC (compatible) and MQTT (preferred)
4. After 6 months, v3.1 gRPC code removed

Detailed deprecation timeline see [ADR-033 §6](../adr/zh/ADR-033-mqtt-replace-grpc-websocket.md#6-deprecation-timeline).

## 7. Lessons Learned

v3.0 → v3.1 → v3.2 evolution taught us:

1. **IPC is not just transport protocol, it's also API design**. Socket API's pain isn't just transport, but also lack of schema and codegen.
2. **Schema validation value underestimated**. gRPC solved this but introduced complexity; MQTT + protobuf payload strikes a balance.
3. **Pub/Sub semantics matter**. When cross-Agent broadcast and multi-consumer scenarios emerge, point-to-point streams become insufficient.
4. **Web ecosystem is first-class citizen**. Desktop App in browser/WebView, MQTT's WebSocket support is more natural than gRPC-Web.
5. **Don't over-engineer**. v3.0 Socket API was "simple enough" at design time, but didn't anticipate Desktop App and cross-Agent scenarios. v3.1 gRPC was "fully featured" at design time, but pub/sub and browser support insufficient. Iteration is normal; don't seek one-time perfect design.

## 8. Cross-references

| Document | Relationship |
|----------|-------------|
| [06-communication.md](./06-communication.md) | Current v3.2 MQTT + HTTP architecture |
| [ADR-033](../adr/zh/ADR-033-mqtt-replace-grpc-websocket.md) | v3.1 → v3.2 migration decision |
| [ADR-034](../adr/zh/ADR-034-mqtt-http-boundary.md) | MQTT/HTTP boundary decision |
| [18-user-identity-simplified.md](./18-user-identity-simplified.md) | Identity management simplification (replaces v3.1 IdentityDelivery) |