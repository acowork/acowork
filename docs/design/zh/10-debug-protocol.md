# Debug Protocol（调试协议）

> 版本：v3.1 | 更新日期：2026-04-14

---

Agent Runtime 的 DevMode 通过 Debug Protocol 与 Desktop App 通信。Debug Protocol 在 ADR-048 之后与生产 IPC 通道完全对齐：RPC 走 HTTP REST、事件走 MQTT pub/sub（与 ADR-033 的生产通道复用同一组传输），不再使用 JSON-RPC over WebSocket。

Desktop App 的开发者模式完全依赖本协议，UI 层设计见 [14-desktop-app.md](./14-desktop-app.md)。

## 1. 总览

```
Desktop App (Tauri)              Agent Runtime (DevMode)
       │                                │
       │  Debug RPC  (HTTP REST)        │
       │  POST/GET .../api/debug/{path} │
       │  Gateway 反代 → Runtime        │
       ├───────────────────────────────>│
       │                                │
       │  Debug Events (MQTT, QoS 0)    │
       │  acowork/agents/{id}/debug/    │
       │    events/{type}               │
       │<───────────────────────────────│
       │                                │
       │  同时，Agent Runtime 仍通过       │
       │  Gateway Service API 获取       │
       │  Key、收发 Intent 等            │
       │         │                       │
       │         ▼                       │
       │  ┌─────────────────┐           │
       │  │  Gateway        │           │
       │  │  (独立进程)      │           │
       │  └─────────────────┘           │
```

Desktop App 与 Agent Runtime 之间有两条独立通道：
- **Debug Protocol**（HTTP RPC + MQTT events，与生产 IPC 同构）：开发模式专用，控制执行流、编辑状态、热加载。RPC 走 Gateway `/api/agents/{id}/debug/{*rest}` 反代到 Runtime `/api/debug/{path}`；事件走 MQTT topic `acowork/agents/{id}/debug/events/{type}`
- **Gateway Service API**：生产通道，Agent 仍通过 Gateway 获取 Key、收发 Intent 等

### 1.1 DevMode 启动方式（v3.1 起新增运行时启用）

DevMode 现在支持**两种**激活方式：

1. **启动时激活**（`--dev-mode` 启动参数）
   - Gateway 在 spawn Agent 时传入 `--dev-mode` 标志，Runtime 在 Phase C 通过共享 helper `enable_debug_mode_and_fill_slot` 完成 per-session controller + MQTT event publisher + Debug service slot 的填充。
   - 启动后第一次 `/api/debug/*` 请求即有 service 响应。
   - 适用于：已知要调试的开发场景，签名/元数据 `dev_mode: true`、`debug_state: enabled`。

2. **运行时激活**（`POST /api/agents/{id}/debug/enable`，v3.1 新增）
   - 桌面侧：Desktop App 在 Debug 面板里检测到「Agent 不在调试模式」时显示 **「启用调试」按钮**（`ResultsPanel.tsx`，见 ADR-048 follow-up）。
   - 调用链：`enable_agent_debug`（Tauri command）→ Gateway `POST /api/agents/{id}/debug/enable`（透传）→ Runtime `POST /api/debug/enable` → `enable_debug_mode_and_fill_slot`。
   - 调用是**幂等**的：DevMode 已开启时第二次调用返回 `already_enabled: true`。
   - Gateway `proxy_debug_rpc` 钩子在 2xx 响应时同步把 `running_agents[id].debug_state` 翻为 `Enabled`，下次 `GET /api/agents` 即返回 `debug_state: "enabled"`，Desktop 的 `agentStore` 触发 React 重渲染并自动连接到 debug tab。
   - **不需要重启 Agent，不需要重启 Gateway**，不需要重启 Runtime HTTP server——同一进程的 SessionManager、HTTP server、MQTT client slot 全部沿用。
   - 适用于：Agent 已启动后才意识到需要调试，或用户希望零停机开启调试。

**已废弃**：`POST /api/agents/{id}/restart-debug`（完整进程重启进入调试模式）已被运行时启用替代。Gateway 端点仍保留可用以作操作逃生口，Desktop 端右键菜单「Restart in Debug」已移除（`AgentList.tsx`）。计划在下一个 minor 版本后完全下线——见 ADR-048 follow-up。

### 1.2 已存在 session 的注入路径与并发约束（v3.1.1 修复）

运行时激活不仅让 **Runtime** 进入 DevMode，还需要让**已存在**的 session（它们的 `AgentCore` 此刻可能正被 `agent_loop.run()` 阻塞）也切换到 DevMode observer。这条注入路径在 v3.1 引入运行时激活时存在一个并发缺陷，v3.1.1 已修复：

#### 注入的两种时序

| 时序 | 路径 | 结果 |
|------|------|------|
| **idle 状态**启用调试 | SessionTask 主循环空闲，`EnableDebugMode` 消息被立即消费（`session_task.rs` 的 `SessionMessage::EnableDebugMode` 分支 → `set_debug_mode`） | 快照正常更新 ✅ |
| **LLM 推理中途**启用调试 | SessionTask 主循环阻塞在 `agent_loop.run()`，`EnableDebugMode` 消息在 inbound 队列排队；必须靠 **bypass 通道**把 `DebugHandles` 直接写入共享槽位，由 AgentLoop 在**下一次 iteration 开头**拾取 | v3.1.1 起正常 ✅ |

#### Bug 根因（v3.1.1 修复）

