# 原型（Prototypes）

单文件交互设计稿，无构建依赖，浏览器直接打开。原型**不是**产品代码，只用于在
动手前把布局和交互意图对齐；确认后再落成 `apps/acowork-desktop` 下的组件。

```bash
# Windows
start docs/prototypes/pm-list-view.html
start docs/prototypes/mobile-im-v1.html
```

---

## 1. PM 任务视图：卡片（看板）/ 列表 — `pm-list-view.html`

`acowork-pm` 的任务展示当前只有「四个状态卡片 + 列内列表」这一种形态。本原型在
它之上加**顶部视图切换**，并新增一整套**列表视图**（一行一个任务，默认只显示核心
列，可按需添加字段列）。数据字段与 `apps/acowork-desktop/src/lib/pm-types.ts`
的 `PmTaskResponse` 对齐，样式令牌取自 `apps/acowork-desktop/src/styles/globals.css`。

| 原型区域 | 落点（已实现） |
| --- | --- |
| 顶部视图切换 segmented | `views/pm/ProjectHeader.tsx`（右上，紧邻 `新建任务`；离线**不**禁用） |
| 卡片视图（原型里是简化还原） | `views/pm/KanbanBoard.tsx` + `KanbanColumn.tsx` + `TaskCard.tsx`（本次未改） |
| 视图路由与偏好读写 | `views/pm/ProjectBoard.tsx`（乐观镜像 + 失败回退） |
| 列表视图主体 | 新增 `views/pm/TaskListView.tsx`（派生链装配 + 交互） |
| 列注册表 / 筛选 / 排序 / 树 / 分组 / 分页 | 新增 `lib/pm-list-columns.ts`（纯函数，脱离 React 单测） |
| 行渲染 / 工具栏 / 分页器 / 批量条 | 新增 `views/pm/TaskListRows.tsx`、`TaskListToolbar.tsx`、`TaskListPager.tsx`、`TaskListBulkBar.tsx` |
| 字段列配置弹层 | 新增 `views/pm/TaskFieldPicker.tsx`（上移 / 下移代替拖拽排序） |
| 弹层原语 / 列宽拖拽 / 状态与优先级配色 | 新增 `views/pm/pmPopover.tsx`、`PmColumnResizer.tsx`、`taskListLabels.ts` |
| 偏好持久化（**项目 metadata，非 localStorage**） | 新增 `lib/pm-view-settings.ts`：`pmView` / `pmListColumns` / `pmListPageSize` |
| 内联改状态 / 批量操作 | 复用 `stores/pm/boardStore.ts` 的 `moveTask` / `reviewTask` / `upsertTask` |

验证情况：`node --check` 通过；jsdom 驱动 34 项断言（视图切换 / 加列 / 排序 /
搜索 / 内联改状态 / 批量选择 / 分组 / 子任务折叠 / 行内新建）全通过。驱动脚本
一次性使用，未入库。

---

## 2. Mobile App（IM 形态 v1）— `mobile-im-v1.html`

`mobile-im-v1.html` — 单文件交互设计稿，无构建依赖，浏览器直接打开。

```bash
open docs/prototypes/mobile-im-v1.html
```

### 验证情况

> ⚠️ 下列数字来自一次性验证运行，其驱动脚本位于 `dev/tmp/` 且**未入库**。
> 当前提交只包含设计稿本身；在此重跑前，这些数字不可复现，也不应被当作
> 持续通过的 CI 门禁引用。

- 语法：9 个 `<script>` 块全部通过解析（提交时已复验：`node --check` 通过）
- 行为：jsdom 驱动 60 项断言（6 项 IA 需求 + 多会话 + 权限矩阵）全通过
- 布局：WKWebView 实测 390×844 下无横向溢出（scrollWidth == clientWidth），
  TabBar 贴底，会话行 66px，无小于 32px 的点击目标
- 视觉：13 张快照见 `_shots/`（已 gitignore，仅本地核对用）

### 与 Desktop 的映射

| Mobile | Desktop 源 |
| --- | --- |
| 底部 Tab Bar | `components/layout/NavBar.tsx`（Harness/Extensions 不上 App） |
| 聊天列表 | `agent-list/AgentList.tsx` + `user-list/UserList.tsx` 合并为 IM 会话流 |
| 聊天详情 | `chat/ChatPanel.tsx` |
| 左滑抽屉 | `right-panel/RightPanel.tsx`（RightNavBar 的 6 个 tab） |
| 项目 | `views/ProjectsView.tsx` + `views/pm/*` |
| 文档 | `views/DocsView.tsx` + `views/doc/*` |
| 设置 | `components/settings/SettingsPage.tsx`（4 个二级页） |
