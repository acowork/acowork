# 桌面端/节点端浏览器自动化 —— 决策记录

状态：决策已定；工程只剩一件事（§4）。
相关：ADR-118（desktop 子 webview 页面预览，已实现）、ADR-046（附件 blob）、ADR-035（`record_complete` 事件）。

## 1. 决策

| # | 决策 | 状态 |
|---|---|---|
| D1 | desktop 的"页面预览"= ADR-118 同窗口子 webview，**保留现状，不再改动** | 已实现 |
| D2 | 浏览器自动化 = **Playwright MCP**（三方），不自研引擎、不自研 MCP 服务器 | 已定 |
| D3 | 我们唯一要写的工程：**工具产物落盘 + 每轮一行交付物**（Playwright 截图是第一个用例） | 待做（§4） |

## 2. 两个平面，互不依赖

| 平面 | 跑在哪 | 干什么 | 远程拓扑下 |
|---|---|---|---|
| 页面预览（ADR-118 子 webview） | 用户 PC 的 desktop 进程内 | 给人看网页，绕开 `X-Frame-Options` / `frame-ancestors` 对 iframe 的封锁 | 与 agent 无关，不需要 agent 在线 |
| 浏览器自动化（Playwright MCP） | **agent 所在节点** | agent 自己调试、自动化、验证 | 不需要 desktop 在线 |

**agent 永远不向 desktop 请求浏览器。** desktop 关掉，agent 的自动化照跑；desktop 打开，只是能看到 agent 刚才干了什么。

## 3. 今天用户看到什么（沿代码路径核对，不是推测）

