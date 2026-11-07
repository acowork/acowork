# ADR-109：per-session 工具收窄（只收窄层）与输入框 `+` 菜单

- **编号**：ADR-109
- **状态**：已实施（2026-05-20）
- **日期**：2026-05-20
- **决策者**：Nicholas（方案确认）
- **关联**：ADR-029（内置工具持久化与使能）、ADR-069（MCP 工具级 opt-in）、ADR-076（多用户账号体系：会话读写授权）、ADR-082（Runtime 侧 SQLite 边车）、ADR-090（MCP 安装归属）、ADR-101（记忆服务边界）、ADR-108（右侧栏面板合并）
- **英文 counterpart**：[docs/adr/en/ADR-109-session-tool-narrowing.md](../en/ADR-109-session-tool-narrowing.md)（同 commit 维护）

---

## 摘要

**一句话**：工具开关从**只有 Agent 一层**变成**两层**——Agent 层（`agent-tools.json` / `agent-mcp.json`，右侧设置面板编辑）决定这个 Agent 有什么，会话层（`SessionMeta.tool_policy`，新增 SQLite v13 JSON 列）只能**从 Agent 层里拿掉**。同一改动把输入框的两个按钮（左侧技能下拉、右侧回形针）合并成一个 `+` 菜单：一级四行（技能 / MCP 服务 / 内置工具 / 附件），二级下钻同列替换，MCP 三级为单个 server 的 tool 列表。

**一句话（为什么）**：会话是**用户当下的一次工作**，它的合理诉求经常是"这次别让它跑 shell"，而不是"改这个 Agent 的定义"。今天这个诉求只能靠改 Agent 配置实现，改完所有会话一起变——把一个会话级意图写进了 Agent 级状态。同时输入框横排已经有 5 个控件，再加一个入口只会继续挤。

---

## 背景（代码取证）

1. **内置工具本来就是 per-session 存活的。** `SessionTask` 在创建时深拷贝 Agent 的 template 工具集，Agent 级 `UpdateBuiltinTools` 广播只是覆盖这份副本；也就是说"这个会话用哪些内置工具"在内存里已经是一个 per-session 事实，只是没有任何持久化的、用户可写的表达。
2. **MCP 的 tool 级开关在**连接期**就被剪掉。** Agent 级 `agent-mcp.json` 没放行的 tool，Runtime 连接时根本不会为它建 wrapper（`prefixed_name = server__tool`）。因此会话层"打开一个 Agent 关掉的 tool"在数据上无处可放——不是策略问题，是可表示性问题。这条事实直接决定了本 ADR 的单向性。
3. **`SessionMeta` 已有 JSON 列先例。** `todos` 就是 `Option<Vec<_>>` 序列化进 Meta（内存态 + `write_meta` 回写），v12 之前的迁移脚本里能查到同类 ALTER。新增一列不需要新的存储范式。
4. **右侧面板是 Agent 层的编辑器，不是会话层的。** ADR-108 把内置工具与 MCP 两卡并入设置面板后，它们的写入路径是 `PUT /agents/{id}/builtin-tools` / `mcp-servers` / `mcp-tools`——改的是 Agent 的定义。把它同时当成会话开关会让同一个面板有两个语义。
5. **仓库里没有多级横向 flyout 的先例。** 所有 popup（model / workspace / skills / session）都是单列单层。横向 flyout 需要三块面板宽度，而输入框位于可拖窄的右栏与可拖窄的面板之间——第二个层级就会在窄窗口里被裁掉。

---

## 决策

### D1 — 会话层是"拒绝列表"，只能收窄

`SessionToolPolicy` 只有三组，全部是**拒绝**语义：

```jsonc
{
  "disabled_builtin_tools": ["shell"],
  "disabled_mcp_servers": ["playwright"],
  // 落盘形状是 Rust 的 BTreeMap<String, BTreeSet<String>>：server → 该 server
  // 下被剪掉的（未加前缀的）tool 名。**不是** [{server, tool}] 列表。
  "disabled_mcp_tools": { "playwright": ["screenshot"] }
}
```

