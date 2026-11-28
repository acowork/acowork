# ADR-119 轮次交付物行：工具产出的文件与图片按轮次汇总为一条转录行

- **状态**：Accepted（运行时 + 桌面端已实现，Windows 已验证；2026-11 修订决策 4 与决策 7，见各节；**2026-11 机制重构：回合末系统行退役，产出事实改盖 tool_result，展示策略归前端**——见下方修订节）
- **日期**：2026-11-05
- **决策人**：大鱼
- **相关**：ADR-046（附件作为用户轮次的组成部分）、ADR-107（消息引用 part）、ADR-118（桌面子 webview 内嵌浏览器）、ADR-024（会话合并元数据进索引）

## 修订（2026-11）：产出事实改盖 tool_result，展示策略归前端

回合末的 `deliverables` 系统行**退役**。本修订取代决策 1/2/3/5/6 中"独立系统行 + 运行时裁决"的机制；决策 4（图片落盘、通道只走坐标）与决策 7（点击复用查看器）不变。

**新机制**：

- **后端只供事实。** 工具结果持久化时（`persist_and_emit_tool_results`），把该 tool_call 声明的产出按坐标映射后，盖到**这条 `tool_result` 自己的 `metadata.produced`** 上：`[{workspace_id, rel_path, mime, bytes, created}]`。`turn_deliverables::to_coordinates` 只做路径→坐标映射与噪声过滤（MIME 可判、大小上限），不再去重/限量。图片字节落盘（决策 4）保留。
- **事实不进模型上下文。** restorer 重建模型消息只取 `content` / `tool_call_id` / `tool_name`，`metadata.produced` 对 LLM 输入零影响——它是给前端的事实注解，不是推理材料。
- **实时通道同步携带事实。** `record_complete` 事件新增 `metadata_json`（proto 加性字段），镜像该条 JSONL 行的 metadata；桌面端把它合进 `toolData`，使**实时投影与还原投影一致**——banner 在回合内即可推导，无需额外 HTTP 回源（旧机制靠"只持久化不下发"的系统行制造 count 分歧触发刷新，已随系统行退役）。
- **前端统一推导。** [deriveDeliverables.ts](../../../apps/acowork-desktop/src/lib/deriveDeliverables.ts) 一个纯函数，输入一个回合的全部 `tool_result`，输出该回合的交付物：文件/图片读 `metadata.produced`（`created` 过滤、去重、每类上限 8、图片在前），doc 读 MCP 结果信封。展示时机/位置/取舍全在前端。
- **两种行形状，一段推导（2026-12 补）。** 历史页走 `groups=stub` 投影，一段 explore run 折成一条 `explore_stub`，承载事实的 `tool_result` 行随之消失。折叠因此**原样转发**这些行的产物事实（[artifact_facts.rs](../../../core/acowork-runtime/src/agent/artifact_facts.rs)：按 `FACT_KEYS` 白名单取键、逐字转发、只做投影不落盘、不认工具名），前端在上面的同一段推导里读 `exploreStub.facts`。不转发的话，同一个回合在 live 与重新加载的历史页里会重建成两个样子——历史页干脆没有交付物行。
- **新增交付物类别：doc 服务在线文档。** `doc_write`/`doc_move` 成功（`status=applied`）的结果信封由 [deliverableDocs.ts](../../../apps/acowork-desktop/src/lib/deliverableDocs.ts) 解析为 doc 卡片（按 doc_id 去重保留最后一次），点击在文件 tab 打开**只读** Markdown 预览（`fileEditorStore.openDocPreview`，按 doc_id 走 Gateway doc 代理，复用 `DocMarkdownView`）。doc 识别是独立适配器，不污染文件交付物的工具无关路径。
- **文件类型图标。** 文件胶囊改用工作区同款 SetiIcon（复用 `fileIcons.getFileIcon`），不再统一 FileText。

**兼容**：旧会话已持久化的 `deliverables` 行仍由 `parseDeliverableFiles` 读取；新会话不再写该行。

