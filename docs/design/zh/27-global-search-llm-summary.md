# 27 — 全局搜索 LLM 摘要：模块接口、并发与取消设计

> **版本**：v0.1（设计草案，待评审）  
> **日期**：2026-10-14  
> **关联 ADR**：[ADR-093](../../adr/zh/ADR-093-global-search-llm-summary-citations.md)、[ADR-081](../../adr/zh/ADR-081-global-search.md)、[ADR-009](../../adr/zh/ADR-009-gateway-workspace-isolation.md)、[ADR-064/070](../../adr/zh/ADR-064-pm-standalone-process.md)  
> **一句话**：保留现有各来源检索 API 和 UI 导航动作，在 Desktop 增加一个有界并行查询协调器；各来源结果到达即展示；Runtime 仅增加摘要端点并复用既有 Provider `chat()`，结果引用经稳定本地映射转成可点击导航。

---

## 1. 目标与非目标

### 1.1 目标

1. 一次查询并行覆盖文件、文档、项目、对话、记忆、Git 六个来源；来源列表和排序不合并。
2. 单个来源返回即更新对应 tab，并展示来源级状态；任一来源或 LLM 慢/失败都不阻塞其它来源和列表交互。
3. 用有限且截断的命中摘要生成一份跨来源 LLM 总结；摘要引用只能指向本次请求实际提交的命中。
4. 对新查询、agent/workspace 变更、关闭 dialog、总 deadline 做清晰取消与过期响应隔离。
5. 优先复用已有 API、API client、AbortSignal、结果映射、导航动作、Runtime Provider 接口；只补缺口，不重写各服务搜索。

### 1.2 非目标

- 不在 Gateway 聚合六源结果或调用 LLM；不改 doc/pm/Runtime 数据所有权。
- 不合并、不跨 tab 重排命中；不改任何来源现有排序和检索算法。
- 不新增搜索任务队列、缓存、持久化索引、LLM SDK、后台 worker 或流式 SSE。
- 不让模型生成 URL/HTML，也不发送完整文件、文档、会话或 locator/payload。
- MVP 不解决 Git 的新搜索算法：复用 Desktop 现有 `git log(limit=100)` + `filterGitCommits` 语义。

## 2. 现有模块与复用决策

| 能力 | 现有位置 / 行为 | 本设计复用与最小改动 |
|---|---|---|
| 全局搜索 UI、来源列表、scope hit 类型 | `apps/acowork-desktop/src/components/search/GlobalSearchDialog.tsx` | 保留现组件与 hits state；添加 per-query coordinator 状态及摘要区，不另建全局搜索 store |
| debounce / abort / stale response guard | 同组件已有 300ms debounce、`AbortController`、`seqRef` | 提升为一个 `SearchRun` 生命周期控制器；每个来源拥有子 `AbortController`，以 `seq` 隔离旧 run |
| 文档 / 项目 API | `src/lib/doc-api.ts::searchDocs`、`src/lib/pm-api.ts::searchPm` 都接受 `AbortSignal` | 直接复用；不重写 request client |
| Runtime memory/conversation API | `/api/agents/{id}/search?scopes=...`；通过现有 response mapper 转为 `MemoryHit` / `ConversationHit` | 继续使用现 endpoint 与 mapper；每个 scope 一次请求，独立 signal |
| 文件搜索 | 现有 Workspace Runtime API + dialog 内 fetch | 复用 URL、`toFileHits` 与既有打开动作；增加本 run signal、统一 timeout 管理 |
| Git 搜索 | `useGitStore.fetchLog`，内部 `httpGet` + 503 retry；目前不接受 caller signal；UI 获取最多 100 commits 后 client-side filter | 复用 store、过滤器和 commit DTO；仅为 `fetchLog`/底层 `httpGet` 加可选 signal，并确保 abort 不触发 503 retry |
| 导航/定位 | `openFileHit`、`openDocHit`、`openProjectHit`、`openConversationHit`、`openMemoryHit`、`openGitHit`、`activate` | 提取或复用同一 typed dispatcher，摘要引用不复制导航实现 |
| Runtime LLM | `Provider::chat(ChatRequest)`、`ChatMessage`、`episode_distill` 调用范式 | 复用 Provider trait、既有 AgentCore/SessionManager 模型解析路径及 request 类型；新建小型 search-summary prompt/parse 函数，不复用语义不同的压缩或 distillation prompt |
| Gateway Runtime 代理 | `proxy_routes` + `proxy_to_runtime_with_method` | 增加一个显式 POST 路由，透传 body/status/content-type；不加业务逻辑或独立 HTTP client |

