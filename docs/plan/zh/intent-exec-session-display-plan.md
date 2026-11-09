# 执行会话视图的 intent 人类投影计划（read model 收口）

> 版本：v1.0 | 日期：2026-11-09
>
> **一句话**：聊天里的执行块已经能读人类投影（`RoundProjection.user.display`），但**执行会话视图**（`ChatPanel` 的气泡流）读的是 `GET /sessions/{sid}/messages` 返回的**持久化 DTO 本身**，其 `content` 是机器信封、人类投影藏在 `metadata.intent` 里——前端转换时又把非 turn-part 的 `metadata` 丢掉，于是 agent↔agent 的 intent 在执行会话里只剩 `[intent:…:create_dev_tasks] {"project_id":…}`。本计划按"先定数据模型、业务在后端、前端只显示"的原则收口：人类投影收敛成**一个类型 + 一条读取规则**，由传输层**显式**返回 `display` 字段，前端只搬运与渲染。
>
> 前置调查已完成，证据带 file:line（§1）。§3 是数据模型，§4 是三个模块（+ 一个可后置项），§5 明确不做，§7 记录已知瑕疵。

---

## 1. 缺陷与证据

**现象**：Product.Lucy 在其执行会话里向 PM 发 intent；PM 的执行会话里，这条消息渲染成一个右对齐气泡，内容是 `[intent:31839e13…:create_dev_tasks] {"project_id":"p-66e68d1a","request":"…"}`。

**链路段落**（每一段都可复现）：

