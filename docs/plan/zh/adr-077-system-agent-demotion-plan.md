# ADR-077 System Agent 降级实施计划

> 版本：v0.1（草案）| 日期：2026-11-12
>
> 关联 ADR：[`docs/adr/zh/ADR-077-system-agent-demotion-to-default-agent.md`](../../adr/zh/ADR-077-system-agent-demotion-to-default-agent.md)（本计划唯一权威）
> 同步修订：[ADR-059 §4.3 / §6.1 / §7.5 / §12.2 / §15.1](../../adr/zh/ADR-059-parallel-onboarding-handshake.md)、[ADR-055 §6.2](../../adr/zh/ADR-055-remote-runtime-node-topology.md)、[ADR-075 D6](../../adr/zh/ADR-075-node-identity-uuid-and-node-name.md)
> 范式先例：[`docs/plan/zh/user-dev-plan.md`](user-dev-plan.md)、[`docs/plan/zh/pm-dev-plan.md`](pm-dev-plan.md)
>
> **一句话**：把 `com.acowork.system` 从「Gateway auto-install/auto-start + BootstrapState.required capability + 不可卸载特判」路径中完整剥离，落地为「bundled default 普通 agent，与 senior-engineer-agent / document-manager-agent 平权」。预估总工期 **3-4 人日**（单人全职）。**预估 review 1d、合并 0.5d** —— 单测失败 / e2e flake 需留 buffer。

---

## 1. 排期假设