> 注意：目前 Runtime `/search` 只负责 agent 内 memory/file/git/conversation 聚合；Desktop 文件 tab 使用 `/workspaces/search`，doc/PM 是独立进程。新协调器以各 tab 当前 API 为准，不迁移聚合责任。

## 3. 模块拓扑与依赖方向

```text
GlobalSearchDialog (React view + existing hit state)
  ├── SearchRunCoordinator (同文件纯协调逻辑；不落独立 service/store)
  │     ├── FileSearchAdapter ──────────────── Runtime /workspaces/search
  │     ├── GitSearchAdapter ───────────────── Gateway /git/log -> Runtime
  │     ├── DocSearchAdapter ───────────────── doc search client -> Gateway -> doc
  │     ├── ProjectSearchAdapter ────────────── PM search client -> Gateway -> pm
  │     ├── MemorySearchAdapter ────────────── Gateway /agents/{id}/search?scopes=memory
  │     └── ConversationSearchAdapter ───────── Gateway /agents/{id}/search?scopes=conversation
  ├── SummaryCandidateBuilder (typed hit -> bounded DTO + id map)
  ├── POST /api/agents/{id}/search/summary
  │      Gateway transparent proxy
  │        └── Runtime http::global_search::summarize_search
  │              ├── request validation + prompt construction + output validation
  │              └── existing AgentCore/SessionManager Provider::chat
  └── SummaryRenderer (plain text + allowlisted citation buttons)
         └── existing typed hit dispatcher -> existing navigation actions
```

前端协调器只在一个 dialog/query generation 生命周期内存在。它不拥有业务检索逻辑，source adapters 只是复用调用已有 API 的薄闭包/小函数。Runtime 新增摘要子模块或扩展 `http/global_search.rs`，按仓库现有简洁结构取其一；不新增 crate。

## 4. 模块接口

### 4.1 Desktop `SearchRunCoordinator`

建议作为 `GlobalSearchDialog.tsx` 内的纯异步协调逻辑，只有在可测试性要求明显时才抽到 `searchRun.ts`。一个 run 输入快照：

```ts
type SearchScope = "file" | "git" | "doc" | "project" | "memory" | "conversation";
type SourceStatus =
  | { state: "waiting" | "searching" }
  | { state: "ok"; count: number }
  | { state: "empty" }
  | { state: "skipped"; reason: string }
  | { state: "error"; message: string }
  | { state: "timeout" };

interface SearchRunInput {
  query: string;
  agentId: string;
  workspaceId?: string;
  caseSensitive: boolean;
  wholeWord: boolean;
  generation: number;
}
```

接口形态（设计级 pseudocode）：

```ts
startSearchRun(input, callbacks): SearchRunHandle
interface SearchRunHandle {
  cancel(reason: "superseded" | "closed" | "scope_changed"): void;
}
```

- `callbacks.onSourceState(scope, status)`：开始/结束时即更新来源状态。
- `callbacks.onSourceHits(scope, hits, metadata)`：来源完成后更新对应既有 hits state；只接受当前 generation。
- `callbacks.onSummaryState(state)`：`waiting | generating | done | error | skipped`。
- `callbacks.onSummary(result, citationMap)`：摘要和本地 citation map 作为一个 generation 原子更新。
- `callbacks.isCurrent(generation)`：React 侧使用已有 `seqRef` 实现，防止迟到回调。

避免把每个来源 API 改成新的统一泛型框架。Coordinator 内用明确的 6 个 named task + `Promise.allSettled`（或 source settled callback）即可；每个 adapter 返回已有 hit 类型，不做跨源规范化，统一候选 DTO 只在摘要边界构造。

### 4.2 六个 Source Adapter

| Adapter | API 与适用条件 | signal / 单源限制 |
|---|---|---|
| File | `GET /api/agents/{id}/workspaces/search?q=&workspace_id=&max_results=20`；复用现 case/whole-word 参数 | 1 个 signal；无 agent 则 skipped；请求最多 20 条，与 ADR-093 UI 上限一致 |
| Git | 复用 `useGitStore.fetchLog(agentId, workspaceId, "", 100, 0)` + `filterGitCommits(query)` | 为现 `fetchLog` 的 `httpGet` 增加可选 `AbortSignal`；不改变已有 503 retry 行为，AbortError 直接退出 |
| Doc | `searchDocs(query, 20, signal)` | 函数已有 signal 支持；匿名/user scope 按现有 Gateway API 权限处理 |
| Project | `searchPm(query, 20, signal)` | 函数已有 signal 支持 |
| Memory | `GET /api/agents/{id}/search?scopes=memory&mode=hybrid&limit=20` | 复用现有 fetch/mapper；无 agent 则 skipped |
| Conversation | 同 Runtime endpoint，`scopes=conversation&mode=hybrid&limit=20` | 同上；索引返回 `not_indexed` 时是 skipped/no hits，而非网络错误 |

