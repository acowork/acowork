# ADR-113：协议自洽 —— 启动关系不入协议，install 必须显式 `node_id`

- **编号**：ADR-113
- **状态**：已实施
- **日期**：2026-11-12
- **决策者**：Nicholas（架构判据与范围确认）
- **关联**：ADR-055（远端 Runtime / Node 拓扑）、ADR-059 §15.1（`node.{id}` 为 Optional）、ADR-075（node_id = UUID v4）、ADR-087（node owner 与 `can_manage`）、ADR-009（Gateway 边界）
- **修订**：ADR-055 §6.3.4 / §6.11 / §6.12、ADR-075 D5 / D9 / §实现清单、ADR-087 D2 中有关 `gateway_managed` 的段落由本 ADR 取代；`docs/runbooks/single-machine-remote-topology.md` 同步
- **英文 counterpart**：[docs/adr/en/ADR-113-protocol-self-consistency-explicit-node-target.md](../en/ADR-113-protocol-self-consistency-explicit-node-target.md)（同 commit 维护）

---

## 摘要

**一句话**：Node 由谁启动、以什么顺序启动、从哪台机器启动，都不属于协议状态。所有 install / ensure / create 请求必须携带显式 `node_id`；Gateway 只用协议内可见事实判断「该节点存在 / 在线 / 控制面就绪 / 调用方可管理」，不再推断默认落点。

**一句话（为什么）**：`node_id` 一旦可缺省，Gateway 就必须用协议外知识（谁启动了我、hostname、`local_node.json` anchor、reuse window）去补一个默认目标。这些填充物在分布式拓扑下必然失效，于是产生 `Gateway 409: the Gateway's own-machine node is not online`，而事实上 node 进程正在运行。

**架构判据（不可协商）**：任意替换「node 由谁启动」（Gateway / Desktop / CLI / 远程 / 根本不启动），协议行为必须完全一致。凡在该替换下结果会改变的代码，都是架构违规。**协议自洽的标志是：它完全不需要知道进程是怎么来的。**

---

## 背景（事实取证）

1. **复现路径**：编译 release 版 Desktop → 清空用户目录 `.acowork` → 重装 → 启动 → 手动 `install agent` 选本地 `.agent` 包 → 报 `Gateway 409`，文案要求「等 `/api/bootstrap` READY 或显式传 `node_id`」；但 `acowork-node` 进程确实在运行。

2. **直接触发点**：`core/acowork-gateway/src/http/agents.rs` 的 `resolve_local_node_id()`。multipart 未带 `node_id` 时它被调用，失败返回普通 `ApiError::conflict`。该错误不是 structured `dependency_not_ready`，所以 Tauri 的 `submit_with_retry` 不会重试，用户看到的就是一次性 409。

3. **为什么「进程在跑」不等于「能解析本机 node_id」**：`NodeRegistry` 只由 MQTT retained `status` / `info` 填充，不看进程表；`local_node_id()` 旧实现要求 `gateway_managed && online && hostname == system_hostname()`。启动方式一变（Desktop 启动、CLI 启动、旧版本升级），这个合取式就断。

4. **bootstrap READY 与 node 可用是两件事**：`BootstrapOrchestrator::recompute()` 只按 Required subsystem 判 READY；生产静态注册里 `vault` / `mqtt` / `publisher` 是 Required，`embedding` 是 Optional，`node.{node_id}` 是动态注册的 Optional（ADR-059 §15.1）。`ensure_local_node()` 既不向 bootstrap registry 注册，也不 `mark_ready`。因此 READY 时本机 node 可以完全不存在、未上线、未 `NodeReady`，甚至 spawn 失败。**把 node 改回 Required 会破坏 remote 拓扑**——远端节点允许永远不在线。

5. **Desktop 也有启动本机 Node 的入口**：`apps/acowork-desktop/src-tauri/src/commands/gateway.rs::create_local_node`，且它 spawn 时并不带 `--gateway-managed`。所以 `gateway_managed` 连 local 内部都不自洽：它实际表达的只是「谁按了启动键」。

---

## 决策

### D1 — local 只是启动关系，不是协议分支

