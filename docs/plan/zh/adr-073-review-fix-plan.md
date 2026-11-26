# ADR-073 实施 Review 修复计划

**日期**：2026-10
**分支**：`feature/pm-doc`（未提交 diff，59 文件 +2007/−578）
**关联**：[ADR-073-agent-instance-identity-decomposition.md](../../adr/zh/ADR-073-agent-instance-identity-decomposition.md)
**状态**：阶段 1/2/3 已完成；阶段 4（Node UUID gate、Runtime UUID 校验、fs-changed instance topic、Desktop 必填化、Node fallback 清理）已完成；Broker ACL 待验证项保持打开。

---

## 1. Review 结论摘要

ADR-073 三层身份迁移方向正确、覆盖面广（5 crate），lib/TS 测试全绿。但存在 3 个 P1 级问题会破坏 ADR 核心目标（多实例零冲突、重启恢复），以及若干架构决策偏差。P1 全部位于**未被测试覆盖的跨组件协议路径**。

| # | 级别 | 问题 | 证据位置 |
|---|---|---|---|
| P1-1 | 严重 | MQTT `client_id`/CONNECT 用户名仍为 `bot:{agent_id}`，同包双实例同 broker 互踢 | [client.rs:956](../../../core/acowork-runtime/src/mqtt/client.rs#L956)、[spawn.rs:151](../../../core/acowork-node/src/process/spawn.rs#L151) |
| P1-2 | 严重 | AgentRegistry envelope 解码分支以 `status.agent_id` 为 key，与 dispatch loopback（身份已放 `instance_id`、`agent_id` 置空）错位 → `""` 空 key 垃圾项、重启后 online 状态丢失 | [agent_registry.rs:143](../../../core/acowork-gateway/src/mqtt/agent_registry.rs#L143)、[dispatch.rs:424-430](../../../core/acowork-gateway/src/mqtt/dispatch.rs#L424) |
| P1-3 | 严重 | `auto_install_bundled_agents` 每次启动 copy 系统 Bot 到新 UUID 目录且无清理 → 跨重启积累重复实例 | [gateway/mod.rs:193-213](../../../core/acowork-gateway/src/gateway/mod.rs#L193) |
| P2-1 | 架构 | `AgentInstanceId` opaque 类型生产代码 0 使用；HashMap key 全 String + resolve 兼容层，与 ADR 决策 1/2 不符 | [state.rs:205-207](../../../core/acowork-gateway/src/gateway/state.rs#L205) |
| P2-2 | 安全 | Node 对入站 `instance_id` 无 UUID/路径校验，直接拼文件系统路径 | [install.rs:98](../../../core/acowork-node/src/package/install.rs#L98) |
| P2-3 | 范围 | ADR 决策 5.1（两条安装流水线收敛）未实现 | [gateway/mod.rs:170-214](../../../core/acowork-gateway/src/gateway/mod.rs#L170) |
| P2-4 | 一致 | resolve 兼容层在歧义场景 first-wins；CLI 退化传参（agent_id 当 instance_id） | [node_manager.rs:856](../../../core/acowork-gateway/src/gateway/node_manager.rs#L856) |
| P3 | 卫生 | 术语残留（`AgentOnlineState.agent_id` 实际承载 instance）、fixture 非 UUID、缩进、prompts e2e 混入非本 ADR 改动 | 多处 |

---

## 2. 用户裁决（逐条）

| 项 | 裁决 |
|---|---|
| P1-1 | 所有标识 bot 实例的场景一律 `instance_id`；`agent_id` 是"包类型"，使用场景极少；**standalone 也不回退 `agent_id`**——instance_id 是实例唯一标识，与 standalone 无关 |
| P1-2 | 同意方案：registry 按 instance 为 key（envelope `instance_id` 优先、topic 兜底），补回归测试 |
| P1-3 | 不搞复杂：只装自己的、**不删别家目录**、不做旧数据兼容/迁移（未上线，数据可手动清） |
| P2-1 | 同理不做数据兼容，**代码不留旧物，按 ADR"新的"来**（即 opaque `AgentInstanceId` 类型模型） |
| P2-2 | 同意：Node 入口做 UUID 校验，安全不能忽视 |
| P2-3 | 上传包走 node install、内置包在 gateway 多一步拷贝，**均可接受**；不做流水线收敛，唯一分叉是内置包源在 gateway |
| P2-4 | agent_id 必须全换 instance_id；agent_id 除显示外无应用场景，**存在即 bug** |
| P3 | 看着办，该改就改 |

---

## 3. 实施记录与剩余（每阶段保持 clippy + lib 测试全绿）

### 阶段 1 — P1-2：AgentRegistry instance 化（✅ 完成）

- [x] `agent_registry.rs`：`AgentOnlineState.agent_id` → `instance_id`；envelope 分支按 `status.instance_id` 寻址、topic 变量兜底；明文分支显式 instance 语义
- [x] 新增回归测试（gateway lib 449→451）：
  - `test_update_from_mqtt_loopback_envelope_keys_by_instance_id`（production 形状：`agent_id=""`、`instance_id=uuid`）
  - `test_two_instances_same_package_are_independent`（uuidA/uuidB 同包互不覆盖）

### 阶段 2 — P1-1 + Runtime instance_id 必填化（✅ 完成）

- [x] `runtime/src/cli.rs`：`--agent-instance-id` 必填（`String`，无 default/无 Option）
- [x] `runtime/src/config.rs`：`agent_instance_id: String`；`validate()` 升级为 `AgentInstanceId::from_string`（UUID 形状校验，非仅空串）；`instance_id()` 无 fallback
- [x] `runtime/src/mqtt/client.rs`：`client_id = bot:{instance_id}`（无 agent_id 回退）；`MqttConnectConfig.instance_id: &str` 必填；status/meta/config/ready/session 各 topic 全部 instance 化
- [x] 测试 fixture 全部 UUID 化：client.rs 内部 broker 测试 ×2、http/server.rs 测试、agent_config_publisher_e2e（helper 返回 instance）、fs_watcher_e2e、mqtt_e2e_full（8 处 runtime + gw 控制 publish target + LWT 订阅/断言同步 instance）
- [ ] **待验证项**：broker CONNECT/ACL 中 `bot:{id}` 用户名是否需要 instance 维度（§6.8）——保持打开，另行验证（ACL 测试 fixture 仍用 `bot:foo` 等包名）

### 阶段 3 — P1-3：auto_install 幂等且零清理（✅ 完成）

- [x] `gateway/mod.rs::install_agent_from_dir`：copy 前 fs 自检 `{packages_dir}/{agent_id}` 已存在 → 跳过；删除 `remove_dir_all`；零清理、无旧数据兼容

### 阶段 4 — P2-1/P2-2/P2-4 主体（选项 C，核心项完成）

- [x] Node control 入口 P2-2：`agent_lifecycle_instance_id` 提取 + `AgentInstanceId::from_string` gate，空/非法 instance_id 在任何 fs/进程副作用之前 error reply
- [x] 删除 control 路径 3 处 `instance_id.is_empty() → agent_id` key fallback（uninstall / skills_import / upgrade）
- [x] Runtime config P2-1 边界：`AgentInstanceId` 解析贯穿校验（core 类型在边界校验；Gateway/Node 内存表保持 String——选项 C 拍板口径）
- [x] P2-4 新发现修复：`MqttFsEventSink` fs-changed topic 原以 package `agent_id` 为 key → 发布时取绑定 client 的 `instance_id`（envelope 内仍携带包 id 供显示；e2e 订阅端同步 INSTANCE_ID）
- [x] Desktop：`AgentInfo`/`AgentDetail.instance_id` 必填；`instanceIdOf` 删除 legacy 回退；store 注释同步；tsc + vitest 全绿
- [x] **已完成**：Node manager/package/clone 及 control fallback 清理完成。reap（老进程兼容）和 Gateway inventory 加载（老格式兼容）保留，其余删除。
- [x] **已完成**：Gateway 侧 CLI/http 调用点核对完毕；resolve_* 双查已删，instance_id 传参全链路一致。

### 阶段 5 — 收尾与全量验证

- [x] 全量验证：runtime 1409 / node 123 / gateway 451 / Desktop tsc+vitest 全绿；clippy --all-targets 干净（仅 node 1 doc-list warning 可忽略）
- [x] 剩余 Node manager/package/reap 10处防御 fallback 清理完成（仅保留 reap 老进程兼容和 Gateway inventory 老格式兼容）
- [ ] P3 术语重命名、缩进修正（随剩余项一并）

### 全局约束

- 生产 gateway/runtime 进程在跑：只用 `cargo check`/`cargo test --lib`，禁止覆盖 `target/debug/*.exe`；不跑 e2e（避免与生产 broker :19875 冲突）
- 不做任何旧数据/旧消息的代码级兼容（项目未上线，数据可手动迁移）

---

## 4. 阶段 4 口径（已确认：选项 C）

**2026-10 用户拍板：core 层用类型，网关存储用 String（折中方案）**

- `AgentInstanceId` 在 **proto/HTTP/MQTT 入站边界做 UUID 校验**（Node control 命令、envelope `instance_id`、topic 路径变量），并贯穿 topic 构造；
- Gateway/Node **内存表与结构字段保持 String**（instance_id 值语义）；
- 但 **resolve/fallback/包级寻址等旧物一律删除**（P2-1/P2-4 不变）：`resolve_installed_key`/`resolve_running_key`/`installed(id)` 双查、`is_installed(pkg)`/`is_running(pkg)` 包查询、Node 侧 `instance_id.is_empty() → agent_id`、CLI 退化传参、Desktop `instanceIdOf` fallback 全部移除；`agent_id` 仅保留显示/日志。


---

## 5. 验证基线（review 时实测）

- `cargo check --all-targets`（node/runtime/gateway）：干净
- lib 测试：core 193 / runtime 1409 / gateway 451 / node 123 — 全绿
- Desktop vitest：385（31 文件）— 全绿；`tsc --noEmit` 干净
- e2e 未运行（避免与生产 broker :19875 冲突；fixture 已同步 instance 语义）
