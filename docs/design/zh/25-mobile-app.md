# Mobile App（移动应用）

> 版本：v1.3 | 更新日期：2026-10-12
> 状态：设计已确认（v1.1 补齐启动与连接 §7、实时事件流 §8、边界态 §7.5；v1.2 修订 §8 为双通道模型——relay 形态走 MQTT-over-WSS 精确订阅，轮询降级为 local/LAN 通道；v1.3 把同一条 WS 桥也挂到主 LAN 监听器，两形态同权，并新增全局 presence 连接）；工程开发中

---

ACowork Mobile App 是基于 Tauri v2 的移动端客户端，定位为 Desktop App 的**便携操作终端**。它不是 Desktop 的移植版，而是一个**功能受限、形态不同**的第二终端。

## 1. 定位与职责

### 1.1 一句话

**Mobile App = IM 形态的对话控制终端**：用主流通 IM 软件（企业微信 / 钉钉）的交互范式承载"与 Agent 和同事的会话"，首版只做基础对话控制，不承担平台管理职责。

### 1.2 与 Desktop App 的关系

| 维度 | Desktop App | Mobile App |
|------|-------------|------------|
| 定位 | 主工作台，创建/调试/发布 Agent | 便携终端，随时收发消息 |
| 形态 | 左中右多栏 | 底部 Tab + 全屏层级（IM 形态） |
| 导航 | 单一导航栏 + 左侧列表 | 每 Tab 独立导航栈 + iOS push 语义 |
| 会话 | 一个 Agent 可**并行打开多个 Session Tab** | 一个 Agent 多个会话，**串行切换**（见 §5） |
| Agent 管理 | 创建/安装/克隆/发布 | ❌ 不做（v1） |
| 调试 | DevMode 协议、Git 状态条 | ❌ 不做（v1） |
| 本地 Gateway | 内嵌/管理 Gateway 进程 | ❌ **无本地 Gateway 进程**（见 §11.1） |

**关键认知**：两个 App 共享同一份服务端状态（同一批 Agent、同一批 Session、同一套权限），但**客户端状态各自独立**。Desktop 上打开的会话，手机上能继续看；手机上新建的会话，Desktop 上能看到（受权限约束）。Mobile 永远不是 Desktop 的"远程显示器"，而是一个平级终端。

### 1.3 Mobile App 不做的事（v1）

| 不做 | 理由 |
|------|------|
| DevMode 调试协议 | 需要本地文件系统与进程级控制，移动端不具备 |
| Git 状态条 | 依赖 workspace fs 实时监听（ADR-078），移动端无本地 workspace |
| Agent 创建/安装/克隆/发布 | 高频低频次的开发者操作，留在桌面端 |
| Provider / API Key / MCP / Embedding 管理 | 属于 Harness 职责，且涉及密钥，移动端不应持有 |
| 富文本编辑器 | 文档编辑需要大屏与物理键盘 |
| Harness / Extensions 导航 | 开发者概念，普通用户不需要 |
| 本地 Gateway 进程管理 | 移动端不运行 Gateway（§11.1） |
| 推送通知 | v1 无后台通道：移动 WebView 退后台即挂起（§11），relay 的 MQTT 连接也随之停摆，不存在"后台送达"；厂商推送（APNs/FCM）需要独立服务端与证书体系，属 v2 评估项。前台新鲜度由 §8 双通道与回前台刷新承担 |
| 注册 / 邀请 | 账号注册入口留在桌面端与邀请链路（ADR-076 §决策 6）；移动端只做**登录**，注册开放时提示"请在桌面端注册" |
| 扫码配对 Gateway | 需要桌面端配合生成一次性配对码，v1 用手动地址输入（§7.2），配对码列入 v1.1 |

> **注意**：上表是"v1 不做"，不是"永不做"。多文档协同编辑（Yjs，见 ADR-079）在 v2 之后评估。

## 2. 信息架构（IA）

### 2.1 底部 Tab Bar（固定四栏）

```
┌─────────────────────────────────┐
│  ← 返回      ADR-085…      ⌄  ⋯ │  ← 二级页：TabBar 整体滑出隐藏
│                                  │
│         （内容区）                │
│                                  │
├─────────────────────────────────┤
│  ┌──────┬──────┬──────┬──────┐  │
│  │ 聊天 │ 项目 │ 文档 │ 设置 │  │  ← 一级页才显示
│  └──────┴──────┴──────┴──────┘  │
└─────────────────────────────────┘
```

| Tab | 对应 Desktop | 说明 |
|-----|--------------|------|
| 聊天 | AgentList + UserList + ChatPanel | 主入口，占日常使用 80% |
| 项目 | ProjectsView | 只读看板 + 任务流转 |
| 文档 | DocsView | 目录树 + 文档阅读 + 审阅 |
| 设置 | SettingsPage | 二级设置 |

**Harness 与 Extensions 不进移动端导航**。设置页里以"仅桌面端"的灰色条目保留占位，让用户知道功能存在、在别处可用——这是**诚实优于沉默**的取舍。

### 2.2 导航模型：每 Tab 一条独立栈

```
Tab 栈结构（内存态，切换 Tab 时保留）
chat:     [会话列表] → [聊天详情]
projects: [项目列表] → [看板] → [任务详情]
docs:     [文档目录] → [文档内容] → [审阅详情]
settings: [设置根页] → [Profile|General|Appearance|Gateway]
```

| 规则 | 行为 |
|------|------|
| 进入二级页 | 底部 TabBar **整体滑出隐藏**（iOS / 微信行为） |
| 切 Tab | 保留各自的栈深，退出会话再进来仍在原会话 |
| 再次点击当前 Tab | 回该 Tab 根（清空栈） |
| 返回 | 逐级 pop；二级页 pop 后 TabBar 滑回 |

**为什么二级页隐藏 TabBar**：iOS/微信都不在二级页显示底部栏。若同时显示，用户面对"两个导航出口"会困惑——底部 Tab 和右上返回到底该用哪个？隐藏后只有一条返回路径（右上角 / 右滑手势）。

### 2.3 聊天首页 = 统一会话流

Desktop 的 AgentList 与 UserList 是两个独立分组。移动端合并成**单一 IM 会话流**：

```
┌─────────────────────────────────┐
│  🔍 搜索 Agent 或联系人          │
├─────────────────────────────────┤
│  AGENTS (7)                      │
│  ┌───┬──────────────────────┐   │
│  │ ● │ 架构师        14:32  │   │
│  │   │ 已完成 ADR-085 状态机…│ 2 │
│  └───┴──────────────────────┘   │
│  ┌───┬──────────────────────┐   │
│  │ ● │ 高级工程师    13:05  │   │
│  │   │ cargo test 全绿…     │   │
│  └───┴──────────────────────┘   │
│  联系人 (4)                      │
│  ┌───┬──────────────────────┐   │
│  │ ● │ Alice（算法）  15:10 │   │
│  │   │ embedding 换了 4 个… │ 3 │
│  └───┴──────────────────────┘   │
└─────────────────────────────────┘
```