| 信息 | 现状 | 依据 |
|---|---|---|
| 工具调用行 | 有：`mcp_playwright__browser_click` + 参数 | 既有 `tool_call` 事件与折叠块 |
| 结果文本 | 有，但**只前 5 行**（JSONL 存全量） | [core/acowork-runtime/src/mqtt/client.rs:2533](../../../core/acowork-runtime/src/mqtt/client.rs#L2533) |
| 截图 | **完全没有** | 见下 |

两处卡点：

1. **5 行截断正好切在 Playwright 的 snapshot 之前。** 它的返回是 `### Page` / `- Page URL` / `- Page Title` / `### Snapshot` / ` ```yaml ` / 真正的元素树。前 5 行 = 网址和标题，**用户看不到页面结构**。
2. **截图到不了 UI。** MCP 的 `image` content 在 [core/acowork-mcp/src/wrapper.rs:39](../../../core/acowork-mcp/src/wrapper.rs#L39) 被解析成 `ContentPart::ImageUrl`，但按 [core/acowork-core/src/tools/traits.rs:30](../../../core/acowork-core/src/tools/traits.rs#L30) 的契约，`content_parts` **只进模型请求**，`content` 字符串才是 transcript 持久化的东西。截图落在模型上下文里，UI 侧为零。

结论：今天的体验是"一串工具名 + 几行网址"，没有任何画面。这就是 D3 要补的全部。

## 4. 要做的：轮次交付物行（每轮一行，不进过程）

### 4.1 为什么不是"每块一个入口"

真实会话的形态（截图实测）：`Explored (1 step)` → 助手一句话 → `Explored (25 steps)` → `Explored (111 steps)` → 助手一句话 → …，一屏十几个块。任何挂在块上的入口（折叠头缩略图、块下画面条、tool_result 行内缩略图）都会变成十几个入口，跟文本抢注意力。

根因是**数量级**，不是位置：块是过程的粒度，交付物是轮次的粒度。所以入口只能有一个，且必须挂在轮次上——**一轮一行**。中间过程（111 步里的每次截图）只记录，不渲染。

顺带解决了"实时性"这个问题：一轮结束才出结论，与用户既有的"分块、不要求实时"心智一致，也不需要新的推流机制。

### 4.2 形态

挂在轮次最后一条消息之后：

```
[▣][▣][▣]  report.pdf  page.tsx  +3
```

- **图像产物** → 缩略图；点开**编辑器图片查看器 tab**（可缩放/平移、区域回投给 agent），与右侧工作区点图片完全一致，零新增查看器
- **非图像产物** → 文件胶囊（复用 `DocumentChip` / `AttachmentCapsule`）
- 上限：图像 8 张、文件 8 个，超出显示 `+n`；按路径去重，同路径多次改只留最后一次
- 本轮没有产物 → **整行不渲染**（不是渲染一个空行）

### 4.3 怎么检测产出文件

**由工具自报，不靠事后探测。** 仓库里已经有这个模式：`ToolContext.image_read_budget` 就是"session loop 持有一个 per-turn 的 `Arc<Mutex<_>>`，工具往里记账"（[tools/traits.rs:78](../../../core/acowork-core/src/tools/traits.rs#L78)）。照抄它加一个 per-turn 收集器：

```rust
pub struct ProducedFile {
    pub rel_path: String,   // 相对工作区根/agent home，绝不存绝对路径
    pub workspace_id: String,
    pub created: bool,      // 写前是否不存在 —— 只有工具知道
    pub bytes: u64,
    pub mime: Option<String>,
}
```

两个生产者，一个收集器：

| 生产者 | 报什么 | 谁改 |
|---|---|---|
| `file_write` / `file_edit` / doc 工具 / 下载类 MCP 工具 | 自己写出的路径 + `created`（写前 stat 一次）+ mime + 大小 | 各工具内部，各一处 |
| loop 层 | 从 `ToolOutput.parts` 取图像 → 落盘到 `artifacts/<sid>/` → 推一条 | [loop_tools.rs:922](../../../core/acowork-runtime/src/agent/loop_tools.rs#L922)，**MCP 侧零改动** |

为什么不用 `ToolResult` 加字段：它有 133 处构造点，加必填字段是纯噪音；`ToolContext` 只在 12 处构造，且 `Default` 兜得住。

#### 计入条件（四条同时成立）

1. **本轮内由本 agent 的某次工具调用写出**，而不是"轮开始时它已经存在"；
2. **动作是 created** —— 写前不存在。改写已存在的文件不计入（那是过程）；
3. **落在本轮用过的工作区根内**（含 agent home），且不在排除表：`artifacts/`、`logs/`、`conversations/`、`config/`、`.git/`、`node_modules/`、以及仓库既有 ignore 规则；
4. **可打开**：能判出 mime、大小 ≤ 上限、数量 ≤ 上限（图像 8 + 文件 8）。

同一路径一轮内先 create 后 modify → 保留 create，只出一条；按首次产出顺序。

#### 明确不用的检测手段

| 手段 | 为什么不用 |
|---|---|
| fs-changed 事件 | demand-driven，`zero targets` 起步（[fs_watcher.rs:205](../../../core/acowork-runtime/src/workspace/fs_watcher.rs#L205)），桌面没开工作区就一条事件都没有 → 静默漏 |
| 全树 mtime/尺寸快照 diff | 远端节点 IO 不可控，一轮扫两遍大仓不划算 |
| 解析工具结果文本里的路径 | 文本是给人的，不是给机器解析的；一句 "wrote foo.ts" 里可能有相对/绝对/引用三种写法 |
| 按 mtime 判"是不是新文件" | checkout / 拷贝 / 恢复 都会让 mtime 说谎 |
| 让 agent 自称交付物 | 幻觉，且把耦合塞进 prompt |

#### 已知的洞：`bash` 写文件看不见

`bash` 不经过写入类工具，它创建的文件在收集器里没有痕迹。三个选择：

- **(a) 不纳入**（v1 采用）：shell 造物不出现在交付物行；
- (b) git 工作区在 turn 边界做一次 `git status --porcelain`，`??` 即新建——毫秒级、语义正好是"这一轮新增了什么"，只在仓库里有效；
- (c) 全树快照 diff：不做。

先按 (a) 上线；如果真实任务里 shell 造物常见（例如脚手架生成），再加 (b)。

### 4.4 持久化与回放

一轮一行，写成 `system` 行，`metadata.type = "deliverables"`，与 ADR-046（附件）/ ADR-107（引用）的 turn-part 同构——它们的 `metadata.type` 已经证明能穿过 JSONL 回放（[chatStore.ts:3987](../../../apps/acowork-desktop/src/stores/chatStore.ts#L3987)）。

三个必须处理的点：

1. **写入点要收敛。** 轮次结束有四个 `ChunkEvent::Done` 出口（[session_task.rs:990](../../../core/acowork-runtime/src/agent/session/session_task.rs#L990)、1413、1833、2246）。聚合逻辑必须提到一个共用点，否则总有一条消息路径漏写。
2. **`messageParts` 的谓词要拆。** 它现在用一个 `isTurnPartMeta` 同时回答"要不要保留 metadata"和"要不要折进 user turn"（[messageParts.ts:49](../../../apps/acowork-desktop/src/components/chat/messageParts.ts#L49)）。交付物要前者、不要后者——直接加进那个集合，交付物会被折到**上一轮用户消息**底下。该模块的注释里已经记过一次同类漂移，这次别再踩。
3. **文件可能已不在。** 回放时缓存被清 → 显示文件名 + 不可用态，不能渲染空白。

### 4.5 落盘目录：`<work_dir>/artifacts/<session_id>/`

`work_dir` 下已有 `conversations/` `config/` `files/` `logs/` `memory/`，没有给"工具产物"留位置，所以新增 `artifacts/`。名字必须中性：叫 `browser-artifacts` 就等于把 Playwright 写进了目录结构。

写进用户的代码目录是错的——污染 git status、被 agent 自己的搜索命中、还可能被误提交。agent home 下既在既有反代的可达范围内，又不属于任何项目。

**当作可丢弃的缓存**：按会话分目录，不需要回收机制。图在了就显示，被清掉就少几行——截图不是资产，不该进附件库。

### 4.6 会不会耦合 Playwright

不会——**前提是按通用规则实现**。这一层只用到两样既有的、与浏览器无关的东西：`ToolResult.content_parts`（[tools/traits.rs:30](../../../core/acowork-core/src/tools/traits.rs#L30)，MCP `ImageContent` 与内置 `image_read` 共用）和 `work_dir`。Playwright 在这里的全部"存在"就是它返回了 `image` content。

四条禁令，破一条就是耦合：

| 不许 | 因为 |
|---|---|
| 目录/字段名带 browser、playwright、screenshot | 名字是最长寿的耦合形式 |
| 按工具名前缀（`mcp_playwright__`）分支 | 换个截图工具就得改 runtime 代码 |
| 在 runtime 里实现"动作后自动截图""视口尺寸"这类 Playwright 语义 | 那是 MCP 服务器的事 |
| 在 desktop 面板里判断工具名决定要不要显示 | 有产物文件就显示，没有就不显示 |

**可证伪的判据**：把 Playwright MCP 删掉、换成自建截图工具或别的三方 MCP，上面两处改动一行都不用改。

顺带修掉一个既有缺口：内置 `image_read` 的图今天同样只进模型、不进 UI，这条规则落地后它一起有了。

## 5. 前置条件（部署事实，不是我们的设计）

节点必须有一份浏览器。Playwright 自己下载（`playwright install chromium`），Linux 上还需要字体与 `libnss3`/`libgbm` 一类系统库（要 root 或镜像预装）。**没装浏览器时 MCP 服务器照样起得来、工具列表照样出得来，第一次导航才失败**——排障时按这个顺序看。

## 6. 不做什么

- 自研浏览器引擎 / 自研 CDP 驱动 / 自研替代 Playwright 的 MCP 服务器
- 实时画面、实时流、分块快照推送
- 把过程画面塞进对话流（折叠头缩略图、块下画面条、tool_result 行内缩略图）——理由见 §4.1
- 右栏"过程时间线"面板：过程可见是另一个需求，等交付物行跑过真实任务再定，不先做
- 让远程 agent 借用 desktop 的 webview 做自动化
- 给 preview webview 开 CDP 端口（它加载任意外部站点，端口一开就是给页面一个执行面）
- 跨机器 webview 协作

## 7. 什么情况下才值得翻盘自研

1. 需要在**没有 Node 的节点**上跑自动化（Playwright MCP 是 Node 进程）；
2. 需要操作**用户本人已登录的真实浏览器会话**（那是另一个平面，与 §2 的自动化平面不同）；
3. Playwright 的工具面在真实任务里反复不够用——要拿具体的失败任务单列证据，不能凭感觉。

## 8. 存档：两版被推翻的方案

- **v1 桌面托管 loopback MCP**：把 MCP 端点放在 desktop 上。错在 agent 与 desktop 是远程关系，`127.0.0.1` 在远端节点上不指向用户 PC。
- **v2 Gateway 端点 + 设备编址**：把端点搬到 Gateway，靠 presence 找一台有桌面的机器。错在把"agent 需要浏览器"绑到了"某台 desktop 必须在线"——桌面是投影，不应是依赖。

两版的共同错误：**把投影平面当成了执行平面的输入**。

## 9. 附：P0 实测数据（自研路线被叫停前的唯一资产）

一个 Rust CDP spike 在这台 Windows 开发机上跑过（代码已删除）：

| 引擎 / 模式 | `Runtime.evaluate` | `Page.captureScreenshot` | `Input.dispatchMouseEvent` | `Accessibility.getFullAXTree` |
|---|---|---|---|---|
| Chrome 157 / `--headless=new` | ✅ | ✅ PNG 6.9 KB | ✅ 页面 `onclick` 真跑 | ✅ 12 节点 / 10 有效 |
| Chrome 157 / headed | ✅ | ✅ PNG 20 KB | ✅ | ✅ 同上 |
| Edge 157 / `--headless=new` | ✅ | ✅ PNG 6.9 KB | ✅ | ✅ 同上 |

它证明的是"CDP 协议层不是难点"，所以自研省不下真正的成本——**难点在 actionability（可点性判定、自动等待、stale 重放）**，那才是 Playwright 值钱的部分。另外 `--headless=new` 与 headed 的 a11y 树逐项相同，服务器节点用 headless 不牺牲观察语义。

**这批数据不能用来推断节点上有浏览器**——Chrome/Edge 都是这台开发机自带的（见 §5）。