`local` 唯一语义是「Gateway 由 Desktop 启动，且 Gateway 附带启动一个本机 Node」。Gateway 附带启动 Node 是**附带行为，不是必须**：可以先只启动 Gateway，再单独启动本机 Node；启动方可以是 CLI、Desktop 或 Gateway。除启动关系外，三者的一切行为都走同一套协议与机制。

**MUST NOT**：以 `gatewayMode` / `gateway_managed` / spawn 参数 / 进程表作为协议判定输入。
**MUST NOT**：为 local 写一套机制、再为 remote 写一套机制。

### D2 — install 目标显式化（病根修复）

| 入口 | 契约 |
|------|------|
| `POST /api/agents/install`（multipart） | `node_id` **必填**；缺失/空白 → 400 `missing required field: 'node_id' — installs must name an explicit target node; query GET /api/nodes for available nodes` |
| `POST /api/agents/ensure`（multipart） | 同上 |
| `POST /api/agents/{id}/publish/install-locally` | 请求体 `node_id: String` 必填 |
| `acowork-gateway` 的 node-scoped CLI 子命令 | `--node <node_id>` **必填**；缺失 → 报错并提示 `acowork-gateway nodes list` |
| Tauri `install_agent` / `install_bundled_agent` / `install_from_hub` / `ensure_local_agent` / `create_agent` | `node_id: String`（不再是 `Option<String>`），`require_node_id()` 前置校验 |

删除 `resolve_local_node_id()`；`resolve_agent_node_id()` 只从 inventory 已记录的运行/安装记录里取 `node_id`，取不到就报错要求显式传入，不 fallback 到本机节点。

Gateway 侧的最终门禁只做协议内查询：**存在 → online → `ensure_node_ready()`（NodeReady）→ 权限**。没有节点是合法的「无可用目标」，不是 409，也不是等待 bootstrap。

**边界**：`POST /api/agents/{id}/upgrade` 不在此列。upgrade 作用于**已存在实例**，其目标节点由该实例在 inventory 中已记录的 `node_id` 唯一确定——这是协议内事实，不是默认落点推断，也不依赖启动者。相对地，任何「新建实例」的路径都不允许省略 `node_id`。

### D3 — 启动者身份从协议中彻底移除

`gateway_managed` 表达的是「谁启动了我」这一进程生命周期事实，却曾经进入：

- `NodeInfo.gateway_managed`（protobuf `mqtt_payload.proto` field 12）
- `NodeIdentity.gateway_managed`（`identity.json` 持久化）
- `NodeInfoState.gateway_managed`（Gateway NodeRegistry 缓存）
- `NodeResponse.gateway_managed`（`GET /api/nodes`）
- Desktop `NodeInfo.gateway_managed`（`lib/types.ts` / `gateway-api.ts`）

以上全部删除。proto 保留 `reserved 12; reserved "gateway_managed";`，字段号永不复用。旧 `identity.json` 中的遗留字段由 serde 忽略，因此升级不需要迁移。

`local_node_id()` 保留为 **Gateway-local deployment query**，只用于本地部署判断 / loopback 下载优化 / claim 的 local 范围；判定只按协议可见事实（`online` + `hostname`），**不得**作为 install / ensure 的默认落点。

### D4 — launcher-local process marker `--spawn-marker`

`--gateway-managed` 改名为 `--spawn-marker`。它只供 supervisor 扫描本机进程 argv、清理自己启动的孤儿 child：

- 由 Node CLI 消费后立即丢弃（`let _ = spawn_marker;`）
- **不**写入 `identity.json`
- **不**进入 `NodeConfig`
- **不**发布到 `NodeInfo`
- **不**参与任何协议行为分支

升级兼容：`cleanup_orphaned_local_nodes()` 同时匹配新 `--spawn-marker` 与旧 `--gateway-managed` 字符串，避免旧 binary 启动的 child 无法回收。启动、reap、孤儿清理是**各启动方的本地事务**，不属于协议。

### D5 — `control_ready` 作为协议内就绪事实暴露