每行展示：头像（带在线状态点）、名称、在线状态、**最近会话摘要**、时间、未读角标。

**会话摘要取自"最近活动会话的最后一条消息"**，而非 Agent 维度的固定文案。这是 IM 语义的必然要求：用户关心的是"上次聊到哪了"。

## 3. 手势与导航交互

| 手势 | 作用域 | 行为 |
|------|--------|------|
| 左滑 → | 聊天详情页 | 打开 Agent 设置抽屉（Desktop RightPanel） |
| 右滑 ← | 二级页 | 返回上一级（逐级 pop） |
| 右滑 ← | 一级页 | **无动作** |
| 垂直滑动 | 任意 | 让位给页面滚动（不触发导航） |
| 横向滑动 | 二级页顶部（列表横滑区） | 让位给子组件 |

**判定规则**：只有当 `|dx| > |dy|` 且 `|dx| > 6px` 时才判定为横滑；提交阈值为屏宽 38% 或足够速度（<300ms）。聊天页必须按**实际滑动方向**在"左滑开抽屉"和"右滑返回"之间解析——这是最容易写错的分支。

## 4. 聊天详情页

### 4.1 布局

```
┌─────────────────────────────────┐
│  ←      ADR-085 生命周期状态机⌄  ⋯ │  ← 标题 = 会话切换器
│         gpt-5 · 24 条             │
├─────────────────────────────────┤
│  今天 14:02                       │
│                                  │
│              ┌─────────────┐    │
│              │ 帮我看下…    │    │  ← 我的消息（右对齐）
│              └─────────────┘    │
│  ┌──────────────────────────┐   │
│  │ ⚡ file_read  412ms       │   │  ← 工具调用卡片
│  │ docs/adr/zh/ADR-085…      │   │
│  └──────────────────────────┘   │
│  ┌─────┐                        │
│  │ 架构│ 两处歧义：1) rollback │  │  ← Agent 消息（左对齐）
│  └─────┘ 2) RESTARTING…       │   │
│                                  │
│  ┌──────────────────────────┐   │
│  │ ⚠ 需要确认：执行 shell   │   │  ← 审批卡
│  │ cargo test -p …          │   │
│  │        [拒绝]  [允许]     │   │
│  └──────────────────────────┘   │
├─────────────────────────────────┤
│ [⚙] [发送消息…          ] [🌐] [🙂] [↑] │  ← Composer
└─────────────────────────────────┘
```

### 4.2 Agent 设置抽屉（左滑唤出）

对应 Desktop 的 RightPanel，六个分区：状态 / 工作区 / 记忆 / 工具 / 配置 / 会话。

移动端改用**原生感组件**（inset 分组列表、Switch、Segmented、Stepper、select 下拉、Action Sheet），而不是缩放 Desktop 控件。

**工作区分区只做 Add to Chat，不做文件预览**。移动端没有 Monaco，文件内容由 Agent 在对话中读取。这条边界同时满足 [ADR-009 §5](../../adr/zh/ADR-009-gateway-workspace-isolation.md) 的 Gateway 工作区隔离原则——移动端不直连文件系统，只走 Gateway 反代。

### 4.3 消息形态

| 元素 | 移动端处理 |
|------|-----------|
| 用户 / Agent 消息气泡 | 保留，支持 Markdown 渲染 |
| 工具调用 | 折叠卡片，默认收起（屏幕窄）；数据来自 `GET /messages` 重载，非事件流 |
| 审批卡 | 保留允许/拒绝按钮（`POST .../approval`）；卡面详情按通道分级——WS 通道完整（工具/风险/理由），轮询通道通用卡（§8.6） |
| AskQuestion 卡片 | relay/WS 通道渲染问答卡（`ask_question` 事件 + `POST …/answer`）；轮询通道不可得，降级提示"请在桌面端处理"（§8.6） |
| 流式输出 | **v1 不做**（§8）：状态指示承担"正在工作"感知，内容到达即完整呈现 |
| 代码块 | 横向滚动，**不做语法高亮**（无 Monaco） |
| 图片 / 文件附件 | 只读展示，不做预览器 |
| Think / Compaction 卡片 | 折叠为一行摘要 |

## 5. 多会话模型（核心设计）

> 交互稿初版曾建议"一个 Agent 一个活动会话"，**已否决**。Agent 多会话是业务事实，Mobile 只是多个操作终端之一；不支持多会话会丧失可用性。

### 5.1 为什么不能只保留一个会话

一个 Agent 同时被多个话题驱动是常态：架构师一边在写 ADR、一边在 review 别人的 PR、一边在调试线上问题。Desktop 用**并行 Session Tab**承载。移动端屏幕只有 390pt 宽，放不下 N 个并排标签——但这**不构成"不需要多会话"的理由**，只构成"不能用标签条承载"的理由。

### 5.2 承载形式：导航栏标题 = 会话切换器

```
┌─────────────────────────────────┐
│  ←      ADR-085 生命周期状态机⌄  ⋯ │
│         gpt-5 · 24 条             │  ← 点击这里
└─────────────────────────────────┘
              ↓ 点击
┌─────────────────────────────────┐
│      架构师 · 会话（8/12）         │
│  已加载最近 8 条，列表可滚动加载更多  │
├─────────────────────────────────┤
│ ✓ ADR-085 生命周期状态机评审    🌐  │
│   已完成 ADR-085 状态机评审…       │
│ ─────────────────────────────────  │
│   Relay 模式降级清单        🌐 只读│
│   node-local 功能降级清单已…      │
│ ─────────────────────────────────  │
│   ADR-084 用户独立进程        🔒  │
│   ADR-084 已定稿，等待评审        │
│ ─────────────────────────────────  │
│   Vault 密钥轮转            🌐 只读│
├─────────────────────────────────┤
│        ＋ 新建会话                │
│        管理全部会话               │
│        [   取消   ]               │
└─────────────────────────────────┘
```

| 设计点 | 说明 |
|--------|------|
| 标题显示 `session.title` | 不是 agent.name——用户认的是话题，不是机器人 |
| 每行显示 🌐/🔒 | 可见性标记（§6） |
| 只读行显示"只读"徽章 | 明确告知不可写 |
| ✓ 标记当前会话 | 位置感 |
| 列表可滚动 + 8/12 分页提示 | 对齐 `agentStore.fetchSessions(agentId, page)` |
| 底部"管理全部会话" | 跳转到抽屉的会话分区（批量管理） |

### 5.3 切换会话的原子迁移