`EnableDebugMode` 消息只能在 SessionTask 主循环空闲时被处理——而主循环在 LLM 推理期间长时间阻塞（一次 tool-call 推理常达数十秒到数分钟）。为覆盖该场景，`SessionManager::push_debug_mode_to_existing_sessions` 额外把 `DebugHandles` 写入 `session_handle.pending_debug_handles`（bypass 通道），并期望 AgentLoop 在 `execute_single_iteration` 开头通过 `check_pending_injection()` 拾取。

缺陷出在 **ADR-013 Observer Pipeline 迁移时**：`pending_debug_handles` 被从 `AgentCore` 移入 `DebugObserverSlot`，而 `DebugObserverSlot::set_pending_injection` 对 `Production` 变体是 **no-op**（observer.rs）。session 创建时 observer 几乎总是 `Production`，因此 `session_task.rs` 在 `SessionTask::new` 中绑定的 bypass 通道被**静默丢弃**——此后 AgentCore 从未持有该通道，mid-loop 写入的 `DebugHandles` 永远不可见，iteration 计数冻结，直到 `run()` 返回后消息才被消费。

> 这解释了「idle 启动正常、推理中启动不更新」的规律：idle 时消息立即可达，推理中只能靠（已失效的）bypass 通道。

#### 修复（v3.1.1）

让 `AgentCore` 在**自身字段**持有 bypass 通道（`pending_debug_handles`），与 observer 是否 `Production` 解耦：

```
SessionManager                        SessionTask (AgentLoop)
─────────────────                     ────────────────────────
push_debug_mode_to_existing_sessions
  └─ session_handle.pending_debug_handles  ← 同一 Arc ──  AgentCore.pending_debug_handles
       写入 DebugHandles ──────────────────────────────▶  execute_single_iteration 开头
                                                            take_pending_debug_handles()
                                                            → DebugObserverImpl::new → set_debug_mode
```

- `AgentCore::set_debug_pending_injection`：把通道同时转发给 observer slot（Dev 场景保留）**并**存入 AgentCore 字段（真正的持有者，对 `Production` 也有效）。
- `AgentCore::take_pending_debug_handles()`：`try_lock` + `take`，不阻塞 agent loop；锁被 writer 持有时返回 `None` 待下一 iteration 重试。
- **disable 安全性**：`disable_debug_mode` 会把 `session_handle.pending_debug_handles` 清为 `None`（session_manager.rs）——由于 AgentCore 与该 handle 共享**同一个 Arc**，disable 后任何在途 handle 都无法被后续 iteration 应用，与 teardown 语义严格一致。
- **clone 隔离**：`AgentCore::clone` 不携带 pending 通道（传 `None`）——它是 per-session 状态，由每个 `SessionTask::new` 重新绑定，模板/sibling session 不得继承。

> **架构说明（对 ADR-013 的偏离）**：ADR-013 的目标是把 `AgentCore` 从 6 个 debug 字段收敛为 1 个 `DebugObserverSlot`，`pending_debug_handles` 正是在那次迁移中被移入 observer slot 的字段之一。v3.1.1 发现该迁移在「已存在 session 的 mid-loop 注入」场景下不成立：bypass 通道必须在 DevMode 激活**之前**就已存在，而 observer slot 的 `Production` 变体恰恰在那个阶段丢弃通道（no-op）。因此把该通道放回 `AgentCore` 是**有意为之的最小例外**——替代方案（给 `Production` 变体挂数据）会破坏「Production = 零成本 no-op」的核心不变量，改动更侵入。ADR-013 的其余目标（observer 收敛、主循环钩子化、ContextBuilder 解耦）保持不变。

## 2. 传输层

| 平台 | RPC | 事件 | 说明 |
|------|-----|------|------|
| Linux | HTTP REST（Gateway 反代 → Runtime `/api/debug/*`） | MQTT（`localhost:19875` 共享 broker） | 跨平台一致 |
| macOS | 同上 | 同上 | 同上 |
| Windows | 同上 | 同上 | 同上 |

HTTP RPC 与 MQTT 共享 Gateway 现有端口（HTTP `:19876`、MQTT `:19875`）——Debug Protocol **不再占用独立端口**。Agent Runtime 以 DevMode 启动时，在 localhost HTTP server 上注册 `/api/debug/*` 路由并 spawn MQTT 调试事件 publisher。Desktop App 通过 Gateway 反代访问时，自动获得 `localhost-only` 网关 + ACL 的多用户隔离（详见 ADR-040 / ADR-048）。

## 3. 协议定义

Debug Protocol 由两类消息构成，分别走两条独立通道（ADR-048）：

### 3.0 通道与载荷

| 方向 | 通道 | 载荷格式 |
|------|------|----------|
| Desktop → Runtime（RPC） | HTTP REST（Gateway 反代 → Runtime `/api/debug/*`） | JSON request/response |
| Runtime → Desktop（事件） | MQTT `acowork/agents/{id}/debug/events/{type}`（QoS 0） | Protobuf（`DebugStepEvent` / `DebugContextBuiltEvent` / `DebugStateChangeEvent`，定义见 `acowork-core/src/protocol.rs`） |

RPC 请求是普通的 HTTP 调用（`method` / `path` / `query` / `body`），由 Gateway 透明转发到 Runtime 的 axum handler；Runtime 解开 Gateway 的 `{ok, data?, error?}` 信封后返回业务结果。事件侧 Runtime 直接 publish 到 MQTT broker，Desktop 通过已建立的 MQTT 订阅收到，由 `chat_mqtt.rs` 解码后 re-emit 到 Tauri `debug-event` 通道。