> 这个形状是跨端契约：Rust `SessionToolPolicy`（`BTreeMap`）与 Desktop 的
> `WireSessionToolPolicy`（`Record<string, string[]>`）一致，UI 侧的语义模型
> `McpToolRef[]` 只活在 `toolInventory.ts` 里，进出网络都要过
> `fromWirePolicy` / `toWirePolicy`。两侧各有一个断言同一字面量的测试
> （`session_meta::tool_policy_tests::mcp_tool_denies_serialise_as_a_map` 与
> `toolInventory.test.ts` 的 "converts the per-server tool map both ways"），
> 因为各自的 mock 都看不见对方。

- 不允许表达"打开一个 Agent 层关掉的东西"（取证 2：它连接期就不存在）。
- 三组互相独立：server 级拒绝覆盖其下所有 tool；tool 级拒绝在 server 重新打开后**仍然保留**（用户此前逐项剪掉的，不该因为重新打开 server 而复活）。

**被否的方案**：allow-list 覆盖（会话存"我要用哪些"）。它要求会话层在 Agent 每次变更后重新枚举全部工具，且一旦 Agent 新增工具，老会话会静默看不到——把"继承"变成了"快照"。

### D2 — `NULL` 即继承，只有改过才写

`SessionMeta.tool_policy: Option<SessionToolPolicy>`，`None` = 跟随 Agent 层，是**新建会话的默认值**。全空策略在写入时归一为 `None`（`set_session_tool_policy` / `PUT` 都会 filter 掉空值）。

因此"新建 session 默认用 per-agent 配置，只有用户改了才写 per-session 配置"不是前端约定，而是**数据模型的性质**：一次都没点过的会话，那一列永远是 `NULL`。

### D3 — 唯一合成入口，四个调用点

`agent/session/session_tool_layer.rs::SessionToolLayer` 是唯一把两层合成 effective 工具集的地方：它持有 Agent 层基线（`agent_builtin` / `agent_mcp`）与会话层 `policy`，`effective_builtin()` / `effective_mcp()` 返回 `A ∧ B`，`apply_to(core)` 写回会话自己的 `AgentCore`。任何"绕过它自己算一遍"的实现都会让两处语义漂移。

**为什么必须在写入时合成而不是读取时过滤**：`rebuild_all_tools`（派发）与 `rebuild_context_tool_definitions`（LLM 的 spec 表）都直接读 `core`，读时过滤等于要让每个现有与未来的读者都懂会话层；写进 `core` 只保留一条派生管线。

合成发生在四个位置：

1. `SessionTask::new` —— 会话打开 / 恢复 / fork 时 `from_core` + 按 hydrated policy 合成（必须在第一次 `apply_to` 之前，否则基线已是收窄后的值，收窄会变成永久）；
2. Agent 级 `UpdateBuiltinTools` 广播到达时 —— `set_agent_builtin` 换基线后重新合成；
3. `UpdateMcpTools` 广播到达时 —— `set_agent_mcp` 后重新合成（该广播的载荷是**整个** Agent 层期望态，直接写进 `core.mcp_tools` 会把会话层的开关抹掉）；
4. per-session `PUT` 到达时 —— `SessionMessage::UpdateToolPolicy` → `set_policy`（内容相等则跳过）→ `apply_to`。

也就是说 Agent 层变更**永远不能清掉**会话层：两条广播的载荷只更新基线，随后各自重新把会话层叠上去。

### D4 — `GET/PUT /sessions/{sid}/tool-policy`，整体替换

Runtime 暴露 `GET /sessions/{sid}/tool-policy`（读授权）与 `PUT`（写授权），Gateway 反代到 `/api/agents/{id}/sessions/{sid}/tool-policy`，ADR-087 分类沿用 `sessions` 段的既有规则：GET → `view`、PUT → `use`。