切换**不是本地页面切换**，必须走完整流程（对齐 Desktop `chatStore.openSession`）：

```
用户点击会话行
      │
      ├─→ ① 前端切换当前 session + 重载消息历史
      ├─→ ② POST /api/agents/{id}/sessions/{sid}/open（ADR-038 激活，幂等）
      └─→ ③ 并行拉取 session config + state
```

**只读会话跳过 ②**。激活是 per-session 的全局生命周期变更：观众无权开启（close 是写授权，owner 也不该知道被谁占着），且只读浏览不需要激活——历史走 `GET /messages`。只读会话的新鲜度与可写会话同通道承担（§8 双通道，订阅判据是"可读"而非"可写"）。

### 5.4 会话管理（抽屉 → 会话分区）

| 能力 | 支持 | 说明 |
|------|------|------|
| 列表 | ✅ | 分页，8/12 形式，可加载更多 |
| 新建 | ✅ | 默认落 Private（§6.3） |
| 切换 | ✅ | 等价于 §5.2 |
| 删除 | ✅ | 仅可写会话（owner/admin） |
| 重命名 | ❌ v1 | 需桌面端 |
| 可见性切换 | ✅ | composer 区的 🌐/🔒 一键切换 |

## 6. 多用户权限模型

> 完整推导见 [ADR-076](../../adr/zh/ADR-076-multi-user-account-system.md) §决策 4。本节只定义移动端的**消费方式**。

### 6.1 唯一权威是后端的 `can_write`

```rust
// core/acowork-runtime/src/conversation.rs
pub struct SessionListView {
    pub session_id: String,
    pub visibility: Option<SessionVisibility>,  // public / private
    pub can_write: bool,                        // ← 唯一权威
}
```

**移动端必须原样消费 `can_write`，绝不能从 `visibility` 标签反推写权限。** 公开 = 所有人都能读，但通常只有 owner 能写。`🌐` 公开的会话完全可能是只读的。

判定规则（与 Desktop `isReadOnlySession` 同构）：

```ts
// 只读态只在 can_write === false 时成立；字段缺失按可写处理。
// 降级方向是「让后端拒绝」而不是「锁死全部控件」——
// 一个乐观创建的会话、或一个省略该字段的旧 Runtime，
// 都不该让用户面对一整排死按钮。
function isReadOnly(canWrite: boolean | undefined): boolean {
  return canWrite === false;
}
```

### 6.2 可见性/可写性矩阵

| scope \ session | public | private |
|---|---|---|
| admin | 可读可写 | 可读可写 |
| owner | 可读可写 | 可读可写 |
| 其他 user | **可读（只读）** | **不可见**（404） |
| local（无头） | 可读可写 | 可读可写 |

移动端 UI 映射：

| 状态 | 移动端表现 |
|------|-----------|
| 可写 | 正常 composer + 🌐/🔒 开关 |
| 只读（`can_write === false`） | 顶部只读 banner + 隐藏审批卡/AskQuestion + composer 替换为只读行 |
| 不可见（private 且非 owner） | 会话根本不出现在列表里（后端分页前过滤） |

### 6.3 只读态的具体处理

```
┌─────────────────────────────────┐
│ 🔒 只读 · 来自 Alice 的共享会话，你不能发送消息 │  ← banner
├─────────────────────────────────┤
│           （消息列表，只读）        │
├─────────────────────────────────┤
│ 🔒 共享自 Alice · 只读            │  ← 替换掉 textarea
└─────────────────────────────────┘
```

| 控件 | 只读时 | 理由 |
|------|--------|------|
| textarea | **不渲染** | 不能用 placeholder 假装可输入 |
| 发送 / 工具 / 附件 | 不渲染 | 无写入路径 |
| 审批卡 / AskQuestion | **隐藏** | 它们是"让观众替 owner 做决定"，不该出现 |
| 可见性图标 | **禁用，不隐藏** | 观众仍需知道自己看的是什么 |
| 改可见性 / 删除 | 菜单项 disabled + 原因副标题 | 不做成"点了才 toast" |

**为什么可见性图标禁用而非隐藏**：对齐 Desktop `SessionVisibilityToggle` 的既有规则——观众要看清自己正在看的是一个私有会话，隐藏图标等于让他失去这个信息。

### 6.4 新建会话的默认值

新建会话默认落 **Private**，对齐 ADR-076 的 `create_frontend_session`：写入 `user_id` 的同时落 `Some(Private)`。移动端新建时前端就按 private 提交，避免出现"新建即公开、bob 可读"的窗口。

## 7. 启动与连接

> 本节补齐"从安装到第一次说话"的路径。移动端**没有本地 Gateway 进程**（§10.1），
> 所以连接与认证是所有功能的先决条件，必须设计成显式状态机而不是错误处理边角。

### 7.1 启动状态机

```
冷启动
  │
  ├─ 无已保存 Gateway 地址 ──────────────→ [连接屏]（输入地址）
  │                                          │ 探测 GET /api/status
  ├─ 有地址 ──→ 探测 GET /api/status ──失败──→ [断连屏]（显示上次地址 + 重试/更换）
  │                  │成功
  │                  ├─ requires_setup=true ─→ [受限提示屏]（"Gateway 未完成初始化，请在桌面端完成设置"）
  │                  ├─ 无有效 token ───────→ [登录屏]
  │                  └─ 有 token ───────────→ 进入主界面（token 有效性由首个 401 触发刷新验证）
  │
[登录屏] ── POST /api/auth/login ──成功──→ 持久化 token 对 → 主界面（实时通道按 §8.0 惰性建立：进入会话详情才订阅/轮询）
                └───失败──→ 表单内联错误（用户名或密码错误 / 网络不可达）
```

**设计原则**：每个状态都有唯一的出口文案，不做"点了才知道"的试探。`/api/status` 是公开路径（无需 token），返回 `auth_mode / registration_open / requires_setup / version`——启动探测一次就够，不重复打。

### 7.2 连接屏（首启 / 更换地址）

| 元素 | 规则 |
|------|------|
| 地址输入 | 单行 URL，允许 `http://host:port`（局域网）与 `https://<gw-id>.<relay域>`（relay 设备域，与 Desktop 的 Gateway URL 同一个值）；粘贴自动 trim。**scheme 决定实时通道的协议与端口**（§8.0）：https → `wss://{authority}/mqtt`，http → `ws://{authority}/mqtt`，两者都是 MQTT-over-WS 主通道 |
| 局域网发现 | **不做 mDNS/自动扫描**（v1）。原因：① 多 NIC 机器上广告地址与实际可达地址不一致（容器 bridge / WSL / VPN 网卡会广播错误 IP）；② 扫描类 UX 在移动网络权限下引入不必要的权限面。v1 用"桌面端显示本机局域网 IP + 手动输入"替代（设置 → 网关，v1.1 加配对码即消除此手工步骤） |
| 校验 | 保存前必须 `GET /api/status` 探测成功；失败时显示具体原因（DNS 失败 / 超时 / 非 ACowork Gateway），**不落库** |
| 版本提示 | 探测成功后显示 Gateway 版本号，让用户确认连的是预期实例 |
| `auth_mode=local` | 提示"该 Gateway 为本地模式，移动端需多用户模式"，阻止进入登录 |
| 更换入口 | 设置 → 网关 → 服务器地址（复用同一屏） |