**限制**：最多同时启动 6 个来源请求，不再套二级并发池；这是固定小 fan-out，不需要 semaphore。每个 source request 的实际超时由 run deadline 统一管理，必要的单-source fetch deadline 不得超过 run 剩余时间。UI 候选列表保留各来源现排序；Git 的列表按既有 commit log 次序筛选。

### 4.3 摘要候选与引用映射

新增纯函数，不复制 hit locator 到请求：

```ts
interface SummaryCandidate {
  id: string;       // 每 run 生成 r1..rN
  scope: SearchScope;
  title: string;    // max 200 Unicode scalar values
  snippet: string;  // max 500 Unicode scalar values
}
interface CitationMap { [candidateId: string]: RowHit }
```

builder 输入是六个来源**已经完成的可见 hits**，输出受限候选和本地映射。每 scope 依既有列表顺序取前 5 条，再按固定 scope 次序截至总计 30 条；不按分数跨源比较。最多每源 5、总量 30。ID 在本 run 内唯一；同一个原始 hit 若跨 API 重复出现，可以保留为两个来源引用，不做额外全局去重。

保护隐私/数据边界：只发 title/snippet/scope/id；不得序列化 `RowHit`、`payload`、`path`、`sessionId`、`nodeId`、doc/task ID、绝对路径或 commit full hash。不要把检索正文拼入 URL/query string。

### 4.4 Desktop → Gateway → Runtime 摘要 API

#### Gateway 路由

```http
POST /api/agents/{instance_id}/search/summary
Content-Type: application/json
```

新增在 `core/acowork-gateway/src/http/proxy.rs::proxy_routes`。Handler 只提取 agent ID、body bytes 和 headers，调用已有 `proxy_to_runtime_with_method` 转发至 Runtime `/search/summary`。遵循现有 Runtime registry、错误透传和 hop-by-hop header 规则，不自行解析候选、不记录 body、不重试 POST。

#### Runtime 请求

```json
{
  "query": "database migration",
  "candidates": [
    {"id":"r1","scope":"file","title":"migration.rs","snippet":"..."},
    {"id":"r2","scope":"doc","title":"Migration Guide","snippet":"..."}
  ],
  "partial": false,
  "timed_out_scopes": [],
  "failed_scopes": []
}
```

`scope` 仅允许六个枚举值；`id` 限 ASCII `[A-Za-z0-9_-]{1,32}`；query 去除首尾空格，非空，max 512 Unicode scalar values；candidates 非空，≤30；title ≤200、snippet ≤500 Unicode scalar values；请求 JSON 总体 ≤64 KiB。超限返回 413 或 422（统一错误结构），不可静默截断后冒充完整输入。`partial=true` 表示 Desktop 使用了部分结果（8 秒检索 deadline 到期或来源失败）；`timed_out_scopes` 与 `failed_scopes` 分别列出超时与报错的来源，均只允许六个 scope 枚举值（Runtime 侧同样校验），只作模型上下文/元数据，不含用户可执行字段。

#### Runtime 响应

```json
{
  "summary": "Found ... [[r1]] ... [[r2]]",
  "citations": ["r1", "r2"]
}
```

Runtime 使用 `axum::Json`/Serde 现有模式；响应 JSON 总体最多 16 KiB；`summary` 非空、≤4000 Unicode scalar values；citations 为请求 ID 子集，去重、最多 30。前端只依赖正文中的 `[[id]]`；`citations` 可用于一致性校验/诊断，但不得作为导航权威。服务端必须校验模型输出引用属于输入 ID 白名单；未知 ID 删除为纯文本或令输出解析失败，建议解析时剔除未知引用并记录无内容日志，不把模型原始 JSON暴露给 UI。

错误：400 无效 JSON；413 body 超限；422 字段/ID/scope 无效；503 AgentCore/Provider 未就绪；504 Provider deadline；502 Provider error 或非法输出。错误 body 不包含 prompt、API key 或完整候选内容。