```text
RPC 请求（Desktop → Gateway → Runtime）：
POST /api/agents/{agent_id}/debug/{path}
Content-Type: application/json

{ "agent_id": "...", "session_id": "...", ... }

→ 200 { "ok": true,  "data": { ... } }
→ 4xx { "ok": false, "error": { "code": "...", "message": "..." } }

事件（Runtime → MQTT → Desktop）：
acowork/agents/{agent_id}/debug/events/onStep            (DebugStepEvent)
acowork/agents/{agent_id}/debug/events/onContextBuilt    (DebugContextBuiltEvent)
acowork/agents/{agent_id}/debug/events/onStateChange    (DebugStateChangeEvent)
```

### 3.0.1 端点映射（方法名 → HTTP / MQTT 主题）

§3.1–§3.9 用业务方法名（`debugger.resume` 等）描述语义。运行时所有 RPC 走 HTTP，所有事件走 MQTT。下表给出方法名到传输端点的一一对应（由 ADR-048 D2 定义、`http/debug.rs` 实现）：

> **实现状态**：✅ = 已迁移（D1-D4）；⏳ = 文档预留、未实现（旧 WebSocket server 同样未实现，见 ADR-048 范围声明，未来走 ADR-053）。

| 业务方法 | 方向 | 端点 / 主题 | 状态 |
|----------|------|-------------|------|
| `debugger.enable` | RPC | `POST /api/debug/enable` | ✅（v3.1 运行时启用，见 §1.1） |
| `debugger.resume` | RPC | `POST /api/debug/resume` | ✅ |
| `debugger.pause` | RPC | `POST /api/debug/pause` | ✅ |
| `debugger.step` | RPC | `POST /api/debug/step` | ✅ |
| `debugger.stop` | RPC | `POST /api/debug/stop` | ✅ |
| `debugger.restart` | RPC | `POST /api/debug/restart` | ⏳（Desktop 走 `restartAgentInDebug` 进程级重启） |
| `debugger.getState` | RPC | `GET /api/debug/state?session_id=…` | ✅ |
| `debugger.setBreakpoint` | RPC | `POST /api/debug/breakpoints` | ⏳ |
| `debugger.removeBreakpoint` | RPC | `DELETE /api/debug/breakpoints/{bp_id}` | ⏳ |
| `debugger.listBreakpoints` | RPC | `GET /api/debug/breakpoints` | ⏳ |
| `debugger.getContextSnapshot` | RPC | `GET /api/debug/context/{iteration}?session_id=…` | ✅ |
| `debugger.getSection` | RPC | `GET /api/debug/context/{iteration}/sections/{section}?session_id=…` | ✅ |
| `debugger.rewind` | RPC | `POST /api/debug/context/rewind` | ✅ |
| `debugger.patchContext` | RPC | `POST /api/debug/context/patch` | ✅ |
| `debugger.reExecute` | RPC | `POST /api/debug/context/re-execute` | ✅ |
| `debugger.editMessage` | RPC | `PATCH /api/debug/messages/{index}` | ⏳ |
| `debugger.rollback` | RPC | `POST /api/debug/messages/rollback` | ⏳ |
| `debugger.reloadSkills` | RPC | `POST /api/debug/skills/reload` | ⏳ |
| `debugger.switchProvider` | RPC | `POST /api/debug/provider/switch` | ⏳ |
| `debugger.startRecording` | RPC | `POST /api/debug/recording/start` | ⏳ |
| `debugger.stopRecording` | RPC | `POST /api/debug/recording/stop` | ⏳ |
| `debugger.loadRecording` | RPC | `POST /api/debug/recording/load` | ⏳ |
| `debugger.stopReplay` | RPC | `POST /api/debug/recording/replay/stop` | ⏳ |
| `debugger.onStep` | 事件 | MQTT `acowork/agents/{id}/debug/events/onStep` | ✅ |
| `debugger.onBreakpoint` | 事件 | MQTT `acowork/agents/{id}/debug/events/onBreakpoint` | ⏳ |
| `debugger.onRecordStep` | 事件 | MQTT `acowork/agents/{id}/debug/events/onRecordStep` | ⏳ |
| `debugger.onStateChange` | 事件 | MQTT `acowork/agents/{id}/debug/events/onStateChange` | ✅ |
| `debugger.onContextBuilt` | 事件 | MQTT `acowork/agents/{id}/debug/events/onContextBuilt` | ✅ |

> **路径前缀**：Desktop 调 Gateway 时挂 `/api/agents/{agent_id}/debug/{rest}`，由 Gateway 透明转发到 Runtime `/api/debug/{rest}`（详见 `acowork-gateway/src/http/proxy.rs` 中的 Debug 通配反代规则，ADR-048 D5）。

### 3.1 执行控制

```rust
/// 恢复自动执行
method: "debugger.resume"
params: {}

/// 暂停主循环，停在下一个迭代步
method: "debugger.pause"
params: {}

/// 执行一步主循环后暂停
method: "debugger.step"
params: {
    /// 断点粒度
    "granularity": "iteration" | "phase"
}

/// 终止当前对话
method: "debugger.stop"
params: {}

/// 重启对话（清空历史，从初始状态重新开始）
method: "debugger.restart"
params: {}
```

### 3.2 状态查询

