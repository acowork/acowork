# PRD：ACowork Agent Hub（官网 Agent 下载页）

| 项 | 内容 |
| --- | --- |
| 状态 | 待评审（v1 已随本文档实现，见 §11 交付清单） |
| 作者 | Product |
| 日期 | 2026-06 |
| 涉及仓库 | `ACoworkWebSite`（页面）、`ACowork`（数据源与打包脚本） |
| 关联 | Roadmap Phase 7「remote `.agent` registry / Agent store」（Planning）；`dev/common/build-agent.sh`；`examples/agent-packages/` |
| 非目标声明 | 本文档不覆盖 skills 目录、付费分发、在线试用 |

---

## 1. 问题定义

**用户侧**：ACowork 处于 alpha，官方已经写好 7 个可直接用的 agent（Product / PM / QA / Architect / Senior Engineer / Docs / Ponytail），并且已经产出了签好名的 `.agent` 包。但今天**没有任何地方能让一个人看到"ACowork 有哪些现成 agent、各自能干什么、要什么权限、怎么装"**。想用的唯一路径是知道 `examples/agent-packages/` 这个目录的存在——这是一个只有源码读者才知道的路径。

**平台侧**：Agent 是 ACowork 的核心分发单元（等价于 Chrome 的扩展、VS Code 的插件），但它的分发目前完全依赖 git clone。生态能不能起来，取决于第三方作者有没有一个"发布有地方去、用户有地方找"的锚点。

**不做的代价**：alpha 用户装机后面对空运行时，转化流失在"我不知道接下来该干什么"；Phase 7 的 registry 缺少一个已经存在的数据契约和展示层。

> 本文档把它定义为**分发问题**，不是**商店问题**。这个区分决定了 v1 的全部范围（见 §4）。

---

## 2. 目标与非目标

### 目标

| # | 目标 | 衡量 |
| --- | --- | --- |
| G1 | 让 alpha 用户在 60 秒内找到一个 agent 并拿到本地可导入的 `.agent` 文件 | 下载点击 → 后续 App 内导入成功（需 App 埋点，v1 为已知缺口） |
| G2 | 让 agent 的能力与**权限**变得可决策 | 权限徽章在卡片层可见率 100% |
| G3 | 给第三方作者一个明确的发布路径 | 合并的外部 PR 数 |
| G4 | 建立 Phase 7 registry 的**数据契约**（`agents.json` schema），网页与未来 App 共用 | schema 被 Desktop App 直接消费，无需改造 |

### 非目标（v1 明确不做）

评分 / 评论 / 安装计数 / 个性化推荐 / 付费与订阅 / 自动更新 / 浏览器内安装 / 用户账号 / 网页上传表单 / 服务端搜索 / skills 独立目录。

**skills 处理方式**：作为 agent 详情内的清单展示（数据来自包内 `skills/*/SKILL.md`），不建独立目录。理由：7 个 agent 已经不足以填满"商店"叙事，同时开两个目录会把稀薄感翻倍。

---

## 3. 用户与场景

| 角色 | 场景 | v1 满足度 |
| --- | --- | --- |
| 评估者 | 装完 ACowork，想知道"能拿来干什么" | ✅ 完整 |
| 实践者 | 有明确需求（"我要个做 PRD 的 agent"），要找最接近的包 | ✅ 搜索 + 角色筛选 |
| 谨慎者 | 担心 agent 在自己机器上干什么 | ✅ 权限外露 + 校验和；⚠️ 但安装侧签名强制校验尚未落地（R10，见 §10） |
| 第三方作者 | 想发布自己调的 agent | ⚠️ 仅 PR 路径，无自助上传 |
| 竞品研究者 | 看生态规模 | ❌ 只有 7 个，这是事实，不做粉饰 |

---

## 4. 关键决策

### D1：搜索做在客户端，不做服务端

**决定**：全量元数据一次性下发（`agents.json`，7 条约 15KB），浏览器内过滤。

**理由**：在 N≤200 的量级，客户端搜索在服务端搜索面前没有短板——没有冷启动、没有网络往返、没有配额消耗、没有索引维护，而且能即时响应输入。Cloudflare Pages 静态请求无限免费，JSON 走边缘缓存。

**放弃的方案**：Pages Functions + D1 全文索引。它在 N>500 或多语言分词需求出现时才划算，届时 §11 的升级路径已预留。