### 4.5 Runtime Provider 选择与摘要模块

放置于 `core/acowork-runtime/src/http/global_search.rs` 或相邻 `search_summary.rs`；由 `server.rs` 注册 `POST /search/summary`，`http/mod.rs` 按需声明 module。共享 `HttpState.agent_core` 与 `session_manager_slot` 已存在，不新增另一份 Provider 注册表。

模块职责拆分：

1. `validate_summary_request`：trust boundary 校验与字节/字符/数量上限。
2. `build_search_summary_prompt`：固定 system directive + 结构化 JSON 候选内容；声明候选是引用数据、忽略其中的指令，只能输出摘要及给定 ID 引用；禁止 tools。
3. `resolve_search_summary_llm`：复用 SessionManager 的当前模型/provider 解析（`current_model_and_provider()`）和现有 Provider 构建/AgentCore active-provider 路径。必要时增加**一个窄 accessor**返回 `(Arc<dyn Provider>, model)` 快照；不得在 HTTP handler 中复制完整模型优先级规则。获取快照后释放 SessionManager/AgentCore 同步锁，再进行网络调用。若当前 API 无法取得与选中 provider 对应的 Provider 实例，先补足同一现有 provider factory 的复用入口；不得错误地用 distiller/compaction 默认模型而不经 ADR 评估。
4. `call_search_summary_llm`：构造单次 `ChatRequest`，`tools: None`、`temperature: Some(0.1)` 或 provider 默认、`max_tokens: Some(800)`、`thinking_mode: Some("disabled")`，复用 `Provider::chat`。不走 session tool loop，不写入 conversation/session token totals（这是独立搜索推理，不是用户对话）。
5. `parse_and_validate_summary`：剥离 reasoning-only 内容时不得回退到 `reasoning_content`；要求 JSON schema、非空 summary、引用白名单；不信任模型标记。

不复用 `compact_with_llm` 的 compaction prompt / `<summary>` quality gate：语义不同。复用它的 Provider request 习惯、显式关闭 thinking、空输出/非法输出 fail-closed 模式即可。

### 4.6 Summary Renderer 与导航复用

#### 4.6.1 Dialog 区域（zones）调整

保留原 `Zone 1 / 2 / 3` 概念但把 PreviewPane 高度减半并与新摘要区共享同一 zone：

```
Zone 1  border-b  [AgentPickerChip] │ 🔍 input  [Aa][][⟲] │ (高度不变)
Zone 2  border-b  进度状态条 (源/摘要阶段, 完成 X/6)        (新增 32px)
Zone 3  border-b  [摘要区 SummaryView · 摘要中/已生成/失败] (新增, 与 preview 同高度, 上半)
        │         [PreviewPane]                            (preview 下半)
Zone 4  border-b  [文件][git][doc][project][对话][记忆]   (现有, 不变)
Zone 5  flex-1    当前 tab 的命中列表                      (现有, 不变)
Footer  border-t  [Close]                                  (现有, 不变)
```

原 `h-[35vh] max-h-[300px] min-h-[140px]` 的 PreviewPane 改为**两行 flex-1 等分**：上面 SummaryView 区域，下面 PreviewPane 区域，最小高度 120px。PreviewPane 缩到原高度一半，但保持现有渲染/预览能力；摘要区与 preview 共享 `min-h-[120px]`，仍能放下约 5 行正文。`max-h` 也由 300px 改为 240px，保证两个面板均不撑破 dialog 上限。

dialog 高度上限仍使用现 70vh。`Zone 2 + 3` 在 summary disabled（query 为空或未启用）时折叠成 0 高度，避免占用键入体验。

#### 4.6.2 摘要区渲染

摘要区单独组件 `SearchSummaryView.tsx`（JSX/状态足够复杂，单独文件比塞回 dialog 清楚；保持本设计模块独立）。状态机：

| 状态 | 显示 | 可点元素 |
|---|---|---|
| `idle` (query 空) | 不渲染（折叠） | — |
| `waiting_for_results` (8s 检索期内) | “搜索中，已完成 N/M 个来源”+ 6 个源圆点 (灰/绿/红) | 无 |
| `generating` (已发摘要) | “正在基于 N/M 个来源生成摘要…” + 内嵌小 spinner | 摘要区右上「取消摘要」按钮 |
| `partial_done` (摘要完成 + 至少一个来源 timeout) | 摘要正文 + “已收到 N/M 个来源；M 个来源超时” | 引用 button + 「重新生成」按钮 |
| `done` | 摘要正文 | 引用 button + 「重新生成」按钮 |
| `skipped_no_results` | “暂无命中可摘要” | 无 |
| `error` (Provider 503/timeout/invalid) | “摘要暂不可用（原因）”，重试按钮 | 「重新生成」按钮 |
| `cancelled` (用户主动) | “已取消当前摘要” | 「重新生成」按钮 |