```rust
/// 获取当前对话完整状态
method: "debugger.getState"
params: {}
result: {
    "iteration": 3,
    "phase": "ToolExecution",
    "messages": [...],
    "snapshot_ids": ["snap-0", "snap-1", "snap-2"],
    "breakpoints": [...],
    "usage": { "prompt_tokens": 1500, "completion_tokens": 300 }
}
```

```rust
enum Phase {
    BudgetCheck,
    BuildContext,
    LlmCall,
    ParseResponse,
    ToolExecution,
    AppendHistory,
    Idle,
}
```

### 3.3 断点

```rust
/// 设置断点
method: "debugger.setBreakpoint"
params: {
    "condition": {
        "type": "on_phase" | "on_tool_call" | "on_iteration" | "on_tool_result",
        // 根据 type 不同：
        // on_phase: { "phase": "ToolExecution" }
        // on_tool_call: { "tool_name_pattern": "http_*" }
        // on_iteration: { "iteration": 3 }
        // on_tool_result: { "is_error": true }
    }
}
result: { "breakpoint_id": "bp-001" }

/// 移除断点
method: "debugger.removeBreakpoint"
params: { "breakpoint_id": "bp-001" }

/// 列出所有断点
method: "debugger.listBreakpoints"
params: {}
result: { "breakpoints": [...] }
```

### 3.4 上下文快照与检查（Context Snapshot & Inspection）

DevMode 下，Agent Runtime 在每轮迭代的 `BuildContext` 阶段完成后，自动捕获**上下文构建结果**。调试面板按轮次将其树状展开。

> **ADR-054（2026-09）**：section 从硬编码 7 字段改为**内容寻址列表**（`Vec<NamedSection>`），并按 `ContextBuilder::build()` 的实际注入顺序产出。新增 3 个此前被合并/丢失的 section（`workspace_prompt_file` / `todo_context` / `ambiguous_confirmation_hint`）与 `messages`（对话消息，懒加载）。前端不再硬编码 section 集合——**渲染 snapshot 实际产出的 sections**，未启用的 section 自然不出现。

| Section | 内容 | 调试用途 |
|---------|------|---------|
| `system_prompt` | 系统级指令（base prompt，**不再合并** workspace_prompt_file） | 调试 prompt 工程 |
| `identity_context` | 用户身份字段 | 检查身份注入 |
| `workspace_context` | Workspace 自格式化上下文 | 验证 workspace 注入 |
| `retrieved_memory` | SQLite 记忆层 检索的记忆节点 | 验证记忆检索质量 |
| `ambiguous_confirmation_hint` | P3-4 冲突确认提示（≥3 个 pending ambiguous conflict 时注入） | 排查"Agent 为何突然问消歧问题" |
| `skill_instructions` | 加载的 SKILL.md 内容 | 调试 Skill 行为 |
| `todo_context` | Agent 内部 active task list | 排查"在错误 todo 上循环" |
| `environment` | 平台环境信息（override 或自动检测） | 检查环境注入 |
| `workspace_prompt_file` | CLAUDE.md / AGENTS.md 内容（独立呈现） | 区分"agent 自带 prompt"与"workspace 配置" |
| `tool_definitions` | 可用工具及参数 Schema | 验证工具注册、修复 Schema 错误 |
| `messages` | 构建时点的完整对话历史（元数据在快照，**内容懒加载**） | 排查"LLM 实际看到了什么" |

> **设计决策**：`messages` 在 ADR-054 之前**排除**在调试面板外。2026-09 起纳入——对话内容是否被正确喂给 LLM 是 debug 最大盲区，且通过懒加载（仅展开时传输）保证性能。

```rust
/// 获取指定轮次的上下文构建快照（仅返回元数据摘要，不含完整内容）
method: "debugger.getContextSnapshot"
params: {
    "iteration": 3  // 轮次编号（0-based）
}
result: {
    "iteration": 3,
    "built_at": "2026-05-09T12:00:00Z",
    // ADR-054: sections 是带 key 的元数据数组（按 build() 注入顺序）
    "sections": {
        "sections": [
            { "key": "system_prompt",      "size_bytes": 2048, "token_estimate": 512,  "hash": "a1b2..." },
            { "key": "tool_definitions",   "size_bytes": 4096, "token_estimate": 1024, "hash": "e5f6..." },
            { "key": "skill_instructions", "size_bytes": 1536, "token_estimate": 384,  "hash": "i9j0..." },
            { "key": "retrieved_memory",   "size_bytes": 3072, "token_estimate": 768,  "hash": "m3n4..." },
            { "key": "identity_context",   "size_bytes": 512,  "token_estimate": 128,  "hash": "q7r8..." },
            { "key": "messages",           "size_bytes": 0,    "token_estimate": 0,    "hash": "lazy"  }
        ]
    },
    "total_token_estimate": 2816,
    "phase": "BuildContext",
    // ADR-054: 构建该快照的 ChatRequest 控制参数（缺省项为 null）
    "request_params": {
        "model": "gpt-4o",
        "temperature": 0.7,
        "max_tokens": 4096,
        "reasoning_effort": "medium",
        "thinking_mode": null
    }
}

/// 懒加载某个 section 的完整内容（用户在调试面板点击展开时按需拉取）
/// ADR-054: section 名不再硬编码——传入 snapshot.sections 中的任意 key；
/// "messages" 走特殊懒加载路径（从 messages_by_iteration 序列化返回）。
method: "debugger.getSection"
params: {
    "iteration": 3,
    "section": "tool_definitions"  // snapshot.sections[].key 之一
}
result: {
    "content": "...",               // 完整文本内容；messages 为 JSON 数组字符串
    "hash": "e5f6...",              // 内容完整性校验
    "token_count": 1024
}
```