### 7.3 登录屏

| 项 | 规则 |
|----|------|
| 字段 | 用户名 + 密码；`EnterNext` 键盘流；提交后按钮进 loading，禁止重复提交 |
| 错误 | 401 → 表单内联"用户名或密码错误"（不区分"用户不存在"，与后端语义一致）；网络错误 → 保留输入 + 重试 |
| `registration_open=true` | 底部提示"注册请在桌面端完成"（v1 移动端不做注册，§1.3） |
| 锁定 | 后端策略（ADR-076），移动端只透传错误文案，不做本地锁定计数 |
| 忘记密码 | v1 提示"请在桌面端由管理员重置"，不做邀请链路 |

### 7.4 Token 生命周期（与 Desktop 同构）

```
access_token (15min)  ── 每个请求 Authorization: Bearer ──┐
refresh_token         ── 仅用于 POST /api/auth/refresh ───┤
持久化：tauri-plugin-store（原生）/ localStorage（浏览器 dev）
```

| 规则 | 行为 |
|------|------|
| 401 → refresh 阶梯 | 任一请求 401 → 用 refresh_token 换新对 → **重放原请求一次**；refresh 也 401 → 清 token → 回登录屏（保留 Gateway 地址） |
| 并发去重 | 多个请求同时 401 时只发起一次 refresh，其余等待同一结果（对齐 Desktop `authFetch` 的 `_refreshPromise` 模式） |
| 主动刷新 | **不做**。15 分钟窗口内被动刷新足够；主动预刷新会制造第二套时间语义 |
| 登出 | 设置 → 网关 → 退出登录：`POST /api/auth/logout` + 清本地 token → 回登录屏 |
| 明文风险 | token 存 WebView 存储而非系统 Keychain，是 v1 已知妥协；原生化存储（plugin 已就位）列为 v1.1 加固项 |

### 7.5 断连与重连（全局）

| 状态 | 表现 |
|------|------|
| 请求失败（非 401） | 顶部出现一条**离线 banner**（"无法连接 Gateway"），持续到下一次成功请求；页面数据保留上次快照，不清空 |
| 发送消息失败 | 气泡右下角红色 `!` + "重试"（点击重发同一 message_id，Runtime 幂等） |
| 历史加载失败 | 列表区显示错误卡 + "重新加载"，**不显示为"空会话"**（§12 决策表"半成品可见"原则） |
| 回前台 | 触发一次会话列表 + 当前会话消息刷新（§8.4），不依赖后台任务 |

## 8. 实时事件流（v1：双通道）

> **决策（v1.2 修订，v1.3 再修）**：实时通道由**连接健康度**决定，事件处理策略由产品决策统一——
> relay 与 LAN **两种形态都走 MQTT-over-WS 精确订阅**（与 Desktop 同权同通道），
> **前台会话轮询是降级通道**（WS 未建立 / 被拒 / 断开期间）。两个通道送达的"内容变了"信号走
> **同一条 HTTP 全量重载**，只显最终结果。
>
> **修订记录**：v1.1 曾决策"v1 不接入 MQTT、一律轮询"，其前提"移动 WebView 拿不到
> MQTT"只对 local/LAN 拓扑成立。relay 部署下 `wss://<gw-id>.<relay域>/mqtt` 经云端
> 隧道暴露在公网（[24-cloud-relay](./24-cloud-relay-remote-access.md)），且 broker
> 严格监听器原生接受 `user:{name}:mobile:{id}` 形状（[ADR-076 §决策3](../../adr/zh/ADR-076-multi-user-account-system.md)、
> `core/acowork-gateway/src/mqtt/broker.rs::remote_client_shape`）——mobile 与 Desktop
> 使用同一用户 token、同一远程 listener、同一 ACL，不存在"mobile 不能订 MQTT"。
> v1.2 因此把 relay 形态升到 WS，但把 LAN 形态留在轮询上——理由是"主监听器没有桥"。
> v1.3 补上这座桥：同一条 WS↔TCP 字节管道也挂在**主 LAN 监听器**上
> （`core/acowork-gateway/src/relay/remote_listener.rs::lan_mqtt_bridge`），LAN 形态
> 自此与 relay 同权；桥本身不改任何鉴权，CONNECT 仍由严格监听器校验用户 access token，
> 未启用账号体系的 Gateway 一律拒绝（fail closed），客户端自动退回轮询。

### 8.0 通道选择

```
连接屏探测（§7.2）
  │
  ├─ baseUrl 有 http(s):// 前缀 ──→ [WS 主通道] {ws|wss}://{host}/mqtt
  │        │ 连接失败 / CONNACK 被拒 / 掉线重连超限
  │        └──────────────────────→ [轮询降级通道]（§8.3）+ 状态行"实时已降级"
  │
  └─ 无桥可用（历史形态：主监听器未挂 `/mqtt`）─→ [轮询通道]
```

| 规则 | 值 |
|------|----|
| 判据 | scheme 只决定 WS 的**协议与端口**（`https` → `wss://{authority}/mqtt`，`http` → `ws://{authority}/mqtt`，与 Desktop `relay_mqtt_wss_url` 同一派生规则）；两种形态都有桥，通道选择的判据变成"WS 是否健康" |
| 收口 | `src/lib/realtime.ts` 暴露 `startWatching/stopWatching`，上层（chatStore）对通道无感；轮询循环是 `realtime.ts` 的内部降级实现，不再是 chatStore 的直接职责 |
| 并存 | 两通道不同时驱动重载；WS 活着时轮询停摆（省电省流量），断开即接管 |
| 被拒即降级 | 未启用账号体系的 Gateway 在 CONNECT 处 fail closed（无 CONNACK 的 close）；客户端按重连退避处理，期间由轮询承担新鲜度——LAN 形态因此不会因缺账号体系而失去功能 |

### 8.1 MQTT-over-WS（主通道，relay 与 LAN 两形态）