正文解析 `[[rN]]` 为 React text segments，未匹配标记按普通文本安全显示或移除。**不得使用 `dangerouslySetInnerHTML` 或 Markdown link renderer**。

#### 4.6.3 输入框右侧按钮

- `[Aa]` 区分大小写 toggle：现有。
- `[]` 整词 toggle：现有。
- **新增** `[⟲]`「重输」按钮：仅在 query 非空时显示。点击行为：
  1. 同步递增 generation。
  2. abort 旧 run 的所有源/摘要 signal。
  3. 立即清空输入框焦点，保留旧生成结果直到新结果回来（避免空白闪屏）。
  4. 重新触发 debounce（300ms）后开始新 run。
- 该按钮只对**当前 query**生效；空 query 时隐藏。

#### 4.6.4 摘要取消按钮

- 仅 `generating` 状态显示「取消摘要」按钮（摘要区右上角，`aria-label="取消生成摘要"`）。
- 点击行为：
  1. abort 摘要 signal，取消 Gateway `POST /api/agents/{id}/search/summary`。
  2. 状态转为 `cancelled`；列表、进度、输入框**完全保留**。
  3. 不取消检索信号（若仍在 8 秒期内，源结果继续到位并更新列表）。
  4. 用户可点「重新生成」手动重试。

#### 4.6.5 重新生成按钮

- 在 `partial_done / done / error / cancelled` 状态显示「重新生成」按钮（摘要区右下角）。
- 复用同一摘要 endpoint，不重发检索；使用已 frozen 的候选快照。`partial` 仍带原 timeout scopes。
- 同一 generation 内最多 1 次「重新生成」自动重试；超过则提示用户「请修改查询后重试」。这条避免 Provider 持续出错时静默循环。

#### 4.6.6 引用按钮

- 解析后的有效引用渲染为 `Button`：`aria-label` 含「打开：标题」，并附 `data-citation-id`。
- 点击：查询 `CitationMap` → 调用 typed dispatcher `activateSearchHit(hit)` → 复用 `openFileHit / openDocHit / openProjectHit / openConversationHit / openMemoryHit / openGitHit`。
- 摘要区不直接打开 tab；命中类型若与当前 tab 不一致，可由 dispatcher 内部决定是否切 tab。文件定位行、对话 message index、Memory node、PM task 必须从本地 `RowHit` 取得；模型永远不提供 locator。

#### 4.6.7 Git 摘要引用

当前 Git `openGitHit` 打开当前列表 log 而不支持指定 commit 定位；MVP 在引用点击路径：把 `tab` 切到 `git`，在 `gitHits` 列表里把 `focusedIdx` 设为匹配 commit hash 的行；用户按 Enter 或双击触发 `openGitHit()`。如果现有 UI 结构无法安全聚焦（命中不在前 100 条 log 内），则只切到 Git tab + 结果列表，不伪造行内跳转。后续精确 commit detail 属独立需求。

## 5. 并发、性能与可靠性

### 5.1 一个 generation，一个固定 fan-out

- 保留现有 300 ms debounce。只有 query trim 非空、dialog open、agent 条件满足才创建 run。
- 每个 run 固定最多 6 个请求，使用 `Promise.allSettled`/每源回调；无动态递归、无无限队列、无重试风暴。
- 不串行等待源；每个返回按 source callback 立即更新对应 hits/status。
- 每源显示 waiting/searching/ok(count)/empty/skipped/error/timeout；overall 计数 `completed / applicable`，不展示虚假的精确百分比。
- 每源最多展示 20 条；文件请求 `max_results=20`。Git 仍最多取 100 条 log 作本地筛选，但展示/摘要候选最多 20 条，保持最近提交优先顺序。

### 5.2 检索 deadline 与 partial semantics

Run 自 `start` 起设绝对 monotonic deadline `t0 + 8s`（不是每源分别重置的 8s）。每源仍可先成功。deadline 时：