**性能策略**：`getContextSnapshot` 仅返回元数据（<500 字节/轮）。section 内容通过 `getSection` 按需懒加载。配合前端默认折叠 + 虚拟滚动，100+ 轮对话的调试面板仍保持流畅。

### 3.5 上下文编辑与回退（Context Editing & Rewind）

当调试发现 tools/skills 上下文有问题时，用户可回退到指定轮次、修补上下文后重新执行：

```
调试工作流:
  1. debugger.getContextSnapshot({ iteration: 3 })  → 检查上下文
  2. debugger.rewind({ to_iteration: 3 })            → 回退到第 3 轮起始状态
  3. debugger.patchContext({ patches: {...} })       → 修补上下文 section
  4. debugger.reExecute()                            → 以修补后的上下文重新执行
```

```rust
/// 回退到指定轮次的起始状态
/// 清除该轮次边界之后的所有消息，允许以修改后的上下文重新执行。
/// 同时清除所有已设置的 patchContext 补丁。
method: "debugger.rewind"
params: {
    "to_iteration": 3  // 回退到第 3 轮 BuildContext 之前的状态
}
result: {
    "rewound_to_iteration": 3,
    "messages_trimmed_to": 12  // messages 数组截断后的长度
}

/// 为下一次 reExecute 修补上下文 section
/// 补丁是临时的——仅在下次 reExecute 时生效，执行后或 rewind 后自动清除。
/// 可多次调用以增量构建补丁。
/// ADR-054: PatchSet 改为 HashMap<String, PatchValue>，每个值带
/// `{ type: "text" | "json", value }` 标签；未知 section 名被拒绝（typo 安全）。
method: "debugger.patchContext"
params: {
    "patches": {
        "system_prompt":            { "type": "text", "value": "Updated system instructions..." },
        "tool_definitions":         { "type": "json", "value": [{ "name": "...", ... }] },
        "skill_instructions":       { "type": "text", "value": "Updated skill content..." },
        "retrieved_memory":         { "type": "json", "value": [...] },
        "identity_context":         { "type": "json", "value": { "field": "value" } },
        "workspace_prompt_file":    { "type": "text", "value": "... (空串清除)" },
        "todo_context":             { "type": "text", "value": "... (空串清除)" },
        "ambiguous_confirmation_hint": { "type": "text", "value": "..." }
    }
    // 每个 key 均为可选——仅传入的 section 会被修补，其余保持不变
}

/// 以修补后的上下文重新执行当前轮次
/// 如果已通过 patchContext 设置了补丁，则在此次执行中生效。
/// 执行完成后补丁自动清除，Runtime 恢复正常流程（或在断点/Step 模式下暂停）。
method: "debugger.reExecute"
params: {}
result: {
    "iteration": 4,  // 新轮次编号（递增）
    "output": { ... }
}
```

**设计约束**：
- `rewind` 和 `patchContext` 是**分离的操作** —— rewind 不会自动触发 reExecute，必须显式编辑后再执行
- 补丁是**临时性**的 —— 在 reExecute 完成后或 rewind 调用时自动清除
- `patchContext` 可在 reExecute 前**多次调用**以增量构建编辑

**消息级操作**（轮次内的细粒度编辑）：

```rust
/// 编辑对话历史中的某条消息
method: "debugger.editMessage"
params: {
    "index": 2,
    "content": { ... }  // 新的 MessageContent
}

/// 回滚到指定消息索引，丢弃后续消息
method: "debugger.rollback"
params: { "target_index": 2 }
```

### 3.6 Skill 热加载

```rust
/// 重新加载 skills 目录
method: "debugger.reloadSkills"
params: {
    /// 可选，只重载指定 skill
    "skill_name": null | "weather-query"
}
```

### 3.7 Provider 切换

```rust
/// 动态切换 LLM Provider
method: "debugger.switchProvider"
params: {
    "provider": "openai" | "ollama" | "anthropic" | ...,
    "model": "gpt-4o" | "qwen3:8b" | ...,
    /// 可选，覆盖 base_url
    "base_url": null | "http://localhost:11434/v1"
}
```

切换流程：
1. Desktop App 发送 `debugger.switchProvider`
2. Agent Runtime 更新 LLM Client 的当前 provider 配置
3. 如果需要新 Key → 通过 Gateway KeyRelease 获取（如果 Vault 中有）
4. 如果是本地 Provider (ollama) → 直连，无需 Key
5. 下一次 LLM 调用使用新 provider

典型工作流：
```
初始开发 → ollama/qwen3:8b（本地免费，快速迭代）
基本可用 → openai/gpt-4o-mini（低成本测试真实 API）
最终验证 → openai/gpt-4o（全功能测试）
```

### 3.8 录制回放

```rust
/// 开始录制当前会话
method: "debugger.startRecording"
params: {}

/// 停止录制并保存
method: "debugger.stopRecording"
params: {
    /// 录制文件保存路径（默认工作区 recordings/ 目录）
    "output_path": null | "/path/to/recording.jsonl"
}

/// 加载录制文件并回放
method: "debugger.loadRecording"
params: {
    "path": "/path/to/recording.jsonl",
    "mode": {
        "type": "auto" | "manual",
        // auto: { "delay_ms": 500 }
        // manual: {}
    }
}

/// 停止回放
method: "debugger.stopReplay"
params: {}
```

### 3.9 事件通知（Runtime → Desktop App，MQTT 通道）

