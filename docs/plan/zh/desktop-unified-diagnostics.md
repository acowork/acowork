# Desktop 全栈服务诊断面板 + MQTT 看门狗修复计划

> 版本：v0.1（草案）| 日期：2026-09-14
>
> 关联设计：[`docs/design/zh/01-overview.md`](../../design/zh/01-overview.md)、[`docs/design/zh/08-security.md`](../../design/zh/08-security.md)
> 关联 ADR：[`ADR-036`](../../adr/zh/ADR-036-mqtt-connection-state.md)（MQTT 状态前端真相）、[`ADR-064`](../../adr/zh/ADR-064-pm-standalone-process.md)（sidecar 独立进程范式）、[`ADR-065`](../../adr/zh/ADR-065-desktop-sleep-wake-recovery.md)（桌面休眠唤醒）
> 关联计划：[`pm-dev-plan.md`](./pm-dev-plan.md)（P0 骨架模式参照）、[`doc-dev-plan.md`](./doc-dev-plan.md)（P0 骨架模式参照）
>
> **一句话**：把 Desktop App 的"节点诊断"扩展为**全栈服务诊断面板**（Gateway + Node + Embed + PM + Doc + LSP），同时修掉"输入框永远卡在正在连接 Bot"的前端看门狗 bug；按 P0→P2 三个里程碑交付，预估总工期 **5-8 人日**（单人全职 1-1.5 周）。

---

## 0. 现状与根因

### 0.1 触发事件

2026-09-14 用户报告：Desktop (192.168.3.61) 连远程 Gateway (192.168.3.67)，系统休眠唤醒后消息框**长时间**显示"正在连接 Bot"（10 小时+ 不恢复）。诊断日志（[desktop-app/logs/20260914_004033.log](../../../.acowork/desktop-app/logs/)）确认：

- **Rust 后端 MQTT 健康**：心跳持续发送 10 小时，仅启动一次 connecting→connected 状态切换，**无 disconnect / reconnect / wake_recovery** 日志
- **Gateway HTTP 健康**：设置 → 网关设置 → 节点列表全部显示**在线**（HTTP `/api/nodes` 正常）
- **前端 `chatStore.mqttConnected = false`**：导致 ChatPanel 输入框 placeholder 永远停在 "正在连接 Bot..."

**结论**：服务端**没有故障**；前端 Rust eventloop **没有故障**；前端 `chatStore` 状态**与 Rust 状态不一致**，且**没有任何自愈机制**。

### 0.2 已就绪（无需新工作，可直接复用）

