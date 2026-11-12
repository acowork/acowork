---
# ADR-075: Node 身份模型归位 — `node_id` 升级为稳定 UUID，`node_name` 承接 display 职责

> **⚠ ADR-113 已取代本文 D5 / D9 及实现清单中的启动者身份条款（2026-11-12）**：`gateway_managed` 已从 `NodeIdentity`（`identity.json`）、`NodeConfig`、`NodeInfo`（proto field 12，已 `reserved`）、`NodeInfoState` 中删除；`--gateway-managed` 改名为 `--spawn-marker`，仅作 launcher 本机进程表标记、由 CLI 消费即丢弃；`local_node_id()` 不再查询启动者标记，只按 `online` + `hostname` 判定，且不再作为 install 默认落点；Q1（"`gateway_managed` 放 NodeInfo 还是 enroll"）随字段删除而作废。`node_id` = UUID v4、`node_name` 纯展示、enrollment 幂等、rename 只改 `node_name` 等决策继续有效。详见 [ADR-113](./ADR-113-protocol-self-consistency-explicit-node-target.md)。

**状态**：草案
**日期**：2026-09-13
**决策者**：大鱼

**前置**：
- [ADR-055](./ADR-055-remote-runtime-node-topology.md)（Node 拓扑与双身份模型 — 本文档修订其 §6.12/§6.13.2 及 line 278 决策表）
- [ADR-073](./ADR-073-agent-instance-identity-decomposition.md)（Agent 实例身份分解 — 本 ADR 与之一致：稳定 ID 进路由键，display 元数据不进路由键）

---

## 1. 决策摘要

### 1.1 一句话

**Node Agent 的 `node_id` 从"hostname 派生 slug"升级为"持久化 UUID v4"（稳定、全局唯一、永不因改名而变），新增 `node_name`（slug、可改、纯 display）承接原 `node_id` 的展示职责，删除 `machine_uid`（其机器指纹职能被 `node_id` 吸收）。**

### 1.2 语义决策（本文档核心，已定稿）