事件走 MQTT 主题 `acowork/agents/{agent_id}/debug/events/{type}`，载荷用 Protobuf（见 `acowork-core/src/protocol.rs` 的 `DebugStepEvent` / `DebugContextBuiltEvent` / `DebugStateChangeEvent`）。下表以 JSON 形态给出 payload 字段含义，便于阅读；线缆格式以 Protobuf 为准。

```text
topic: acowork/agents/{agent_id}/debug/events/onStep
payload (DebugStepEvent):
{
    "session_id": "…",        // 事件来源会话
    "iteration": 3,
    "phase": "ToolExecution",
    "input": { ... },         // 本步输入（如有）
    "output": { ... },        // 本步输出（如有）
    "usage": { ... }          // LLM 用量（如有，prompt/completion/total_tokens）
}

topic: acowork/agents/{agent_id}/debug/events/onBreakpoint  （⏳ 未发射，见 ADR-048 范围声明）
payload (DebugBreakpointEvent):
{
    "session_id": "…",
    "breakpoint_id": "bp-001",
    "iteration": 3,
    "phase": "ToolExecution"
}

topic: acowork/agents/{agent_id}/debug/events/onRecordStep  （⏳ 未发射）
payload (DebugRecordStepEvent):
{
    "session_id": "…",
    "step_index": 5,
    "phase": "LlmCall",
    "step_data": { ... }      // 序列化的步骤数据
}

topic: acowork/agents/{agent_id}/debug/events/onStateChange
payload (DebugStateChangeEvent):
{
    "session_id": "…",
    "new_state": "Paused"     // DebugState（Running/Paused/Stepping/Stopped）
                              // 或 DebugPhase 名（LlmCall/…，Runtime 复用同一 topic）
    "iteration": 4
}

topic: acowork/agents/{agent_id}/debug/events/onContextBuilt
payload (DebugContextBuiltEvent):
{
    "session_id": "…",
    "iteration": 3,
    "sections": {
        "system_prompt":      { "size_bytes": 2048, "token_estimate": 512,  "hash": "a1b2..." },
        "tool_definitions":   { "size_bytes": 4096, "token_estimate": 1024, "hash": "e5f6..." },
        "skill_instructions": { "size_bytes": 1536, "token_estimate": 384,  "hash": "i9j0..." },
        "retrieved_memory":   { "size_bytes": 3072, "token_estimate": 768,  "hash": "m3n4..." },
        "identity_context":   { "size_bytes": 512,  "token_estimate": 128,  "hash": "q7r8..." }
    },
    "total_token_estimate": 2816
}
```

> **实现侧**：`mqtt/debug_events.rs::DebugEventMqttPublisher` 把 `DebugEventBus` 收到的 `TaggedEvent` 编码为对应 Protobuf 后 PUBLISH 到上述主题。Desktop 侧 `commands/chat_mqtt.rs` 三个 proto 解码 arm 解码后 emit Tauri `debug-event` 事件，由 `stores/debugStore.ts` 接收。

## 4. 消息快照机制

Agent Runtime 在 DevMode 下的每一轮迭代结束，自动创建一个轻量快照：

```rust
struct ConversationSnapshot {
    /// 快照 ID（递增）
    id: String,
    /// 对应的迭代轮次
    iteration: u32,
    /// 快照时刻 messages 数组的长度（截断点）
    message_count: usize,
    /// 快照时刻的 LLM 用量
    cumulative_usage: Usage,
    /// 时间戳
    timestamp: SystemTime,
}
```

快照的实现极其轻量——messages 数组是 append-only 的，快照只需要记录长度。回滚时截断到目标长度即可，无需深拷贝。

```
messages: [msg0, msg1, msg2, msg3, msg4, msg5]

快照 @ iteration 2: message_count = 4  →  回滚: [msg0, msg1, msg2, msg3]
快照 @ iteration 3: message_count = 6  →  回滚: [msg0, msg1, msg2, msg3, msg4, msg5]
```

## 5. 录制格式

录制的会话保存为 JSONL 文件，每行一个步骤：

```jsonl
{"type":"recording_header","agent_id":"com.example.weather-dev","timestamp":"2026-04-09T12:00:00Z","provider":"openai","model":"gpt-4o"}
{"type":"user_input","content":"北京今天天气怎么样","iteration":0}
{"type":"llm_request","messages_count":2,"iteration":0}
{"type":"llm_response","content":"tool_call(http_request,...)","usage":{"prompt_tokens":150,"completion_tokens":30},"iteration":0}
{"type":"tool_call","name":"http_request","params":{"method":"GET","url":"https://api.weather.com/v1?city=Beijing"},"iteration":0}
{"type":"tool_result","name":"http_request","result":{"temp":25,"condition":"晴"},"iteration":0}
{"type":"llm_request","messages_count":4,"iteration":1}
{"type":"llm_response","content":"北京今天25度，晴天","usage":{"prompt_tokens":200,"completion_tokens":20},"iteration":1}
```

录制文件保存在 Agent 工作区的 `recordings/` 目录下。

### 5.1 回放模式

| 模式 | 说明 | 适用场景 |
|------|------|---------|
| **自动回放** | 按录制顺序自动推进，每步可设延迟 | 全流程演示、回归测试 |
| **手动步进** | 每步需用户手动 Step 推进 | 逐帧检查、调试特定步骤 |
| **对比回放** | 加载多个录制文件，同屏对比 | A/B 测试不同 Provider/Prompt 的效果 |

### 5.2 回放与编辑结合