**可证伪判据依旧成立**：文件交付物路径（`turn_deliverables.rs` / `deliverableFiles.ts` / `ChatDeliverablesRow.tsx`）不判工具名；doc 识别隔离在 `deliverableDocs.ts` 适配器内。

## 背景

一个轮次可能跑上百次工具调用。用户要看的是**结果**，不是这一百步。而目前的转录结构里：

- 工具调用/结果/思考全部折进 `explore_group` 块，**默认折叠**；
- 一屏之内会出现十几个 `Explored (N steps)` 与 assistant 文本交替的块；
- 因此任何"挂在工具块上"的产物入口，要么在折叠里面（看不见），要么每个块都挂一个（很乱）。

同时存在两个独立需求汇到同一处：

1. **浏览器自动化**：agent 用 Playwright 之类的第三方 MCP 做页面调试，截图是它唯一的输出形态。桌面端必须能让用户看到这些截图，否则"agent 在远端浏览器里做了什么"永远不可见。
2. **通用产出**：agent 写出的报告、图表、PDF、数据文件，同样是"这一轮做了什么"的证据。

这两件事在实现上必须解耦：运行时不认识 Playwright，也不认识"浏览器自动化"。它只知道一件事——**工具的返回里有图像字节**。任何返回图片的 MCP 都自动获得展示能力；任何不再返回图片的工具，一行代码不改就从行里消失。

## 决策

### 决策 1：一行**一个轮次**，不按步

交付物行是轮次结束时写入转录的一条 `system` 行（`metadata.type = "deliverables"`），位于该轮最后一条 assistant 消息**之后**（视觉上在其下方）。中间过程的产出只记录、不渲染。

理由只有一个，且是可量化的：**行的数量不能随步数增长**。按步渲染在 3 步的轮次里好看，在 111 步的轮次里就是噪声；按轮渲染在任何轮次里都是一行。

### 决策 2：判定依据是"工具自报 + 运行时裁决"，不是事后探测

把文件变成交付物需要两个角色，缺一不可：

- **工具自报**（`ToolContext::record_produced_file`）：只有写者知道"这个路径在我写之前是否存在"。这不是能猜出来的事实——`mtime` 会被 checkout/拷贝骗，全树快照在远端节点上 IO 不可控，解析工具结果文本要同时猜三种路径写法。
- **运行时裁决**（`turn_deliverables::collect`）：读取上下文无关的规则——是否 `created`、是否落在已知工作区根内、是否踩到噪声目录、MIME 是否可判、大小与数量是否在上限内、按 `(workspace_id, rel_path)` 去重。

裁决留在运行时、声明交给工具，是为了让"新增一个写文件的工具"只做一件事：声明。所有规则只有一份，不会被某个工具偷偷绕过。

### 决策 3：`created` 是唯一的"产出"语义，改写不算

`created = 写入前该路径不存在`。

- 改写一个已存在的源文件，是**过程**，不是产出——用户不会想在交付物行里看到自己 `src/main.rs` 的每一版。
- 本轮内多次写同一路径：第一次 `created=true` 入账，后续写入 `created=false`，去重保留第一条。
- 上一轮创建、这一轮修改：不算本轮产出（这正是需要的语义）。
- `file_edit` 只可能编辑已存在的文件（`old_text` 必须匹配），因此它**不上报**——一个只可能报告 `created=false` 的调用点没有存在的意义。

### 决策 4：图像的字节落在节点上，消息通道只走路径

工具返回的 `content_parts` 里的 `data:` URI 图片，由运行时解码后写入 `<agent_home>/artifacts/<session_id>/<stem>-<content tag>.<ext>`，并作为一条 `ProducedFile` 入账。

> **修订（2026-11）**：初版是 `<tool_call_id>-<index>.<ext>`。它确定，但对人毫无意义——胶囊、编辑器标签页、系统文件管理器里都写着 `call_01a2095c7dba7734ae7a-0.png`；同一张图被读两次还会变成两条交付物。现在名字的两半都由运行时从**已经掌握的事实**算出：`<stem>` 是这次调用针对的文件（`image_read` 的 `path`；不针对文件的工具退化为工具名，再退化为 `image`），`<content tag>` 是字节的摘要前缀。确定性反而更强：同文件同 detail ⇒ 同路径同字节 ⇒ 行里只有一条；同 stem 不同字节（不同源文件，或不同 detail）⇒ 不同名字，互不覆盖。`<stem>` 必经净化（只取最后一段、非字母数字归一为单个空格、Windows 保留名加前缀、48 字符截断、保留非 ASCII），因为那个路径来自模型。