`GET /api/nodes` 的 `NodeResponse` 新增 `control_ready: Option<bool>`，取自 `bootstrap_registry.is_ready(SubsystemId(format!("node.{}", n.node_id)))`，即 Node 控制面已宣布 `NodeReady`。序列化保持 snake_case（`control_ready`），测试固定断言不含 `gateway_managed` / `gatewayManaged` / `controlReady`。

该字段是**三态，不是布尔值**：

| wire 取值 | 含义 |
| --- | --- |
| `true` | registry 存在，`node.{id}` 已 Ready |
| `false` | registry 存在，`node.{id}` 未 Ready |
| *缺失* | 未接入 bootstrap registry（MQTT 关闭 / 精简 server）——就绪状态**未知** |

缺失绝不能塌缩成 `false`。`ensure_node_ready()` 在「无 registry」这一配置下本来就是 fail-open，此时若报 `false`，`GET /api/nodes` 与安装门禁会互相矛盾，Desktop 会把所有 online 节点误判为不可用。用 `#[serde(skip_serializing_if = "Option::is_none")]` 让「未知」在 wire 上与旧 Gateway 的「无此字段」完全等价——两者都正确表达「交给 Gateway 自己的门禁决定」。

**不改** ADR-059 §15.1：`node.{id}` 仍是 Optional 动态子系统。就绪是在节点粒度上查询的事实，不是全局 READY 的前置条件。

### D6 — 上层消费「可用节点」，不消费「启动状态」

启动成功/失败是绑定启动者的一次性事件；与启动方式无关、可重放的状态是「当前 online / ready / manageable 的节点集合」。`apps/acowork-desktop/src/components/nodes/nodeAvailability.ts` 是**唯一**定义：

```ts
usableNodesOf(nodes) = nodes.filter(
  (n) => n.online && n.control_ready !== false && n.can_manage !== false,
);
```

`control_ready` / `can_manage` 为可选，用于兼容旧 Gateway 以及未接入 readiness registry 的 Gateway；缺失时最终门禁仍由 Gateway `ensure_node_ready()` + Tauri 结构化重试承担。因此 `normalizeNode()` 只透传真正的 boolean，把「缺失」保留为 `undefined`（见 D5）。

- `GET /api/nodes` 是单一数据源；`gatewayMode` 不参与可用性判断（`AgentLibraryDialog.test.tsx` 有 `settingsReads === 0` 架构护栏）。
- `AgentLibraryDialog` / `OnboardingFlow` 共用 `usableNodesOf()` 与共享 `NoNodeBanner`；`CreateWizard` 由 `AgentList` 显式传入 `nodeId`。
- 空状态统一提示「无可用节点」，并按 `none` / `notReady` / `notManageable` / `blocked` 给出协议内原因（`noNodeDetailOf()`）。
- 所有拓扑给两个**并列**恢复选项，不按模式分叉：① `create_local_node` 启动本机 Node；② 远程启动引导。
- **启动重试与安装重试解耦**：启动动作不等待 node 上线、不串联安装；安装重试是用户再次点击，且始终带显式 `node_id`。

### D7 — 远程引导给真实可复制命令

`NoNodeBanner` 展开时按需调用 Tauri `build_remote_node_command`：

- `GET /api/status` → `mqtt_port`
- `POST /api/nodes/enrollment-tokens` → 真实 enrollment token（TTL 3600s）
- gateway base URL → `suggested_host`

生成 `acowork-node start --gateway <host>:<port> --token <token>`；host 可编辑、命令可复制、loopback 时给提示。token 只在用户展开引导时签发，不在渲染空状态时预取。引导展开状态与 token 生命周期由 banner 自管理，卸载即重置。

### D8 — i18n 契约

`noOnlineNode` 语义化重命名为 `noAvailableNode`；新增 `noNodeDetailNotReady` / `noNodeDetailNotManageable` / `noNodeDetailBlocked` / `remoteNodeLoading` / `remoteNodeLoadFailed` / `remoteNodeHost` / `remoteNodeHostHint` / `remoteNodeCopy` / `remoteNodeCopied` / `remoteNodeTokenHint`。`zh-TW.json` / `ja.json` / `ko.json` 补齐完整英文 `agentHub` fallback，保证 5 个 locale 占位符合法。