| 项 | 规则 |
|----|------|
| 端点 | `wss://{relay设备域}/mqtt`（relay）或 `ws://{host}:{port}/mqtt`（LAN 主监听器），均由已保存 baseUrl 派生，不新增配置项。WebSocket 握手必须携带 `mqtt` 子协议（两条桥按 rumqttc 约定回显，实测验证） |
| client_id | `user:{sub}:mobile:{device_uuid}`。`device_uuid` 首启生成并持久化（与 token 同一存储）；`{sub}` = access token 的 `sub` 声明（**user_id UUID**，非登录名）——broker 交叉验证，冒充他人身份直接断连（无 CONNACK 的 close）。**会话通道与 presence 通道是两条连接，client_id 必须不同**：MQTT 3.1.1 按 client_id 认会话，重复 CONNECT 会接管（踢掉）另一条；presence 在设备段加 `-presence` 后缀（`remote_client_shape` 禁止第四段冒号，故不能用 `:presence`） |
| CONNECT 认证 | username=`{sub}`（信息性字段，broker 以 client_id+password 为准），password=**当前 access token**。MQTT 3.1.1 不在活连接内重认证：token 旋转（15min）后由重连携带新密码——对齐 Desktop `MqttCredentials::refresher` 语义：每次重连前向 authStore 取最新 token，过期则先触发单飞刷新 |
| 订阅集（**只订当前前台会话**） | `acowork/agents/{id}/sessions/{sid}/state`（retained：status + message_count，等价轮询快照的推送版）<br>`…/messages/done`、`…/messages/error`、`…/messages/stopped`<br>`…/messages/tool_approval_needed`（retained QoS1，含工具详情）<br>`…/messages/ask_question`（retained QoS1，`question_json`） |
| presence 订阅（**全局第二条连接**） | `acowork/agents/+/status`（QoS1）——一条通配过滤器覆盖整个 agent fleet。payload 是 `DataEnvelope{agent_status = 9}`（`AgentStatus`：online / instance_id / state / detail），映射与 HTTP 目录的 `alive`/`lifecycle` **同一词表**，两者不会读出两个互斥结论（`src/lib/presence.ts`）。该话题 Retained + 心跳 + LWT（`online=false`），因此刚订阅即回放每个 agent 的最近判定，断线期间的迁移不会丢。生命周期：登录（phase=ready）建、登出/过期关；**它没有降级轮询**（目录轮询本就带 presence），桥不可用时列表停在最后一次轮询结果 |
| 用户聊天唤醒订阅（**同一条后台连接、同一个 SUBSCRIBE 包**） | `acowork/users/{me}/chat/changed`（QoS1，**空载荷**，non-retained）——`acowork-user` 在每次 send / read 落库后敲的门铃（[ADR-076 §决策 8](../../adr/zh/ADR-076-multi-user-account-system.md)）。门铃刻意不说明"哪条会话、和谁"，因为内嵌 broker 没有订阅级 ACL，载荷一旦写清就是把社交图谱发布出去。反应只有一条：回 HTTP 重读（§8.8）。两个过滤器共用**一个** SUBSCRIBE 包——packet id 空间按连接计，两次往返会留下"presence 订上了、门铃静默失联"且无人记录的状态。`chatTopic()` 每次 CONNECT 重新派生，账号换手不需要拆连接 |
| 后台通道按**过滤器**分别授权 | SUBACK 返回码逐个对齐过滤器顺序，`0x80` 是 MQTT 3.1.1 唯一的失败码。"presence granted + chat `0x80`"是合法组合（未启用账号体系的 Gateway 只拒 mailbox）：presence 半边成功即连接算活着，chat 半边失败即上报 `onChatChannel(false)`，store 据此武装 5s 兜底轮询（§8.8），而不是让线程静默停在旧数据。状态**只在翻转时上报**——每次重连都重复上报会把兜底定时器反复停掉再武装 |
| **不订** | `messages/chunk`、`tool_call`、`tool_result`、`reasoning_*` 等一切增量/过程事件——"只显最终结果"在订阅面就兑现，不是收到再丢 |
| payload | protobuf `SessionMessage`（`core/acowork-core/proto/mqtt_payload.proto`）。WebView 侧用手写最小 wire decoder 只解所需字段（oneof 判别号 + 少量 string/uint 字段），**不引入 protobuf 运行时依赖** |
| 出口 | 离开会话页 / 切换会话 / App 退后台 → UNSUBSCRIBE 旧集（连接可留着复用）；登录失效 → DISCONNECT |
| retained 陷阱 | `tool_approval_needed`/`ask_question` 是 retained——刚订阅可能收到**上一轮已处理的旧卡**。判"现在是否真在等"以 `state.status` 为准（`waiting_approval` 才渲染审批卡），事件 payload 只用于**补详情**（工具名/风险/原因），不用于判存在 |
| 可达性验证 | 已对真实 relay 端点实测通过（2026-10-04）：`/api/status` 探测 → `/api/auth/login` → `wss://{设备域}/mqtt` 握手（`mqtt` 子协议）→ CONNECT（`user:{sub}:mobile:{id}` + access token）→ **CONNACK code 0** → SUBSCRIBE state → **SUBACK granted**。错误 token 的 CONNECT 被无 CONNACK 直接断开，与严格监听器语义一致 |

### 8.2 事件语义 → 统一重载

| 事件 | 动作 |
|------|------|
| `state`（status/count 变化） | `message_count ≠ loadedCount` → HTTP 全量重载（§8.4 判据）；status 驱动状态行/审批卡存在性 |
| `done` / `stopped` | 触发一次重载（与 state 判据幂等合并，不双载） |
| `error` | 重载 + 错误态渲染 |
| `tool_approval_needed` | 若 `state.status=waiting_approval`：渲染**完整审批卡**（工具名、动作、风险级、理由、超时）；决策仍走 `POST …/approval`（HTTP，防伪造通道） |
| `ask_question` | 渲染**问答卡**（`question_json`）；回答走 `POST …/answer`（HTTP） |
| 列表类事件（`sessions/created` 等） | v1 不订——列表新鲜度走进入前台/下拉刷新（§8.5） |

### 8.3 轮询（local/LAN 通道 + WS 降级）

```
进入聊天详情 / 发送消息后
  │
  ├─ 每 2s  GET /api/agents/{id}/sessions/{sid}   （读 live_state.status + meta.message_count）
  │     status ∈ {Thinking, LlmStreaming, ToolExecuting, …} → 顶部"工作中"指示，继续轮询
  │     status = WaitingApproval{request_id}          → 渲染审批卡（轮询版：通用卡，§8.6），轮询继续（决策走 HTTP）
  │     status = Paused / Errored                      → 渲染对应状态卡；Errored 触发一次历史重载
  │     meta.message_count ≠ 本地已加载计数            → GET /messages?tail=… 全量重载 → 更新计数基线
  │
  └─ 出口：离开会话页 / 切换会话 / App 退后台 → 立即停止轮询
```