1. 尚未结束来源标记 `timeout`；其 signal abort。
2. 已完成的结果 retained；绝不因兄弟来源失败清空。
3. 立即构造摘要候选并发送摘要请求，`partial=true`，附上 timeout scope 名单。
4. 不为迟到结果自动再生成第二份摘要；迟到结果如果在 deadline cancellation race 中到达，仅在 generation 当前且来源尚未终态时可更新列表；不改变已锁定的摘要输入。

如果六源在 8s 前均终结，立即摘要，不必空等 deadline。无有效候选则摘要状态为 `skipped: no_results`，不请求 LLM。

### 5.3 来源数与资源上限

Fan-out 固定 6，因此并发上限就是 6；每个 dialog 同时仅一个 current generation。新 query 启动时先 abort 所有旧请求，再启动新 fan-out。debounce 减少键入风暴；每源限量、摘要候选限量、Runtime body limit 限制内存/上下文放大。禁止为每个来源创建长时间轮询或后台 retry。

Git endpoint 现有 `with503Retry` 必须遵守 AbortSignal：AbortError 不应重试。其它 source client 的既有 503/重试策略保持原样，但 deadline Abort 优先，不能被 retry loop 延长到 run deadline 之后。

### 5.4 摘要 deadline 与故障隔离

- 摘要开始后独立设绝对 20s deadline；列表不受其生命周期影响。
- Runtime handler 使用 `tokio::time::timeout(20s, provider.chat(...))`。timeout 会 drop chat future；底层 HTTP Provider 的 request future drop 后尽力关闭连接，不能承诺远端模型服务一定停止计算。
- Desktop 同时用 `AbortController` 的 summary signal；用户关闭/新查询会取消 Gateway HTTP request。不可自动 retry LLM POST，避免重复花费和多个迟到摘要。
- Provider unready/error/timeout/invalid JSON/empty output 只改变 summary 状态；保留所有来源 hits。UI 展示本地化、非技术性恢复提示。
- 统一记录耗时、来源状态与数量、摘要 token usage（若响应提供）、错误类别、generation ID；不记录 query/snippet/summary/API key。generation ID 用于日志关联，不跨查询持久化。

### 5.5 无背压扩展前的容量估算

单次最坏摘要候选 30 × (200 + 500) Unicode scalar values，再加 JSON/prompt 开销，控制在 64 KiB HTTP 请求以内；最坏 6 个检索请求 + 1 个摘要请求由用户主动输入触发，dialog 每个 generation 串行替换。当前不引入全局 semaphore：这是显式的轻量 ceiling（6 per active dialog），但多个同时打开的 dialog/大量并发用户需要全局请求限流时，再按真实并发数据增加 Runtime semaphore/Gateway rate limit；不在 MVP 先造调度器。

## 6. 取消协议与状态机

### 6.1 Desktop 状态


`Zone 2` 进度状态条始终展示当前 generation 的最高阶段（`idle / searching / generating / done / partial / error / cancelled`），并把已完成来源数显示为 `已完成 X/Y 来源`。状态条不显示精确百分比；不阻塞交互。

`Zone 3` 摘要区与 preview pane 等分高度；query 为空或 summary 关闭时，`Zone 2 + 3` 折叠为 0 高度，preview pane 恢复原高度。详情见 §4.6.1。


```text
Idle
  └─ debounce expires → Searching(generation, source statuses)
       ├─ source settled → update source, remain Searching
       ├─ all settled or 8s → Summarizing(generation, frozen candidates)
       │    ├─ success → Done(summary, citations)
       │    ├─ no candidates → Skipped(no_results)
       │    ├─ 20s/provider/parse error → SummaryError (hits remain)
       │    └─ cancel → Cancelled (no error toast)
       └─ cancel/new query/close/scope change → Cancelled → new Idle/Searching
```

### 6.2 取消层级

1. **Generation scope**：每个 query generation 一个父 `AbortController`；每来源、摘要创建子 controller，通过组合 signal 或父 abort listener 传播取消。若 WebView runtime 对 `AbortSignal.any` 支持不一致，用少量本地 helper，不引入包。
2. **UI stale guard**：controller abort 是尽力释放资源，不替代 `generation === seqRef.current` 检查；所有 `then/catch/finally` 状态写入都须校验 generation。
3. **Deadline**：一个 run-level `setTimeout` 负责 8 秒检索截止；摘要单独 20 秒 timer。每个 fetch 自己的超时不能重置这两个绝对 deadline。timer 在 finally/cancel 清理。
4. **Gateway hop**：请求取消会使 Desktop fetch 停止等待，并经 reqwest proxy future drop 尽力中止上游调用；取消只影响该 HTTP request，不向 Runtime 发全局 process signal。
5. **Runtime LLM**：handler future drop / `tokio::time::timeout` drop 当前 `Provider::chat` future；没有子进程、写盘或跨请求共享状态需回滚。Provider 需遵守 async future cancellation；远端服务端停止生成 best-effort。
6. **无取消的后端检索**：来源 API 是普通 HTTP GET。若已进入文件扫描/数据库查询，服务端取消是否能中止内部阻塞工作由其底层实现决定；本功能只保证 Desktop 立刻解除等待、旧结果不覆盖新结果，并在服务支持时传播断连取消。不新增数据库 cancel token。

