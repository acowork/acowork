# Chat 转录轮次头（Turn Header）统一设计

> 版本：v1.0 | 更新日期：2026-11-08
>
> 关联：[ADR-106](../../adr/zh/ADR-106-chat-execution-reference-projection.md)（D9 呈现契约，本设计提出其修订）、[ADR-049](../../adr/zh/)（ProcessingPhase 单一词表）、[ADR-105](../../adr/zh/)（群默认 responder）、[ADR-055](../../adr/zh/)（跨机可达性约束）、设计 27（Intent 消息与群聊）、设计 29（Chat Router）

## 1. 执行摘要

执行 session 在用户发送后**立即**显示 assistant 头像 + 名字 + 阶段状态行；chat session（单聊与群聊）在同一时刻只显示一条居中的通用等待行，agent 的头像 + 名字要等到轮次引用行与块投影落地才出现。这个延迟是结构性的：chat 转录的内容面走磁盘投影 + 门铃（ADR-106 D3/D5），而等待窗口（D9 claim）被刻意做成了"平台自己的声音"——无名、居中、无气泡。

本设计把等待窗口的呈现从"居中通用行"升级为**轮次头（Turn Header）**：发送后立刻在转录尾部渲染"预期响应者"的头像 + 名字 + 状态行；当轮次的执行块或 agent 回答行到达时，claim 结束、块自己的头像头（已有）在同一锚点接管——即"内容覆盖状态"。状态词表不新建，复用 `getProcessingPhase()`；预期响应者的名字不是猜测，而是投递决策的输入本身（@ 提及 / 群默认 responder / 单聊对端），服务端回执与收口行负责证伪。

改动是纯呈现层的：无新网络接口、无 proto 变更、无存储变更。需要一份 ADR-106 D9 的修订章节（草案见 §9.1）。

## 2. 背景

### 事实（已核对代码）