回放过程中可以随时：
- 编辑某步的消息内容，然后 Re-execute
- 切换 Provider 后从某步重新执行
- 插入新的用户消息，偏离原录制路径，进入自由调试

录制文件既是"回归测试用例"，也是"调试起点"。

### 5.3 录制 + Provider 切换

回放模式下可以切换 Provider，实现"同样的对话，不同的 LLM"对比：

```
录制: 用 gpt-4o 录制了一段完整对话
回放: 切换到 qwen3:8b，回放同样的用户输入和工具调用
      → 对比两个模型对同样上下文的回复差异
```

## 6. DevMode vs 生产模式

Agent Runtime 的 DevMode 是生产模式的**超集**：

| 维度 | DevMode | 生产模式 |
|------|---------|---------|
| Debug Protocol | HTTP RPC `/api/debug/*`（localhost Runtime HTTP）+ MQTT 调试事件 publisher（共享 `:19875`） | 不注册 routes、不 spawn publisher |
| 主循环 | 受调试器控制（Pause/Step/Resume） | 自动连续执行 |
| 上下文快照 | 每轮自动创建上下文快照（ADR-054：动态 section 列表，最多 11 个） | 不创建 |
| 上下文编辑 | 支持迭代级回退与修补（`rewind`/`patchContext`/`reExecute`） | 不支持 |
| 消息快照 | 每步自动创建 ConversationSnapshot | 不快照 |
| Provider 切换 | 动态可切换（`debugger.switchProvider`） | 按 manifest 固定配置 |
| 录制 | 可录制/回放（JSONL） | 不录制 |
| Skill 加载 | 热加载（`debugger.reloadSkills`） | 启动时一次性加载 |
| 消息编辑 | 支持（`debugger.editMessage`） | 不支持 |
| 消息回滚 | 支持（`debugger.rollback`） | 不支持 |

生产模式下 Agent Runtime 与 03-agent-runtime.md 设计完全一致。DevMode 的复杂度全部封装在 Agent Runtime 和 Desktop App 内部，Gateway 不需要任何修改。

DevMode 启动方式（Gateway 侧，v3.1 起推荐运行时启用）：

```toml
# Gateway 启动 Agent 时，如果 Agent 标记为 dev: true，则追加 --dev-mode 参数
agent-runtime /path/to/agent --endpoint pipe://agent-gateway --agent-id com.example.weather-dev --dev-mode
```

或者在 Agent 已启动后由 Desktop App 通过 `POST /api/agents/{id}/debug/enable` 运行时启用——见 §1.1。**推荐**第二种方式，因为不需要重启 Agent、不打断 session 状态；第一种方式仅保留用于 CLI / CI 场景下的「以调试模式启动」测试。

## 7. Agent 克隆协议

Agent 克隆通过 Gateway HTTP API 执行（Desktop App 调用，Gateway 执行），不通过 Debug Protocol。定义如下：

### 7.1 克隆请求

```http
POST /api/agents/{instance_id}/clone
Content-Type: application/json

{
  "mode": "skeleton" | "full",
  "new_agent_id": "com.example.weather-dev"
}
```

`{instance_id}` 是源 **实例**（ADR-073），`new_agent_id` 是**包** id（反域名）。
两个身份互不比较，也不互相代替。

### 7.2 克隆流程

```
Desktop App → Gateway POST /api/agents/:id/clone
       │
       ▼
Gateway → Node 控制平面（克隆是 node-local 的，ADR-055 §6.6 L2-5）：
  ├─ 读取源安装目录 {packages_dir}/{agent_id}/{instance_id}/
  ├─ 按模式复制（边界定义见 §7.3）：
  │   ├─ skeleton：包级全部文件
  │   │           （manifest.toml 且 agent_id 已改写、prompts/、
  │   │            skills/、assets/、…）
  │   │
  │   └─ full：skeleton + 该实例的 workspace 状态：
  │       conversations/（当前会话 JSONL 快照）
  │       memory/private.sqlite（快照，含 -wal/-shm 边车文件）
  │
  ├─ 写入 {packages_dir}/{new_agent_id}/{new_instance_id}/
  ├─ 标记克隆件 dev: true
  └─ 返回克隆结果
```

### 7.3 骨架 / 完整 边界

一条标准划分：**包级静态内容 vs. 实例运行时状态**。

| 模式 | 包含 | 说明 |
|---|---|---|
| **skeleton** | **包级全部文件** —— `manifest.toml`（`agent_id` 已改写）、`prompts/`、`skills/`、`assets/`、`tools/`、`resources/`…… | 安装时即存在，与用户无关 |
| **full** | skeleton **+ workspace 下的实例状态**：`workspace/config/`（用户改的模型、MCP、自定义目录——`agent_workspaces.json`）、`workspace/conversations/`、`workspace/memory/`（含 `-wal`/`-shm`） | 运行中产生，是该实例见过什么的痕迹 |

两条约束：

1. **两份清单都不写死。** 包级排除由 `PACKAGE_ALWAYS_EXCLUDE_DIRS`（`memory`/`workspace`/`runtime`）驱动；workspace 级排除由 `WORKSPACE_LOCAL_DIRS`（`files`/`logs`）驱动。其余条目一律复制。早先包级 `["prompts","config","tools","resources"]` 白名单把 `skills/`/`assets/` 从 skeleton 里漏掉；workspace 级 `(conversations, memory)` 白名单把 `config/` 从 full 里漏掉。
2. **实例状态在 `workspace/` 下，不是包级。** 两者是兄弟，不是父子：`{instance}/prompts/` 是包内容，`{instance}/workspace/conversations/` 是状态。把它们混在一起会让 full 模式从一条根本不存在的路径去拷——结果就是静悄悄给克隆体打个无历史空壳。