1. Runtime 把 intent 轮写成一行：`content` = 机器信封，`metadata = { round_id, intent: { origin, action, summary, body, truncated } }`（[session_task.rs:1596](../../../core/acowork-runtime/src/agent/session/session_task.rs#L1596) + [intent_turn.rs:703](../../../core/acowork-runtime/src/agent/intent_turn.rs#L703)）。已实测命中：PM 的 `conversations/20261109_152121_76eca8.jsonl` 第 0 行 user 行 metadata 里 `intent.origin=request, action=create_dev_tasks, truncated=false`，`summary`/`body` 均为 markdown。
2. 执行会话视图经 `GET /sessions/{sid}/messages` 拉历史：该接口把**持久化 DTO** `ConversationEntry { id, ts, role, content, metadata?, kind? }`（[conversation.rs:53](../../../core/acowork-runtime/src/conversation.rs#L53)）直接序列化返回（[server.rs:1489](../../../core/acowork-runtime/src/http/server.rs#L1489) 只做 `to_value`）。
3. 桌面 [chatStore.ts:3865](../../../apps/acowork-desktop/src/stores/chatStore.ts#L3865) `convertConversationEntry` 转成 `ChatMessage`：`content` 原样带走（[chatStore.ts:3905](../../../apps/acowork-desktop/src/stores/chatStore.ts#L3905)），`metadata` **只对 turn-part / tool 行保留**（[chatStore.ts:3933](../../../apps/acowork-desktop/src/stores/chatStore.ts#L3933)），普通 user 行丢弃。
4. [MessageBubble.tsx:300](../../../apps/acowork-desktop/src/components/chat/MessageBubble.tsx#L300) 的 user 分支渲染 `message.content` → 信封被当正文画出来。

**这条路为什么没被上次的改动覆盖**：上次改的是 `RoundProjection.user.display`（聊天块，走 `/chat-blocks` + [ChatExecutionBlock.tsx](../../../apps/acowork-desktop/src/components/chat/ChatExecutionBlock.tsx)）；执行会话视图是另一条读模型（`/sessions/{sid}/messages` + [ChatPanel.tsx](../../../apps/acowork-desktop/src/components/chat/ChatPanel.tsx) → `MessageBubble`）。agent 从自己的执行会话发起的 intent 不进任何 chat（`turn_surface.echo_chat()` 为空时 [loop_.rs:970](../../../core/acowork-runtime/src/agent/loop_.rs#L970) 不写引用行），所以它**只在**这条链上出现。

---

## 2. 根因（数据模型层，而非渲染层）

**持久化 DTO 兼作传输 DTO，于是"哪些 metadata 键算数据"的判据被复制到每个消费者。** 桌面已编码两遍（turn-part 注册表 + tool 字段），移动端再一遍（[api.ts:350](../../../apps/acowork-mobile/src/lib/api.ts#L350)）。人类投影只以 `metadata` 里的一个键存在，没有任何读模型显式宣告它——所以：

- 前端要显示它，就必须**自己解析 metadata、自己判断"可用"**，这就是把业务判据搬进 UI（也是上次 review 里"同一形状四份定义"的来源）。
- 投影现在 Rust 侧已有两份：生产者 `IntentDisplay`（[intent_turn.rs:487](../../../core/acowork-runtime/src/agent/intent_turn.rs#L487)，5 字段）与读侧 `rounds::RoundUserDisplay`（[rounds.rs:676](../../../core/acowork-runtime/src/rounds.rs#L676)，3 字段 + 自己的"空即缺失"判据）。再加一条读路径就是第三份判据。

所以修法不是"在执行会话组件里补一段解析"，而是**在数据模型上把人类投影升级为一等读模型字段**。

---

## 3. 数据模型（先模型，后业务）

**写入模型（已定，本计划不改）**

| 载体 | 内容 | 受众 |
|---|---|---|
| 行 `content` | `[intent:{from}:{action}] {compact JSON}` + 坐标行 | 模型（当轮 + 重启回放） |
| 行 `metadata.round_id` | 轮次归属 | rounds 索引 |
| 行 `metadata.intent` | `IntentDisplay { origin, action, summary, body, truncated }` | 人（各显示面） |

**读模型（本计划补第二个）**

| 读模型 | 入口 | 人类投影字段 | 现状 |
|---|---|---|---|
| 轮次投影 | `/chat-blocks` → `RoundProjection` | `user.display` | 已有 |
| 会话消息 | `/sessions/{sid}/messages` → 每条 entry | **缺** → 本计划加 `display` | M2 |

**边界规则（唯一实现，写在类型上）**

- 人类投影的类型：`IntentDisplay`（生产者写什么，读模型就返回什么，不另立子集类型）。
- 读取规则：`IntentDisplay::from_metadata(&Value) -> Option<Self>`——唯一知道 `META_KEY_INTENT_DISPLAY`、唯一决定"不可用即 `None`"（空 body 且空 summary）的地方。序列化侧对所有字段 `#[serde(default)]`，使部分写入的旧行被当作宽容读取而不是整轮失败。
- 消费规则：显示面只读读模型字段（`display`），**永不解析 `metadata`**；`display == None` 一律回落到 `content`，旧行零迁移。

---

## 4. 模块

### M1（runtime，模型收敛）— 纯重构，无行为变化

- `IntentDisplay` 加 `from_metadata`（唯一读取规则），字段加 `#[serde(default)]`。
- 删除 `rounds::RoundUserDisplay` 及其 `from_entry` 判据，`rounds::project` 改用 `IntentDisplay::from_metadata`（序列化多出 `origin`/`action` 两个字段，客户端已忽略未知字段，无害）。
- 验收：现有 rounds 投影测试全绿（含"坏形状当缺失"那条）；新增 `from_metadata` 的部分写入用例。

> 为什么删而不是留两份：读规则一旦有两份，"什么算可用"就会分叉；这次要新增第三条读路径，必须先把它收敛成一条。这是**删除**，不是新增抽象。

### M2（runtime，传输层读出显式字段）

- 新增 `ConversationEntryView { id, ts, role, content, metadata?, kind?, display: Option<IntentDisplay> }` + `From<ConversationEntry>`；`get_messages` 把 `PaginatedMessages.messages` 逐条映射后返回（分页三个字段 `offset/limit/total` 原样透传，不加包装层）。
- `metadata` 原样保留（附件/工具/内部标记的现有消费者零改动）；`display` 为**新增可选字段**，旧客户端忽略。
- 验收：序列化形状测试（有/无 display 各一条）；`display` 只由 `from_metadata` 填充，无第二处判断。

> 为什么不干脆在 `ConversationEntry` 上加一个 `skip_serializing_if` 的可选字段：那个 struct 就是 JSONL 的持久化形状，加一个"只允许读时出现"的字段等于把两个角色再混一次——正是本缺陷的根因。视图结构是**边界**，不是抽象。

### M3（前端，只搬运与渲染）

- 类型：`lib/types.ts` 的 `ConversationEntry` 加 `display?: IntentDisplay`；`IntentDisplay` 只在这一处定义，`chatBlocks.ts` 的 `RoundUserDisplay` 改为 import 它（不保留第二个同名形状；这是改 import，不是重构）。移动端同改（[types.ts:222](../../../apps/acowork-mobile/src/lib/types.ts#L222)）。
- 搬运：`convertConversationEntry` 加一行 `base.display = entry.display`；不做校验、不做瘦身、不看 metadata。移动端 `mapConversationEntries` 同理。
- 渲染（桌面）：`MessageBubble` user 分支 `message.display ? <ChatMarkdown content={display.body}/> + truncated 提示 + <details>原始信封(content)</details> : content`。复用现有 i18n key `chatView.block.rawEnvelope` / `displayTruncated`。
  - **要主动破一条现有规则**：`speakerRendersMarkdown("user") === false`（[speakerBubble.ts:56](../../../apps/acowork-desktop/src/components/chat/speakerBubble.ts#L56)），即 user 行一律纯文本。intent 的 `display.body` 是 agent 写的 markdown，必须按 markdown 渲染。规则写成"`display` 优先于 speaker 判定"，而不是改 `speakerRendersMarkdown`（那会把人类输入也变成 markdown，[speakerBubble.test.ts:22](../../../apps/acowork-desktop/src/components/chat/speakerBubble.test.ts#L22) 明确守着的断言）。
  - `UserWithAttachmentsBubble` 走同一判定，但 display 行按构造不带附件，不动它；万一将来带上了，退化成纯文本是可接受的降级。
- 渲染（移动）：气泡当前对 user 轮传 `literal`（纯文本），intent 轮因此把信封当纯文本画出来（[ChatDetailScreen.tsx:387](../../../apps/acowork-mobile/src/screens/chat/ChatDetailScreen.tsx#L387)）。规则改为：**有 `display` 时按 markdown 渲染 `display.body`（`literal` 不适用，它是 agent 写的），信封折叠**；无 `display` 走原 `literal` 行为。同桌面：`literal` 优先于 `display` 的反向覆盖是不允许的，且 [chat-row-text.test.tsx](../../../apps/acowork-mobile/src/__tests__/chat-row-text.test.tsx) 里"人类输入按纯文本"的断言保持不变，另加一条"带 display 的 user 行按 markdown"。
- 验收：转换器测试（有 display 保留 / 无 display 为 undefined）；气泡渲染测试（有 display 渲染 markdown 且信封折叠 / 旧行不变）。

### M4（runtime，等待队列预览）

会话忙时队列行的 `preview` 也来自信封（[session_state.rs:126](../../../core/acowork-runtime/src/agent/session_state.rs#L126) `pending_preview(content)`，桌面 [ChatPanel.tsx:2529](../../../apps/acowork-desktop/src/components/chat/ChatPanel.tsx#L2529) 渲染它）。队列是人的界面，规则与气泡、轮次行同一条：**有投影就用投影的名字**。取舍落在 `preview_source(content, refs, display)` 一处（`display.summary` → 本轮的正文 → 第一个 reference block），`deliver_intent_turn` 传 `display.as_ref()`，路由路径（只有人的原话、没有信封）传 `None`。前端不动。

**顺序**：M1 → M2 → M3（一次可验证的闭环）→ M4。

---

## 5. 明确不做（够用即可的边界）

1. **前端解析 `metadata.intent`**：任何"前端自己认出投影"的实现都否决，包括在上次方案里写过的 `intentDisplayOf(meta)`。
2. **为执行会话另立投影类型**：复用 `IntentDisplay`，不与 `RoundProjection.user.display` 分叉。
3. **投影进 `content`**：模型输入与重启回放依赖信封（含回写坐标），任何替换都视为回归。
4. **把 `content` 改名或替换成"已解析正文"再给前端**：那会让调试/搜索/复制等确实需要原文的消费者无处可取；`display` 是并列的新字段，`content` 语义不变。
5. **旧行迁移**：`display == None` 回落 `content`，历史记录保持原样。
6. **实时推送链路**：执行会话气泡来自历史读；`stream_delta` / MQTT payload 不加投影字段。若将来要"投递瞬间也显示人话"，单独评估。
7. **新的抽象层**（统一 renderer / 通用 metadata 解析框架）：本计划只有"一个类型 + 一条读取规则 + 一次视图映射"。

---

## 6. 验证

- Rust：`cargo clippy -p acowork-runtime --all-targets -- -D warnings`；`cargo test -p acowork-runtime --lib`（含 `rounds` 与 `intent_turn` 全部既有测试）；视图序列化形状测试。
- 桌面：`npx vitest run src/components/chat src/stores`；`npm run check:i18n`；`npx tsc --noEmit`（仅允许既有的 editor/* 缺可选依赖报错）。
- 移动端：`npx tsc --noEmit`。
- 端到端手验：Lucy 从其执行会话发 intent → PM 执行会话里该气泡显示 `display.body` 的 markdown，展开可见原始信封；同一轮的聊天块（若有）行为不变。
- 移动端手验：执行详情页（`ChatDetailScreen`）同一轮显示 markdown 而非信封纯文本。

---

## 7. 遗留项

本轮 review 的六条已全部清理，只剩签名/朝向一条（本计划范围外）。

### 已清理

1. **`assignment_brief` 语义过载** → 改名 `round_name`（`b9426305`）：字段名与它所存的事实一致；JSONL 键同步改名，前端读新键优先、回落旧键（旧行照旧可读，写入只写新键）。PM 派发文本仍只在 system block 用，门禁未动。
2. **分页循环重复 + 同一文件读两次盘**（`57818d6a`）：`read_first_user_turn` 一次读盘产出 `{ body, display_summary }`，标题优先级挪到 `title_seed()`；顺带让 display 侧也走 `IntentDisplay::from_metadata`，不再自己 `metadata.get("intent").get("summary")`。
3. **截断逻辑重复 + 240 两份常量**（`f5d93d84`）：`truncate_marked(body, max_bytes)` + `capped_one_line(text, max_chars)` 各一份，标记统一为 `\n[truncated]`；`reference_brief` 改用 `DISPLAY_SUMMARY_MAX_CHARS`。
4. **写链欠账**（`13f2ad27`）：`intent_display_meta` → `from_metadata` 的 round-trip 等值测试，生产者与读取规则从此由断言绑在一起。
5. **桌面空 body 回落**（`b08ada27` / `f9e94496`）：规则不放在各处调用点，而是收进渲染器本身——`IntentDisplayBody`（桌面）与 `Bubble`（移动）画 `display.body || display.summary`，"有 display 就绝不画信封"因此对每个调用点自动成立；`questionBodyOf`/`questionBody` 同步收口（复制导出同源）。
6. **移动端新分支无测试**（`4319c941` / `f9e94496`）：`chat-row-text.test.tsx` 覆盖"有投影画 markdown / 折信封 / 截断提示 / 空 body 用 summary / 无投影照旧 literal"。文案硬编码中文沿用移动端现状（该 app 无 i18n 基础设施，全站同构，不是本次引入的欠账）。

### 未做（范围外，另行安排）

1. **气泡署名与朝向**：执行会话里 user 行的昵称来自"人类"，而 agent-origin intent 的发送者其实是另一个 agent，于是 Lucy 的消息被画成"你自己的消息"。正解是给投影加一条 `from` 事实（生产侧已持有 `env.from`，前端不解析信封），属于模型加字段，单独一轮。

---

## 8. 实施进度

| 模块 | 状态 | 提交 |
|---|---|---|
| M1 | 已完成 | `13f2ad27` 删 `RoundUserDisplay`，加 `IntentDisplay::from_metadata`（唯一读取规则），字段全 `serde(default)`；round-trip + 宽容读取两条测试 |
| M2 | 已完成 | `62abf82d` `ConversationEntryView`（`flatten` + `display`）+ `get_messages` 映射；`265fcea6` 补 HTTP 级断言（视图自测抓不到装配错误） |
| M3 桌面 | 已完成 | `ebcb9d11` 类型收敛到 `types.ts`、`convertConversationEntry` 只搬运、`MessageBubble` 渲染 → 抽 `IntentDisplayBody` 与执行块共用 |
| M3 移动 | 已完成 | `4319c941` 同规则；`display` 优先于 `literal`（`ChatDetailScreen` 对 user 轮传 literal） |
| M4 | 已完成 | `f23a5b72` `preview_source` 取 `display.summary` 优先（唯一规则），两个调用点按"有无信封"传参 |

**实施中与原计划的五处偏差（都已就地处理，记录理由）**：

1. **视图放在 `usecases/session_metadata.rs` 而不是 `http/server.rs`**：逐条 entry 的转换本来就在 usecase 里（tool_result 截断也在那），handler 只做 `to_value`。边界不变，只是落在真正做转换的那一层。
2. **视图用 `#[serde(flatten)]` 包住 `ConversationEntry`，而不是逐字段列一遍**：逐字段列会让"将来给 entry 加字段，忘了加到视图"变成静默丢字段。`flatten` 让存储形状自动透传，视图只声明它多出的那一件事（`display`）。
3. **桌面端把"markdown + 截断提示 + 折叠信封"抽成 `IntentDisplayBody`**，执行块与气泡共用：两处若各写一遍，i18n key 与折叠形状没有理由保持一致。移动端渲染器不同（`renderMarkdown` + CSS 类），未抽。
4. **M4 比原计划多改一处签名**：原计划"在 `deliver_intent_turn` 里优先用 `display.summary`"，实现改成把取舍放进 `preview_source` 的唯一规则里——否则"预览从哪来"的第 N 个来源会散落在第 N 个调用点。两个调用点因此各传一个 `display` 实参，其中路由路径明确传 `None` 并注释为什么。

5. **空 body 的收口位置下移一层**：原计划改 `questionBodyOf` 的回落顺序，实现改成渲染器自己画 `body || summary`。调用点有三处（气泡、执行块面板、复制/导出），逐个改必然漏一处；改在渲染器上则"有投影就画投影的字"对所有调用点自动成立。