### D2：上传走 GitHub PR，不做网页表单

**决定**：页面上的 "Submit an agent" 按钮跳转到 registry 的 PR 模板，不建任何上传接口。

**理由**：网页上传会一次性引入**鉴权、审核队列、恶意包处置、滥用与配额、备份**五条战线。而 PR 流程用 GitHub 免费提供了同一套能力：作者身份（GitHub 账号）、审核（review + CI）、版本历史（git）、失败回滚（revert）。对一个 alpha 阶段的开源项目，这不是妥协，是更正确的机制。

**代价（明确接受）**：非技术作者门槛高；发布节奏受 git 约束；没有"即时上架"体验。

### D3：`.agent` 二进制放站点同源目录，但放独立子目录

**决定**：`agents/downloads/*.agent`，由脚本从 `ACowork/examples/agent-packages/` 拷入。

**理由与权衡**：

| 方案 | 国内可达 | 成本 | 版本管理 | git 历史 |
| --- | --- | --- | --- | --- |
| 站点同源（选定） | 好（走 Pages CDN） | 0 | 弱 | ⚠️ 每版二进制永久留档 |
| GitHub Releases | 差 | 0 | 强（天然 tag） | 无 |
| R2 公网桶 | 中 | 需实测配额口径 | 中 | 无 |

单包 70–170KB，Pages 单文件上限 25MiB、站点文件上限 20,000，短期无风险。选同源的唯一动机是**下载必须能成功**——一个下载按钮点不开的商店比没有商店更伤。git 历史膨胀的代价在 agent 数 >20 或单包 >5MB 时触发迁移（§11）。

**假设（需验证）**：站点已绑定自定义域名。若仍用 `*.pages.dev`，国内不稳定，D3 的选型收益不成立——列为上线前置项 P0。

### D4：元数据必须生成，禁止手抄

**决定**：`agents.json` 由脚本从 `examples/*/manifest.toml` 解析生成。

**理由**：手抄第二份元数据 = 制造第一份必然过期的文档。manifest 是 agent 的唯一真相源，Hub 只是它的一个投影。这条同时保证了未来 Desktop App 的 registry 与网页看到的是同一份数据。

### D5：文案叫 Library，不叫 Store

**决定**：页面标题 "Agent Library"，副标题说明 alpha 状态与 PR 投稿路径；不出现 Store / Marketplace / 应用商店。

**理由**：7 个包 + 无评分 + 无评论 + 无一键安装 = 不是商店。命名超出实际能力会直接消耗信任，而 alpha 阶段最稀缺的就是信任。

### D5b：不设"官方 / 社区"等级标签（2026-10-29 修订）

**决定**：卡片与详情**不出现** Official / Community 徽章，也不出现"官方 agent""第三方来源""未经审计"这类分级措辞。数据契约删除 `tier` 字段（`schema_version` 1 → 2）。取代它的是**逐个 agent 如实展示 `author`**（来自 manifest 的事实字段）。默认排序由"官方优先"改为**按 display name 字母序**，不再保留 `Featured` 选项。

**理由**：