### D9 — 就绪状态必须可重放，不能是一次性事件

`status=online` 与 `NodeReady` 都是**状态**，但历史实现只在 ConnAck 发布一次 retained，60s 的 `info` 心跳不带它们。冷启动时 Gateway 的 SUBSCRIBE 可能仍在途中，node 的首次 PUBLISH 被错过；此后没有任何消息会修复它 —— 进程健康、端口在听，但 `GET /api/nodes` 里没有它、`control_ready` 永远为 `false`。只有再起一个 node 进程（或 node 重连）重走 ConnAck 才会「意外」自愈。

这与 D6 是同一条判据的两侧：**上层要消费可重放状态，协议层就必须真的把状态做成可重放的。**

修复：

- `NodeControlPlane::run_loop` 增加 presence 重断言：连接后 1s 首次，之后每 5s 重新发布 `status=online` 与 `NodeReady`（retained）。仅在 `state.connected` 为真时执行，避免排队中的 `online` 覆盖关闭时发布的 `offline`。
- 两个信封由 `node_ready_envelope()` 统一构造，ConnAck 与周期重断言共用，避免漂移。
- 下游幂等：`NodeRegistry::update_status_from_mqtt` 只在 `false→true` 边沿写 `online_since`；`SubsystemReadinessRegistry::transition` 在状态未变时直接返回，所以重断言不会刷 bootstrap snapshot version、不会引起 Desktop 频繁重渲染。

配套的上层修复（同一判据）：

- 新增 `useNodeTopologyRefresh(active)`：`AgentLibraryDialog` 打开时立即读 `GET /api/nodes` 并每 2s 刷新；`OnboardingFlow` 复用同一实现。**「需要节点状态的那一刻」开始持续读协议**，不再依赖 mount 快照或启动事件。
- `create_local_node` 的 dedup 先给协议态一个 1.5s 收敛窗口（仅当 `identity.json` 已存在时），避免在冷启动窗口里为同一个 node id 起第二个进程。

一句话：**协议状态可以迟到，但不能丢失；上层也不能只在挂载时读一次。**

### D10 — 启动器不得把启动链的 console 句柄泄漏给被启动进程

D9 修复后冷启动仍 100% 卡死，日志取证揭示第二个根因：Gateway `spawn_node_child` 只配了 `stdout(null) / stderr(inherit)`，stdin 默认继承。dev 链路里 Desktop（console 子系统）→ Gateway（`CREATE_NO_WINDOW`，attach 到新建隐藏 console，但 std 句柄仍是 dev 终端的）→ node（继承这些「属于别的 console」的句柄）。全部观察由此闭环：进程活着、19 线程全 Wait、零 MQTT 连接、identity.json 不创建、文件日志 0 字节但文件已建、每次全新环境必现、且仅在 Desktop 直启 / 管道 stdin / identity 已存在时「意外」自愈。

致命机制是 **`IsTerminal` 是句柄级判断**：node 继承了 dev 终端的 console input handle，`interactive_enrollment_token` 判定「stdin 是 TTY」，全新机器（无 identity）走进 ADR-087 交互式登录，`read_line` 从一个 node 未 attach 的 console 输入缓冲区读 —— 永久阻塞。这不是偶发竞态，是 spawn 契约缺陷。

修复（`spawn_node_child` 与 Desktop `create_local_node` 两侧对齐为同一份 stdio 契约）：

- `stdin/stdout/stderr` 全部显式 `Stdio::null()`；Windows 加 `CREATE_NO_WINDOW`。stdin=null 使 `IsTerminal` 恒为 false —— 无论谁启动 node、启动链的终端拓扑如何，fresh 机器都走 headless enroll（Gateway 默认 owner 兜底），与 D1 的判据同构：**协议/交互行为不依赖「进程是怎么被启动的」**。
- node 的诊断通道是它自己的 `{home}/logs/` 滚动文件，不是继承的 stderr（跨 console 的 stderr 写行为在 Windows console 宿主间未定义，且实测会把 file 层也拖成 0 字节）。
- 复现矩阵：bash 前台 / 管道复现器 / 隐藏真实 console 复现器（`Start-Process -WindowStyle Hidden` 复刻 dev 句柄拓扑）三种拓扑下，修复后 fresh-identity node 全部正常 enroll 且文件日志有内容；修复前真实链路 4 次启动 4 次卡死。

