# ADR-110：per-agent 新会话初值（默认模型 / 默认推理强度）

- **编号**：ADR-110
- **状态**：已实施
- **日期**：2026-11-08
- **决策者**：Nicholas（需求与方案确认）
- **关联**：ADR-012（模型选择是 per-session）、ADR-010（上下文压缩）、ADR-099（Chat Router 的 per-agent 模型字段）、ADR-101（记忆服务边界）、ADR-108（右侧栏面板合并）、ADR-109（per-session 工具收窄）
- **英文 counterpart**：[docs/adr/en/ADR-110-per-agent-session-defaults.md](../en/ADR-110-per-agent-session-defaults.md)（同 commit 维护）

---

## 摘要

**一句话**：`agent_config.json` 新增 `default_model` / `default_provider` / `default_reasoning_effort`，三者是**新会话的"种子"**——新建 session 时用它作为 model / provider / reasoning effort 的初值，取代此前"继承最近一次活跃 session"的策略。

**一句话（为什么）**：用户对一个 Agent 的期望是稳定的（"这个 Agent 用 Opus 跑 high"），而"最近一次用过的值"是一个**隐式全局变量**：只要有一次会话切到别的模型试一下，之后所有新会话都被污染，且没有任何 UI 能把这个状态显式设回去。把初值显式化，等于把"隐式继承"换成"显式默认"，与 ADR-012（模型选择是 per-session 事实）不冲突——session 自身的 meta 仍是权威，per-agent 字段只决定**它出生时是什么**。

---

## 背景（代码取证）

1. **per-agent 持久化已经存在且有唯一写入口。** `AgentConfig`（`core/acowork-runtime/src/agent_config.rs`）序列化为 `{work_dir}/config/agent_config.json`，`max_output_tokens` / `temperature` / `context_window` / `session_language` / `chat_router` 都在里面；唯一写路径是 `PUT /agents/{id}/config` → `RuntimeAgentConfigService::put_config`（`usecases/agent_config_impl.rs`，模块文档自称 agent_config.json 在 HTTP 路径上的 "single audit point"）。新增字段不需要新存储。
2. **per-session 的权威存储是 session meta，不是这个文件。** `ConversationSession` 的 JSONL meta 持有该 session 的 model / provider / reasoning_effort。ADR-012 的"模型选择是 per-session"因此不被本 ADR 触碰：per-agent 字段**从不**写进一个已存在的 session。
3. **新建 session 的初始化有唯一收敛点。** `SessionManager::build_initial_session_state`（`src/agent/session/session_manager.rs`）在 session 自身无 model/provider 时调用 `current_model_and_provider()`；该函数取"最近激活 session 的 (model, provider) 对"，没有则取 `global_provider_list.first()`。这就是被替换的那条隐式继承。
4. **`reasoning_effort` 的解析在本 ADR 之前被手抄了两份，且两份当前等价。** `build_initial_session_state` 里手写四级链（persisted → caps.default → reasoning-capable 时 Auto → `None`），HTTP/MQTT 的读取端用共享的 `session_config::llm_effects::resolve_effective_reasoning_effort`，两条链今天逐层等价（**已核对，无行为漂移**）。本 ADR 只是插入新的一层时改走共享实现，避免"以后只在一边加层"的漂移风险——不是修 bug。
5. **`RuntimeConfigOverrides` 不能表达"清除"。** 这份 per-agent 内存缓存服务**活跃 session 的热更广播**，语义是 "`Some` 覆盖 / `None` 保留"，因此无法表达"用户把这个默认删掉了"。默认值必须能在下一次建 session 时立刻消失，所以它不进这条链（见 D3）。
6. **`put_config` 的 read-modify-write 跨线程无锁。** PUT DTO 是 partial body，而"选一个模型"在 UI 上必然要写两个字段（`default_model` + `default_provider`）。若无序列化，两个请求各自 load 旧文件 → merge 自己那一半 → 整文件 save，后完成者丢弃前者的半个字段。实测：把锁换成 `try_lock` 后，并发成对写入在第 0 轮就丢掉 provider 半个（见 D4 的回归测试）。ADR-099 的 router 模型/供应商是同一形状，属于同一条既存缺陷。

---

## 决策

### D1 — 三个字段进 `agent_config.json`，语义是"新会话初值"

```jsonc
{
  "default_model": "claude-sonnet-4-6",      // 模型 id（ProviderModelEntry::id）
  "default_provider": "anthropic",           // 与 model 成对的 provider id
  "default_reasoning_effort": "high"         // 与输入框强度菜单同一套拼写
}
```

三个字段都是 `Option<String>` + `skip_serializing_if = "Option::is_none"`：**不写键 = 没有意见**。`default_provider` 独立成字段而不是把两者拼成 `provider::model`，因为同一个模型 id 可能被两个已配置 provider 同时提供，成对才是无歧义的选择；而只设 `default_model` 时由运行时在 provider 清单里找第一个提供该 id 的 provider（见 D2）。

### D2 — 初始化链：per-agent 默认 → 原来那条隐式继承

`model` / `provider`（第一个产出 model 的层胜出，provider 永远与 model 同源，不会出现"模型来自 A 层、provider 来自 B 层"）：

```
1. agent_config.json default_model（+ default_provider；缺失则取清单中第一个提供该 model 的 provider）
2. current_model_and_provider()   ← 本 ADR 之前的唯一行为，保留为兜底
```