桌面端**不接收字节**：它拿着 `(workspace_id, rel_path)` 走既有的工作区文件路由（`GET /workspace-files/{agentId}/{workspaceId}/{path}`，ADR-055 L2-7）取回，字节缓存复用 [workspaceFileBlob.ts](../../../apps/acowork-desktop/src/lib/workspaceFileBlob.ts) 的 LRU。这与桌面右侧工作区预览文件是**同一个流程**，不是第二套。

后果是好的：base64 不上 MQTT，远端节点不必为"用户可能看一眼"付出传输成本，缩略图与全图共用同一份缓存。

`http(s)` 的图片 part 不解码也不下载：那会让一条交付物行的可用性依赖别人的服务器。

### 决策 5：写入点收敛到轮次唯一出口

"一行一个轮次"要成为代码的性质，而不是每个调用点都要记得的规则。轮次结束有四个 `ChunkEvent::Done` 出口（正常、重放、错误、中断），但它们在 `run()` / `replay()` 之下都收敛到同一个函数 `AgentLoop::close_turn_round`——交付物行就在那里写，且**在** `end_round` **之前**（行要盖上本轮 round id，也要排在它所描述的 assistant 行之后）。

同一次收口顺带保障了另一件事：sink 在每个轮次入口被清空，上一轮的文件不会漏进下一轮的行。

### 决策 6：行的位置是"独立行"，不是"用户轮次的一部分"

`metadata.type` 参与两个不同的问题，此前只有一种答案：

| 问题 | 附件 / 引用 | 交付物行 |
|---|---|---|
| 这条行的 metadata 要不要保留？ | 要 | **要** |
| 这条行要不要折进上面的用户轮次？ | 要 | **不要** |

因此注册表拆成两组：`isTurnPartMeta`（保留 metadata）包含全部三类，`isUserTurnPart`（折叠进用户块）**不包含**交付物行。把交付物行直接加进现有集合，会让缩略图折到上一轮用户消息底下——它在语义上属于 assistant 的回答，不属于人的提问。

桌面端渲染时的额外收益：`system` 在 `avatarAnchor` 的 `SKIP_TYPES` 里，行本身不会长出多余的头像，也不会打断后续 agent 块的既有规则。

### 决策 7：点击复用既有查看器，且走聊天侧那一条开文件规则

缩略图/胶囊的点击与聊天里**其他一切可点的文件引用**（引用胶囊 `openWorkspaceRef`、消息里的 `<a>`）共用同一个判定 `pickOpenActionForPath`：图片 ⇒ `openPreview`，其余 ⇒ `openFile`。落到同一个编辑器标签页（缩放、平移、区域回投），零新增查看器组件。

> **修订（2026-11）**：初版写的是"点击调 `openFile`，与工作区树里点同一张图片完全同一个动作"。这句话当时就不成立：`openFile` 建的是 **edit 模式**的标签页，而编辑器 `FileEditorPanel` 的图像分支要求 `mode === "preview"`（工作区树对可预览文件走的是 `openPreview`）。于是交付物行里的图片点开是 Monaco 里的一串 base64——图片在 `BINARY_EXTENSIONS` 内，JSON 读接口返回的就是 base64 文本。修法不是再发明一条规则，而是接入既有的那条：聊天侧统一用 `pickOpenActionForPath`。
>
> 之所以不与工作区树的 `isWorkspacePreviewable` 合并：那条规则把 `.md` 也判为预览，而聊天侧点开 `.md` 期望的是源码（大 JSON / Markdown 走预览渲染曾经直接冻结页面）。两套规则的分工见 [filePreview.ts](../../../apps/acowork-desktop/src/components/editor/filePreview.ts) 头注释。