> **新鲜度信号是 `message_count`，不是状态迁移。** 若只监听 active→idle 迁移，
> 一个在两次轮询间隔之间开始并结束的回合（快回复）不会被观察到，历史永不重载。
> `meta.message_count` 是后端权威计数，比较"服务端计数 ≠ 本地视图加载时的计数"
> 是无状态判据，任何两次 tick 之间的内容变化都必然改变它。状态迁移保留为附加
> 触发器（用于状态行渲染），但重载的正确性只依赖计数。WS 通道的 `state` 推送
> 携带同一计数，判据两边同构。

| 规则 | 值 | 理由 |
|------|----|------|
| 轮询间隔 | 2s（常量 `POLL_INTERVAL_MS`） | 移动端"最终结果"场景 2s 足够；1s 翻倍流量无感知收益 |
| 并发上限 | **全局 1 个轮询循环**，只盯当前前台会话 | 多会话并行轮询是流量风暴的客户端版本 |
| 超时退避 | 设计目标：连续失败 → 间隔翻倍（2→4→8→16→30s 封顶），任一成功即复位。**v1 工程未实现**（当前为固定 2s 重试 + 断连 banner），列入 v1.1 | 断网时轮询变成 hammering，必须退避 |
| 空闲不轮询 | status=Idle 且计数无变化且无进行中操作 → 完全停止 | IM 语义：没有"正在进行的事"就不该有周期流量 |
| 404 语义 | 会话对 Runtime 未知（未 open / Runtime 重启）= 视为 idle，**不**计入网络失败、不弹 banner | 404 是业务态不是连通性故障，误判会造成假断连 |
| 请求身份 | 与所有 HTTP 请求同一条 Bearer 链路（§7.4） | 不为轮询开第二套认证 |

### 8.4 消息渲染：只显最终结果（两通道共同）

- 任一通道发现 `meta.message_count` 变化（或收到 `done`）→ `GET /api/agents/{id}/sessions/{sid}/messages?tail=N` 重载尾部，整段渲染（Markdown、工具卡片折叠态、代码块横滚——§4.3 的静态形态全部保留）。
- **不做增量拼接、不做打字机效果**。"Agent 正在工作"由状态指示承担（导航栏副标题 `工作中…` + 输入区上方进度行），内容到达即完整呈现。
- 分页语义沿用现有端点：`offset/limit/tail`，首屏 tail=50。

### 8.5 列表与未读的新鲜度

| 数据 | 刷新时机 |
|------|---------|
| 会话流列表（§2.3） | 启动后一次、下拉刷新、回前台、会话增删操作后 |
| 未读角标 | v1 = "自上次打开该会话后 `message_count` 是否变化"的本地计数（会话 meta 端点可得），**没有推送就没有真未读**——角标语义如实降级为"有新消息"，不显示数字 |
| Agent 在线状态 | presence 连接推送（§8.1）为主：秒级翻转在线/离线/异常；目录轮询（启动一次 + 回前台 + 下拉刷新）仍是底稿与竞态纠偏——一次晚到的 poll 可能把推送刚点亮的行改回离线，下一条心跳/判定即复位 |

### 8.6 通道差异矩阵（如实记录，防止"轮询版语义"固化成全局真相）

| 能力 | WS 通道（relay 与 LAN） | 轮询通道（降级 / 无桥） |
|------|-----------------|---------------------------|
| 状态行 | `state` 推送 | 2s 快照 |
| 审批卡 | 完整详情（工具名/风险/理由/超时） | 通用卡："Agent 请求执行工具，请在桌面端查看细节" |
| AskQuestion | 问答卡 + `POST …/answer` | 降级提示"请在桌面端处理" |
| 决策/回答回流 | 均走 HTTP（两通道一致） | 同左 |
| 延迟 | 亚秒 | ≤2s |

### 8.7 已知缺口（不阻塞 v1，上线外网前须评估）

- **per-user 订阅授权**：远程 ACL 目前是 `sessions/+/messages/#` 通配（[ADR-076 §5.5](../../adr/zh/ADR-076-multi-user-account-system.md)），订到他人会话事件在协议上可行。mobile 的订阅纪律（只订前台可读会话）是客户端行为，不是授权。Desktop 同水同深，非 mobile 特有；补 topic-level 授权是后端事项，列入 v1.1 协同项。
- **presence 通配同样无授权**：`agents/+/status` 在严格监听器上同样没有 topic ACL，因此任何一个有效用户 token 能看到该 Gateway 上全量 agent 的生命周期判定（在线/离线/失败 + 启动失败理由）。这与 Desktop 本地可见面平水；若要把 agent 存在性按所有权过滤，属同一项 topic-level 授权工作。
- **流量风暴防线**：宽订阅防线在"客户端只订前台会话"这一纪律上；中继侧连接级限流（24 §5.3）承担异常行为兜底。
- **聊天门铃同样没有订阅级授权**：`acowork/users/{uid}/chat/changed` 按账号命名，但 broker 无 topic ACL，所以任何有效 token 都能订阅他人账号的门铃。载荷为空，内容不泄露，泄露的是"该账号正在活跃"这一存在性事实。客户端按 `user_id` 过滤是行为纪律，不是授权；补 topic-level 授权与上两条同批。

### 8.8 用户↔用户聊天：唤醒与不位移刷新（Desktop 同规则）

用户聊天不是 agent 会话：没有"进行中的回合"可轮询，内容何时出现完全由对端决定，所以新鲜度只能靠唤醒 + 重读。
规则两端共用（Desktop `src/stores/userChatStore.ts`、Mobile `src/stores/userChatStore.ts`）——**两端在同一份服务端数据上必须读出同一个窗口，任何一端单独改动都是缺陷**。

