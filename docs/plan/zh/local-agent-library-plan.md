# Local 本机 agent 文件库 — 开发计划

> 版本：**v1.0（可执行）** | 日期：2026-11-07
>
> 上游 PRD：[`prd-agent-hub-install-dialog.md`](../../prd/zh/prd-agent-hub-install-dialog.md) **§14（v0.2 增补，D9–D14 / REQ-021–034）是唯一需求来源**，本文只排期与拆卡，不重复需求。
> 关联 ADR：[`ADR-073`](../../adr/zh/ADR-073-agent-instance-identity-decomposition.md)（实例身份 / `ensure`）· [`ADR-009`](../../adr/zh/ADR-009-gateway-workspace-isolation.md) §5（Gateway 不碰 node 文件系统）· [`ADR-104`](../../adr/zh/ADR-104-agent-manifest-declaration-surface.md)（`agent-packages/` 产物路径）
>
> **一句话**：把 Local tab 变成"本机 agent 文件库"——**T1** 列表 + 播种 + 幂等安装，**T2** publish 产物回写。合计 **2 人日**，两张卡无共享文件，可并行。

---

## 1. 拆卡理由与依赖

| 卡 | 内容 | 估时 | 依赖 |
| --- | --- | --- | --- |
| **T1** | 库目录 + 播种 + 列表 UI + 末行添加入口 + `ensure` 安装 | 1.5d | 无（`agents/` 改名已完成） |
| **T2** | publish 产物复制进库 + 向导收尾 + 列表刷新 | 0.5d | 依赖 T1 的库目录命令（`library_dir` / 复制函数） |

T1 单独合入即可交付价值（用户能看到并安装 7 个内置 agent）；T2 是闭环增强，T1 不落地时无法进行。

**共享代码只有一个函数**：T1 建的 `copy_into_library(path) -> Result<PathBuf>`，T2 直接调用。所以 T1 必须先把该函数写成 `pub(crate)`。

**目录改名已完成**（commit 范围内 `examples/` → `agents/`，90 个文件纯 rename），本计划不再包含改名工作。

---

## 2. T1 — 桌面端：本机 agent 文件库（列表 + 播种）

**标题**：`feat(desktop): Local tab 列出本机 agent 文件并支持幂等安装`
**类型**：feature ｜ **优先级**：P0 ｜ **估时**：1.5d
**覆盖需求**：REQ-021 ~ REQ-028、REQ-030 ~ REQ-032

### 2.1 背景

