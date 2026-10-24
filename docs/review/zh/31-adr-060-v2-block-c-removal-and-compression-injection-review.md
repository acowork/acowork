# Code Review：ADR-060 v2（移除 Block C + 压缩注入）

**Reviewer**:Senior Software Engineer
**日期**:2026-09-02
**范围**:11 个未提交文件（含 1 个 ADR 文档），相对 HEAD +2 commit 的增量
**主题**:prompt cache 友好的 context block 重构 v2 — 移除 Block C（尾部 todo 快照），改为「history 承载 + 压缩注入」两条路径

---

## 1. 总体结论

| 维度 | 评级 | 说明 |
|---|---|---|
| 架构合理性 | **A** | 移除 Block C 决策正确，"history 承载 + 压缩注入"思路清晰；所有权收敛于 `ConversationSession`；新增 `flush_pending` 补齐 writer fire-and-forget 缺口 |
| 实现准确性 | **C** | **P0 bug 一项**：JSONL 写入路径与 history idempotency 不一致，已被 `multiple_compressions_write_exactly_one_synthesized_round_to_jsonl` 测试实证（left=4, right=1）|
| 性能 | **A** | 移除 Block C 后每轮少 ~hundreds token cache miss；`recalibrate_tokens` 仅在压缩事件触发，可忽略；reverse-scan 单次 O(n) |
| 一致性 | **B-** | DebugPanel.tsx 文案停留在 v1 语义；3 处与 ADR-060 无关的「顺手修复」夹在本 PR 中 |
| 可测试性 | **A** | 7 个新 e2e 测试覆盖 inject / restart / no-todo / 多重压缩 / 全流不变式；测试自身就是合约文档 |
| 编译/clippy | **A** | `cargo check -p runtime -p gateway` 通过；`cargo clippy --all-targets -- -D warnings` 通过 |

**建议**:**修复 P0 bug 后再提交**；其余问题可在同 PR 或 follow-up 解决。

---

## 2. 变更概览

```
 core/acowork-gateway/src/gateway/node_manager.rs            |   1 +   (无关 clippy fix)
 core/acowork-runtime/src/agent/context.rs                   |  ----  (移除 Block C 拼装)
 core/acowork-runtime/src/agent/e2e_prompt_cache.rs          | +472 行  (v2 端到端测试)
 core/acowork-runtime/src/agent/history.rs                   | +443 行  (find/inject + 6 单元测试)
 core/acowork-runtime/src/agent/loop_.rs                     |   1 行   (无关 clippy fix)
 core/acowork-runtime/src/agent/loop_context.rs              | +166 行  (compact 路径串接 inject + 持久化)
 core/acowork-runtime/src/agent/session/session_manager.rs   |  +12 行  (let_chains 语法糖)
 core/acowork-runtime/src/conversation.rs                    |  +56 行  (WriterCommand::Flush + flush_pending)
 core/acowork-runtime/src/debug/handlers.rs                  |   12 行  (测试用 key 替换)
 core/acowork-runtime/src/debug/observer_impl.rs             |  +50 行  (latest_todo_write_content)
 docs/adr/zh/ADR-060-prompt-cache-friendly-context-block-reorg.md | +148 行  (v2 修订记录)
```

**前置背景**：v1（commit `2986994d`）已完成 traits.rs 的 `CacheControl`、Anthropic system 提升、`SessionMeta.todos` 持久化、debug 面板 v1 等基础；本次 diff 是 v2 增量，仅移除 Block C 并新增压缩注入路径。

---

## 3. 架构合理性 ✅

### 3.1 移除 Block C 的决策 — 正确

v1 的 Block C（每轮把完整 todo 列表作为 User 消息追加到 Block B 之后）违反了 ADR 自己 §4.1 的"稳定前缀 + 末尾追加"原则——Block B 一旦增长，Block C 的位置漂移，永不命中 cache。v2 修复正确。

### 3.2 "history 承载 + 压缩注入" — 思路正确

两条 todo 承载路径：
1. **正常路径**：Block B 中的真实 `todo_write` 工具结果 → append-only，缓存命中
2. **压缩注入路径**：压缩后从 history 复用最后一次 `todo_write` 轮插入 marker 之后