| 规则 | 值 | 为什么 |
|------|----|--------|
| 信号 | `acowork/users/{me}/chat/changed`，**空载荷**、QoS1、non-retained | 载荷一旦说明"哪条会话/哪个人"，就是把社交图谱发布到无 ACL 的 broker（§8.7）。门铃只说"该重读" |
| 反应 | 一律 HTTP 重读 `GET /api/users/{me}/chats` + 当前线程 `GET …/messages` | HTTP 响应是唯一真相；MQTT 不承载内容，因此信号丢失不需要补投机制 |
| 合并 | 一次在途重读期间到达的信号折叠成"随后再来一轮"，不并发堆叠 | 一条消息触发两个门铃（消息 + 读游标）是常态 |
| 边沿补拉 | MQTT connected 边沿主动重读一次 | 非 retained 的信号在订阅生效前发出的就永久消失，边沿补拉是唯一纠偏点 |
| **刷新策略（核心）** | 页恒为"最新 N 条正序"，用服务端 `total` 与本地已知计数求差 `d`：`d=0` 不动；`0<d≤页长` 只追加 `page.slice(页长-d)` 的尾部；其余（`d<0`、`d>页长`、`total` 缺失）整窗替换 | 用户可能停在顶部/中间/任意位置读长历史，秒级信号不能每次都把视口抽走。数组头部引用不变 ⇒ React 不重挂已渲染气泡 ⇒ 零位移 |
| 计数缺失 | `total` 缺失必须归一化为 `null` 再进规则，绝不带 `undefined`/NaN 参与比较 | NaN 走穿所有分支会退化成"整页再追加一次"，每来一个信号历史就翻一倍 |
| 新消息浮标 | 整窗替换时未读数按服务端报告的差值计，`d=0` 不动；**首屏加载不计入**（窗口原本为空 → 0） | 刚打开线程时那 50 条不是"错过的消息"，否则浮标一进线程就闪一下"N 条新消息" |
| 已读回执 | 只在"贴底"翻转时清未读，且由滚动事件（不是 render）驱动 | 用户滚回底部这个动作本身就是"读完了"的信号；render 不等同于位移 |
| 兜底轮询 | 仅当唤醒通道不可用（桥未连上 / chat 过滤器被拒）时武装 5s（`POLL_MS`）；通道起来即完全停摆 | push 与 poll 不得同时驱动重载（§8.0 同一原则）；Desktop 的定时器归会话所有，Mobile 的归"线程在屏 && 通道 down" |
| 登录/登出 | 只有**会话变更**才重置 store；MQTT 连上不是登出 | 把"桥起来了"当作会话变更会清空用户正在看的线程 |
| 切线程 | 已知计数随线程清零，绝不带旧值进新线程 | `total` 是每线程的量：带着上一条线程的计数进新线程，首屏必然被误判成负差值 → 整窗反复替换 |

验证：`apps/acowork-desktop/src/stores/userChatStore.test.ts`（planPage / 信号重载 / 合并 / push-poll 互斥）、`apps/acowork-desktop/src/lib/stickToBottom.test.ts`（贴底门控）、`apps/acowork-desktop/src/stores/chatStore.userChatPush.test.ts`（Tauri 事件扇出 + 账号过滤）、`apps/acowork-mobile/src/__tests__/user-chat-push.test.ts`、`apps/acowork-mobile/src/__tests__/stick-to-bottom.test.ts`、`apps/acowork-mobile/src/__tests__/presence.test.ts`。

## 9. 项目与文档

| 模块 | 层级 | v1 降级 |
|------|------|---------|
| 项目 | 列表 → 看板 → 任务详情 | **看板列降级为横向状态 Chip 筛选**（窄屏不做拖拽看板） |
| 文档 | 目录树 → 文档内容 → 审阅详情 | 只读为主，无富文本编辑 |

所有详情页支持右滑逐级返回。

## 10. 设置

一级四项，进入后是二级设置页：

| 一级 | 二级内容 | v1 状态 |
|------|---------|---------|
| 个人资料 | 账号、头像、角色 | ✅ |
| 通用 | 语言、通知、默认行为 | ✅ |
| 外观 | 主题（浅/深/跟随系统）、高亮色 | ✅ |
| 网关 | 连接地址、模式 | ⚠️ **降级**（见 §11.1） |

## 11. 运行时与平台约束

### 11.1 移动端没有本地 Gateway 进程

Desktop 内嵌/管理本地 Gateway 进程，因此设置里有"启动/停止/重启 Gateway"。**移动端不存在这个进程**——它通过 HTTP/MQTT 连接到某个已运行的 Gateway（通常是用户自己的机器，或 relay 部署）。

因此 v1 中：
- 启动 / 停止 / 重启 Gateway → **禁用**
- Gateway 地址配置 → **保留**（这是移动端最关键的连接设置）
- Gateway 状态显示 → ✅ 保留

### 11.2 技术选型

| 项 | 选择 | 理由 |
|----|------|------|
| 框架 | Tauri v2 | 与 Desktop 同栈，共用 HTTP/MQTT 客户端与类型定义 |
| 前端 | React + TypeScript | 同 Desktop，可复用 `session-control.ts` 等纯逻辑 |
| 样式 | CSS Modules / 原生 CSS | 移动端手感需要精细控制；Tailwind 的 Desktop 断点模型不适用 |
| 组件 | 自建移动端组件集 | inset 列表 / Switch / Segmented / Stepper / Action Sheet |
| 状态 | 复用 Desktop store 模式（Zustand） | 概念一致，迁移成本低 |

**不共用 Desktop 前端代码库**。两个 App 的信息架构、组件、交互范式完全不同，强行共享会积累大量条件分支。共享的是**协议层类型定义**（`SessionInfo`、`can_write` 语义）。

### 11.3 后台执行约束

移动 OS 对 WebView 的后台执行不做保证：退后台数秒内 JS 挂起，WS 连接随之冻结，回前台时可能已死。因此：

- §8 两通道的出口都挂在 `visibilitychange` 上（退后台即停轮询/UNSUBSCRIBE，回前台重启并立即重载一次）；
- 不依赖连接"活着"做任何正确性判断——回前台的第一件事永远是 HTTP 快照（state + messages），事件流只是加速器；
- 后台送达（推送）v1 不做（§1.3），这是平台约束不是功能取舍。

## 12. 与现有文档的关系

| 文档 | 关系 |
|------|------|
| [01-overview.md](./01-overview.md) | §5「移动端深度适配」的具体化 |
| [14-desktop-app.md](./14-desktop-app.md) | Mobile 是 Desktop 的子集终端，共享后端契约 |
| [21-pm-project-management.md](./21-pm-project-management.md) | 项目 Tab 的数据来源 |
| [20-doc-online-document.md](./20-doc-online-document.md) | 文档 Tab 的数据来源 |
| [ADR-009](../../adr/zh/ADR-009-gateway-workspace-isolation.md) | 工作区只做 Add to Chat 的红线依据 |
| [ADR-076](../../adr/zh/ADR-076-multi-user-account-system.md) | 多用户会话可见性/可写性的**权威定义** |
| [ADR-085](../../adr/zh/ADR-085-agent-lifecycle-state-machine.md) | 会话激活的状态前置条件 |
| [24-cloud-relay-remote-access.md](./24-cloud-relay-remote-access.md) | relay 形态的连接拓扑与 `wss://<gw-id>.<域>/mqtt` 入口（§8.1 主通道的存在前提）；mobile 与 Desktop 同权同 ACL |

## 13. 设计决策记录