| # | 决策 | 结论 |
|---|---|---|
| D1 | `node_id` 格式 | **UUID v4**，Node 首次启动生成，持久化于 `{node_data_dir}/identity.json`，重装前不变。**一切 MQTT topic / client_id / ACL / NodeRegistry 主键 / installed_agents 引用的路由键** |
| D2 | `node_name` 格式 | **slug**（`^[a-z0-9](?:[a-z0-9]-?)*[a-z0-9]$`，2-32 字符，**不允许连续 `--`**；**`"local"` 为保留字**，拒绝，避免与 Gateway 直管 agent 占位撞名）。默认值 = hostname 规整（沿用现 `node_id_from_hostname` 逻辑，更名 `node_name_from_hostname`；规整结果不合法（单字符 hostname / 全非法字符 / 保留字 `local`）→ **回落为 `node`**——Gateway spawn 本机节点时不传 `--name`，拒绝启动会让整条本机拓扑起不来，且按 §3.6 重名本就被接受、用户可 rename 消除歧义）；`--name` 显式指定。**仅 display**：UI 展示 / 日志 / CLI 默认值，**不进任何路由键**。重名允许，UI 不做自动去重，同名歧义由用户通过 rename 自行解决 |
| D3 | `machine_uid` | **删除**。其职能（机器指纹、冲突检测、enrollment 幂等）全部由 `node_id`（UUID）承担。双身份合并为单身份 + display 名 |
| D4 | rename 语义 | `acowork-node rename <new_name>` 只改 `node_name`，**不改 `node_id`**。现有 rename 的 `migrate_retained` / 重建 installed inventory / 停 daemon 防 client_id 冲突 / 旧 topic 搬迁逻辑**全部删除** |
| D5 | 本机节点判定（`local_node_id()`） | Gateway 侧 `local_node_id()` **不再是纯函数**（UUID 由 Node 生成，Gateway 无法从 hostname 推导），**签名改 `Option<String>`，7 个文件调用点逐个适配**。Node 首次由 Gateway spawn（cmdline `--gateway-managed`）时把 `gateway_managed=true` **持久化进 identity.json**（cmdline 仅作初始来源，防崩溃后由服务/容器重启丢参）；`NodeInfo` 每次上线携带该标记。Gateway `local_node_id()` 查询 NodeRegistry 中 `gateway_managed=true` 且在线的节点：**多命中取 `last_updated` 最新并打 WARN**；查不到返回 `None`，调用点 **fail loud**（报错提示"本机节点未上线"，install 等默认落点不得静默落到任一远程节点）。**Gateway 直管 agent（node_id=`"local"`）路径统一锚定 `"local"` 字面量，不经过 `local_node_id()`**（dispatch.rs 的 running 记录 / 分组键全部固定 `"local"`）——`local_node_id()` 只服务 Node 控制面 |
| D6 | `"local"` 保留字边界 | 生产代码中剩余的 `node_id: "local"`（state.rs `AgentInfo.node_id` / `RunningAgentInfo.node_id` 兜底、http/agents.rs `track_running_agent`、mqtt/dispatch.rs `handle_plaintext_message` / `track_running_agent_for_status` / `reconcile_running_agents`、http/fs_browse.rs `browse_fs` 的入参哨兵）均为**「宿主 Node 尚未知 / 本机」的簿记哨兵**，不代表任何 Runtime 的宿主。`browse_fs` 的 `"local"` 仅用于**比较**（空 / `local` = 浏览 Gateway 本机），从不作为 MQTT topic 发布。**`local_node_id()` 只服务 Node 控制面**，不再与 `"local"` 字面量混用。语义锚点迁移与"Gateway 直管 agent"类别移除见 [ADR-077 §3.4](../ADR-077-system-agent-demotion-to-default-agent.md) |
| D7 | 冲突检测 | **删除** ADR-055 §6.12 的"重名不同 machine_uid 拒绝"逻辑（UUID 永不撞）。`NodeTokenStore` 的 `machine_uid` 字段、`machine_uid_of()`、`upsert` 的 placeholder claim 分支、预注册 placeholder（`upsert(&local_node_id(), "")`）一并删除。enroll 简化为"按 node_id 查 token：存在则复用，不存在则签发" |
| D8 | UI display 优先级 | 前端节点显示名统一为 **`node_name ?? hostname ?? node_id`**（`node_name` 是用户显式设置的 display 名，`hostname` 是 OS 当前机器名，`node_id` 是 UUID fallback）。更新 `partitionAgentsByNode.ts:nodeDisplayName` 与 SettingsPage 节点列表 title |
| D9 | 协议 wire | protobuf 字段名不变（`node_id` 仍是 string），**值语义从 slug 变 UUID**；`NodeInfo` 新增 `node_name = 12`、`gateway_managed = 13`；`NodeEnroll` / `NodeEnrollResult` 仅删除 `machine_uid = 2`（字段号保留空位，不重用；**enroll 不带 `gateway_managed`**，权威源是每次上线的 NodeInfo）。`NODE_PROTOCOL_VERSION` 从 1 → 2 |
| D10 | 兼容性 | **不保留**。项目未上线，旧数据文件直接删除 / 重新生成，无迁移代码 |

---

## 2. 问题背景

### 2.1 现状（ADR-055 §6.12 双身份模型）

| 身份 | 格式 | 生成 | 生命周期 | 用途 |
|---|---|---|---|---|
| `machine_uid` | UUID v4 | 首次启动生成，持久化 | 永不变 | 机器指纹、冲突检测 |
| `node_id` | slug（hostname 规整） | `--name` 或 hostname 派生 | 持久化；可 rename | 一切 topic / client_id / ACL / UI 逻辑名 |

### 2.2 命名错位

两个字段的**名字与语义互相错位**：

- 叫 `node_id` 的字段（slug、可改名）实际承担的是 **display name** 职责；
- 叫 `machine_uid` 的字段（UUID、永不变）实际承担的是 **node id** 职责。

名字起错导致所有围绕它的讨论都在两个名字间绕圈（"为什么 node_id 不是 UUID"、"hostname 派生稳定吗"、"改名会不会断路由"），ADR-055 的"双身份分离"其实是**同一个概念被错误拆成两个字段 + 一个字段用错名字**。

### 2.3 真实 bug：display name 被塞进路由键