- 执行 session 的即时反馈来自对本地执行 session live 状态的订阅：`chatStore.agentStates[instanceId].sessionStates[sid].sessionStatus`，词表为 ADR-049 的 `ProcessingPhase`（[lib/types.ts:1615](../../../apps/acowork-desktop/src/lib/types.ts#L1615)）。
- chat 侧现状：单聊 `AgentChatView.tsx:1260` 与群聊 `GroupChatPanel.tsx:1789` 在 `composerLocked`（本设备发送的 D9 claim 在窗）时渲染居中 `ChatTurnStatusLine`，文案通用（群聊）或带名字的单聊文案，但**不带头像、不带阶段**。
- 执行块 `ChatExecutionBlock` 已经渲染头像 + 名字头（`AgentAvatar`，行 436），头部状态词遵循"live 词优先、读派生词兜底"（行 297），且 live 订阅仅限视口内块所属会话、不入库、不进上下文、不进未读（ADR-106 D9 第 4 条）。
- claim 机制：`chatSessionStore.armTurn(chatSessionId, rowId)`，窗口起于本设备那次发送的 `delivered` 回执，止于第一条应答行或截止（ADR-106 修订 2026-11-03）；`archived`（无投递目标）与全失败 fan-out 不 arm。
- 发送时前端已知的响应者信息：单聊 = 会话对端 agent；群聊带 @ = `sendGroupText` 的 `targets` 参数；群聊不带 @ = chat session 的 `responder` 字段（GroupResponderCard 读写的同一个字段，服务端存储）。
- 头像锚定机制已存在（`avatarAnchor.ts` / `bubbleMetaAnchor.ts`），同发言者相邻气泡折叠头部、块总是开新轮。
- Mobile 的 `AgentChatScreen.tsx` 有同款等待行，转录形状与 Desktop 统一（ADR-106 P6）。

### 假设

- 投递决策的输入（提及 / 默认 responder）与实际投递目标在 claim 窗口内一致；不一致的情形（router 改判、投递失败）都由服务端收口行表达，且收口行即终止 claim——轮次头随之消失，因此错误名字的生命周期被收口行天然封顶。

### 约束

- ADR-106 D9 的三条不变量必须保留：(1) 终态（aborted / failed / timeout / waiting）仍是居中平台行——停止不是 agent 的话；(2) live 状态只作呈现层附加，不入库、不进上下文、不进未读；(3) 不可达 ≠ 进行中。
- ADR-049：状态词表唯一真相是 `getProcessingPhase()`，禁止平行枚举。
- ADR-055：轮次所在执行 session 可能在远端节点，live 状态不可得是常态而非异常。
- Gateway 边界红线：chat 呈现数据只能经 Runtime/Gateway HTTP 取，不得直接读文件系统（本设计不新增任何数据面）。

## 3. 需求

### 功能需求

- FR-1: 单聊发送后（`delivered` 回执起），转录尾部立即显示对端 agent 的头像 + 名字 + role + 状态行（role 行为 2026-11-08b 修订补入，见 §9.1）。
- FR-2: 群聊发送后，按响应者解析规则显示预期响应者的头像 + 名字（多目标显示头像堆叠 + 名字列表，且不渲染 role 行）+ 状态行。
- FR-3: 轮次引用行到达（执行块渲染）或 agent 回答行到达时，轮次头消失，内容由同一头像锚点下的块头 / 气泡头接管，无视觉跳动。
- FR-4: `archived` 发送、全失败 fan-out、页面重载（无本机 claim）不显示轮次头——与现等待行的判据完全一致。
- FR-5: 终态收口（aborted / failed / timeout）仍渲染为居中平台行，不套头像。
- FR-6: claim 窗口内若轮次绑定的执行 session 在本机驻留且可得（Phase 2，可选），状态行显示真实 ProcessingPhase 词；否则显示通用"处理中"shimmer。

### 非功能需求

- NFR-1: 呈现层改动，不新增网络接口、proto 字段、存储写入。
- NFR-2: 不劣化转录虚拟化性能——轮次头是单行合成节点，随窗口快照派生，不进 messageFolder 的块折叠。
- NFR-3: Desktop 与 Mobile 形状统一（同一契约，两个宿主）。

### 非目标

- 不做 chat→execute 的消息镜像（ADR-106 已废止"副本"路线）。
- 不改投影管道的新鲜度（提前 flush、加门铃触发点等）。
- 不改路由与投递语义（Chat Router、默认 responder、fan-out 判据均不动）。

## 4. 备选方案

| 方案 | 概述 | 优点 | 缺点 | 风险 | 可逆性 |
|------|------|------|------|------|--------|
| **A（推荐）** | 轮次头：claim 窗口内渲染预期响应者头像 + 名字 + 状态行，内容行到达即被块头接管 | 纯呈现层；复用 claim / 锚定 / 词表全部既有机制；反馈即时 | 需要 ADR-106 D9 修订；群聊名字依赖投递决策输入 | 低：收口行封顶错误名字的生命周期 | 高：还原两处调用点即回滚 |
| B | 提前投影：轮开始即 flush + 加门铃触发点，让块更快出现 | 不引入新呈现形状 | 动 D5"终态轮不可变"缓存不变量的邻域；send→路由→announce 的链路延迟仍无法归零；跨机场景收益更差 | 中：缓存与门铃语义变更 | 低 |
| C | 把执行 session live 流镜像进 chat 转录 | 反馈最丰富 | 直接违反 ADR-106"引用而非副本"的边界（该路线正是被废止的对象） | 高：架构契约倒退 | — |

选 A 的理由：用户感知的延迟只来自"claim 窗口无身份"这一段，而块本身到达后的呈现（头像 + 名字 + live 词）已经和执行 session 一致。A 精准补这一段，且不碰任何数据面。

## 5. 推荐架构

```
用户发送 ──202 delivered──▶ armTurn(claim)
                              │
                              ▼
                 ┌─ ChatTurnHeader（合成行，转录尾部）
                 │   avatar(s) + 名字 + 状态行(shimmer "处理中" / Phase 2 真实阶段词)
                 └── 第一条应答行(引用行/agent 消息/回执) 或截止 ──▶ claim 结束
                              │
                              ▼
                 ChatExecutionBlock 自有头像头在同一锚点接管（现状机制）
```

### 模块：ChatTurnHeader（新，Desktop `components/chat/`）

- **职责**：渲染轮次头——头像（多响应者时是本组件自己的堆叠头像，非侧栏 `GroupAvatarStack` 的方形拼贴）+ 名字链 + 一行状态词；复用 `ChatEventText` 的 live shimmer 语法。
- **依赖**：agent 目录（头像 / 显示名解析，与两视图现用同一解析路径）；`getProcessingPhase`（Phase 2）。
- **公开接口**：`<ChatTurnHeader identities={TurnIdentity[]} text={string} className?={string} />`——多身份即堆叠；名字链按 locale 由 `Intl.ListFormat` 拼接（zh/ja 为 "A、B"）；Phase 2 在此追加 `phase` 入参，缺省即通用"处理中"。
- **失败模式**：身份解析不出（目录未就绪）→ 退化为现居中通用行，不渲染无名头像。

### 模块：claim 元数据扩展（`chatSessionStore`）

- **职责**：armTurn 时把发送时已知的"预期响应者实例 id 列表"记入 claim 窗口状态，供视图派生轮次头。
- **依赖**：调用方在 `sendGroupText` / 单聊 send 处已有 `targets` / 对端 id / `responder` 读取。
- **公开接口**：`InflightTurn` 新增只读字段 `responders?: string[]`（claim 结束即清除）；不改 armTurn 的收口判据。
- **失败模式**：字段缺失（旧数据 / 重载）→ 无轮次头，等同现行为。

### 模块：AgentChatView / GroupChatPanel 调用点

- **职责**：把 `composerLocked && <ChatTurnStatusLine …/>` 替换为按 §5 响应者解析规则渲染 `<ChatTurnHeader>`；`block.notFound` 等空洞类文案保持居中平台行不变。
- **响应者解析规则**（单点函数 `resolveExpectedResponders`，两视图共用；逐级镜像服务端 `deliver_mentions` / `addressed_execution_round`）：
  1. 单聊：对端 agent 实例（1 个）；
  2. 群聊回复引用某个轮次：该轮的 agent 单独一个——服务端对 `in_reply_to` 按全量日志解析，命中轮次时忽略正文里的 `@`；被引用的行不在本面已加载窗口内时属"未解析"而非"没有"，此时不具名（退化为居中行），不拿 `@` 列表去猜；
  3. 群聊带 @：`targets` 列表；
  4. 群聊不带 @：chat session `default_responder` 字段指向的 agent；无 responder（归档路径）→ 不渲染（该发送不 arm）；
  5. 多目标：全部列出（堆叠头像 + locale 的名字链）。
- **失败模式**：responder 读取失败 → 通用行兜底。

### 模块：Phase 2（可选）——claim 窗口真实阶段词

- **职责**：claim 在窗且轮次绑定执行 session 本机驻留时，订阅其 `sessionStates[sid].sessionStatus` 并显示 `getProcessingPhase` 词，规则与 `ChatExecutionBlock` 头一致（live 词优先、不入库、仅视口）。
- **依赖**：轮次坐标在 claim 早期未必存在（引用行未到）；解析路径 = 按 D11 派生索引"该 chat 绑定的执行会话"查本机驻留会话，查不到即回退通用词。
- **失败模式**：远端 / 未驻留 → 通用"处理中"（与 Phase 1 相同，非异常）。

### 模块：Mobile `AgentChatScreen`

- 与 Desktop 同一契约与形状；身份解析复用 Mobile 现有目录读取路径。

## 6. 数据流

1. 发送 → `delivered` 回执 → `armTurn(chatId, rowId, responderIds)`；
2. 视图从窗口快照派生轮次头（无新请求）；
3. （Phase 2）本机驻留时经既有 MQTT `session_state` 推送更新阶段词——呈现层附加，零持久化；
4. 第一条应答行 / 回执到达 → claim 结束 → 轮次头随快照消失，块头 / 气泡头渲染；
5. 截止 → claim 结束，无头（终态由服务端收口行表达，若有）。

## 7. 安全与可运维性

- 信任边界：无变化。轮次头是纯客户端呈现，名字来源是本机已知的投递决策输入；权威事实（谁真正应答）仍由服务端行表达。
- 权限：无新增；头像 / 名字解析走参与者既有可读路径（chat 参与者本就可见成员资料）。
- 可观测性：轮次头渲染与 claim 窗口同生命周期，现有 `[chatSessionStore]` 日志已覆盖 arm/resolve；不新增日志面。
- 运维风险：错误名字（改判 / 失败）的存活窗口 = 发送→收口行，与现等待行同窗，无新增持久错误状态。

## 8. 交付计划

| 阶段 | 目标 | 验证 | 回滚 |
|------|------|------|------|
| P0 | ADR-106 D9 修订章节评审（草案 §9.1） | 架构组评审通过 | 不合码 |
| P1 | `ChatTurnHeader` 组件 + 单聊调用点 + armTurn 元数据 | 单测：delivered 后立即可见、应答行到达即消失、archived/重载不可见、终态仍居中 | 还原调用点（一个 commit） |
| P2 | 群聊调用点 + `resolveExpectedResponders`（@ / responder / 多目标堆叠） | 更新 `GroupChatPanel.waitLine.test.tsx` 等既有用例 + 新用例 | 同上 |
| P3 | Mobile `AgentChatScreen` 对齐 | Mobile 形状快照测试 | 独立 revert |
| P4（可选） | claim 窗口真实阶段词（本机驻留订阅） | 与 `ChatExecutionBlock` 头部行为一致性用例 | 关闭即回 P1 通用词 |

P1/P2 可独立评审、独立合入；每阶段不跨模块。

## 9. 关键决策

| 决策 | 选择 | 理由 | 被否备选 | 权衡 |
|------|------|------|----------|------|
| 状态词表 | 复用 `ProcessingPhase` + 一个 claim 态"处理中" | ADR-049 单一真相；claim 态是"窗口存在"而非"阶段"，不扩枚举 | 新建 chat session state 枚举 | 词表少一档精确性，换零漂移 |
| 群聊命名依据 | 投递决策输入（@ / responder），由收口行证伪 | 名字是决策事实不是猜测；D9"不猜响应者"针对的是转录形状反推 | 保持通用无名行（现状） | 极短窗口内可能显示随后被改判的名字 |
| 终态呈现 | 维持居中平台行 | D9 不变量：停止不是 agent 的话 | 终态也套头像 | 与执行 session 的终态行风格本就一致 |
| 接管机制 | 复用块自有头像头 + avatarAnchor | 机制已存在，覆盖=claim 结束，无新折叠逻辑 | 轮次头常驻并向下生长 | 依赖引用行到达的既有时序 |
| Phase 2 live 词 | 可选、本机限定、视口限定 | 与 D9 第 4 条同构；跨机不可得是常态 | 强制全链路即时 | 跨机场景仍显示通用词 |

### 9.1 ADR-106 D9 修订草案（待评审，通过后并入 ADR 文末修订章节）

> **修订（2026-11-08·轮次头）**：D9 的等待行在"本设备发送的 claim"窗口内由居中通用行升级为**轮次头**：渲染预期响应者（单聊对端 / @ 提及 / 群默认 responder，均为投递决策输入而非转录形状反推）的头像 + 名字 + 状态词；第一条应答行或截止即由块自有头像头在同一锚点接管。"不猜响应者"的禁令收窄为"不从转录形状反推响应者"；投递决策输入可用，未解析成功时退化为通用居中行。三条不变量（终态居中行、live 仅呈现层、不可达≠进行中）原样保留。判据、窗口与收口机制不变（仍是 D9 的 claim）。

> **修订（2026-11-08b·角色行）**：轮次头形状定为**头像 + 名字 + role + 状态词**。role 取自 agent 目录的同一份 meta（`ChatExecutionBlock` 块头与常规 agent 消息头用的就是它），层级也与它们相同（name 下一档）。理由不是"信息更全"，而是 ADR-106 决策四的接管不变式：块头渲染名字 + role，若 claim 窗口只有名字，接管时就要在名字下**插入**一行、整列下移，而轮次头存在的意义正是"内容覆盖状态"而非"重排"。多目标 fan-out **不渲染** role 行：拼接的 role 列表不对应任何单一响应者，头像堆已经表达"多人"。

## 10. 开放问题

- OQ-1: 群聊多目标时，头像堆叠 + 名字列表 vs 每目标一行轮次头——推荐堆叠（一发送一事实），待评审定。
- OQ-2: "处理中"claim 态的 zh/en 文案表（与现 `chatView.notice.pendingTurn*` 的取舍）。
- OQ-3: Phase 2 的"该 chat 绑定的执行会话"本机解析是否已有现成前端索引可用；若无，是否值得为其加只读查询（P4 前定）。

## 11. 自检

- [x] 需求覆盖（FR-1..6 均有模块落点与验证）
- [x] 无占位符（接口、解析规则、回滚均为具体条款）
- [x] 接口显式（组件 props、store 字段、解析函数单点）
- [x] 测试与交付定义（每阶段验证 + 独立回滚）
- [x] 契约变更有修订草案（§9.1）且评审门前置（P0）
- [ ] OQ-1..3 在 P0/P2/P4 评审时关闭——不阻塞 P1