| 决策 | 选择 | 理由 |
|------|------|------|
| 形态 | IM 风格，非 Desktop 三栏直译 | 移动端是"随时收发消息"，不是"管理一堆面板" |
| 底部 Tab 数量 | 4（无 Harness/Extensions） | 开发者概念不进普通用户导航 |
| 会话列表 | Agent + 联系人合并为单一会话流 | IM 的心智模型是"找人聊天"，不是"选 Agent" |
| 导航栈 | 每 Tab 独立栈 | 切 Tab 保留上下文，符合 IM 习惯 |
| 二级页 TabBar | 隐藏 | 避免两级导航并存 |
| 多会话承载 | 导航栏标题 + Action Sheet | 窄屏放不下并排标签条 |
| 会话摘要来源 | 最近活动会话的最后一条消息 | IM 语义要求 |
| 写权限来源 | 后端 `can_write`，不本地推导 | 公开 ≠ 可写；客户端推导会产生第二份真相 |
| 只读态 textarea | 不渲染 | placeholder 假装可输入是更差的体验 |
| 只读态可见性图标 | 禁用而非隐藏 | 观众需要知道自己在看什么 |
| 新建会话可见性 | 默认 Private | 对齐 ADR-076 创建路径 |
| 工作区 | 只做 Add to Chat，无预览 | 无 Monaco；且文件读取由 Agent 完成更自然 |
| 看板 | 降级为 Chip 筛选 | 窄屏拖拽不可用 |
| Gateway 进程管理 | v1 禁用 | 移动端无本地 Gateway 进程 |
| 前端代码复用 | 仅复用协议层类型 | IA 与组件完全不同，共享会积累条件分支 |
| 启动连接 | 显式状态机：连接屏→`/api/status` 探测→登录屏 | 移动端无本地 Gateway，"从安装到第一次说话"必须先于一切功能存在 |
| 认证 | 复用 ADR-076 `/api/auth/*`（login/refresh/logout），401→refresh→重放一次 | 与 Desktop 同一契约，不开第二套认证；主动预刷新会制造第二套时间语义 |
| 注册 / 配对码 / 忘记密码 | v1 不做，提示回桌面端 | 低频且依赖邀请链路（ADR-076 §决策 6），移动端不承载 |
| 实时事件流 | 双通道：relay 形态 MQTT-over-WSS 精确订阅（主），local/LAN 与降级走前台轮询（2s 全局单循环）；两通道同一 HTTP 重载收口 | 通道由部署拓扑决定，产品策略（只订前台、只显最终结果）不随通道变；v1.1"一律轮询"的前提"WebView 拿不到 MQTT"漏看了 relay 公网 WSS 入口，v1.2 修正 |
| 历史重载判据 | `meta.message_count` 无状态比较（状态迁移仅作附加触发器） | 快回复在两次 tick 之间完成时迁移不可观察；计数是后端权威，任何内容变化必然改变它 |
| 流式输出 | v1 不做，状态指示 + 完整呈现 | 与轮询模型一致；打字机效果是 chunk 通道的理由，通道本身已砍 |
| AskQuestion | WS 通道问答卡；轮询通道降级"桌面端处理"提示 | 事件面可达性决定能力面；伪造一个 HTTP 问答轮询面不如如实分级 |
| 推送通知 | v1 不做 | 无后台通道；厂商推送需独立服务端与证书体系，v2 评估 |
| 未读角标 | 降级为"有新消息"点，不显示数字 | 没有推送就没有真未读；如实降级优于假装精确 |

## 14. 交付物

| 阶段 | 交付物 | 状态 |
|------|--------|------|
| 交互设计 | [`docs/prototypes/mobile-im-v1.html`](../../prototypes/mobile-im-v1.html) | ✅ v1.1（含首启/连接/登录/断连/发送失败屏）；v1.2 待补：WS 通道完整审批卡、问答卡、"实时已降级"状态行 |
| 架构决策 | [ADR-086](../../adr/zh/ADR-086-mobile-app-im-ia-and-multi-session.md) | ✅ 已完成 |
| 工程骨架 | [`apps/acowork-mobile`](../../../apps/acowork-mobile) | ✅ 已完成（Vite + React 19 + Tauri v2） |
| 协议层复用 | 共享类型定义 crate | 计划中 |

### 14.1 工程骨架技术选型

| 项 | 选择 | 理由 |
|---|---|---|
| 壳 | Tauri v2（独立 crate，同 Desktop） | 复用 core crates；WebView 首屏与内存表现优于 RN |
| 前端 | Vite + React 19 + zustand + TypeScript strict | 与 Desktop 同栈，但**不共享代码** |
| 样式 | 原生 CSS + 设计令牌，无 UI 框架 | 移动端规范组件是"少而固定"的；引入框架反而要覆盖它的默认样式 |
| 原生层 | 近乎空壳 | 无本地 Gateway、无托盘、无 LSP 侧车，详见 [§13 的"Gateway 进程管理"](#13-设计决策记录) |
| 移动端 crate 形态 | `[workspace]` 空表，独立解析 | 同 `apps/acowork-desktop/src-tauri`；移动端目标平台与 core 不同，不应继承 feature 统一 |

**目录结构**

```
apps/acowork-mobile/
  src/
    lib/       types.ts（线格式子集）· session-write-access.ts（权限门禁）· api.ts（HTTP transport）
    stores/    navStore（四 Tab 独立栈）· agentStore（目录 + 会话列表唯一持有者）· chatStore（视图态 + 原子 openSession）
    components/ ui.tsx（ListSection/Row·Switch·Segmented·Stepper·Sheet）· EdgeSwipe.tsx
    screens/chat/  ChatListScreen · ChatDetailScreen
    routes.tsx  路由注册表
  src-tauri/    独立 crate，近空壳
```

### 14.2 搭建期实测发现的架构缺陷

`agentStore` 与 `chatStore` 各自持有一份 `sessions` 数组。`session-write-access.ts` 读前者，`chatStore.openSession` 读后者（后者恒为空），因此 `can_write` 永远读不到 `false`——只读会话会错误地发送 `open_session`，composer 门禁失效。

这不是可以"两边都更新一下"的同步问题：**同一事实的两个副本必然分叉**，而分叉方向恰好是权限门禁 fail-open。修正为 `agentStore` 单独持有会话列表，`chatStore` 只保留 per-agent 视图态（当前会话、消息、loading），并通过 `getSession()` 读 `can_write`。该缺陷由 `session-access.test.ts` 的删除用例暴露。

**推论：权限门禁读取的字段，其所有权必须唯一。**

### 14.3 原型验证记录

交互稿经两轮自动化验证：

| 验证层 | 工具 | 用例 | 结果 |
|--------|------|------|------|
| 行为 | jsdom（真实点击 + 合成 touch 事件） | 60 | 60 通过 |
| 布局 | WKWebView 实测 `getBoundingClientRect` | 44 | 44 通过 |

布局层不可省略：jsdom 对所有几何值返回 0，会掩盖真实溢出。原型验证中实测出 4 个真实缺陷（导航栏副标题未渲染、会话切换器撑破导航栏、会话列表因 `max-height` 过高而永不滚动、会话名重复渲染）。