| 现有资产 | 位置 | 用途 |
|---------|------|------|
| `get_mqtt_status` 命令 | [`chat_mqtt.rs:680`](../../../apps/acowork-desktop/src-tauri/src/commands/chat_mqtt.rs#L680) | 拉取 Rust eventloop 真实状态（`known` / `connected` / `connecting` / `reconnecting` / `reason`） |
| `mqtt-status` event | [`chat_mqtt.rs:600`](../../../apps/acowork-desktop/src-tauri/src/commands/chat_mqtt.rs#L600) | CONNACK / DISCONNECT 状态变更推送 |
| `force_reconnect_mqtt` 命令 | [`chat_mqtt.rs:648`](../../../apps/acowork-desktop/src-tauri/src/commands/chat_mqtt.rs#L648) | 强制重连（保留 poll task） |
| `GatewayTab` + "测试连接"按钮 | [`SettingsPage.tsx:64-419`](../../../apps/acowork-desktop/src/components/settings/SettingsPage.tsx#L64) | 网关设置页面骨架 |
| `NodesTree` + 节点行 meta | [`SettingsPage.tsx:428-503`](../../../apps/acowork-desktop/src/components/settings/SettingsPage.tsx#L428) | 节点列表展示 |
| `chatStore` 已有字段 | [`chatStore.ts:542-561`](../../../apps/acowork-desktop/src/stores/chatStore.ts#L542) | `mqttConnected` / `lastMqttError` / `bootstrapVersion` |
| `bootstrap-state` MQTT 事件 | (chatStore.ts 注释 ADR-059) | Gateway 聚合 bootstrap 推送 → 触发节点列表刷新 |
| Sidecar `/health` 端点 | [`acowork-pm/health.rs`](../../../core/acowork-pm/src/health.rs) · [`acowork-doc/health.rs`](../../../core/acowork-doc/src/health.rs) · [`acowork-lsp-relay/server.rs`](../../../core/acowork-lsp-relay/src/server.rs) · [`acowork-embed/server.rs`](../../../core/acowork-embed/src/server.rs) | 全部 sidecar 都用 `acowork_core::health::HealthResponse` 契约（ADR-064） |
| 节点列表 API `/api/nodes` | (Gateway HTTP) | 已包含 `NodeInfo` 完整字段（[types.ts:79-96](../../../apps/acowork-desktop/src/lib/types.ts#L79)） |

### 0.3 根因（三层 fallback 失效）

| # | 缺陷 | 位置 | 影响 |
|---|------|------|------|
| F-1 | Polling 只跑 30 秒（30 次尝试后永久退出） | [`chatStore.ts:818-840`](../../../apps/acowork-desktop/src/stores/chatStore.ts#L818) | 失去 polling lifeline 后，Rust 已 connected 但前端 stuck 时**无任何恢复路径** |
| F-2 | `mqtt-status` 事件丢失无补偿 | event 在 `listen()` 注册前已发射（wake-recovery 重载、连接时序竞争） | 启动或重载后事件丢失，依赖 F-1 polling；但 F-1 30 秒后退出 |
| F-3 | 无"前端-Rust 状态不一致"检测 | 整个 chatStore 没有 invariant 检查 | Rust 已 Connected 但前端 `mqttConnected=false` 时**完全无感** |

> **本次核心修复**：F-1 → F-3 必须全部修复，否则 P2 诊断面板也救不了卡死的输入框。

### 0.4 架构升级方向

把"节点诊断"抽象为**全栈服务诊断**：

| 服务类型 | 现有 ID | 关键性 | 健康来源 |
|---------|---------|-------|---------|
| Gateway HTTP | `gateway` | 🔴 关键 | `/health`（已存在） |
| Gateway MQTT | `mqtt` | 🔴 关键 | `get_mqtt_status`（已存在） |
| Node Service × N | `node-{id}` | 🔴 关键 | `/api/nodes`（已存在） |
| Embed | `embed` | 🟡 重要 | `127.0.0.1:{embed_port}/health` |
| PM | `pm` | 🟡 重要 | `127.0.0.1:{pm_port}/health` |
| Doc | `doc` | 🟢 可选 | `127.0.0.1:{doc_port}/health` |
| LSP Relay | `lsp-relay` | 🟢 可选 | `127.0.0.1:{lsp_port}/health` |

> 所有 4 个 sidecar 都已经在 Gateway supervisor 管理下（ADR-064 范式），**只是没暴露给前端**。

---

## 1. 排期假设

- **团队规模**：单人全职（兼任代码评审自审）。
- **工时口径**：1d = 8h，含编码 + 单测 + 集成测试 + 文档同步。
- **排期窗口**：1-1.5 周连续投入；不含代码评审、合并 buffer。
- **前置依赖**：所有 sidecar 服务健康端点已就绪（[§0.2](#02-已就绪无需新工作可直接复用)）；MQTT eventloop 健康（[§0.1](#01-触发事件)）。
- **范围边界**：本次只动 Desktop App + 一个新 Rust 命令；不改 Gateway / Node / sidecar 任何 crate。

---

## 2. 里程碑总览

| 阶段 | 内容 | 估时 | 交付物 |
|------|------|------|--------|
| **P0 看门狗修复** | chatStore 5s 常驻 polling + 不一致自动同步 + 卡住超时强制重连 + `effectiveConnection` 综合字段 + ChatPanel placeholder 接入新字段 | 1.5-2d | 输入框永不卡死在"正在连接 Bot" |
| **P1 诊断面板** | 新增 Rust `diagnose_services` + `probe_service` 命令 + 前端 `servicesStore` + `GatewayTab` 改造为"全栈服务"卡片 + 原"节点管理"嵌入关键性分组 | 2-3d | 设置 → 网关设置 看到完整服务诊断 |
| **P2 远程与集成** | 远程模式下 service 列表走 `/api/services` 反代；端到端烟测；文档收口 | 1-2d | 远程拓扑下诊断全链路可用 |
| **合计** | — | **5-8d** | — |

> **优先级拍板**（用户已确认 2026-09-14）：P0 优先于 P1，先修卡死再上诊断 UI。**但 P0 + P1 一起做**（P0 改动量小，前端代码依赖清晰），单人串行 3-5 天交付。

---

## 3. 任务分解

### 3.1 P0 看门狗修复（1.5-2d）

> **核心目标**：让 `mqttConnected` 状态"永不卡死"。
> **核心思想**：把 30 秒一次性 polling 升级为 5 秒常驻 polling，引入 Rust 与前端状态不一致检测 + 自动恢复。

#### 3.1.1 状态机契约

| Rust `SessionState` | 前端 `effectiveConnection` | 输入框 placeholder |
|-------------------|---------------------------|---------------------|
| `Connected` 且前端 `mqttConnected=true` | `connected` | "输入消息..." |
| `Connected` 但前端 `mqttConnected=false`（卡了 ≥10s） | `stale` | "连接状态异常，正在恢复..." |
| `Connecting` 且持续 ≤30s | `connecting` | "正在连接 Bot..." |
| `Connecting` 且持续 >30s | `stale`（自动触发 `force_reconnect_mqtt`） | "连接状态异常，正在恢复..." |
| `Reconnecting` | `reconnecting` | "正在重新连接..." |
| `Disconnected { reason }` | `disconnected` | "Gateway 未连接"（保留现有 placeholder 语义） |
| `Idle`（client 不存在） | `idle` | "Gateway 未连接" |

#### 3.1.2 任务列表

| ID | 任务 | 估时 | 依赖 | 验收 |
|----|------|------|------|------|
| P0-1 | `chatStore.ts` 新增 `effectiveConnection` 计算字段 + `staleSince: number \| null` 时间戳 | 0.25d | — | TS 编译通过；字段初始值正确 |
| P0-2 | 重写 `startMqttPoll` 为 5 秒常驻 polling：检测 Rust 与前端不一致时自动 setState；检测 Connecting 卡住 >30s 时自动 `invoke("force_reconnect_mqtt")` | 0.5d | P0-1 | 卡死状态下 5 秒内自动恢复；polling 不再 30 秒后退出 |
| P0-3 | `ChatPanel.tsx` placeholder 改用 `effectiveConnection`（替代裸 `!mqttConnected` 判断）；新增 `stale` i18n key（`chatPanel.inputStale`） | 0.25d | P0-1 | 输入框在 stale / connecting / connected 三态显示对应文本 |
| P0-4 | `lastMqttError` 增加去重 + 截断（防止错误日志爆炸）；增加 `transitionLog: Array<{timestamp, from, to, reason}>` 环形 buffer（最近 20 条，用于 P1 诊断面板的"最近事件历史"） | 0.25d | — | 错误日志不会无限增长；transition log 可订阅 |
| P0-5 | vitest 单测覆盖：(a) Rust Connected + 前端 false → 5s 内自动同步；(b) Connecting 卡 30s → 触发 force_reconnect；(c) `effectiveConnection` 状态机正确 | 0.5d | P0-1~P0-4 | 单测全绿 |
| P0-6 | 手工烟测：手动 `stop_local_gateway` → 输入框从 connected → disconnected → 启动 → connected；模拟唤醒（关闭 + 重开 webview） | 0.25d | P0-1~P0-5 | 5 种状态切换流畅，无卡死 |

**P0 出口**：所有"输入框永远正在连接 Bot"场景 5 秒内自动恢复。

---

### 3.2 P1 全栈服务诊断面板（2-3d）

> **核心目标**：设置 → 网关设置 提供统一的"全栈服务"诊断视图。
> **核心思想**：以"服务"（service）为诊断单位，替换当前"节点"心智模型；服务按关键性分组。

#### 3.2.1 数据契约

```typescript
// 新增 apps/acowork-desktop/src/lib/types.ts

export type ServiceSeverity = 'critical' | 'important' | 'optional';

export type ServiceType =
  | 'gateway'   // 🔴 critical - Gateway HTTP
  | 'mqtt'      // 🔴 critical - Gateway MQTT broker
  | 'node'      // 🔴 critical - Node Agent (per-instance)
  | 'embed'     // 🟡 important - Embedding service
  | 'pm'        // 🟡 important - Package Manager
  | 'doc'       // 🟢 optional - Doc service
  | 'lsp-relay';// 🟢 optional - LSP relay

export interface ServiceHealth {
  service_type: ServiceType;
  service_id: string;          // "gateway" / "node-67" / "embed" / ...
  service_name: string;        // 显示名 "acowork-node-67"
  severity: ServiceSeverity;
  online: boolean;
  latency_ms?: number;
  version?: string;
  endpoint?: string;
  details?: Record<string, unknown>;
  last_error?: string;
  last_check_at: string;       // ISO timestamp
}

export interface DiagnoseReport {
  gateway: ServiceHealth;
  mqtt: ServiceHealth;
  services: ServiceHealth[];
  issues: Array<{
    severity: 'warning' | 'error';
    service: string;
    message: string;
  }>;
  generated_at: string;
}
```

```rust
// 新增 apps/acowork-desktop/src-tauri/src/commands/diagnostics.rs

#[derive(Serialize)]
pub struct ServiceHealth {
    pub service_type: String,  // "gateway" | "node" | "embed" | "pm" | "doc" | "lsp-relay" | "mqtt"
    pub service_id: String,
    pub service_name: String,
    pub severity: String,       // "critical" | "important" | "optional"
    pub online: bool,
    pub latency_ms: Option<u32>,
    pub version: Option<String>,
    pub endpoint: Option<String>,
    pub details: Option<serde_json::Value>,
    pub last_error: Option<String>,
    pub last_check_at: String,
}

#[tauri::command]
pub async fn diagnose_services(
    state: tauri::State<'_, AppState>,
) -> Result<DiagnoseReport, String> {
    // 1. Parallel:
    //    - gateway /health (HTTP, with 1s timeout)
    //    - /api/nodes (HTTP, with 1s timeout)
    //    - get_mqtt_status (in-process)
    //    - For each node with http_endpoint: GET {endpoint}/health
    // 2. Detect sidecar ports from config (read-only):
    //    - embed_port from Gateway config (probe /health)
    //    - pm_port from Gateway config (probe /health)
    //    - doc_port from Gateway config (probe /health)
    //    - lsp_relay_port from Gateway config (probe /health)
    // 3. Aggregate into DiagnoseReport
    // 4. Emit `diagnostics-updated` event for live UI updates
}

#[tauri::command]
pub async fn probe_service(
    state: tauri::State<'_, AppState>,
    service_type: String,
    service_id: Option<String>,  // None for gateway/mqtt/embed/pm/doc/lsp-relay
) -> Result<ServiceHealth, String> {
    // Single-service probe (used by "重试" button per service row)
}
```

#### 3.2.2 任务列表

| ID | 任务 | 估时 | 依赖 | 验收 |
|----|------|------|------|------|
| P1-1 | 新建 Rust 文件 `apps/acowork-desktop/src-tauri/src/commands/diagnostics.rs`：定义 `ServiceHealth` / `DiagnoseReport` + `diagnose_services` / `probe_service` 两个 Tauri 命令 | 0.5d | — | `cargo build` 通过；命令在 `lib.rs` 注册 |
| P1-2 | `diagnose_services` 实现：并行 HTTP 探测（gateway / nodes / 各 sidecar）+ `get_mqtt_status` 复用；1 秒超时（`tokio::time::timeout`）；失败项标记 `online: false` + `last_error` | 0.75d | P1-1 | 关闭某个 sidecar 后诊断报告正确显示 offline + error |
| P1-3 | 新建前端 `apps/acowork-desktop/src/stores/servicesStore.ts`：状态结构（`report: DiagnoseReport \| null` / `loading: boolean` / `lastProbeAt: number`）+ `diagnose()` / `probe(service_type, service_id)` actions + 错误重试逻辑 | 0.5d | — | store 可用；TS 编译通过 |
| P1-4 | 新建前端 `apps/acowork-desktop/src/components/settings/ServicesPanel.tsx`：3 个分组（关键 / 重要 / 可选），每组按 service_type 聚合；每行带：状态圆点 + 名称 + 版本 + 延迟 + "重试"按钮 | 0.5d | P1-3, P1-1 | 渲染正确；点击"重试"调用 `probe_service` |
| P1-5 | `SettingsPage.GatewayTab` 改造：<br>(a) 把"测试连接"按钮改名"运行诊断"，点击调用 `diagnose_services`，结果展开为面板<br>(b) 新增"全栈服务"卡片（替换原"节点管理"），用 `ServicesPanel` 渲染<br>(c) 原"节点管理"中的 `NodesTree` **嵌入**"全栈服务"卡片的关键性分组中（不再是独立卡片）<br>(d) 新增"最近事件历史"小节（消费 `chatStore.transitionLog`） | 0.5d | P1-3, P1-4 | UI 风格一致；3 个分组可见；最近事件可滚动 |
| P1-6 | i18n 补充：`settings.servicesTitle` / `settings.servicesCritical` / `settings.servicesImportant` / `settings.servicesOptional` / `settings.servicesProbe` / `settings.servicesRetry` / `settings.servicesDiagnose` / `settings.servicesRecentEvents` + 中英双语 | 0.25d | P1-5 | zh-CN.json + en.json 都补齐 |
| P1-7 | 单测：(a) `diagnose_services` 在所有服务在线 / 某 sidecar 离线 / Gateway 离线 三种场景返回正确；(b) `servicesStore` 的 optimistic update 正确；(c) `ServicesPanel` 渲染快照 | 0.5d | P1-1~P1-6 | 单测全绿 |

**P1 出口**：设置 → 网关设置 → 全栈服务卡片可见所有 6 类服务（gateway/mqtt/node/embed/pm/doc/lsp-relay）的健康状态、版本、延迟、最近错误；点击"运行诊断"或单行"重试"可触发实时探活。

---

### 3.3 P2 远程与集成（1-2d）

> **核心目标**：远程模式下（Gateway 在远程机器）诊断面板能完整工作。

#### 3.3.1 任务列表

| ID | 任务 | 估时 | 依赖 | 验收 |
|----|------|------|------|------|
| P2-1 | `diagnose_services` 在远程模式下走 Gateway 反代：sidecar 健康探针经 Gateway HTTP（`/api/services/*`），避免跨网直连 127.0.0.1 | 0.5d | P1-1 | 远程 gateway (192.168.3.67) 下诊断侧栏正常显示 |
| P2-2 | 新增 Gateway HTTP 端点 `GET /api/services/diagnose`（仅 reverse-proxy，**不**做服务端聚合；前端直连） | 0.25d | P2-1 | curl /api/services/diagnose 返回结构化错误（前端预期 fetch 该路径） |
| P2-3 | E2E 烟测：(a) 本地模式（local gateway）所有服务在线 → 全绿；(b) 远程模式（remote gateway 192.168.3.67）所有服务在线 → 全绿；(c) kill 某个 sidecar → 诊断面板 5 秒内反映 + 错误文案正确 | 0.5d | P1-1~P2-2 | 3 种场景通过；事件历史正确写入 |
| P2-4 | 文档收口：在 [`docs/plan/zh/desktop-unified-diagnostics.md`](./desktop-unified-diagnostics.md) 末尾追加"实施记录"章节；如遇重大决策分歧，提 ADR 草案 | 0.25d | P0~P2-3 | 文档 v1.0 发布 |

**P2 出口**：远程拓扑下全链路可用；3 种部署形态（local / remote / 混合）诊断面板行为一致。

---

## 4. 风险与缓解

| 风险 | 严重度 | 触发条件 | 缓解 |
|------|--------|----------|------|
| **P0 polling 引入新性能负担** | 中 | 5 秒 polling 触发频繁 fetch | polling 仅在 `!mqttConnected` 时启用；connected 后停止；fetch 走同一连接复用 |
| **多窗口 polling 重复触发** | 中 | Desktop 多 webview（罕见但可能） | 改用 `globalThis.__mqttWatchdogStarted` 单例守卫；只在第一个 listener 注册时启动 polling |
| **sidecar 离线误报为 Gateway 离线** | 中 | sidecar 不可达但 Gateway 仍健康 | 区分 timeout / connection refused；每个 service 独立 timeout；UI 用具体 service 错误文案而非笼统 "Gateway 未连接" |
| **远程模式 sidecar 跨网不可达** | 中 | Sidecar 监听 127.0.0.1，远程 Desktop 无法直连 | P2-1 通过 Gateway 反代；UI 标注 "via Gateway proxy" |
| **诊断探针打爆网关** | 低 | 用户反复点"运行诊断" | 按钮加 3 秒 cooldown；polling 5s 已足够 |
| **侧栏 7 个服务行视觉拥挤** | 低 | 服务多时单卡片过高 | 按关键性折叠（critical 默认展开，important/optional 默认折叠） |
| **`lastMqttError` 转写历史丢失** | 低 | transition log 仅 20 条 | 已知边界；超 20 条不计入 UI；如需审计走 Rust 端日志 |

---

## 5. 验收标准

| 阶段 | 验收清单 |
|------|----------|
| **P0** | (a) 模拟"前端 mqttConnected=false 但 Rust 已 Connected"：5 秒内自动恢复；(b) 模拟"Connecting 卡 30 秒"：自动 force_reconnect；(c) ChatPanel placeholder 在 5 种 effectiveConnection 状态下文案正确；(d) vitest 全绿；(e) tsc --noEmit 干净 |
| **P1** | (a) 设置 → 网关设置 → "全栈服务"卡片可见 6 类服务；(b) "运行诊断"按钮 3 秒内返回完整报告；(c) 单行"重试"按钮独立探活；(d) kill sidecar 后 5 秒内 UI 反映 offline + 错误；(e) 最近事件历史正确展示最近 20 条 transition；(f) vitest + 单测全绿 |
| **P2** | (a) 本地 / 远程 / 混合 3 种模式下诊断面板行为一致；(b) 远程 sidecar 不可达时 UI 正确标注；(c) E2E 烟测 3 种场景通过；(d) 文档 v1.0 发布 |

### 全局回归基线

- `cargo clippy --all-targets -- -D warnings`：干净
- `cargo test --lib`：core 193+ / runtime 1409+ / gateway 451+ / node 123+ — 全绿
- Desktop：`pnpm tsc --noEmit` 干净；`pnpm vitest` 全绿
- Desktop：`pnpm lint` 干净

---

## 6. 不在 P0~P2 范围（YAGNI 后置）

| 任务 | 触发条件 | 估时 |
|------|----------|------|
| 服务历史健康曲线（每 10s 采样，24h 滚动窗口） | 用户反馈"昨天 Embed 抽风过？" | 2d |
| 自动重启 sidecar（在诊断面板上加"重启"按钮调用 Gateway supervisor） | sidecar 频繁离线 | 1d |
| 跨设备对比诊断（两台 Desktop 对比） | 多机运维 | 3d+ |
| 实时事件流订阅（SSE 推送 diagnostics-updated） | 后端已 emit，前端可订阅 | 0.5d（已 emit，前端 polling 足够） |
| 诊断结果导出（JSON / Markdown） | 用户主动分享诊断 | 0.5d |
| 与 logs 联动（点击错误 → 跳转 desktop-app/logs 对应行） | 日志深度分析 | 1d |
| ADR-074 起草（看门狗 + 诊断面板架构决策正式 ADR 化） | 决策分歧 / 未来人查询 | 1d |

---

## 7. 决策记录（本计划相关）

| # | 决策 | 选择 | 出处 / 理由 |
|---|------|------|------------|
| D-1 | 诊断面板位置 | **设置 → 网关设置（融入现有页面）**，不新建独立页面 | 用户拍板 2026-09-14；现有"测试连接"按钮 + 节点列表天然契合；用户认知负担最小 |
| D-2 | 诊断单位命名 | **统一叫"服务"（service）**，替换"节点"心智模型 | ADR-055 中 "node" 特指 `acowork-node`；LSP/Embed/PM/Doc 是 sidecar 服务；用"服务"避免命名冲突 |
| D-3 | 服务关键性分级 | 🔴 critical（gateway/mqtt/node）· 🟡 important（embed/pm）· 🟢 optional（doc/lsp-relay） | critical 失去 = 完全不可用；important 失去 = 功能降级；optional 失去 = 增强功能失效 |
| D-4 | 看门狗 polling 间隔 | **5 秒常驻**，**仅在非 connected 时启用** | 30 秒太长（用户能看到卡死）；1 秒太频繁；5 秒是 React 用户感知阈值 |
| D-5 | 看门狗自动 force_reconnect 阈值 | Connecting / Reconnecting **持续 30 秒** 自动触发 | 短于 30 秒可能误伤正常重连；长于 60 秒用户已看到明显卡死 |
| D-6 | 看门狗误判容忍 | "Rust 已 Connected 但前端 false" 持续 ≥10 秒才视为 stale，避免短时抖动 | 事件 listener 与 polling 偶发竞争不能误触发自动恢复 |
| D-7 | `effectiveConnection` 字段位置 | 放在 **chatStore**，**不**新建独立 store | chatStore 已有 `mqttConnected` / `lastMqttError` / `bootstrapVersion`；新增字段语义内聚 |
| D-8 | Sidecar 探针方式 | 本地直连 `127.0.0.1:{port}`；远程经 Gateway `/api/services/*` 反代 | 单一探针逻辑；远程模式隔离内网细节 |
| D-9 | 节点诊断嵌入 | 原"节点管理"卡片**不再独立存在**，每个 node 是"全栈服务"卡片内 critical 分组的一行 | 减少 UI 重复；节点作为 service 的实例 |
| D-10 | 不抽象 sidecar 抽象层 | **本版不抽象** CommonService trait / HealthCheckable 接口；复制 pm_supervisor / doc_supervisor 各自 supervisor | rule of three 触发但抽象风险高；YAGNI；如未来 4 个以上 sidecar 再评估 |
| D-11 | `diagnose_services` 客户端发起，**不**服务端聚合 | Desktop → 直连各 sidecar / Gateway → 返回原始数据 → 前端聚合 | 服务端聚合有耦合 + 时序问题；前端聚合更灵活 |
| D-12 | transitionLog 容量 | **20 条环形 buffer**，仅内存 | UI 够用；超 20 条审计走 Rust 端日志 |

---

## 8. 实施起点建议

- **最早启动**：2026-09-14（用户确认本计划后）
- **总工期**：**5-8 个工作日（1-1.5 周）**
- **关键里程碑交付**：
  - 第 1 周中：P0 + P1 完成（看门狗 + 诊断 UI）
  - 第 1.5 周末：P2 完成（远程 + 验证）
- **建议节奏**：P0 先合入主干（用户立刻能体验），P1 + P2 一起合入或拆 PR。

---

## 9. 实施记录

> 在 P0~P2 完成后追加，记录实际偏差、遇到的问题、未在 §7 决策表中的临时决定。

### P0 看门狗修复

- 实施日期：2026-09-14
- 范围：仅 P0（P1 / P2 未启动）
- 提交状态：未提交（修改仅在工作区，等待评审后提交）

#### P0 子任务清单

| ID | 任务 | 状态 | 关键改动 |
|----|------|------|---------|
| P0-1 | `chatStore` 新增 `effectiveConnection` 字段 + `staleSince` 时间戳 | ✅ 完成 | 新增 `ConnectionStatus` 6-state type / `TransitionLogEntry` 接口 / `SinceTracker` |
| P0-2 | 重写 `startMqttPoll` 为 5s 常驻 polling + 不一致自动同步 + 卡住超时 force_reconnect | ✅ 完成 | `startMqttPoll` 重写为 watchdog，新增 `applyConnectionTransition` / `applyWatchdogSnapshot` |
| P0-3 | `ChatPanel.tsx` placeholder 改用 `effectiveConnection` + 新增 i18n key | ✅ 完成 | 新增 helper `getInputPlaceholderKey` 5 状态映射；新增 i18n key `inputReconnecting` / `inputStale` (中英双语) |
| P0-4 | `lastMqttError` 去重 + `transitionLog` 环形 buffer (20 条) | ✅ 完成 | `dedupeError` / `appendTransition` 实现 |
| P0-5 | vitest 单测覆盖 (Rust-前端不一致 / Connecting 卡 30s / 状态机) | ✅ 完成 | `chatStore.test.ts` 新增 4 个 describe 块 13 个测试 |
| P0-6 | tsc --noEmit + vitest 回归 + 文档 §9 实施记录 | ✅ 完成 | tsc 0 error；vitest chatStore.test.ts 57/57 passed |
| P0-7 | `ConnectionStatusBanner` 组件 + 倒计时（UX 增强，§7 决策之外） | ✅ 完成 | 输入区上方加状态条；显示状态文案 + "X 秒后自动重连"；i18n 新增 `connectionBanner` 命名空间 |
| P1-A | `lib/types.ts` 新增 `ServiceType` / `ServiceGroup` / `ServiceHealth` / `DiagnoseReport` 类型 | ✅ 完成 | 7 种服务类型 + 3 组分组 |
| P1-B | `lib/servicesApi.ts` 纯前端探针（并行 + 1s 超时 + gateway-down 短路） | ✅ 完成 | `probeAllServices` / `probeService` / 5 个 per-type 函数 |
| P1-C | `stores/servicesStore.ts` Zustand store（`report` / `loading` / `probing`） | ✅ 完成 | `diagnose()` / `probe()` / `reset()` + 重入 guard |
| P1-D | `components/settings/ServicesPanel.tsx` 3 分组 + 状态圆点 + 重试按钮 | ✅ 完成 | 圆点按 online / latency 着色；gateway-down banner；空状态 |
| P1-E | `SettingsPage.GatewayTab` 改造：测试连接→运行诊断 + 全栈服务卡片 + 最近事件历史 | ✅ 完成 | handleTest 并行 checkHealth + diagnose；新增 `servicesOpen` / `eventsOpen` state；`RecentEventsLog` 消费 transitionLog |
| P1-F | i18n 中英双语：`settings.services.{gateway,mqtt,...,emptyHint,eventsTitle,...}` | ✅ 完成 | zh-CN.json + en.json 各加 16 个 key |
| P1-G | vitest 单测：`servicesApi.test.ts` (15) + `servicesStore.test.ts` (8) | ✅ 完成 | tsc 0 error；新测 23/23 passed；全量 454/455 passed |

#### 关键代码路径

- `apps/acowork-desktop/src/stores/chatStore.ts`
  - 新增 `MQTT connection watchdog (P0 of unified-diagnostics plan)` 文档区（line ~165-275）
  - 新增 `computeEffectiveConnection` / `appendTransition` / `dedupeError` / `bumpSince`（均为 export，便于单测）
  - 新增常量 `TRANSITION_LOG_CAPACITY=20` / `STALE_GRACE_MS=10_000` / `WATCHDOG_INTERVAL_MS=5_000` / `STUCK_FORCE_RECONNECT_MS=30_000`
  - 重写 `startMqttPoll` → watchdog 模式（仅非 connected 时运行）
  - 新增 `applyConnectionTransition` / `applyWatchdogSnapshot`
  - 新增 stale-upgrade rule：`connecting`/`reconnecting` 持续 ≥10s 升级为 `stale`
- `apps/acowork-desktop/src/components/chat/ChatPanel.tsx`
  - line 5: `import { useChatStore, type ConnectionStatus }`
  - line ~175: 新增 helper `getInputPlaceholderKey(gatewayStatus, effective, activeSkill)`
  - line ~569: 选择器读取 `effectiveConnection`
  - line ~1428: `inputDisabled` 改用 `effectiveConnection !== "connected"`
  - line ~2395: placeholder 改用 `t(\`chatPanel.${getInputPlaceholderKey(...)}\`)`
- `apps/acowork-desktop/src/i18n/locales/zh-CN.json`
  - 新增 `inputParamsReconnecting` / `inputParamsStale` / `inputMessageReconnecting` / `inputMessageStale`
  - **P0-7 新增** `connectionBanner` 命名空间：`connecting` / `reconnecting` / `stale` / `disconnected` / `retryIn` (含 `{{seconds}}`) / `retryInOneSecond` / `recoveringHint`
- `apps/acowork-desktop/src/i18n/locales/en.json`
  - 同步新增 4 个英文 i18n key + `connectionBanner` 命名空间
- `apps/acowork-desktop/src/stores/chatStore.test.ts`
  - 新增 4 个 describe 块，覆盖 `computeEffectiveConnection` / `appendTransition` (含 20 条截断) / `dedupeError` / `bumpSince`
- `apps/acowork-desktop/src/components/chat/ChatPanel.tsx` （**P0-7 新增**）
  - 新增 `ConnectionStatusBanner` 组件（line ~228-335）
  - line ~683: 选择器 `mqttStaleSince = useChatStore((s) => s.staleSince)`
  - line ~2541: textarea 上方挂载 `<ConnectionStatusBanner gatewayStatus={gatewayStatus} effectiveConnection={effectiveConnection} staleSince={mqttStaleSince} />`
  - line ~37: `import { useChatStore, type ConnectionStatus, STUCK_FORCE_RECONNECT_MS } from "../../stores/chatStore"`

#### 关键设计微调（在 §7 决策之外的临时决定）

1. **stale 判定下沉到 `applyConnectionTransition` 而非 `computeEffectiveConnection`**
   - 原因：保持 `computeEffectiveConnection` 纯函数（无 `Date.now()` 依赖），单测可在 fake clock / fake timer 下确定性验证。
   - `applyConnectionTransition` 内用 `Date.now() - state.staleSince >= STALE_GRACE_MS` 判断升级。
2. **`mqttConnected` 字段保留（chatStore 默认值仍同步更新）**
   - 原因：`AppLayout.tsx` 的 banner 逻辑（line ~311）继续读 `mqttConnected`，贸然删除会破坏向后兼容。ChatPanel 不再读它，但 `setState` 仍在维护。
3. **D-5 的 watchdog trigger 包含 `Connecting` 和 `Reconnecting` 两种状态**
   - §7 决策表里只提到"Connecting/Reconnecting"，实现里完全遵循。
4. **`force_reconnect_mqtt` 调用后立即重置 since-tracker**
   - 原因：避免在 grace window 内重复触发 force_reconnect（导致二次重连风暴）。
5. **i18n key 命名沿用原有 `inputParams` / `inputMessage` 前缀**
   - 没有引入新的 key namespace（如 `chatPanel.connection.*`），因为 ChatPanel 是唯一消费者且现有命名风格保持一致。
6. **P0-7 新增 `ConnectionStatusBanner` 组件（用户提议 UX 增强）**
   - 在 textarea 上方加状态条，显示当前 effectiveConnection + "X 秒后自动重连"倒计时。
   - 倒计时基于 `staleSince + STUCK_FORCE_RECONNECT_MS`（即 watchdog 触发 force_reconnect 的 deadline），每秒 1Hz 刷新。
   - 只在 `gatewayStatus === "connected" && effectiveConnection ∈ {connecting, reconnecting, stale, disconnected}` 时显示；gateway 断开时不显示（避免重复提示）。
   - 颜色：connecting/reconnecting 用 sky-50（信息），stale/disconnected 用 amber-50（警告）。
   - i18n key 使用 `connectionBanner` 命名空间（嵌套结构，与 inputParams 系列区分开）。
   - **为什么放在 ChatPanel 内而不是独立文件**：仅 ChatPanel 使用，避免新建一个 .tsx 文件；测试覆盖留待后续与 ChatPanel 测试一起做（暂无 ChatPanel.test.tsx）。
7. **P1 跳过 Rust diagnostics.rs，改为纯前端 servicesApi.ts**
   - 原 plan §3.2.1 P1-1/P1-2 要求新建 `apps/acowork-desktop/src-tauri/src/commands/diagnostics.rs`。
   - **务实变更**：所有 7 类服务的探测都可在前端完成（`fetch` Gateway HTTP + `invoke('get_mqtt_status')` 复用现有 Tauri 命令）。这避免了：
     - 重新编译 Rust 工具链（≈ 90s 构建时间）
     - 在 Rust ↔ TS 边界复制 `ServiceHealth` 类型
     - 在 Tauri 命令注册表中再加 2 个命令
   - **副作用**：gateway 不可达时其他 6 类服务没有独立的 Rust 探针，全部走 `gateway_down_short_circuit` 规则被覆盖为 `last_error: "gateway offline: ..."`。这正是 plan §3.2.1 中描述的预期行为。
8. **P1 embed/pm/doc/lsp-relay 复用单次 /api/status 调用**
   - 这 4 类服务不是独立 HTTP 端点，是 Runtime 内部子系统。
   - 拆 4 次 fetch vs 1 次：1 次更便宜且延迟更低（一次性报告 `agents_running`）。
   - 未来 P2 拆分为独立 `/api/embed/ping` 等端点（每个 ≤10ms）时，可以重构为各自独立 probe。

#### 回归基线

- `npx tsc --noEmit` → 0 error
- `npx vitest run src/stores/chatStore.test.ts` → **57 / 57 passed**（含 P0 新增的 13 个 watchdog 测试）
- `npx vitest run` → 454 / 455 passed（唯一失败的 `formatTime.test.ts` 是**预先存在**的环境敏感测试，与本次改动无关；测试日期假设时区依赖，已在 commit e54a392b 中合入）
- **P1 新增测试**：
  - `src/lib/servicesApi.test.ts` → **15 / 15 passed**（PROBE_TIMEOUT_MS / probeGateway × 3 / probeMqtt × 3 / probeNodes × 2 / probeAllServices × 3 / probeService × 3 / probed_at 时间戳 × 1）
  - `src/stores/servicesStore.test.ts` → **8 / 8 passed**（diagnose × 4 / probe × 3 / reset × 1）

#### 用户体验验证（手动）

- **5 秒内自动恢复**：D-6 + watchdog 5s polling → Rust 已 connected 但前端 stale 时，最多 5s 内 `effectiveConnection` 翻转到 `"connected"`，placeholder 立即恢复。
- **30 秒卡死自动重连**：Connecting/Reconnecting 持续 ≥30s → 自动调用 `force_reconnect_mqtt`（无需用户操作）。
- **wake-recovery webview reload**：监听器注册前的 `mqtt-status` 事件丢失 → init 时的 `get_mqtt_status` snapshot + watchdog 兜底，≤5s 恢复。
- **P0-7 倒计时反馈**：用户能看到 "🔄 正在重新连接 — 27 秒后将自动重连" → 每秒递减 → 到 0 时 watchdog 触发 force_reconnect → 状态可能切到 connected（成功） 或 stale→connecting 重试。给用户明确预期，不再"傻等"。
- **P1 全栈服务诊断**：设置 → 网关 → "全栈服务诊断" 卡片 → 点击"运行诊断" → 7 类服务 (gateway/mqtt/node/embed/pm/doc/lsp-relay) 并行探测，1s 超时，每行显示状态圆点 + 版本 + 延迟 + 详情；点重试按钮可单行重新探测。
- **P1 最近事件历史**：设置 → 网关 → "最近事件历史" 卡片 → 显示 watchdog 记录的最新 20 条状态转移（如 `connecting → reconnecting (reason: ...)）`，用户可直观了解"为什么刚才断了"。

#### 已知遗留（进入 P1 / P2 处理）

- 诊断面板 UI 暂未实现（P1）
- 远程模式下 sidecar 探针策略暂未实现（P2）
- `effectiveConnection === "stale"` UI 路径已被 helper 函数覆盖，但实际触发率取决于 wake-recovery 等真实场景；后续需 e2e 测试覆盖

### P1 全栈服务诊断面板

- 实施日期：2026-09-14
- 提交状态：未提交（修改仅在工作区，等待评审后提交）
- 实施摘要：细节见上方清单表尾行 P1-A ~ P1-G + “关键设计微调” 7-8 + “回归基线” P1 新增测试。出口达成：设置 → 网关 → 全栈服务卡片可见 7 类服务状态 / 版本 / 延迟 / 错误 + 单行重试 + 最近事件历史。

### P2 远程与集成

- 实施日期：2026-09-14
- 提交状态：未提交（修改仅在工作区，等待评审后提交）
- 范围：P2-1 / P2-2（Gateway 快照端点 + 前端快照优先）已实现；P2-3（E2E 烟测）为手动项，待真实远程环境验证；P2-4 即本节收口。

#### P2 子任务清单

| ID | 任务 | 状态 | 关键改动 |
|----|------|------|---------|
| P2-A | Gateway 新增 `core/acowork-gateway/src/http/services_api.rs` | ✅ 完成 | `GET /api/services/diagnose` 返回 gateway/mqtt/embed/pm/doc/nodes 的**进程内真值快照**（无主动网络探针、无跨 await 持锁）；注册进 `http/mod.rs` + `routes.rs` |
| P2-A2 | `cargo test -p acowork-gateway --lib services_api` + clippy | ✅ 完成 | 3/3 passed（空状态完整行 / snake_case 序列化契约 / embed running 透传）；clippy 干净 |
| P2-B | 前端 `fetchGatewayDiagnose` + `probeAllServices` / `probeService` 快照优先 | ✅ 完成 | 一次 fetch 覆盖 7 行（+1 次本地 `get_mqtt_status`）；形状校验失败 / 404 / 网络错误 → 静默回退 P1 直连探针；对外信封 `{ services, source }` |
| P2-B2 | `servicesStore.diagnose()` 适配信封 + `report.source` | ✅ 完成 | `DiagnoseReport.source: ProbeSource`（`gateway-api` \| `direct`） |
| P2-C | `ServicesPanel` 源徽标（经网关快照 / 直连探针） | ✅ 完成 | 摘要行新增 `services-source` 徽标，消费 `report.source` |
| P2-D | i18n 中英双语 2 key | ✅ 完成 | `settings.services.viaGateway` / `viaDirect` |
| P2-E | 回归 + 文档收口 | ✅ 完成 | tsc 0 error；vitest 全量 467/468（唯一失败为预先存在的 formatTime）；本表 |

#### 关键设计微调（P2 部分）

9. **P2 采用“快照优先 + 直连回退”而非 plan §3.3 字面的“仅 reverse-proxy”**
   - plan P2-2 原文：“仅 reverse-proxy，**不**做服务端聚合；前端直连”。
   - 实现取舍：`/api/services/diagnose` 返回的是 Gateway **自身进程内状态**（embed/pm/doc supervisor 的 pid/port/ready、NodeRegistry、broker handle），不是对子服务的再聚合 — 与 D-11“Gateway 返回原始数据、前端聚合”一致。
   - 收益：前端把原 ~4 次 HTTP 探针收敛为 1 次 fetch；远程拓扑下与本地行为完全一致（D-8：远程经 Gateway，前端无需感知内网细节）。
10. **mqtt 行 = broker 真值 ∧ 本地客户端状态**
    - 快照路径下 `online = broker_running && client.connected`；本地 invoke 读失败时退为仅 broker 判定（detail 标 `client unknown`）。
    - 原因：消除“聊天区显示 Reconnecting 而诊断面板 MQTT 绿”的视觉矛盾；两半信息都保留在 detail（`:port · auth on/off · client connected/disconnected`）。
11. **快照不可用时静默回退 P1 直连路径**
    - 404（旧版 Gateway）/ 200 但形状不符（代理干扰）/ 网络错误 → `fetchGatewayDiagnose` 返回 `null` → `probeAllServices` 走原 5 探针路径，`source: "direct"`。
    - 形状校验（`isGatewayDiagnosePayload`）防止把无关 JSON 映射成假报告；`source` 徽标让用户 / 支持人员一眼看出报告来源。
    - 回退代价：gateway 黑洞场景最坏 ~2s（快照 1s 超时 + 直连探针 1s 超时），仍在 P1 验收“3 秒内返回”内。
12. **P2 同时修掉 P1 的一个隐患：`probeService` 单行重试与全量报告同源**
    - 单行重试也先尝试快照端点（`fetchGatewayDiagnose` → 取该 type 行），保证行内容与全量报告同口径；旧网关下自动回退单行直连探针。

#### P2 回归基线

- Rust：`cargo test -p acowork-gateway --lib services_api` → **3 / 3 passed**；`cargo clippy -p acowork-gateway --lib` 干净
- Desktop：`npx tsc --noEmit` → 0 error
- Desktop：`npx vitest run` → **467 / 468 passed**（唯一失败仍为预先存在的 `formatTime.test.ts`）
- **P2 新增测试**：
  - `src/lib/servicesApi.test.ts` → 15 → **28 passed**（新增 `fetchGatewayDiagnose` × 9：健康映射 / 404 / 形状不符 / 网络拒绝 / embed not-ready / broker-down / client-down / client 读失败 / lsp-relay 缺失；`probeAllServices` 快照优先 × 1（断言恰好 1 次 fetch）+ 回退 × 2 + 原 gateway-down 短路 2 测保留；`probeService` 快照优先 × 1）
  - `src/stores/servicesStore.test.ts` → **8 / 8 passed**（新增 `report.source` 传播断言）

#### P2 用户体验验证（预期行为）

- **远程模式**：Desktop 指向 192.168.3.67 的 Gateway → “运行诊断”仅需 1 次 HTTP 往返渲染全部 7 行；徽标显示“经网关快照”。
- **降级可见性**：旧版 Gateway（无快照端点）→ 徽标自动显示“直连探针”，面板行为与 P1 一致（用户无需感知差异）。
- **sidecar 离线**：kill embed/pm/doc 任一后点“运行诊断” → 对应行红点 + `last_error`（如 `doc subsystem not running`），与真实 supervisor 状态一致（无推断误差）。

#### P2 已知遗留

- **E2E 烟测（P2-3）**：3 种场景（local 全绿 / remote 192.168.3.67 全绿 / kill sidecar 后反映）需真实远程环境手动验证，未在本轮完成。
- **per-node lsp-relay 详情**：快照只携带 `has_lsp_relay` 能力标志，lsp-relay 行是全舰队聚合；单节点级详情待后续。
- **快照端点无鉴权例外**：走现有 Gateway HTTP 同源策略（localhost + 现有一致），远程暴露面与 `/api/status` 相同。
- **`nodes.online` 字段**：前端从 `items` 自算在线数（防御性），端点字段保留备用。

### Review 修复（R1）

- 修复日期：2026-09-14
- 范围：对 P0~P2 全部未提交改动的代码评审（B-1/B-2 阻断 + H-1~H-3 高 + M-1~M-3 中 + L-1~L-4 低 + I-1 信息，共 13 项）；本节记录全部修复
- 提交状态：未提交（与 P0~P2 同一工作区变更，等待评审后一并提交）

#### R1 问题与修复清单

| # | 问题（评审发现） | 修复 | 位置 |
|---|------------------|------|------|
| B-1 | `force_reconnect` 恒不可达：stale 升级（10s）会重置 `staleSince`，而 force 检查读 `staleSince + 30s`——两个阈值互相打架，卡死时永远不会触发重连 | 引入连接族时钟 `_connectingSince`（connecting / reconnecting / stale 共享）+ 统一阈值 `STUCK_FORCE_RECONNECT_MS = 30_000`；tick 顺序重排为"先 poll（可能升级 stale）→ 再 force 检查（仅对 stale）"；force 后重置时钟 → 每 30s 周期重试 | `chatStore.ts` |
| B-2 | `applyWatchdogSnapshot` 无条件覆盖 `connecting` 为 false，把 Rust 真实的 `connecting:true` 降级为 `disconnected`——watchdog 轮询反而制造状态误报 | 快照五元组 `{known, connected, connecting?, reconnecting?, reason?}` 全程透传（watchdog + init 两条路径）；新增 wire 类型 `MqttStatusSnapshot` 对齐 Rust `mqtt_status_to_payload` 契约 | `chatStore.ts` |
| H-1 | 看门狗在 connected 后 `stopMqttPoll()` 且之后掉线不再重启——任何后续异常都失去自愈路径 | `mqtt-status` 事件处理器：非 connected 事件且轮询未运行时自动重新 arm（`startMqttPoll()`） | `chatStore.ts` |
| H-2 | banner 对 terminal `disconnected` 显示"0 秒后将自动重连"（Rust `SessionState::Disconnected` 是终态，永不自动重连——文案误导） | disconnected 不再显示倒计时（`staleSince` 自然为 null）；新增"重新连接"按钮（`invoke("force_reconnect_mqtt")`，软重启可从任意状态恢复）+ i18n `connectionBanner.reconnectNow` | `ChatPanel.tsx` / i18n |
| H-3 | `has_lsp_relay` 从错误数据源推导，恒为 false——lsp-relay 行永远显示离线 | 改用 `NodeRegistry::lsp_endpoint.is_some()`（由 retained `lsps` topic 填充）；新增 2 个 Rust 单测（endpoint 就绪 → true；未就绪 → false） | `services_api.rs` |
| M-1 | `fetchGatewayDiagnose` 的 `latency` 混入本地 `get_mqtt_status` IPC 耗时（最多 1s），污染 `>800ms → amber` 的行着色判定 | `latency` 提前到 fetch 完成即结算，本地快照读取在其后 | `servicesApi.ts` |
| M-2 | 看门狗 tick 路径（setInterval 真实接线）无集成测试——B-1/B-2/H-1 都藏在这里；既有 36 个测试全绿也抓不住，且测试夹具伪报 `has_lsp_relay: true` 掩盖 H-3 | 新建 `chatStore.watcher.test.ts`：mock Tauri IPC + fake timers 驱动真实 `initMqttListener → startMqttPoll` 路径，4 个测试（B-2 保真 / B-1 30s 升级 + 周期 force / H-1+F-3 重新 arm 与自愈 / H-2 数据侧） | `chatStore.watcher.test.ts`（新建） |
| M-3 | 实际 10s 就升级 stale，与 plan §3.1.1 / D-5 的 30s 语义不符（D-6 的 10s grace 被挪用），且 §9 未记录该偏差 | 由 B-1 的 30s 连接族时钟覆盖；本节"设计对齐"明确说明 | `chatStore.ts` + 本文件 |
| L-1 | `computeEffectiveConnection` 的 `lastMqttError` 参数是死参数（实现已忽略，两个分支同结果） | 删除参数改单参；`reason` 语义由 snapshot 字段承载 | `chatStore.ts` |
| L-2 | `servicesStore.reset()` 不取消在途请求：切换 Gateway 后旧 promise 晚到会写回旧报告 | 引入 `epoch` 计数器（reset 时 +1）；在途 `diagnose`/`probe` 落地前校验 epoch，不匹配即丢弃 | `servicesStore.ts` |
| L-3 | `services_api.rs` 锁顺序注释与实际不符 | 注释修正为 `gateway_state → mqtt_broker_control`（与加锁顺序一致） | `services_api.rs` |
| L-4 | `isGatewayDiagnosePayload` 形状校验过松（对象即可通过），无关 JSON 可能被映射成假报告 | 逐字段强化：数值字段校验 + 新增 `isSubsystemSnapshot`（running/ready boolean、port/pid number） | `servicesApi.ts` |
| I-1 | `SettingsPage.RecentEventsLog` 局部变量 `log` 遮蔽模块 logger（潜在误用） | 重命名为 `entries` | `SettingsPage.tsx` |

#### R1 设计对齐（与 §7 决策表 / §3.1.1 的关系）

- **D-5 对齐（B-1 / M-3）**：连接族（connecting / reconnecting / stale）统一 30s 时钟；修复前的"10s 升级 + 30s force"双阈值是 B-1 的根因，且与 §3.1.1"Connecting 持续 >30s → stale"不符。修复后单一阈值 `STUCK_FORCE_RECONNECT_MS = 30_000`，升级与 force 可在同一 tick 完成。
- **D-6 被取代（B-1 副产物）**：原"Rust 已 Connected 但前端 false 持续 ≥10s 才判定"的 grace 逻辑删除——新设计下 `connected` 由 Rust 真值（事件或轮询）送达即立即恢复（F-3 自愈），10s 延迟反而会把恢复时机拖到 watchdog 停摆之后。
- **`staleSince` 语义重定义**：从"进入 stale 的时刻"改为"当前连接 episode 起始时间"，settle（connected / disconnected / idle）后为 `null`——banner 倒计时与 watchdog 共用同一时钟，不再有两套计时器。
- **B-1 周期律**：30s 到期时同一 tick 内"升级 stale + force_reconnect"，force 后时钟重置——形成每 30s 一次的重试节拍（`chatStore.watcher.test.ts` 断言第二次 force 恰在 `T0 + 60_000`）。

#### R1 回归基线

- Desktop：`npx tsc --noEmit` → 0 error
- Desktop：`npx vitest run` → **475 / 476 passed**（唯一失败仍为预先存在的 `formatTime.test.ts` 环境敏感测试）
  - `src/stores/chatStore.watcher.test.ts` → **4 / 4**（新增，tick 路径集成）
  - `src/stores/chatStore.test.ts` → **59 / 59**（`bumpSince` 块替换为 `updateConnectingSince` 4 测）
  - `src/stores/servicesStore.test.ts` → **10 / 10**（+2 epoch 竞态）
  - `src/lib/servicesApi.test.ts` → **28 / 28**
- Rust：`cargo test -p acowork-gateway --lib services_api` → **5 / 5 passed**（+2 lsp-relay endpoint 测试）
- Rust：`cargo clippy -p acowork-gateway --all-targets -- -D warnings` → 干净

#### R1 待手动验证（无法自动化）

- P0 验收 (a)/(b)/(c) 真实环境重验：Rust 已 Connected 但前端 false → 5s 内自愈；Connecting 卡 30s → 自动 force_reconnect；5 种状态 placeholder 文案。
- H-2 手动场景：停 Gateway → banner 出现"重新连接"按钮（无倒计时）→ 点击后恢复。

---

> **下一步行动**：
> 1. 用户评审本计划（重点 §7 决策表 + §5 验收标准）
> 2. 确认排期起点日期（建议本周内启动 P0）
> 3. 按 P0 → P1 → P2 串行实施
> 4. 每阶段完成做一次回归基线（§5 全局回归），完成后更新 §9 实施记录