两条路径都满足"lossless 保留 todo 状态"。设计原则"压缩摘要（lossy，记过程）与 todos（lossless，记状态）分离"清晰。

### 3.3 单一所有权收敛 — 正确

写盘仅经 `ConversationSession::append_message_with_id()`；`injected_round_for_persistence` 借用此路径，避免双写者。`SessionMeta.todos` 持久化由 v1 完成，v2 无需触碰。

### 3.4 `flush_pending()` — 必要补丁

之前 writer thread 是 fire-and-forget；压缩后立即 kill 进程可能丢注入行（writer 还在 channel 队列里）。新增的 `WriterCommand::Flush` 用 oneshot 做同步握手，补齐了这个缺口。

### 3.5 Debug 面板 UI 契约 — 向后兼容

`observer_impl.rs` 把 `latest_todo_write_content()` 的结果仍放到 `"todo_context"` key，前端无感。

---

## 4. 实现准确性 ⚠️

### 4.1 [P0-致命] JSONL 重复 append bug

**位置**：
- [core/acowork-runtime/src/agent/history.rs:1032-1084](core/acowork-runtime/src/agent/history.rs#L1032-L1084) `inject_todo_write_round_after_marker`
- [core/acowork-runtime/src/agent/loop_context.rs:776-798](core/acowork-runtime/src/agent/loop_context.rs#L776-L798)

**现象**：
测试 `multiple_compressions_write_exactly_one_synthesized_round_to_jsonl` 失败：
```
left: 4, right: 1
```
3 次压缩后，JSONL 中实际有 4 个 synthesized tool_call 行，期望 1 个。

**根因**：
```rust
// loop_context.rs:787-798
if let Some((assistant, tool)) = pending_todo_write_inject {
    self.session.history.inject_todo_write_round_after_marker(
        assistant.clone(),
        tool.clone(),
    );
    injected_round_for_persistence = Some((assistant, tool));  // ← 总是赋值
}
```
- `pending_todo_write_inject` 只要 history 含 todo_write 总是 `Some(...)`
- `inject_todo_write_round_after_marker` 在 round 已在 tail 时是 no-op（return current_tokens，history 不变）
- 但 `injected_round_for_persistence` 仍被赋值 → 下方 JSONL append 块继续执行 → 重复 append 一对 (tool_call, tool_result) 行

**影响**：
- history 内存一致（`in_tail` 检测阻止了重复插入）
- JSONL 失真：每次压缩都会重复 append 同一对行
- 重启后，JSONL 恢复 → `restore_history_from_jsonl` 会看到多对 `tool_call`/`tool_result` 行，replay 时同一 tool_call_id 出现 N 次 → `sanitize_messages` 删除孤儿 → 仅第一对存活——**但**若第一对的 tool_call 行位置异常（不在 assistant 之前），sanitize 逻辑可能把它一起删
- JSONL 体积膨胀（每次压缩 +1 行；长期运行后显著）

**修复方向**：
让 `inject_todo_write_round_after_marker` 返回 `bool`（是否实际插入）：
```rust
pub fn inject_todo_write_round_after_marker(
    &mut self,
    assistant: ChatMessage,
    tool: ChatMessage,
) -> (u64, bool) {  // (new_tokens, did_inject)

    // 现有 skip 分支：return (self.current_tokens, false);
    // 真正插入分支：return (new_tokens, true);
}
```
loop_context.rs 据此决定是否写 JSONL：
```rust
let (new_tokens, did_inject) = self.session.history
    .inject_todo_write_round_after_marker(assistant.clone(), tool.clone());
if did_inject {
    injected_round_for_persistence = Some((assistant, tool));
}
```
这样 round 已在 tail 时（3 次压缩中后 2 次），JSONL 不再被污染。

**优先级**：**P0 — 阻塞提交**。

---

### 4.2 [P1-边界] `tcid` 为 `None` 时 idempotency 检测被跳过

**位置**：[core/acowork-runtime/src/agent/history.rs:1048-1084](core/acowork-runtime/src/agent/history.rs#L1048-L1084)

```rust
if let Some(tcid) = tool.tool_call_id.as_deref() {
    // in_tail 检测
    ...
}
// 直接走到 msgs.insert(insert_at, tool)
```

`todo_write` 工具结果通常必有 `tool_call_id`（来自 Assistant 的 tool_calls），但代码未做防御。极端情况下（如手写注入消息）`tool_call_id = None`，整段 in_tail 检测被跳过，永远进入真正的 insert 路径——即使 round 已在 tail。

**建议**：
```rust
debug_assert!(
    tool.tool_call_id.is_some(),
    "ADR-060 v2: injected tool message must carry tool_call_id"
);
```
或者在 `tool_call_id` 为 `None` 时直接 return + warn（与第 4.1 修复合并：return `(self.current_tokens, false)`）。

**优先级**：P1 — 现实中不易触发，但会让"看起来很合理"的测试用例漏掉。

---

### 4.3 [P1-防御性失真] `find_last_todo_write_round` 取首个 todo_write

**位置**：[core/acowork-runtime/src/agent/history.rs:990-996](core/acowork-runtime/src/agent/history.rs#L990-L996)

```rust
let todo_call_id = assistant
    .tool_calls
    .as_ref()?
    .iter()
    .find(|tc| tc.function.name == "todo_write")?
    .id
    .clone();
```

注释说"be defensive about the choice"，但 `find()` 只取第一个 todo_write tool_call。如果 Assistant 一次发出多个 todo_write（异常但可能：工具允许），后续 todo_write call 在 retained tail 中的部分会被错误判断为"不在 tail"，触发重复注入。

**建议**：取所有 todo_write call_id 的并集：
```rust
let todo_call_ids: Vec<String> = assistant
    .tool_calls
    .as_ref()?
    .iter()
    .filter(|tc| tc.function.name == "todo_write")
    .map(|tc| tc.id.clone())
    .collect();
```
然后 `inject_todo_write_round_after_marker` 用并集做 in_tail 检测。

**优先级**：P1 — 当前 `todo_write` 工具定义限制每次调用只有一个 call，现实中不会触发；但与"defensive" 注释意图不符。

---

### 4.4 [P1-文档不一致] DebugPanel.tsx 注释停留在 v1 语义

**位置**：[apps/acowork-desktop/src/components/debug/DebugPanel.tsx:68](apps/acowork-desktop/src/components/debug/DebugPanel.tsx#L68)

```js
// ADR-060: todo_context is Block C — after the history/messages block.
"todo_context",
```

v2 后 `todo_context` 段不再是"Block C"，而是"Block B 最后 todo_write tool result"。注释误导，渲染顺序仍正确（messages 之后），但用户读代码会困惑。

**建议**：改为
```js
// ADR-060 v2: todo_context is Block B's latest `todo_write` tool result,
// surfaced here for parity with the old Block C display position.
"todo_context",
```

**优先级**：P1 — 文案问题，无功能影响。

---

### 4.5 [P2-范围] 3 处与 ADR-060 无关的"顺手修复"

| 文件 | 修改 | 评价 |
|---|---|---|
| `loop_.rs:1288-1291` | `attempt: attempt` → `attempt`（变量名简化） | 无害，但与本 PR 主题无关 |
| `session/session_manager.rs:1231-1256` | 嵌套 `if let ... if ...` 改为 let_chains 语法 | 无害（Rust 1.88+），与本 PR 主题无关 |
| `gateway/node_manager.rs:55-57` | 加 `#[cfg(unix)]` 到 `KILL_GRACE` 常量 | 无害（Windows 上 kill_process_group 走 taskkill 分支），与本 PR 主题无关 |

**建议**：拆为独立 commit（如 `chore(runtime): clippy cleanups`），便于 bisect 与 blame。混合提交会让 "git log --grep=ADR-060" 无法定位纯重构改动。

**优先级**：P2 — 不阻塞，但工程规范层面应拆分。

---

## 5. 性能 ✅

| 维度 | 评估 |
|---|---|
| CPU | 每轮 build 不再做 todo 快照格式化（Block C 移除）；`recalibrate_tokens` 仅在压缩事件触发，单次 O(n)，可忽略；reverse-scan `find_last_todo_write_round` 单次 O(n) |
| 内存 | 无显著变化；todo 仍只在内存 |
| 磁盘 | meta 文件多一个 todos 字段（v1 已就位）；JSONL 每压缩事件 +1 对（待 P0 修复后正确为 0/1 对） |
| Cache 命中率 | 提升 — Block C 移除后 Block A/B 稳定前缀变长；v2 设计目标达成 |

**潜在风险**：`recalibrate_tokens()` 在 inject 后调用，全量重算。极端大 history（100K+ tokens）下每次压缩事件可能多花 10-50 ms。可接受。

---

## 6. 测试覆盖 ✅

v2 端到端测试（[e2e_prompt_cache.rs](core/acowork-runtime/src/agent/e2e_prompt_cache.rs)）覆盖：

| 测试 | 覆盖场景 |
|---|---|
| `todo_write_roundtrip_v2_layout_and_restart_recovery` | 工具迭代 / 重启恢复 / v2 layout（A→B→D）|
| `compression_injects_removed_todo_round_after_marker` | level-8 压缩移除 todo_write 轮后注入 |
| `compression_no_todo_history_is_safe_noop` | 无 todo_write 历史时压缩为 no-op |
| `block_c_never_in_any_request_output_full_flow` | 全流不变式：任何 ChatRequest 都不含 `## Todo Task List` |
| `restart_after_compression_preserves_todo_state` | 重启后注入轮仍可见 |
| `multiple_compressions_write_exactly_one_synthesized_round_to_jsonl` | **多重压缩下 JSONL 仅 1 对**（**当前失败**，见 P0）|

history 单元测试（[history.rs](core/acowork-runtime/src/agent/history.rs)）：
- `find_returns_none_when_no_todo_write`
- `find_returns_pair_when_todo_write_present`
- `find_returns_latest_when_multiple_todo_writes`
- `inject_skips_when_round_already_in_tail` ✅ 单元层验证 idempotency
- `inject_splices_after_marker_when_round_removed`
- `inject_recalibrates_tokens`

测试自身就是合约文档，写得很好。

---

## 7. 一致性 / 文档

| 项 | 状态 |
|---|---|
| ADR-060 v2 修订记录 | ✅ §修订记录块清晰列出变更理由、原则、两条路径 |
| ADR §5.4 压缩注入约束 1-4 | ✅ 约束 1（完整轮）/ 2（避免重复）/ 3（token 计入）实施；约束 4（无 todo 轮回退到 SessionMeta）标注为 §11 可选 |
| AGENTS.md 注释语言 | ✅ 所有新注释英文 |
| AGENTS.md 代码注释规范 | ✅ |

---

## 8. 验证结果

```text
cargo check -p acowork-runtime -p acowork-gateway     → ✅ pass (47s)
cargo clippy --all-targets -- -D warnings            → ✅ pass (1m20s)
cargo test -p acowork-runtime -- agent::              → ❌ 1 failed
  - agent::e2e_prompt_cache::multiple_compressions_write_exactly_one_synthesized_round_to_jsonl
    assertion `left == right` failed: left=4, right=1
    → 见 §4.1 [P0]
```

其余 90 个相关测试通过。

---

## 9. 行动项 (Action Items)

| 优先级 | 任务 | 阻塞提交 |
|---|---|---|
| **P0** | 修复 JSONL 重复 append：让 `inject_todo_write_round_after_marker` 返回 `(u64, bool)`；loop_context.rs 据此判断是否写 JSONL | ✅ 是 |
| P1 | `inject_todo_write_round_after_marker` 对 `tool_call_id=None` 加 `debug_assert!` 或 no-op 处理 | 否 |
| P1 | `find_last_todo_write_round` 用所有 todo_write call_id 的并集做 in_tail 检测 | 否 |
| P1 | 更新 `DebugPanel.tsx` 注释 v2 语义 | 否 |
| P2 | 拆分 3 处无关改动到独立 `chore` commit | 否 |

---

## 10. 整体评价

**架构层面的设计是正确的**，是 v1 Block C 缺陷的必要修正。"history 承载 + 压缩注入"两条路径互补，单一所有权收敛，新增 flush_pending 补齐 writer 同步缺口——这些都是成熟工程决策。

**实现层面有一个 P0 bug 必须修复后才能提交**——`multiple_compressions_write_exactly_one_synthesized_round_to_jsonl` 测试已经精准钉住了它。修复方式明确（让 inject 返回 bool，调用方决定是否落盘），工作量 < 30 行（含测试更新）。

修复 P0 后即可合并；P1/P2 项可在同 PR 或 follow-up。