装完 Desktop 就有 7 个签名好的 `.agent` 躺在 `resource_dir/agent-packages`，但产品里没有任何入口列出它们：Local tab 只有一个 pick/drop 区（[`AgentLibraryDialog.tsx:850`](../../../apps/acowork-desktop/src/components/agent-list/AgentLibraryDialog.tsx#L850)）。出厂内容用户看不见，等于没出厂。

### 2.2 交付物

**新增 `apps/acowork-desktop/src-tauri/src/commands/agent_library.rs`**

```rust
/// 库目录：app_local_data_dir()/agent-library
pub(crate) fn library_dir(app: &AppHandle) -> Result<PathBuf>;

/// 首次启动播种：resource_dir/agent-packages/*.agent → 库目录。
/// 幂等：目标文件已存在即跳过（不比字节、不覆盖）。F14
pub(crate) fn seed_if_missing(app: &AppHandle) -> Result<usize /* copied */>;

/// T2 复用：把一个 .agent 复制进库（同名覆盖）
pub(crate) fn copy_into_library(app: &AppHandle, src: &Path) -> Result<PathBuf>;

#[tauri::command] pub async fn list_local_agent_packages(app: AppHandle) -> Result<Vec<LocalAgentRow>, String>;

#[tauri::command] pub async fn ensure_local_agent(state: State<'_, AppState>, app: AppHandle,
    package_path: String, node_id: String) -> Result<String /* instance_id */, String>;
```

> **ADR-113 修订**：`node_id` 由 `Option<String>` 收紧为必填 `String`。Gateway 不再推断默认落点；UI 从 `GET /api/nodes` 的可用节点中显式选取并逐层传入。

行结构（`serde::Serialize`）：

| 字段 | 来源 |
| --- | --- |
| `file_name` / `path` | 文件名 / 绝对路径 |
| `agent_id` / `name` / `version` | zip 内 `manifest.toml` → `AgentManifest::from_toml`（`acowork_core::manifest`，[`create.rs:383`](../../../apps/acowork-desktop/src-tauri/src/commands/create.rs#L383) 已在用） |
| `description` | manifest，UI 侧两行截断 |
| `size_bytes` | 文件元数据 |
| `permissions` | manifest 权限列表（只用于"N 项权限"计数，不做高危二次确认——那是 Hub 卡片的事，见 PRD REQ-007 边界） |
| `error: Option<String>` | 单文件损坏时填，**该行仍返回**（PRD §14.4） |

**关键实现约束**

- **一次批量读**：`list_local_agent_packages` 内部循环读 N 个 zip，前端只 invoke 一次（F13）。不要前端 N 次调用。
- **枚举不猜身份**：以目录里的实际文件为准，不维护"名字→文件"映射表（F12 的硬编码 match 就死在这里）。
- **播种时机**：`list_local_agent_packages` 入口处调 `seed_if_missing`，失败不 panic——返回空列表 + 让前端显示 REQ-028 空态。
- **`ensure_local_agent`**：读文件字节 → `GatewayClient::ensure_agent(&bytes, node_id)`（[`gateway_client.rs:558`](../../../apps/acowork-desktop/src-tauri/src/gateway_client.rs#L558) 已有）→ 用 `ack.instance_id` 走已有的 `wait_for_agent_installed`。**不要用 `install_agent`**（那会每次多一个实例，违反 D11/REQ-026）。
- 注册到 [`lib.rs`](../../../apps/acowork-desktop/src-tauri/src/lib.rs) 的 `invoke_handler`。

**前端**

| 文件 | 改动 |
| --- | --- |
| [`src/lib/agentHub.ts`](../../../apps/acowork-desktop/src/lib/agentHub.ts) | 加 `LocalAgentRow` 类型 + `listLocalAgentPackages()` / `ensureLocalAgent()` 包装 |
| [`AgentLibraryDialog.tsx`](../../../apps/acowork-desktop/src/components/agent-list/AgentLibraryDialog.tsx) | `useState<Tab>("library")` → `"local"`（[:87](../../../apps/acowork-desktop/src/components/agent-list/AgentLibraryDialog.tsx#L87)）；open 时 `void listLocalAgentPackages()` + `void fetchHubIndex()`（不 await、失败静默，REQ-022）；`LocalPane` 换成列表组件 + 末行 `+ 添加 agent 文件` |
| 行组件 | 文件名 / name / version / `agent_id` / 大小 / 权限数；主按钮「安装」→ `ensureLocalAgent`；`⋯` →「再装一个实例」→ 现有 `installAgent` |
| [`src/i18n/locales/*.json`](../../../apps/acowork-desktop/src/i18n/locales/) | 五个语言包同步：`tabLocal` 改「本地 / Local」，新增列表空态、行内文案、末行文案、损坏文件提示。`npm run check:i18n` 必须过 |

### 2.3 验收标准

| # | 验收 | 判定 |
| --- | --- | --- |
| A1 | 首次启动后 `%LOCALAPPDATA%\com.acowork.desktop\agent-library\` 出现 7 个 `.agent` | 目录可数 |
| A2 | 打开对话框默认落在 Local，且**不发网络请求**就渲染出 7 行 | 断网（禁用网卡/改 hosts）后仍能打开并列出 |
| A3 | 删掉库目录里一个文件、重启应用，**不会复活** | 手动删 → 重启 → 行数 6 |
| A4 | 列表为空时显示空态 + 可点的「+ 添加 agent 文件」 | 清空目录后验证 |
| A5 | 末行入口选一个外部 `.agent` 后行为与旧版一致（直接装，不落库） | 装完后库目录文件数不变 |
| A6 | 同一行连点两次「安装」只产生 1 个实例 | 侧栏该 agent 实例数不变 |
| A7 | `⋯` →「再装一个实例」后实例数 +1 | ADR-073 多实例合法，不报错 |
| A8 | 往库目录塞一个坏文件（非 zip），其余行照常可用，坏行显示原因 | 不整列表失败 |
| A9 | 同一 `agent_id` 两个版本各占一行 | 手工复制改版本后验证（REQ-030） |
| A10 | 卡片与行都**没有**任何签名/渠道字样 | 组件内 grep「已签名 / official-channel / third-party」无命中 |

### 2.4 地雷（PRD §14.5）

F12 硬编码 match 必须死 ｜ F13 批量命令、前端一次调用 ｜ F14 播种幂等且不覆盖 ｜ F15 禁止按 `agent_id` 去重。

### 2.5 提交粒度建议

1. `feat(desktop): add local agent library dir with idempotent seeding`
2. `feat(desktop): list local agent packages in the install dialog`（含 i18n）
3. `feat(desktop): install local agent files through ensure`

---

## 3. T2 — publish 产物回写本地库

**标题**：`feat(desktop): copy published .agent into the local agent library`
**类型**：feature ｜ **优先级**：P1 ｜ **估时**：0.5d
**覆盖需求**：REQ-029 ｜ **依赖 T1**（`copy_into_library` / `library_dir`）

### 3.1 背景

publish 向导已经能产出 `.agent`（node 侧写 `{agent_id}-{version}.agent`，[`publish.rs:410`](../../../core/acowork-node/src/package/publish.rs#L410)），但产物落在 node 的 `packages_dir` 里，用户拿不到、Local 列表也看不到。回写 = 复制进库目录（D12），**不改 node 的任何写盘逻辑**。

### 3.2 交付物

- [`src-tauri/src/commands/publish.rs`](../../../apps/acowork-desktop/src-tauri/src/commands/publish.rs)：新增
  ```rust
  #[tauri::command] pub async fn save_publish_to_library(
      app: AppHandle, package_path: String,
  ) -> Result<String /* 库内绝对路径 */, String>;
  ```
  内部：校验路径在 node `packages_dir` 下（防任意路径复制）→ `copy_into_library` → 同名覆盖。
- [`PublishWizard.tsx`](../../../apps/acowork-desktop/src/components/agent-list/PublishWizard.tsx) distribute 步骤：sign 成功后自动调用，展示「已保存到本地库」+ 文件名；失败不阻塞向导收尾（只提示，可重试）。
- 关掉对话框后 Local 列表下次打开即含该文件（REQ-032：只在打开/复制后各枚举一次，**不引 watcher**）。
- i18n 五语言包补齐。

### 3.3 验收标准

| # | 验收 |
| --- | --- |
| B1 | 对一个已装 agent 走完 publish（含 sign），库目录出现 `{agent_id}-{version}.agent` |
| B2 | 再次 publish 同一版本 → **覆盖**，库目录不出现第二个文件 |
| B3 | 改了 version 再 publish → 库目录两行都在（不按 `agent_id` 去重） |
| B4 | 回写后打开安装对话框，新文件出现在 Local 列表并可 `ensure` 安装 |
| B5 | remote 模式（gateway 不在本机）下 publish：**不尝试复制**，向导提示"产物在节点 `<node>` 上"（D12 的范围限制，不报错） |

### 3.4 范围限制（写进代码注释）

`output_path` 是**节点本地路径**。单机 local 模式下桌面端可直接读；remote 模式下读不到——按 B5 降级，不做远端拉包（PRD §14.6 非目标）。

---

## 4. REQ → 卡 对照

| REQ | 卡 |
| --- | --- |
| REQ-021 ~ REQ-028、030 ~ 032 | T1 |
| REQ-029 | T2 |
| REQ-033 / 034（目录改名） | **已完成**，不在卡内 |

---

## 5. 收尾时顺手修（不属于任一卡，1 行改动）

PRD §9 **F2 已过期**：原文称"桌面端没有 `ensure` 的客户端方法"，但 Hub 安装链路落地时已经加上了（[`gateway_client.rs:558`](../../../apps/acowork-desktop/src-tauri/src/gateway_client.rs#L558)，[`hub.rs:126`](../../../apps/acowork-desktop/src-tauri/src/commands/hub.rs#L126) 在用）。T1 开工时把该条订正，避免后来者去"新增"一个已存在的方法。

同时 PRD §9 **F17**（`devMode: true` 是死参数）建议在 T1 的提交里一并处理：要么删掉 [`agentStore.ts:940`](../../../apps/acowork-desktop/src/stores/agentStore.ts#L940) 的传参，要么在 Gateway 侧读它——**二选一，不要留着"看着生效其实没生效"的路径**。

---

## 6. 验证命令

```bash
# 前端类型 + i18n 门禁
cd apps/acowork-desktop && npm run check:i18n && npx tsc --noEmit

# Rust 侧单测（不要污染默认 target/，进程占着锁）
cd core && CARGO_TARGET_DIR=target-e2e cargo test -p acowork-core --test example_manifests
cd core && CARGO_TARGET_DIR=target-e2e cargo test -p acowork-sign --test example_packages

# 手工验收：库目录
ls "$LOCALAPPDATA/com.acowork.desktop/agent-library"
```