`reasoning_effort` 由 `resolve_effective_reasoning_effort` 单点解析：

```
1. session meta 里的持久值
2. agent_config.json default_reasoning_effort        ← 本 ADR 新增层
3. 模型能力的 default_reasoning_effort
4. Auto（模型支持推理但没声明默认）
5. None（模型不支持推理控制 → 输入框隐藏该控件）
```

只设 `default_provider` 而不设 `default_model` 是**被忽略**的组合：单独一个 provider 在 UI 上不是可选状态，替它挑第一个模型会静默覆盖"最近使用"，比忽略更糟。

解析结果会被回写进新建 session 的 meta（`update_reasoning_effort` / model / provider），因为**session meta 仍是权威**：默认值是它出生时收到的值，之后改 per-agent 默认不影响任何已存在的会话。

### D3 — 不进 `RuntimeConfigOverrides`：这是种子，不是覆盖

per-agent 默认**不投影**到热更广播里，且 `AgentSessionDefaults::load` 在每次建 session 时从磁盘现读（一次小文件读）。两个理由缺一不可：

- `RuntimeConfigOverrides` 语义是 "`Some` 覆盖 / `None` 保留"，表达不了"清除"，用它当缓存会让"删掉默认"在重启前不生效；
- 广播给活跃 session 是**错误语义**——默认只影响下一个 session，改动不应动到正在跑的会话（ADR-012）。

`test_agent_defaults_are_not_live_overrides` 锁住这条不变式（投影必须为空，且值不得出现在广播 JSON 里）。

### D4 — `put_config` 的 load-merge-save 加锁（单一审计点）

`RuntimeAgentConfigService` 持一把 `std::sync::Mutex<()>`，`put_config` 全程持锁。选 `std::sync` 而非 `tokio::sync`：临界区内没有 await 点，锁不可能跨挂起持有。

这是**为了 D1 能被真正写下去**：D1 的三个字段里有两个必须在同一次用户操作中被写入（模型 + 供应商），partial PUT 的成对写入必须可组合，否则用户选的默认模型会静默丢失半对。修在审计点而不是让每个客户端自己排序，顺带修掉 ADR-099 router 模型/供应商的同形缺陷。回归测试 `concurrent_partial_puts_of_a_coupled_pair_both_persist` 跑 25 轮并发成对写——去掉锁第 0 轮即可复现失败。

### D5 — UI 选项列表与输入框完全同源，只是作用域不同

右侧 Agent 设置的"模型参数"卡片新增两个下拉（默认模型 / 默认推理强度）：

- **模型**：选项来自 `chatStore.availableModels`（与输入框 `ModelMenu` 同一份 vault 派生清单），值为 `provider::model`，标签 `providerName · model`；`""` 是"自动（沿用上次使用的）"。
- **推理强度**：`Auto` / `Off` 两项固定 + `reasoningLevelsFor(该模型的 reasoning_efforts)`——与输入框强度菜单同一个 helper、同一个顺序。选中的模型变了而旧强度不在其声明集里时，旧值仍作为一项保留，避免下拉渲染空白。

两个下拉的写入都走既有的 per-field debounce 写通路径（`saveField`），落盘由 D4 的锁保证成对一致。

---

## 后果

- **正面**：Agent 的"默认行为"从隐式变成显式、可编辑、可清空；清除立即（下一个 session）生效，无需重启；reasoning_effort 的三处解析收敛为一份实现，消除了已有漂移；`agent_config.json` 的成对 partial 写不再互相覆盖。
- **负面**：每个新 session 多一次小文件读（`agent_config.json`，几 KB，已在 OS 页缓存里）；`agent_config.json` 又多三个字段，升级前的文件仍能解析（`serde` 缺省 + `#[serde(default)]`）。
- **不变式**：
  - 已经**显式**持有 model / provider / effort 的 session 不受 per-agent 默认影响——默认只填会话自己"没有意见"的那一半，且**绝不覆盖调用方显式传入的 model**（该覆盖曾是本实现的缺陷，已被 `new_session_is_seeded_from_agent_defaults_not_last_used` 第 5 步锁定）。
  - 一个从来没有任何 effort 取值的会话（含本 ADR 之前的存量会话），首次打开时按新链解析并**固化进自己的 meta**，此后稳定。也就是说：默认值可以在会话首次物化时改变它的**有效** effort，但一旦会话有了自己的取值，就不再跟随默认值变化。
  - session meta 仍是 model / provider / effort 的唯一权威（ADR-012 未被削弱）。

## 测试

- `session_manager::tests::new_session_is_seeded_from_agent_defaults_not_last_used` — 设默认后新建 session 用默认；切到别的模型后新建仍用默认；清掉默认后回落到"最近活跃 session"的继承。
- `agent_config::tests::load_reads_persisted_config_and_survives_a_corrupt_file` — 落盘 / 读回 / 损坏文件降级为空默认。
- `agent_config_impl::tests::test_agent_default_field_patches` — Set / Clear / 空串（等价清除）/ 强度值小写归一。
- `agent_config_impl::tests::test_agent_defaults_are_not_live_overrides` — 不投影进热更广播。
- `agent_config_impl::tests::concurrent_partial_puts_of_a_coupled_pair_both_persist` — 成对 partial PUT 并发不丢半（D4）。