1. **原判据本身是伪造的**。`tier` 由 `agent_id` 前缀 `com.acowork.` 推导——任何人把自己的包命名成 `com.acowork.sybil` 就能骗到官方徽章。而现有签名体系里 `identity_type` 由签名方自行填写、`verify_chain` 对两种 key_type 恒返回 true（[certificate.rs:38-57](../../core/acowork-sign/src/certificate.rs#L38)），**没有任何可验证的信任根**。在 CA 落地前，任何等级标签都是无法兑现的承诺。
2. **现状是全量同源**。7 个包全部来自 `ACowork/examples/`，"社区"仅因 `github.ponytail` 的 id 不以 `com.acowork.` 开头而被区分——标签传达的并非真实差异。
3. **替代信息更诚实也更充分**。`author` 字段（"ACowork Team" / "DietrichGebert"）是 manifest 里的事实，不需要信任根即可核对；它同时满足 R2 搜索（作者可检索）和 R4 详情展示。

**影响**：搜索索引去掉 `tier`；排序去掉 tier 优先级；`?sort=featured` 旧链接回落为 `name`；CSS 删除 `.ah-tag*` 死规则。**未来若 CA 与信任链落地**，等级标签可作为独立能力重新引入，届时依据是**签名链**，不是 id 字符串前缀。

### D6：页面在 `file://` 下也必须可用（内嵌快照兜底）

**决定**：`data/agents.json` 是首选数据源；取不到时回落到页面内 `<script type="application/json">` 的快照。两份都由 `sync-agent-hub.py` 从同一批 manifest 生成，不是手抄的两份。

**理由**：贡献者最常见的第一次接触方式就是"双击打开 index.html"。此时浏览器以 `file://` 协议加载页面，`fetch('data/agents.json')` 被 CORS 拦掉，页面会退化成一块错误提示——首屏即失败。同时它也是线上 `agents.json` 意外损坏时的兜底。

**代价**：页面 HTML 里多约 15 KB 数据；快照与 JSON 文件存在两份投影，因此脚本对标记缺失采取**硬失败**（标记被改名会让快照静默过期）。

**取舍**：不采用"把数据写进 HTML 就完事"的单一方案——线上仍以独立 JSON 为准，便于缓存与将来换成 registry API（§11）。

### D7：`hidden` 属性必须真正隐藏元素

**决定**：站点样式表全局声明 `[hidden] { display: none !important; }`。

**理由**：`hidden` 只在 UA 样式表里生效（`display:none`），任何作者样式表里的 `display` 声明都会盖掉它。本项目的 `.ah-grid` 是 `display:grid`、`.btn` 是 `inline-flex`，一旦用 `hidden` 切换就会失效——骨架屏与错误面板会同时显示。这是真实发生过的缺陷（截图证据：错误框与骨架屏叠在一起），由真实浏览器回归用例守住。

---

## 5. 需求与验收标准

| ID | 需求 | 验收标准 |
| --- | --- | --- |
| R1 | Agent 卡片网格 | 7 个 agent 全部渲染；卡片含头像、`display_name`、`role`、`description`、能力标签、权限徽章、版本号；头像缺失时回落到首字母占位且不破坏布局 |
| R2 | 客户端搜索 | 匹配 `name`/`display_name`/`role`/`description`/capability/skill 名；输入即时响应（本地 7 条 <50ms）；大小写不敏感；支持多词 AND；无结果时显示空态 + "清除搜索" |
| R3 | 分类筛选与排序 | 至少按角色族筛选；筛选/搜索状态写入 URL query，刷新与分享后保持一致；显示"共 N 个"结果计数 |
| R4 | 详情视图 | 点击卡片展开详情：完整 description、capabilities（名称+描述）、skills 清单、`version`、`runtime_version`、`author`、`agent_id`、权限列表、包大小与 SHA-256 |
| R5 | **权限外露** | `permissions`（含 Shell / FilesystemWrite / Network）在**卡片层**可见，不得仅存在于详情内；高危权限（Shell、FilesystemWrite）使用警示色 |
| R6 | 不做等级标签 | 页面**不得**出现 Official / Community 徽章或"官方/社区""未经审计"措辞；每个 agent 卡片与详情如实显示 manifest 的 `author`；`agents.json` 不含 `tier` 字段 |
| R7 | 下载与安装说明 | 下载按钮指向同源 `.agent`；页面明示"下载后在 Desktop App 中导入"；**不得**出现任何"一键安装/自动安装"表述（App 侧无 deep link 协议，已核实） |
| R8 | 投稿入口 | "Submit an agent" → registry PR 模板；页内说明需 `manifest.toml` + 签名（`build-agent.sh`） |
| R9 | 站点保持零构建 | 不引入 `functions/`、`wrangler.toml`、npm 依赖；同步逻辑用 bash/python；`package.json` 不出现 |
| R10 | 主题与可访问性 | 复用 `styles.css` 变量，浅色/深色两套均可读（`prefers-color-scheme`）；键盘可完成"搜索→筛选→展开详情→下载"全链路；焦点可见；`aria-live` 播报结果数 |
| R11 | 数据新鲜度 | 脚本幂等；manifest 变更后重跑即更新；生成文件带 `generated_at` 与来源 commit；缺包或解析失败**报错退出**而非静默跳过 |
| R12 | 本地可预览 | 直接双击 `agents/index.html`（`file://`）时页面仍能渲染出全部卡片；此时显示"离线快照"提示条，说明如何用本地服务器获得实时列表 |
| R13 | 状态切换真实可见 | 骨架屏、网格、空态、错误态之间的切换必须真正隐藏/显示（`hidden` 生效），不得出现骨架屏与错误面板同屏 |

---

## 6. 数据契约（`agents.json`）

```jsonc
{
  "schema_version": 2,
  "generated_at": "2026-06-01T00:00:00Z",
  "source": { "repo": "acowork/ACowork", "ref": "<sha>", "path": "examples/" },
  "agents": [{
    "agent_id": "com.acowork.product-manager",
    "version": "1.0.0",
    "name": "Product",
    "display_name": "Product",
    "role": "Product Manager",
    "description": "…",
    "author": "ACowork Team",
    "category": "product",           // 派生：role → 分类映射
    "runtime_version": "0.1.0",
    "avatar": "avatars/com.acowork.product-manager.jpg",
    "permissions": ["FilesystemRead", "FilesystemWrite", "Network"],
    "capabilities": [{ "key": "prd_writing", "description": "…" }],
    "skills": ["prd-writing", "user-research"],
    "package": {
      "file": "downloads/com.acowork.product-manager.agent",
      "size_bytes": 74489,
      "sha256": "…"
    }
  }]
}
```

**契约要求**：`category`、`sha256`、`skills` 均为**生成期派生字段**，manifest 里没有；消费方（网页、未来 App）只读不猜。字段缺失即视为生成失败。

> **`schema_version` 2 的变更**：删除 `tier` 字段（D5b）。消费方读到 `tier` 不应再做任何等级展示；读到缺失字段也不应报错——它本就不属于契约。
>
> **`author` 是事实，不是评价**：它直接来自 manifest，描述"谁写的"，不描述"可不可信"。展示 author 允许、也期望不同 agent 的 author 不同——那是信息，不是分级。

---

## 7. 设计要点（需覆盖的状态）

| 状态 | 处理 |
| --- | --- |
| 加载中 | 骨架卡片（避免 CLS）；JSON 拉取失败不白屏 |
| 离线快照 | `file://` 或 JSON 不可达时用内嵌快照兜底，顶部显示提示条（不是错误页）——首屏必须始终有内容 |
| 隐藏生效 | 任何用 `hidden` 切换的容器都必须真正不可见；`[hidden] { display: none !important; }` 是硬约束（D7） |
| 空结果 | 文案 + "清除筛选"按钮 + 指向投稿入口 |
| 加载失败 | 明确错误 + 重试；不使用 `alert` |
| 详情展开 | 抽屉/内联展开，非新窗口；ESC 与点击外部可关闭 |
| 响应式 | ≥1024 三列 / 768 两列 / <480 单列；工具栏在窄屏换行不溢出 |
| 权限警示 | Shell/FilesystemWrite 用高对比标记，不用纯色依赖（配文字） |
| 下载中 | 按钮不进入假成功状态；`.agent` 走浏览器原生下载 |

---

## 8. 度量

| 指标 | 定义 | 目标 | 采集方式 |
| --- | --- | --- | --- |
| Hub 到达率 | `/agents` PV ÷ 站点 PV | ≥15% | Pages 分析 |
| 下载转化 | 下载点击 ÷ 卡片曝光 | ≥8% | 前端事件（v1 仅 `data-*` 标记，不接第三方分析） |
| 搜索使用率 | 用过搜索的会话 ÷ Hub 会话 | ≥20%；**低于此说明信息架构失败**，应改分类而非加搜索 | 前端事件 |
| 投稿转化 | "Submit" 点击 → 合并 PR | 合并外部 PR ≥3/月 | GitHub |
| 安装完成率 | 下载 → App 导入成功 | 未知 | ❌ 需 App 侧埋点，v1 记为已知缺口 |

**基线**：全部为 0（新页面），上线首日建立基线，不设"相对提升"目标。

---

## 9. 发布计划

| 阶段 | 内容 | 出口条件 |
| --- | --- | --- |
| P0 前置 | 确认自定义域名已绑定；确认国内可正常打开 | 移动端蜂窝网络实测通过 |
| P1 内测 | 页面 + 7 个 agent，导航栏入口，无投稿按钮 | 本地静态服务全链路可用；R1–R7、R9–R11 通过 |
| P2 公开 | 加投稿入口 + PR 模板；官网导航暴露 | 文档库/README 交叉引用；无 404 |
| P3 复盘 | 看 §8 指标 | 决定是否走 §11 升级路径 |

**回滚**：页面为独立目录 + 一条导航链接，回滚 = 撤导航链接 + 删目录，不影响主站。无需 kill switch。

---

## 10. 风险与开放问题

| 风险 | 等级 | 处置 |
| --- | --- | --- |
| **装了带 Shell+Network 权限的第三方 agent = 把用户机器交给别人的 prompt**；而 `install/mod.rs` 尚未强制签名校验 | **高** | 缓解：权限外露（R5）+ 逐个 agent 如实显示 `author`（D5b）+ 校验和（R4）。**注意：D5b 取消了 Community 标识这一层预警，风险敞口比原先大**——用户看到的是一个自洽的 agent 卡片，而非"这个来源和别的不同"的提示。根治仍是安装侧签名强制校验落地（开放问题 Q1，需 Core 团队答复）；在信任链落地前，第三方向 Registry 投稿需**人工审核**，这是当前唯一的闸门 |
| 7 个包撑不起"商店"叙事，反而显空 | 中 | D5 命名降级；首页只放"agent 库"而非"生态" |
| 国内 `*.pages.dev` 打不开 | 中 | P0 前置项，未解决则不上线 |
| 跨仓库同步靠人工，会忘记重跑 | 低 | 脚本幂等 + 写入 ACowork 发版 checklist；后续可挂 CI |
| Pages Functions 能力未实测（10ms CPU/请求等） | 低（v1 不依赖） | 升级前做一次 spike |

**开放问题**

- **Q1（阻塞生态，不阻塞 v1）**：安装侧何时强制校验签名？在此之前是否应在 Hub 上明示"社区包未经平台审核"？
- **Q2**：Desktop App 是否需要注册 `acowork://` 协议以支持一键安装？若需要，Hub 的下载按钮应同时提供 deep link 形态。
- **Q3**：`agents.json` 是否作为 Phase 7 registry 的正式对外契约？若是，需要版本号与兼容性策略（当前 `schema_version: 1`）。
- **Q4**：分类体系（`category`）由谁维护、扩展时谁审批？当前是脚本内的硬映射表。

---

## 11. 升级路径（触发式，不排期）

| 触发条件 | 动作 |
| --- | --- |
| agent 数 > 20 或单包 > 5MB | 二进制迁出站点仓库 → R2 桶或 GitHub Releases，Hub 改为外链 |
| 出现非 ACowork Team 的包被下载 | **必须**先补：安装侧签名强制校验 + 审核流程 + 包内容扫描 |
| Desktop App 需在线拉列表 | 上 Pages Functions `/api/agents`（KV/D1 存 `agents.json`），网页与 App 共用同一契约（D4 已为此铺路） |
| 需要真实安装计数 | 埋点走 Function 或 Pages 分析事件，不在静态站猜 |
| N > 500 或多语言分词 | 服务端搜索（D1 反转） |

---

## 12. 交付清单（v1）

| 文件 | 仓库 | 作用 |
| --- | --- | --- |
| `agents/index.html` | ACoworkWebSite | 页面骨架与工具栏 |
| `agents/agents.js` | ACoworkWebSite | 渲染 / 搜索 / 筛选 / 详情 / URL 状态 |
| `agents/data/agents.json` | ACoworkWebSite | 生成物，单一数据源投影 |
| `agents/index.html`（`ah-data` 块） | ACoworkWebSite | 生成物，`file://` 兜底快照（D6） |
| `agents/downloads/*.agent` | ACoworkWebSite | 7 个签名包 |
| `agents/avatars/*` | ACoworkWebSite | 头像 |
| `styles.css`（`ah-*` 段） | ACoworkWebSite | 复用变量，双主题 |
| `index.html`（导航） | ACoworkWebSite | Agents 入口 |
| `dev/common/sync-agent-hub.py` | ACowork | manifest → JSON + 拷包 + 头像 + 内嵌快照（幂等、失败即退出） |
| `dev/agent-hub/hub.test.js` | ACowork | 功能测试：jsdom + 真实 Chrome headless，覆盖渲染/搜索/分面/排序/深链/详情/空态/错误态/XSS/file:// 与 http:// 双协议 |