### 6.3 Race 规则

- 用户新输入时先同步递增 generation，再 abort old run；这样 abort 导致的 catch/finally 已不能写当前 UI 状态。
- 8 秒 deadline 与最后来源完成竞争时，以 coordinator 首次完成的状态转换为准；`Searching → Summarizing` 只能一次，候选 snapshot freeze 一次。
- cancel 和 success 同时到达时，只有仍 current 且非 cancelled generation 可 commit。
- 摘要请求已发出后，新来源迟到数据不能修改摘要 citation map；摘要与映射作为同一 generation 同一对象提交，避免不匹配。
- `AbortError` 是中性取消状态，不显示搜索失败，不清空旧数据（新 generation 建立时按 UI policy 清空对应查询 hits）。

## 7. 安全与权限

1. `search/summary` 继承现有 agent instance route 和 Gateway 访问权限；不接受调用方指定的 provider、model、endpoint 或工具。
2. Runtime 校验请求大小、query、scope、id、数量和字段长度；Gateway 全局 body limit 仍生效。
3. 命中 snippets 作为不可信 user data 包在 JSON 数据区域；system prompt 指示忽略数据中的指令；`tools=None`。
4. LLM 输入不含 locators，响应 citation ID 白名单验证，前端再次白名单；两层都不信任模型。
5. UI 将摘要作为 plain text 节点渲染，不解析 HTML/任意 Markdown link。
6. 仅为日志观测记录计数、duration、status 和 error class，不记录搜索内容及凭证。

## 8. 错误与降级矩阵

| 故障 | 来源 tab | 摘要 | 全局 UI |
|---|---|---|---|
| 任一 source 4xx/5xx/网络错误 | 标记 error，其他来源继续 | 仍可总结已返回项，显示 partial/failed source | 继续可切 tab / 打开命中 |
| 单源或总检索到 8s | 标记 timeout；成功结果保留 | 立即使用冻结的部分候选生成 | 进度停止等待该来源 |
| Runtime summary 503/Provider 无配置 | 列表正常 | 显示“摘要暂不可用” | 可继续浏览来源列表 |
| Provider 20s timeout | 列表正常 | 显示可重试提示；不自动重试 | 不锁定输入或列表 |
| LLM 格式坏/引用越权/空文本 | 列表正常 | 拒绝摘要；未知引用不导航 | 原始模型输出不展示 |
| 查询取消/关闭/切换 agent workspace | 新 generation 独立 | abort，无 toast | 旧结果绝不覆盖当前查询 |

## 9. 模块级实施顺序与文件边界

1. **Runtime API first**：在 `acowork-runtime/src/http/global_search.rs` 或新增紧邻 `search_summary.rs` 实现 request DTO、validator、prompt/parser、Provider 调用、route 注册；先用 Mock Provider 自测白名单和 timeout 行为。
2. **Gateway proxy**：在 `core/acowork-gateway/src/http/proxy.rs` 加 `POST /api/agents/{id}/search/summary`，复用已有反代入口，验证 body/status 不变。
3. **Desktop summary client/candidate helpers**：优先放现 dialog 或 `src/lib/global-search-api.ts`（只有请求与纯 DTO helper）；复用 `doc-api`/`pm-api` 的 RequestInit signal convention。
4. **Abort-aware Git path**：给 `useGitStore.fetchLog` 可选 signal，传入现有 `httpGet` 和 `with503Retry`，补 abort 不重试检查。
5. **Coordinator wiring**：合并现在多份 `seqRef/AbortController` 的分裂生命周期为 per-generation coordinator，但不重写每种结果 mapper/state；新增 source status state 和摘要状态。
6. **Summary UI + dispatcher**：增加状态区和可访问 citation button；提取 typed dispatcher供列表和摘要共用，导航路径复用原动作。
6.1. **Layout 调整（先做）**：把 `Zone 3` 改为等分两行；新增 `Zone 2` 进度状态条；折叠逻辑；先于摘要 endpoint 接入，骨架在原 dialog 中可测。
6.2. **输入框重输按钮** + 「重新生成」按钮先到位（仅前端逻辑），空 query 时隐藏。