- **整体替换而非逐项 toggle**：同一份 body 重放不会叠加，UI 的乐观更新与服务端回显一一对应，也就不存在"读-改-写"丢更新。
- **不校验名字是否存在**：拒绝列表只做过滤，一个指向已不存在工具的名字是惰性的；校验反而会让并发的 Agent 级变更把一次无关的开关变成 400。
- `PUT` 也接受 **closed 会话**：只改 Meta，下次打开时合成（活跃会话才需要立即重编工具集）。

### D5 — 输入框 `+` 菜单：下钻单列，不做横向 flyout

一级四行：技能 / MCP 服务 / 内置工具 / 附件。二级在**同一个固定高度的盒子内**替换内容（带返回箭头），MCP 的 server 行右侧还有 chevron 进三级（该 server 的 tool 开关）。

- 附件不是面板：点一下即关闭菜单并打开文件选择器（与原来的回形针同一个 handler）。
- 开关三态在 UI 上的投影：`effective = agentEnabled && !denied`。Agent 层没放行的行显示为**关闭且禁用**，并在组底部说明有多少项由 Agent 配置关闭——这就是"你在这里不能加宽"的诚实呈现，而不是让开关看起来可点。
- 每组标题右侧一个"跟随 Agent 默认"（`resetGroup`），只清该组；空组时禁用。
- 只读会话（ADR-076 §决策 4）整体禁用 `+`：附件、选技能、工具开关都是会话写操作，半个可用比整体禁用更难理解。

### D6 — 右侧面板保持 Agent 层语义，两个面板不共享状态

设置面板继续编辑 Agent 层（写 `agent-tools.json` / `agent-mcp.json`），`+` 菜单编辑会话层（写 Meta）。二者**可以同时显示不同值**——这正是两层的含义，不是缺陷：per-session 的改动只影响当前这个 session（以及用户在该 session 里后续的每一轮），与 per-agent 的配置值互不影响；而 Agent 层的值继续是**下一个新建 session 的默认值**。所以"面板说已启用、本会话实际没跑"是正确呈现——面板描述的是 Agent 层，会话层只是在它之下做了减法。

共享的只有类型与取数（`lib/toolInventory.ts`：`buildToolInventory` 折叠 `/tools` + `/mcp-tools`），不共享"当前状态"。

### D7 — 只有执行面输入框有 `+` 菜单：群聊与聊天面不做

只有执行会话面（`ChatPanel`）的输入框接到 `ComposerPlusMenu`。另外两个输入框刻意不动：

- **群聊（`GroupChatPanel`）**：它有自己的 `@` 路由与多 agent 语义，per-session 工具策略在群聊里没有明确的"哪个 agent 的哪一层"答案。留到需要时再设计，不在这里顺带扩语义。
- **聊天面（`AgentChatView`）**：一个 agent 只有一个 Chat 会话，那一层实际上等价于 per-agent，用右侧设置面板即可，再加一套 per-session 开关只会让"改哪一层"变得含糊。

---

## 后果

- **新增 SQLite 迁移 v13**（`ALTER TABLE sessions ADD COLUMN tool_policy JSON`），`TARGET_SCHEMA` 同步；老库升级后该列为 `NULL`，等价于"全部继承"。
- **工具集可能逐轮不同**：同一 Agent 的两个会话可以跑不同的工具集合。Agent 级的审批 / 风险规则仍按 Agent 配置生效——会话层只是**少给**工具，不会绕过任何审批。
- **每轮 LLM 调用前多一层 filter**：纯内存过滤（三组 `HashSet` 查找），相对 provider 往返可忽略。
- **可测边界清晰**：`session_tool_layer.rs`（纯函数合成）、`SessionManager::set_session_tool_policy`（活跃 / 关闭两条持久化路径）、`lib/toolInventory.ts`（纯语义）、`stores/toolPolicyStore.ts`（乐观写 + 回滚 + 新编辑优先）。
- 若将来要"会话层加宽"，需要先解决取证 2 —— 即让 Agent 层把未放行的 tool 也建 wrapper，那是另一个决定，不能靠改策略形状实现。