`workspace/files/`（宿主机相关的附件）和 `workspace/logs/`（运行时产物）即使在 full 模式也不复制——它们是机器本地的。`workspace/config/` 在 full 模式里随克隆体一起走——用户花了一下午配置的东西不应该重做。

### 7.4 克隆限制

- 克隆体与源 Agent 独立，后续源 Agent 更新不会同步
- 完整克隆的 SQLite 记忆层 快照是克隆时刻的副本，之后双方各自演化

## 8. 发布流程

调试完成的 Agent 从开发态转为发布态，通过 Desktop App 的发布向导执行。

### 8.1 发布步骤

```
Step 1: 检查
  ├─ manifest.toml 完整性校验
  ├─ 必填字段检查（agent_id, version, name, runtime_version）
  ├─ skills/ 目录下每个 SKILL.md 格式校验
  ├─ prompts/ 目录下文件存在性检查
  └─ 权限声明合理性检查

Step 2: 清理
  ├─ 移除 dev 标记（manifest 中 dev: false 或删除该字段）
  ├─ 清空 recordings/ 目录（不打包进发布包）
  ├─ 清空或保留 data/（用户选择）
  ├─ 清空私有 SQLite 记忆层（发布包不含个人记忆）
  └─ 重置 config/ 为默认值（可选）

Step 3: 打包
  ├─ 按 .agent 包格式打包为 ZIP
  └─ 输出到 build/<agent_id>-<version>.unsigned.agent

Step 4: 签名
  ├─ 调用 acowork-sign 签名
  ├─ 可选：选择密钥（已有 / 新生成）
  └─ 输出 build/<agent_id>-<version>.agent

Step 5: 分发
  ├─ 本地安装：Gateway Package Manager 安装到生产位置
  ├─ 仓库上传：推送到配置的仓库源
  └─ 导出文件：仅保存 .agent 文件到指定路径
```

### 8.2 发布 API

```http
# Step 1-2: 验证 + 清理
POST /api/agents/:id/publish/prepare
→ { "ready": true, "warnings": [...] }

# Step 3-4: 打包 + 签名
POST /api/agents/:id/publish/build
body: { "sign_key": null | "/path/to/key" }
→ { "output_path": "build/com.example.weather-1.0.0.agent" }

# Step 5a: 安装构建产物（ADR-113：必须显式指定目标节点）
# 名称里的 "locally" 只表示产物来源在本地，不代表默认落点是本机节点。
# node_id 取自 GET /api/nodes 中的可用节点，Gateway 不做任何推断。
POST /api/agents/:id/publish/install-locally
body: { "package_path": "build/com.example.weather-1.0.0.agent", "node_id": "<node-id>" }

# Step 5b: 导出文件
POST /api/agents/:id/publish/export
body: { "package_path": "...", "export_to": "/user/choosen/path" }
```

## 9. 设计决策记录

| 决策 | 选择 | 理由 |
|------|------|------|
| 协议格式（ADR-048） | HTTP REST（RPC）+ MQTT pub/sub（事件） | 与生产 IPC 完全同构（ADR-033 / ADR-034），无需引入第三种协议栈；ACL / 多用户隔离天然继承 |
| 调试面板范围 | ADR-054：动态 section 列表（最多 11 个，含 messages/todo/ambiguous/workspace_prompt_file） | ADR-054 前仅 5 个控制面 section；2026-09 起 messages 纳入（懒加载），覆盖 LLM 实际看到的全部上下文 |
| 上下文快照 | 元数据摘要 + 懒加载 | 每轮 <500 字节元数据（size/token/hash），section 内容按需拉取；配合虚拟滚动保证百轮对话的流畅性 |
| 上下文编辑模型 | rewind + patchContext + reExecute 分离 | rewind 不自动触发执行，编辑后需显式 reExecute；补丁临时生效，执行后自动清除 |
| 快照机制 | 记录 message_count | 极轻量，无需深拷贝；messages 是 append-only，截断即可回滚 |
| 录制格式 | JSONL | 逐行追加写入，无需完整序列化；崩溃不丢失已录制内容；易于调试和人工审阅 |
| DevMode 启动参数 | `--dev-mode` CLI flag | Gateway 通过启动参数控制，Runtime 侧零配置变更 |
| DevMode 是超集 | 不改变生产逻辑 | 生产模式下代码路径完全不变；DevMode 仅在检测到 flag 后初始化调试组件 |
| 端口默认值 | 19877（ADR-048 后不再占用） | 可配置，但默认值应避免与常见服务冲突；ADR-048 后 Debug RPC 走 Runtime localhost HTTP 随机端口 + Gateway 反代，MQTT 事件复用 `:19875`，不再监听独立 Debug 端口 |
| Agent 克隆走 Gateway API | 不走 Debug Protocol | 克隆是 Gateway 侧的文件操作，与 Agent Runtime 无关 |
| 调试中会话的 DevMode 入口 | Agent 克隆 + 克隆体 `--dev-mode` 启动 | 不采用运行时动态切换 DevMode；克隆体隔离数据，原 Agent 不受影响 |
| 克隆体 conversations 目录 | full 模式复制当前 session JSONL | 支持"聊天到一半开启调试"场景，克隆体恢复对话状态 |