---

## 被否决的方案与理由

| 方案 | 否决理由 |
|------|----------|
| 给 Gateway 增加 `LocalNodeHandle { node_id, pid, instance_id, state }` 与启动握手，把本机 Node 升为一等资源 | 把「Gateway 是创建者」上升为协议身份，local 与 remote 必然分叉；违反 D1 |
| 让 `node.{id}` 重回 Required，使 READY 保证本机 Node 可用 | remote 拓扑下远端节点允许永远不在线，Required 会锁死 Gateway；ADR-059 §15.1 的降级是正确的 |
| 把 409 改成 structured `dependency_not_ready` 让 Tauri 自动重试 | 只修表象：重试的仍是一个没有 `node_id` 的请求，Gateway 依旧要猜落点；病根是 `node_id` 可缺省 |
| UI 按 `gatewayMode === "local"` 监控 Node 启动状态、失败提示重试 | 重新耦合 local；且启动事件绑定启动者，不是协议状态。正确输入是可用节点集合 |
| 保留 `gateway_managed` 但仅用于「显示」 | 只要字段在 wire 上，就必然被拿去做判定；必须删除而非软弃用 |
| Desktop 启动 Node 成功后自动接着安装 | 隐式串联等于 Desktop 替 Gateway 推断落点，且再次绑定启动者 |

---

## 影响面（代码）

| 层 | 文件 | 变更 |
|----|------|------|
| 协议 | `core/acowork-core/proto/mqtt_payload.proto` | 删 `gateway_managed = 12`，`reserved 12` |
| Node | `identity/mod.rs` / `config.rs` / `cli.rs` / `control/mod.rs` / `service.rs` | 删持久化字段与配置字段；`--spawn-marker` 消费即丢弃；`status=online` / `NodeReady` 改为周期重断言（D9） |
| Gateway HTTP | `http/agents.rs` / `http/publish_api.rs` / `http/nodes_api.rs` | 强制 `node_id`；删 `resolve_local_node_id()`；`NodeResponse` 删启动者字段、加 `control_ready` |
| Gateway 其它 | `cli.rs` / `gateway/node_manager.rs` / `mqtt/node_registry.rs` | `--node` 必填；spawn marker 改名 + 兼容旧串；`local_node_id()` 改为纯协议可见事实查询；`spawn_node_child` stdio 三 null + `CREATE_NO_WINDOW`（D10） |
| Desktop Tauri | `commands/{agent,agent_library,create,hub,gateway}.rs`、`gateway_client.rs`、`lib.rs` | 全部命令 `node_id: String`；新增 `build_remote_node_command` |
| Desktop UI | `components/nodes/{nodeAvailability.ts,NoNodeBanner.tsx,useNodeTopologyRefresh.ts}`、`AgentLibraryDialog.tsx`、`OnboardingFlow.tsx`、`CreateWizard.tsx`、`AgentList.tsx`、`lib/{types.ts,gateway-api.ts,agentHub.ts}`、`stores/agentStore.ts` | 共享可用节点定义与空态；显式 `nodeId` 端到端；拓扑状态持续可重放消费（D9） |
| 测试 | `AgentLibraryDialog.test.tsx` / `CreateWizard.test.tsx` / Rust 单测与 E2E / proto golden | 协议护栏：`settingsReads === 0`、旧 Gateway 兼容、`control_ready` 阻塞、启动者字段不入 wire |
| i18n | `en.json` / `zh-CN.json` / `zh-TW.json` / `ja.json` / `ko.json` | key 重命名与补齐 |

---

## 验证