7. **Localization/tests**：现有全部 locale 增补消息；先对协调器纯逻辑与 Runtime parser 写小测试，再做 `GlobalSearchDialog.test.ts` / API route integration checks。

## 10. 验证策略

- 摘要渲染交互（组件测试）：
  - empty query → 进度条 + 摘要区折叠；无 spinner、无摘要显示。
  - 300 ms debounce 内连续输入 → 仅最后一次触发 run，generation 递增，旧 callback 不修改状态。
  - 任一来源返成 → 对应 tab 立即更新；进度条计数递增；其它 tab 继续 loading/empty。
  - 8s 到期、两个来源仍 pending → 标记 timeout；进度条 `已完成 4/6`；摘要状态 `partial_done`。
  - 摘要生成中：右上「取消摘要」可见；点按后状态变 `cancelled`，列表与输入框不变，summary POST 在 network tab 上被取消。
  - 「重新生成」点击一次可触发；同一 generation 二次点击不再次 POST，提示「请修改查询后重试」。
  - 引用点击：合法 ID 触发对应 typed action；伪造/重复/畸形 ID 不响应；正文保留 `[[xxx]]` 文本片段（不渲染为按钮）。
  - 摘要区正文中的 `<script>`、HTML 标签、`http://` 链接都作为普通文本显示，不被解析。
  - input 右侧 `⟲` 按钮仅在 query 非空时显示；点按 abort 旧 run、递增 generation、保留旧列表直到新结果。
  - preview pane + summary 等分高度生效：`min-h-[120px]`、`max-h-[240px]`、总高不超过原 zone 高度。


不引入测试框架或 fixture。复用已有 Vitest/Rust 测试设施，最低验证集：

- Desktop coordinator fake adapters：立即返回、慢源、reject、AbortError、新 generation 覆盖、deadline、六源每源只启动一次、deadline snapshot 不受迟到数据修改。
- 候选 builder：每源前 5、总数≤30、截断按 Unicode 字符而非 UTF-8 byte、locator 不出现在序列化 body。
- Citation renderer/dispatcher：合法 ID 调用正确 typed action；伪造 ID、重复/畸形标记、HTML 字符均不能生成任意导航。
- Runtime request validator/parser：超长/未知 scope/重复 id/未知 citation/坏 JSON/空 summary 拒绝；合法 JSON 过滤白名单。
- Runtime Mock Provider：成功、Provider error、20 秒 deadline、取消后无后续状态副作用；通过可控 pending future 测 timeout，不等待真实 20 秒。
- Gateway route：确认正确 Runtime path、POST body 透明、上游错误状态传播；abort 不触发重试。
- 手工本地场景：PM/doc 停止、Runtime 未就绪、无 workspace、conversation 未索引、超慢 Provider；列表逐步可用，状态准确，输入/切 tab/关 dialog 始终响应。

## 11. 未决实现细节

1. **模型选择 accessor**：`SharedAgentCore` 与 `SharedSessionManagerSlot` 已经进入 HTTP state；SessionManager 已有 `current_model_and_provider()`。实现时要追清活动 session provider 实例构造位置，优先复用其 provider factory；若 HTTP 层取不到同一 `(provider, model)` 快照，增加最窄 getter，不复制选型规则。不要默认拿 distiller model 替代当前 chat model。
2. **Git hit 聚焦**：现 dialog 命中列表与 `openGitHit` 之间缺少单 commit detail；确定以切 tab + focused index 完成摘要引用。
3. **8 秒检索期限**：ADR-093 当前既定；实现应使用可配置常量便于真实机器校准，不要在每来源 client 各自硬编码。

## 12. 结果

- **收益**：沿用已有各服务 search API 和 UI 跳转；不引入 Gateway 聚合；最多固定六路 fan-out；部分结果可先用；摘要及引用 fail-closed。
- **成本**：每次查询增加至多五个此前未触发的来源请求和一次 Runtime LLM 调用；Git store 要补 cancellation 参数；dialog 增加协调与来源状态。
- **回滚**：关闭摘要协调入口，恢复现活动 tab 触发逻辑；新增 Runtime/Gateway summary endpoint 可以独立移除；各来源 API、映射和定位不变。