同一份缓存、同一条路由、同一组坐标还带来一个附带要求：缩略图的取字节必须复用编辑器预览的 **`useWorkspaceFileBlob`**，而不是在组件里手写一次 `fetch`。初版手写的 effect 把"是否还活着"的标记放在了组件实例上而不是单次运行上，StrictMode 双挂载（以及虚拟列表回收行）时第一次运行的 `AbortError` 在第二次运行重新置位之后才到达，被当成下载失败并锁死成胶囊——图片全部退化成文件banner。

## 未纳入 v1 的部分（已知洞，明确记录）

| 洞 | 现状 | 结论 |
|---|---|---|
| `bash` 写出的文件 | 看不见 | v1 不纳入。可选后续：`git status --porcelain` 差异，但会把未跟踪噪音带进来，需先设计过滤 |
| 非 MIME 可判的文件（`.ts`、`.rs`、无扩展名） | 记录后在裁决阶段丢弃 | 交付物行只提供**可打开**的东西；一个点开是坏视图的胶囊比没有更糟 |
| 大于 50 MB 的文件 | 丢弃 | 行要在远端节点上仍可读，超大文件的预览是另一个问题 |
| 中间过程的图片 | 入账但同一轮次被去重/上限截断 | 一行的价值在"这轮产出了什么"，不在"这轮一共画了多少张图" |
| 每个来源的数量上限 | 图片 8、其他 8 | 再多的完整集合仍在磁盘上，行只需要可扫视 |

## 失败形态

| 情况 | 行为 |
|---|---|
| 工作区 resolver 未绑定（未走启动装配的循环、单测） | 不写行。宁可没有行，也不伪造 `__agent_home__` 让链接 404 |
| 会话没有转录写者 | 不写行（`conversation` 为 `None`） |
| 图片 part 不是合法 base64 | 跳过该 part，记 `warn`，不影响其他 part 与该轮结束 |
| 写 artifact 失败（磁盘满、权限） | 记 `warn`，该文件不入账，轮次正常结束 |
| 元数据读取失败 | 视为"文件不存在"，不入账，不报错 |

一条交付物行永远不是轮次失败的理由：所有失败路径都是"少一个缩略图"，不是"这一轮挂了"。

## 可证伪判据（本决策的验收方式）

把 Playwright MCP 换成任何一个返回图片的三方 MCP：

- 运行时：`git diff` 为空。运行时不认识工具名。
- 桌面端：`git diff` 为空。渲染只认 `metadata.type`，不判工具名。
- 唯一变化：新 MCP 的截图出现在同一行里。

反向判据：如果哪天为了某个具体工具在 `turn_deliverables` 或 `ChatDeliverablesRow` 里加了 `if tool_name == ...`，这条 ADR 就被违反了。

## 附带的修复

内置 `image_read` 工具返回的图片，此前只进模型上下文、不进 UI——模型看到了用户看不到。这条通用规则落地后它一并修好：它返回的图片 part 走同一条落盘与入账路径，无需在 `image_read` 里加一行代码。

## 相关文件

- [core/acowork-runtime/src/agent/turn_deliverables.rs](../../../core/acowork-runtime/src/agent/turn_deliverables.rs)
- [core/acowork-core/src/tools/traits.rs](../../../core/acowork-core/src/tools/traits.rs)（`ProducedFile`、`ProducedFileSink`、`ToolContext::record_produced_file`）
- [core/acowork-runtime/src/tools/builtin/file_write.rs](../../../core/acowork-runtime/src/tools/builtin/file_write.rs)
- [apps/acowork-desktop/src/lib/deliverableFiles.ts](../../../apps/acowork-desktop/src/lib/deliverableFiles.ts)
- [apps/acowork-desktop/src/components/chat/ChatDeliverablesRow.tsx](../../../apps/acowork-desktop/src/components/chat/ChatDeliverablesRow.tsx)
- [apps/acowork-desktop/src/components/chat/messageParts.ts](../../../apps/acowork-desktop/src/components/chat/messageParts.ts)