- `check:i18n`：1958 个 `t()` 调用点全部在 en / zh-CN 解析，5 个 locale 占位符合法
- `tsc --noEmit`：改动文件零错误（全量报错均为预存的 `jszip` / `docx-preview` / `pdfjs-dist` / `xlsx` 缺依赖）
- `vitest`：改动相关 6 个测试文件 140 / 140 通过；D9 新增 `useNodeTopologyRefresh.test.ts` 3 / 3，与 `AgentLibraryDialog.test.tsx` 合并 54 / 54；全量 `3321 passed / 6 failed`，其中 5 个为会话前既存问题（4 个 editor 测试文件因 `jszip` / `docx-preview` / `pdfjs-dist` / `xlsx` 未安装无法收集，`GroupChatPanel.blockRef.test.tsx` 5 例 `@agent-a` vs `@Agent A`），第 6 个 `DocEditor.dirty-on-open.test.tsx` 单独复跑 2 / 2 通过 —— 全量并发负载下的时序 flaky，与本轮改动无关
- `cargo test -p acowork-node --lib control::tests`：30 / 30 通过（含 D9 新增 `node_ready_envelope_carries_the_announcing_node_id`）
- `cargo check --manifest-path apps/acowork-desktop/src-tauri/Cargo.toml`（lib+bin）：通过（D9 的 dedup 收敛窗口）
- `cargo test -p acowork-gateway --test node_control_plane_e2e`：6 / 6 通过（含 `spawn_marker_does_not_change_node_protocol_behaviour`；需先 `cargo build -p acowork-node --bin acowork-node` 让 E2E 使用新 CLI 参数）
- `cargo check --manifest-path core/Cargo.toml -p acowork-core -p acowork-node -p acowork-gateway -p acowork-runtime --all-targets` 与 `cargo check --manifest-path apps/acowork-desktop/src-tauri/Cargo.toml --all-targets`：均通过
- Rust lib 单测（清代理后）：`402 + 904 + 212 = 1518` 全通过；`node_proto_golden` 11/11（golden 向量删去 field 12 的 `60 01` 两字节，并新增 `!fields.contains(&12)` 断言）
- Rust 协议护栏：`identity_does_not_persist_a_spawn_marker`、`local_node_id_depends_only_on_online_and_hostname`、`local_node_id_rejects_a_node_on_another_machine`、`spawn_marker_does_not_change_node_protocol_behaviour`、`NodeResponse` 序列化回归；review 追加 `unknown_control_ready_is_absent_not_false`、`list_nodes_marks_readiness_unknown_without_a_registry`、`list_nodes_projects_node_ready_from_the_bootstrap_registry`（`http::nodes_api::tests` 24 / 24）
- Review 后的协议一致性复核：`ensure_node_ready()` 在无 bootstrap registry 时 fail-open，`GET /api/nodes` 相应把 `control_ready` 从「硬报 false」改为「字段缺失 = 未知」；前端 `normalizeNode()` 只透传 boolean、保留 `undefined`，两侧判据不再互相矛盾（`http::agents::tests` 33 / 33）
- 远程命令安全护栏：host 进 shell 命令前过滤元字符、IPv6 加 `[]`、剥除开头 `-` / `.`（防被 clap 当选项）、copy 计时器卸载清理
- 环境说明：会话注入 `HTTP_PROXY` 且 `NO_PROXY` 为空时，5 个回环 HTTP 单测（`proxy` / `lsp_admin_proxy` / `embedding_api` / `probe::runner`，均未改动）会因请求被代理劫持而失败；清除代理变量后 5/5 通过，与本 ADR 无关
- D10：`cargo build -p acowork-gateway` 通过；隔离 E2E（fresh `ACOWORK_NODE_HOME`、独立端口、Desktop 同款 `RUST_LOG` env、管道句柄与隐藏真实 console 两种拓扑）——Gateway spawn 的 node 均在 ~1s 内 MQTT connected → enrollment → bootstrap complete，`identity.json` 创建，文件日志有内容（4.9 KB / 3.9 KB）；修复前同拓扑 4/4 卡死。`cargo test -p acowork-gateway --lib` 清代理后 907 / 907 通过

---

## 回滚

`node_id` 必填是**收紧**契约：旧客户端不带 `node_id` 会收到 400 而不是被猜一个落点。若必须临时兼容，只能在客户端补 `node_id`（从 `GET /api/nodes` 选），**不得**在 Gateway 恢复推断。`gateway_managed` 已 `reserved 12`，字段号不可复用；升级路径只读忽略旧 `identity.json` 字段。