`node_id`（可改 slug）被用作所有 MQTT 路由键（[acowork-core/src/node.rs](core/acowork-core/src/node.rs)）：

| 用途 | 形式 |
|---|---|
| MQTT client_id | `node:{node_id}` |
| **LWT topic**（CONNECT 时定稿） | `acowork/nodes/{node_id}/status` |
| ready / info / events retained | `acowork/nodes/{node_id}/{ready,info,events}` |
| per-agent 控制路由 | `acowork/nodes/{node_id}/agents/{instance_id}/control/{cmd}` |
| per-agent events / installed | 同上 |
| LSP / sidecar status | `acowork/nodes/{node_id}/lsps`、`.../sidecars/{kind}/status` |
| enroll / enroll_result | `acowork/nodes/{node_id}/enroll[_result]` |

**一旦用户执行 rename（ADR-055 §6.13.2 已有此命令），路由全断**：旧 topic 的 retained 全部成为 ghost，Node 重连后用新 `node_id` 订阅，Gateway 按旧 `node_id` 发的控制指令永远到不了 Node。现实现为此付出的代价（[control/mod.rs rename](core/acowork-node/src/control/mod.rs#L1656-L1700)）：

- 重建 installed inventory 并重发布所有 retained 快照到新 topic（`migrate_retained`）；
- 要求 daemon 停止（rename 复用旧 client_id，与运行中的 daemon 冲突）；
- 旧 topic 全量搬迁。

**这些全是"display name 当路由键"的次生灾害。**

### 2.4 决策对比（为什么改 UUID 而不是把路由键换成 machine_uid）

| 方案 | 代价 |
|---|---|
| A：路由键换成 `machine_uid`（保持双字段） | topic 命名空间语义不变但字段错位依旧存在；改名后 `node_id` 和 `machine_uid` 谁是真的 ID 依然混淆；两字段并存无意义 |
| **B（本 ADR）：`node_id` 直接是 UUID，`machine_uid` 删除，新增 `node_name`** | 字段归位、语义清晰；路由键值变化但 topic 字符串形状不变；rename 大幅简化；冲突检测整体删除 |

B 方案下 `acowork/nodes/{node_id}/#` topic 形状不变，只是 `{node_id}` 占位符内容从 `gpu-server` 变为 `f47ac10b-58cc-4372-a567-0e02b2c3d479`。

---

## 3. 详细设计

### 3.1 新身份模型

```
{node_data_dir}/identity.json
{
  "node_id":       "f47ac10b-58cc-4372-a567-0e02b2c3d479",  // UUID v4，路由键，重装前不变
  "node_name":     "gpu-server",                             // slug，display，可 rename
  "node_token":    "...",                                    // 不变
  "gateway_addr":  "...",                                    // 不变
  "enrollment":    "enrolled",                               // 不变
  "created_at":    "...",
  "enrolled_at":   "..."
}
```

`machine_uid` 字段删除。首次启动：`Uuid::new_v4()` → `node_id`；hostname 规整 → `node_name`。`load()` 校验：`node_id` 必须可解析为 UUID，`node_name` 必须为合法 slug。

### 3.2 路由键全部使用 `node_id`（UUID）

[acowork-core/src/node.rs](core/acowork-core/src/node.rs) 的 topic / client_id 构造函数**签名与 topic 形状不变**，仅调用方传入的值从 slug 变 UUID：

- `node_client_id(node_id)` → `node:{uuid}`
- `node_status_topic(node_id)` → `acowork/nodes/{uuid}/status`
- …（其余函数同理）

`NODE_ID_MAX_LEN = 32` 改名为 `NODE_NAME_MAX_LEN = 32` 保留给 slug；UUID 的合法性用 `Uuid::parse_str` 校验，不再走 slug 校验。`node_id_is_valid` 更名为 `node_name_is_valid` 并只用于 `node_name`（`--name` 参数、rename 目标、identity 校验）。

### 3.3 rename 流程（仅在线，大幅简化）

```
acowork-node rename <new_name> [--gateway ADDR]
1. 校验 new_name 为合法 slug（含 "local" 保留字拒绝）
2. 用临时 client_id `node:{uuid}:rename` + node_token 连接 broker（不发布 LWT）
3. 订阅 `acowork/nodes/{uuid}/status` 读 retained：非 "online" → 报错退出（离线不允许 rename）
4. identity.node_name = new_name；identity.json 写盘
5. 重发 info retained（含新 node_name）+ 刷新 bootstrap snapshot，断开
6. 完成 —— 不重连、不迁移 retained、不停 daemon、不动 node_id
```

NodeRegistry 中该节点的 `node_name` 字段随 info 更新，UI 自动刷新。旧 rename 的 `migrate_retained` / installed inventory 重建 / daemon 停止前置条件全部删除。

**ACL 配套**：[broker.rs:95](core/acowork-gateway/src/mqtt/broker.rs#L95) 现按 client_id `node:{id}` 精确拆 id 查 token，`node:{uuid}:rename` 会被拆出 `{uuid}:rename` 而查不到。`check_connect_auth` 增加一个分支：`node:{uuid}:rename` 用 `{uuid}` 查 node_token 校验（与正常 node 同一校验，仅 client_id 解析不同），约 3-5 行。

### 3.4 enrollment（简化）

```
acowork-node enroll
1. 读 identity.json（node_id = UUID，node_name = slug）
2. CONNECT client_id = "node:{node_id}"，LWT = acowork/nodes/{node_id}/status
3. PUBLISH acowork/nodes/{node_id}/enroll { node_id, os, arch, ... }（`node_name` 不进 enroll，随每次上线的 NodeInfo 上报——§4.1）
4. Gateway：node_id 已注册 → 复用 token（幂等）；未注册 → 签发新 token
5. 发布 status=online + info retained
```

冲突检测（"已被不同 machine_uid 占用 → 拒绝"）删除——UUID 永不撞。`NodeTokenStore.upsert` 简化为：存在则返回既有 token，不存在则生成新 token 并持久化；placeholder（空 machine_uid 预注册）机制删除。enroll payload 不带 `gateway_managed`（见 D5/D9，该标记只随 NodeInfo 每次上线上报）。

### 3.5 Gateway 本机节点判定

**问题**：现 `local_node_id()` 是纯函数（`node_id_from_hostname(system_hostname())`），Gateway 与 Node 可独立推导同一值。UUID 由 Node 生成后，Gateway 无法推导。

**方案**（D5）：

1. Node 首次由 Gateway spawn（cmdline 含 `--gateway-managed`）时，将 `gateway_managed = true` **持久化写入 identity.json**——cmdline 仅作初始来源，之后以文件为准（防 Node 崩溃后由服务 / 容器重启时丢失 spawn 参数）；
2. `NodeInfo` 每次上线携带 `gateway_managed`（enroll 不带，权威源是每次上线的 NodeInfo）；
3. Gateway `local_node_id()` 签名改 `Option<String>`，查询 NodeRegistry 中 `gateway_managed == true` 且最近 status=online 的节点：**多命中取 `last_updated` 最新并打 WARN**；查不到返回 `None`；
4. `None` 时调用点 **fail loud**：报错 / 日志提示"本机节点未上线"，install 等默认落点**不得**静默落到任一在线远程节点；
5. `dispatch_url_host`（loopback vs advertise_host 决策）仍按 `node_id == local_node_id()` 判定，本机节点走 loopback；
6. **Gateway 直管 agent（node_id = `"local"`）路径统一锚定 `"local"` 字面量，不经过 `local_node_id()`**——dispatch.rs 等处的 running 记录 / 分组键固定 `"local"`，`local_node_id()` 只服务 Node 控制面（否则直管 agent 的 node_id 会被悄悄从 hostname slug 漂移成 UUID，破坏 D6 边界）。

边界：手动在本机启动的 Node（无 `--gateway-managed`）不再被识别为"本机节点"，其 package 下载走 advertise_host —— 因 `http_endpoint` 可路由，行为不破，仅不再优化为 loopback。可接受。

### 3.6 UI display

- [partitionAgentsByNode.ts:66](apps/acowork-desktop/src/components/agent-list/partitionAgentsByNode.ts#L66) `nodeDisplayName`：`hostname ?? node_id ?? nodeId` → **`node_name ?? hostname ?? node_id ?? nodeId`**
- [SettingsPage.tsx GatewayTab NodesTree title](apps/acowork-desktop/src/components/settings/SettingsPage.tsx)（已修的 `hostname ?? node_id`）：→ **`node_name ?? hostname ?? node_id ?? nodesUnassigned`**
- 前端 `types.ts NodeInfo`：删 `machine_uid?: string`，加 `node_name?: string`

`node_name` 允许重复，UI **不做自动去重**（display 名由用户自定义，同名歧义由用户通过 rename 自行解决，不加 UUID 后缀）。

分组键 `agent.node_id` / `node.node_id` 从 slug 变 UUID——仅内部 key，UI 无感；直管 agent 的分组键固定 `"local"`（D5）。

---

## 4. 影响面清单（double check 结果）

### 4.1 protobuf（[core/acowork-core/proto/mqtt_payload.proto](core/acowork-core/proto/mqtt_payload.proto)）

| message | 改动 |
|---|---|
| `NodeInfo`（L1225） | 删 `machine_uid = 2`（编号留空）；加 `node_name = 12`；`gateway_managed = 13` |
| `NodeEnroll`（L1476） | 删 `machine_uid = 2`（enroll 不带 `gateway_managed`，见 D9） |
| `NodeEnrollResult`（L1500） | 删 `machine_uid = 2` |
| `NodeReady`（L1601） | 注释更新（已是 "Stable Node identifier" 语义，无需字段改动） |
| 其余带 `node_id` 的 message（L404/422/438/668/1052/1295/1458） | 字段不变，值语义变 UUID |

`NODE_PROTOCOL_VERSION` 1 → 2（[node.rs:15](core/acowork-core/src/node.rs#L15)）。

### 4.2 Rust（core/）

| 文件 | 改动 |
|---|---|
| `acowork-core/src/node.rs` | `node_id_from_hostname` → `node_name_from_hostname`；`node_id_is_valid` → `node_name_is_valid`（slug 正则收紧 + `"local"` 保留字）；`NODE_ID_MAX_LEN` → `NODE_NAME_MAX_LEN`；`local_node_id` 注释更新（非纯函数）；`local_node_id_is_valid_hostname_slug` 测试删除 |
| `acowork-node/src/identity/mod.rs` | 字段重构（删 machine_uid、加 node_name、node_id 变 UUID、**加 `gateway_managed` 持久化**）；`load_or_create` / `validate` 更新；测试更新 |
| `acowork-node/src/cli.rs` | `--name` 帮助文本 → node_name；测试 machine_uid 删除 |
| `acowork-node/src/control/mod.rs` | enroll payload 删 machine_uid；rename 流程简化（仅在线 + `node:{uuid}:rename` client_id + status retained 校验）；`validate_rename_target` 用 `node_name_is_valid`；测试更新 |
| `acowork-node/src/control/dedup.rs` | 仅测试 `"local"` 占位，不动 |
| `acowork-node/src/package_http.rs` / `proxy/mod.rs` / `acowork-runtime/src/mqtt/client.rs` | 仅测试 machine_uid，删除 |
| `acowork-gateway/src/mqtt/broker.rs` | `check_connect_auth` 增加 `node:{uuid}:rename` 分支（用 `{uuid}` 查 token，约 3-5 行，见 §3.3） |
| `acowork-gateway/src/gateway/mod.rs` | `local_node_id()` 调用改 registry 查询（Option 适配）；placeholder `upsert(&local_node_id(), "")` 删除 |
| `acowork-gateway/src/gateway/node_manager.rs` | `local_node_id()` 改 registry 查询；注释更新 |
| `acowork-gateway/src/gateway/state.rs` | `"local"` 占位不动（Gateway 直管语义） |
| `acowork-gateway/src/mqtt/node_registry.rs` | `NodeInfoState` 删 `machine_uid`、加 `node_name` + `gateway_managed`；多命中取最新；注释更新 |
| `acowork-gateway/src/mqtt/enrollment.rs` | `NodeTokenRecord` 删 `machine_uid`；`machine_uid_of()` 删；`upsert` 简化（删 placeholder claim 分支）；测试更新 |
| `acowork-gateway/src/mqtt/dispatch.rs` | enroll 决策简化（删 conflict 分支）；`local_node_id()` 调用改 registry（Option 适配）；**直管 agent running 记录 / 分组键锚定 `"local"` 字面量**；测试段更新（L2115-2285 等） |
| `acowork-gateway/src/http/nodes_api.rs` | `NodeResponse` 删 `machine_uid`、加 `node_name`；注释更新；测试 `"local"` 断言更新 |
| `acowork-gateway/src/http/agents.rs` / `publish_api.rs` / `skills_api.rs` / `cli.rs` | `local_node_id()` 调用改 registry 查询（`unwrap_or_else` 改 Option 处理） |
| `acowork-gateway/tests/node_control_plane_e2e.rs` | 删 machine_uid 参数 |

### 4.3 前端（apps/acowork-desktop/src）

| 文件 | 改动 |
|---|---|
| `lib/types.ts` | `NodeInfo` 删 `machine_uid`、加 `node_name` |
| `components/agent-list/partitionAgentsByNode.ts` | `nodeDisplayName` 加 `node_name` 优先级 |
| `components/settings/SettingsPage.tsx` | NodesTree title 加 `node_name` 优先级 |

### 4.4 文档 / 测试脚本

| 文件 | 改动 |
|---|---|
| `docs/adr/zh/ADR-055-remote-runtime-node-topology.md` | §6.12 重写（身份表、enrollment 流程、冲突检测删除）；§6.13.2 rename 描述更新；line 278 决策表"node_id 形式"行重写 |
| `docs/protocols/zh/mqtt.md` + `en/mqtt.md` | machine_uid 引用更新 |
| `core/acowork-core/tests/node_proto_golden.rs` | golden 字节重新生成 |
| `dev/e2e_frontend_smoke/smoke_test.py` | machine_uid 引用更新 |

### 4.5 数据文件（用户手动清理，无迁移代码）

| 文件 | 处理 |
|---|---|
| `{node_data_dir}/identity.json`（Node 侧） | 删除，Node 重启重新生成（新 schema） |
| `{gateway_data_dir}/node_tokens.json` | 删除，所有 Node 重新 enroll |
| `{gateway_data_dir}/enrollment_tokens.json`（如有） | 删除（一次性 token 重发） |
| MQTT broker retained：`acowork/nodes/{old_slug}/#` | 清空或重启 broker |

**克隆 / 镜像部署**：克隆模板预置的 identity.json 会让两台机器共享同一 UUID，enroll 幂等会把第二台静默当作第一台（token 复用、registry 键覆盖）且无告警。克隆 / 镜像部署前**必须删除 identity.json**（文档提示即可，不加防御代码——同 UUID 无冲突检测是本 ADR 明确接受的边界）。

---

## 5. 收益

1. **路由键稳定**：`node_id` 是 UUID，永不因改名 / hostname 变化而变；rename 不断路由
2. **命名归位**：`node_id` 就是 ID，`node_name` 就是名字，讨论不再绕圈
3. **代码删减**：冲突检测、placeholder、migrate_retained、rename 的 inventory 重建与 daemon 停止前置全部删除
4. **多机重名不再是错误**：两台机器同名 `node_name` 不再是注册冲突；display 名由用户自定义，同名歧义由用户通过 rename 自行解决，UI 不做自动去重

## 6. 已确认（2026-09-13 review 定稿）

| # | 原问题 | 定论 |
|---|---|---|
| Q1 | `gateway_managed` 放 NodeInfo 还是 enroll | **只放 NodeInfo**，enroll 不带；权威源 = 每次上线的 NodeInfo（enroll 是低频事件，不可靠） |
| Q2 | `local_node_id()` 查不到时默认行为 | 返回 `None`，调用点 **fail loud**（报错提示，install 不静默落远程节点） |
| Q3 | golden 是否纳入本 ADR 实施 | **是**，协议变必改 |
| Q4 | `"local"` 占位是否统一为 `node_id` 概念 | 维持 `"local"` 字面量：直管 agent 路径统一锚定 `"local"`，不经过 `local_node_id()`；不并入 Node 控制面 `node_id` 概念 |