- **团队规模**：单人全职（兼任代码评审自审）。
- **工时口径**：1d = 8h，含编码 + 单测 + 集成测试 + 文档同步。
- **排期窗口**：4-5 工作日连续投入；不含代码评审、合并、跨服务联调 buffer。
- **前置依赖（已就绪，无需新工作）**：
  - ADR-073 实例身份范式（System Agent 的 instance UUID 与普通 agent 无差别）。
  - ADR-055 Node 拓扑 + ADR-075 Node UUID 化（installed_agents.node_id 已能正确写入宿主 Node UUID）。
  - ADR-076 / ADR-084 用户域剥离完成（identity 业务边界清晰，与 System Agent 互不耦合）。
  - `POST /api/agents/ensure` 已存在且幂等（[apps/acowork-desktop/src-tauri/src/commands/gateway.rs:313](apps/acowork-desktop/src-tauri/src/commands/gateway.rs#L313) 中 `ensure_system_agent` 的核心实现即调用该端点）。
- **可复用，不重写**：
  - OnboardingFlow 的"默认勾选 + 取消"交互范式（已有 `install-system-agent` 步骤）。
  - `AgentList` 中"按 `installed_at` / `last_interaction_at` 排序"的现成路径。
  - `BootstrapState` 的 `mark_ready` / 现有 Required 子系统注册点（移除一项即可，不改机制）。
- **不在范围（YAGNI）**：
  - 不改 `com.acowork.system` 的 `agent_id`、prompt、skills、tool 集。
  - 不改 `IdentityRead` / `IdentityWrite` 权限定义（权限定义与 agent 实现解耦）。
  - 不动 `intent/privacy.rs`、`memory_recall` / `memory_store` 工具。
  - 不动 `acowork-core`（无 Gateway 边界契约变化）。

---

## 2. 里程碑总览

| 阶段 | 内容 | 估时 | 交付物 / 出口条件 |
|------|------|------|-------------------|
| **M0 Rust Gateway** | 删除 auto-start task + Required 注册分支 + sort 特判 + `"local"` 注释收紧 + manifest `system = true` 删除 + 陈旧记录清理 | 1.5-2d | `cargo build` + `cargo test` 全绿；System Agent 与普通 agent 行为一致 |
| **M1 Desktop** | 不可卸载守卫删除 + `ensure_system_agent` Tauri 命令删除 + SplashScreen 移除 + onboarding 默认勾选接管 + i18n 清理 + AgentList 排序放开 | 0.5-1d | `npm run build` + `npm run test` 全绿；onboarding 默认装 System Agent，可取消 |
| **M2 同步修订 + 回归测试** | ADR-059/055/075 引用点同步 + bootstrap_integration.rs 翻转测试 + clippy + e2e smoke | 1d | `cargo clippy -- -D warnings` 干净；新测试 `bootstrap_succeeds_without_system_agent` 绿 |

---

## 3. 里程碑详情

### M0 — Rust Gateway（1.5-2d）

| ID | 任务 | 估时 | 依赖 | 验收 |
|----|------|------|------|------|
| M0-1 | 删除 `gateway/mod.rs` 的 `auto_start_system_agent` 任务（约 [1502-1722](core/acowork-gateway/src/gateway/mod.rs#L1502) 共 220 行）+ 相关 `use SYSTEM_AGENT_ID` | 0.5d | — | `cargo build` 通过；Gateway 启动不再 poll retained inventory、不再 fallback bundled install、不再发 start |
| M0-2 | 删除 `mqtt/dispatch.rs` 中 `SYSTEM_AGENT_ID` 的 `register("system_agent", Required)` 特判分支（[line 885-895](core/acowork-gateway/src/mqtt/dispatch.rs#L885)）+ 紧邻的 `mark_ready()` 调用 | 0.25d | — | `cargo test` 绿；installed inventory 聚合对 `com.acowork.system` 一视同仁 |
| M0-3 | 删除 `http/agents.rs` 的 sort 特判：[line 373-374](core/acowork-gateway/src/http/agents.rs#L373) 的 `a_sys` / `b_sys` 分支 + [line 2678](core/acowork-gateway/src/http/agents.rs#L2678) 的 `sort_pins_system_agent_first` 单测 + `use SYSTEM_AGENT_ID` | 0.25d | — | 单测绿；list 排序按 `last_interaction_at DESC` → `name` 走，不再有特权位次 |
| M0-4 | `"local"` 占位注释收紧：[state.rs `AgentInfo.node_id`](core/acowork-gateway/src/gateway/state.rs) 注释 + [agents.rs `track_running_agent`](core/acowork-gateway/src/http/agents.rs) + [dispatch.rs 三个兜底点](core/acowork-gateway/src/mqtt/dispatch.rs) + [fs_browse.rs `browse_fs`](core/acowork-gateway/src/http/fs_browse.rs) —— 全部改为"宿主 Node 尚未知 / 本机的簿记哨兵，不代表任何 Runtime 的宿主" | 0.25d | — | 注释统一、无歧义 |
| M0-5 | 删除 `examples/system-agent/manifest.toml` 的 `system = true` 行（[line 11](examples/system-agent/manifest.toml)）+ 顶部注释"Always started with Gateway, cannot be uninstalled"改为"随 Gateway 分发的预装 default agent，可装可卸" | 0.1d | — | manifest 解析通过 |
| M0-6 | 启动清理：陈旧 `installed_agents` / `running_agents` 中 `node_id == "local"` 且 `agent_id == "com.acowork.system"` 的记录 → 删除。放在 Gateway 启动早期（state 初始化后），一条循环搞定；加注释说明"ADR-077 陈旧特权路径残留清理，项目未上线，无需迁移代码" | 0.25d | M0-1 | 单元测试覆盖：含陈旧记录的 fixture 启动后该记录消失；正常记录不动 |
| M0-7 | `gateway/mod.rs` 顶部 capability 注释（[line 402-403](core/acowork-gateway/src/gateway/mod.rs#L402)）从子系统清单移除 `system_agent` | 0.1d | — | 注释无 `system_agent` 字样 |

**M0 出口**：Gateway 不再 auto-start System Agent、不再把它注册为 Required、不再有 list 排序特权、不再有 `system = true` 元数据；陈旧 `"local"` 记录自动清理。

---

### M1 — Desktop（0.5-1d）

| ID | 任务 | 估时 | 依赖 | 验收 |
|----|------|------|------|------|
| M1-1 | 删除 `agentStore.ts:619` `throw new Error("System Agent cannot be uninstalled")` 守卫 + `agentStore.ts:659` 排序特判 + `agentStore.ts:18` `SYSTEM_AGENT_ID` 常量（若移除后无任何消费方） | 0.25d | — | `npm run test` 绿；`uninstall_agent("com.acowork.system")` 与普通 agent 一致 |
| M1-2 | 删除 tauri 命令 `ensure_system_agent`（[apps/acowork-desktop/src-tauri/src/commands/gateway.rs:313-?](#)）+ `DependencyNotReady` 中 `com.acowork.system` 变体（[line 287](apps/acowork-desktop/src-tauri/src/commands/gateway.rs#L287)）+ Tauri 命令注册 | 0.25d | — | `tauri build` 通过；命令列表不含 `ensure_system_agent` |
| M1-3 | 删除 `SplashScreen.tsx:80-83` 的 `invoke("ensure_system_agent")` + `recoveryReload.ts` 中相关注释 + `gatewayAuthBridge.ts` 中相关注释 | 0.1d | — | SplashScreen 不再调 `ensure_system_agent`；recovery 路径不再依赖 System Agent |
| M1-4 | `OnboardingFlow.tsx` 调整：`install-system-agent` 步骤默认勾选（Q4）；新增"用户上次卸了则本次不勾选"持久化（key = `localStorage["acowork.onboarding.skipSystemAgent"]`，在 uninstall 路径写入，在 onboarding 读取） | 0.25d | M1-1 | 首次 onboard：勾选 + 取消都能跑；卸过一次 System Agent：下次 onboard 默认不勾选 |
| M1-5 | 删除 i18n `en.json:386` `systemAgentCannotUninstall` 串 + 检查 zh.json 同名字串 + 删除任何 `zh-CN` 同名翻译 | 0.1d | — | `npm run i18n:check` 通过 |
| M1-6 | `AgentList.tsx:347,451` 删除 `com.acowork.system` 特判（若有） | 0.1d | — | System Agent 与普通 agent 走相同渲染路径 |

**M1 出口**：Desktop 端对 System Agent 与对 `senior-engineer-agent` / `document-manager-agent` 行为完全一致；onboarding 接管安装时机。

---

### M2 — 同步修订 + 回归测试（1d）

| ID | 任务 | 估时 | 依赖 | 验收 |
|----|------|------|------|------|
| M2-1 | 同步修订 ADR-059：[§4.3 场景矩阵](../../adr/zh/ADR-059-parallel-onboarding-handshake.md) 移除 `system_agent` 行；[§6.1 依赖 DAG](../../adr/zh/ADR-059-parallel-onboarding-handshake.md) 删 `SYS_PREPARE` / `SYS_INSTALL` 节点及边；[§7.5 / §12.2.3 / §14 / §15.1](../../adr/zh/ADR-059-parallel-onboarding-handshake.md) 同 | 0.25d | — | ADR-059 grep `system_agent` 不再有任何 Required 语义 |
| M2-2 | 同步修订 ADR-055 §6.2：删除"System Agent auto-start"段，引用 ADR-077 | 0.1d | — | ADR-055 §6.2 不再描述 auto-start |
| M2-3 | 同步修订 ADR-075 D6：去掉 `"local"` = "Gateway 直管 agent 占位" 定义，引用 ADR-077 §3.4 | 0.1d | — | D6 与 ADR-077 §3.4 一致 |
| M2-4 | 翻转测试 `core/acowork-gateway/tests/bootstrap_integration.rs`：当前 [line 739-790](../../core/acowork-gateway/tests/bootstrap_integration.rs#L739) 的 `system_agent_delay_keeps_bootstrap_booting` 改为 **`bootstrap_succeeds_without_system_agent`** —— 不注册 `system_agent` 子系统，BootstrapState 直接从 BOOTING → READY；同时 [line 594 / 1091 / 1207 / 1281](../../core/acowork-gateway/tests/bootstrap_integration.rs#L594) 的 `for id in [..., "system_agent"]` 列表移除 `system_agent` | 0.25d | M0-2 | 新测试通过；旧测试删除或改造 |
| M2-5 | `cargo clippy --all-targets -- -D warnings` + `cargo test` + Desktop `npm run build` + e2e smoke（手动跑一次 onboarding） | 0.25d | M0, M1 | 全绿；clippy 干净；onboarding 流程手测通过 |

**M2 出口**：文档引用一致；新增测试证明「System Agent 不存在时 BootstrapState 仍能 READY」—— 这是 ponytail 原则要求的"非平凡逻辑的最小可运行 check"。

---

## 4. 关键依赖与并行机会

- **M0-1 ↔ M0-2 ↔ M0-3** 互相独立，可在小范围内并行（不同文件、不同模块）。单人串行即可。
- **M1 全部** 依赖 M0-1（M1-2 的 `ensure_system_agent` 是 `gateway/mod.rs` auto-start 的客户端镜像，先确认服务端删除干净再删客户端）。
- **M2-4** 必须在 M0-2 之后（先删除注册，再改测试）。
- **并行机会**：M0-4 / M0-5 / M0-7（注释 + manifest 微调）与 M0-1 / M0-2 / M0-3（删除大块代码）可分两个 commit，方便评审。

---

## 5. 风险与缓解

| 风险 | 等级 | 缓解 |
|---|---|---|
| Desktop 启动依赖 `ensure_system_agent` 的 recovery 路径未全部覆盖 | 中 | M1-3 显式列了 `recoveryReload.ts` / `gatewayAuthBridge.ts` 注释同步；启动失败兜底走"无 System Agent 也可启动主聊天区"路径 |
| 删除 `system_agent` Required 后，BootstrapState 在某些边角先于 System Agent ready 切到 READY，导致 UI 短暂状态不一致 | 中 | M2-4 新测试覆盖该路径；Desktop 端 System Agent status 走 `/api/agents` 常规列表，与平台 ready 解耦（§3.2 决策） |
| 陈旧 `node_id == "local"` 记录清理误删正常记录 | 低 | M0-6 限定 `agent_id == "com.acowork.system"` 才删；加 fixture 单测覆盖 |
| `system = true` 删除后，是否有外部 package installer 依赖此字段 | 低 | Q1 已决策删除；grep 全仓库无消费方 |
| 多用户模式下 onboarding 默认勾选 System Agent，但用户首次登录账号时身份记忆未生效（user_profiles 还不存在） | 低 | System Agent 的 identity / preference 存储与 acowork-user 解耦（ADR-076/084 完成后）；空场景下 System Agent 即空数据也不报错 |

---

## 6. 验收标准（每里程碑）

- **M0**：`cargo build` 通过；`cargo test --package acowork-gateway` 全绿；grep `SYSTEM_AGENT_ID` 在 `gateway/mod.rs` / `mqtt/dispatch.rs` 不再有 auto-start / register 调用点。
- **M1**：`npm run build` 通过；`npm run test` 全绿；手测 onboard 流程可勾可卸。
- **M2**：`cargo clippy --all-targets -- -D warnings` 无 warning；新测试 `bootstrap_succeeds_without_system_agent` 通过；ADR-059/055/075 grep `system_agent` 仅剩历史引用（带"ADR-077 已迁移"提示）。

---

## 7. 不在本次范围（YAGNI 后置）

- System Agent 的 memory 数据迁移动作（项目未上线，无用户数据）。
- `manifest.toml` 增加 `default = true` 字段标识"建议 default"（Q1 已决策删除 `system = true`，无需替代字段 —— 任何 agent 都是普通 agent，不存在 default 元数据语义）。
- 把 `ensure_system_agent` 改为通用 `ensure_default_agents`（YAGNI：当前只有 System Agent 一个 bundled default；后续若增加再抽象）。
- 重命名 `com.acowork.system` 为 `com.acowork.identity`（语义虽然更准，但破坏现有 Intent 路由 / 已部署 manifest；本 ADR 不触及）。

---

## 8. 决策记录（本计划相关）

- 2026-11-12 计划起草，对应 ADR-077 从「草案」→「已决策」+ §6 Q1-Q4 落字。
- Q1-Q4 决策：
  - Q1 `system = true` → 删除；
  - Q2 `sort_pins_system_agent_first` → 删除；
  - Q3 Optional 子系统注册 → 不注册；
  - Q4 onboarding 默认勾选 → 默认勾选 + 持久化"上次卸了则不勾选"。

---

## 9. 排期起点建议

从 master 拉 `feature/adr077` 分支，按 M0 → M1 → M2 串行；每个里程碑结束时单独 commit + 自测，方便回滚（删除 Gateway auto-start 是最大风险点）。