# ADR-111：Agent 身份只有一个名字、一个文件

- **编号**：ADR-111
- **状态**：已实施
- **日期**：2026-11-09
- **决策者**：Nicholas（需求与方案确认）
- **关联**：ADR-073（实例身份分层，本 ADR 取代其"用户可重命名走 `display_name`"与"`overrides` 文件"部分）、ADR-104（manifest 声明面，本 ADR 从中删除 `display_name`）、ADR-101（记忆服务边界 — 本 ADR 决定 bootstrap 为收敛同步）、ADR-009（Gateway 边界 — `install_path` 是 node-local，故身份必须由 Node 投影而非 Gateway 读盘）
- **英文 counterpart**：[docs/adr/en/ADR-111-one-agent-one-name-one-file.md](../en/ADR-111-one-agent-one-name-one-file.md)（同 commit 维护）

---

## 摘要

**一句话**：一个 Agent 实例**只有一个名字** `name`，它住在实例自己的 `workspace/config/agent_config.json` 里；`manifest.toml` 的 `display_name` 与 `{instance_id}.overrides.json` 一并删除。

**一句话（为什么）**：`name` / `display_name` 两个字段各自被不同层当成"真名"读，于是**同一个人看到的名字和 Agent 在上下文里看到的自己的名字不是同一个**——这正是 ACowork 的核心能力（人与 Agent 平等沟通）的破坏点。名字不是"显示偏好"，它是身份：一个字段、一个写者、一个解析点。

---

## 背景（事实取证）

1. **两个名字字段，三处各自推导。** 改前：`manifest.name`（包名）、`manifest.display_name`（包作者声明的显示名）、`{instance_id}.overrides.json` 的 `display_name`（用户重命名）。侧边栏、Agent 详情面板、PM 任务指派各自做 `overrides ?? display_name ?? name` 的层叠，其中 PM 侧（`AgentInfo`）甚至没有该字段，导致 `pm_list_agents` 拿不到任何显示名。**同一次读取，三条推导链**——这是"两个名字"的直接产物。

2. **`overrides.json` 是一个额外的、位置错误的文件。** 它由 `acowork_core::agent_overrides::overrides_path` 拼在**安装目录外层**，与它承载的语义（右侧 Agent 设置面板里的一项）分离：同一个面板的其它设置项（模型、温度、语言）都写在 `workspace/config/agent_config.json`，只有头像/名字例外。用户的原话是"凭空多搞出一个 json 文件，存储位置还在 agent 安装位置外面，非常奇葩"。它的存在没有任何技术必要性：`AgentConfig` 本来就是该面板的持久化。

3. **`manifest.display_name` 在 7 个出厂包里与 `name` 逐字相同。** 逐个核对（`agents/*/manifest.toml`）：`display_name = "..."` 与 `name = "..."` 完全一致，删除无信息损失。它唯一可能的语义是"包作者给包起的昵称"，但这个语义从未被消费——唯一读它的地方是给实例当初始名，而那正是 `name` 的职责。

4. **`agent_config.json` 已经在 `workspace/` 内，升级天然不覆盖。** `acowork-node` 的升级保留集合改前是 `data/` + `config/`，而 `workspace/` 才是 Runtime 的 work dir（`workspace/config/agent_config.json`、会话、私有 SQLite 都在里面）。升级保留 `data/` + `config/` 而**删除** `workspace/` 是一个既存缺陷（会静默删掉全部会话），本 ADR 一并修正为保留 `workspace/`。

5. **记忆服务的一次性播种让改名无法自洽。** `POST /admin/bootstrap` 改前的幂等判据是"该 namespace 已有 Identity 节点就整体跳过"。`name` 事实由 Runtime 每次启动声明（值取自 `agent_config.json`），但播种一次之后永不刷新——用户把 `SSE` 改名成 `SSE.Kevin` 后，记忆里的身份节点仍是 `SSE`，即第三处名字漂移。

6. **Gateway 不能读身份文件。** `install_path` 是 node-local（ADR-009 §5 的 fs 硬边界），远端实例的身份只有 Node 能读。因此身份必须以**投影**形式上行：`InstalledAgentInfo.identity`（`AgentIdentity { name, avatar, builtin_avatar }`）取代原来的 `overrides_json`（一个字符串套 JSON，Gateway 还得自己解析）。

---

## 决策

### D1 — 一个实例只有一个名字

`name` 即人看到的名字，也是 Agent 自述的名字。不再有 `display_name`、`alias`、`overrides.display_name` 中的任何一层。判据是可验证的：**任意两个出口（侧边栏 / Agent 详情 / PM 指派 / 群聊归属 / Agent 自己的 prompt / 记忆里的身份节点）对同一实例必须给出同一个字符串**。

### D2 — 身份三元组落在 `agent_config.json`

`AgentConfig` 新增 `name` / `avatar` / `builtin_avatar`，与其它面板设置同文件、同一次 read-modify-write。`{instance_id}.overrides.json` 删除（`acowork_core::agent_overrides` 整个模块移除，仅保留 `AVATAR_EXTENSIONS` / `has_avatar_extension` 为 `acowork_core::avatar`）。

`name` 的语义边界写进字段文档：`None` = **尚未物化**（实例从未启动），首次启动由 Runtime 用包名物化（`startup::session_init`）；此后**只有显式重命名会写它**——升级不会，因为文件在 `workspace/` 内，包够不到。

### D3 — manifest 只声明包

`AgentManifest.display_name` 删除，`ManifestDraft` 同步删除。`manifest.name` 的定位收窄为**包名 = 新实例物化的初值**，不再是"某个实例的名字"。

### D4 — 优先级：config → manifest，且只有一个解析点

```
name  = agent_config.name           // 用户/物化值，权威
      ?? manifest.name              // 未物化时的初值（= 首次启动会写入的值）
avatar= agent_config.avatar|builtin_avatar   // 用户选择，权威
      ?? manifest.avatar|builtin_avatar      // 包默认
      ?? client-side deterministic fallback  // 渲染兜底
```

解析实现单点化：Runtime 侧 `http/avatar.rs::effective_config`，Gateway 侧 `http/agents.rs::resolve_identity`（`ResolvedIdentity`）。**调用方不得二次推导**——第二处推导正是侧边栏与详情面板曾经互相矛盾的成因。

### D5 — 上行投影：`identity` 取代 `overrides_json`

MQTT proto 的 `InstalledAgentInfo.overrides_json`（field 7）替换为 `identity`（新 message `AgentIdentity`，字段 `name` / `avatar` / `builtin_avatar`）。Node 侧读实例的 `agent_config.json` 构造它（`acowork-node::package::read_identity`），Gateway 侧直接落到自己的实例视图里，不再解析字符串。Gateway 的 `AgentListResponse` / `AgentDetailResponse` 对外给 `name`（实例名）+ `package_name`（包名）两个字段，语义互斥、不互相 fallback。PM 侧 `AgentInfo` 同构：`name` + `package_name`，`assignee_meta` / `created_by_meta` 也按这两个字段投影。

### D6 — 名字进入运行时：`AgentCore::instance_name`

新增 `AgentCore.instance_name: Arc<RwLock<String>>`（session clone 共享同一把锁，见回归测试 `an_instance_name_is_shared_across_session_clones`）。三个消费者都读它，不再各自翻文件：

| 消费者 | 用途 |
|---|---|
| `chat_router` Block A | `{{agent_name}}` 占位符替换（Chat 层 prompt） |
| `bootstrap_autobio` | 记忆 bootstrap 声明的 `name` |
| `PUT /agents/{id}/avatar-config` | 改名后**热更新**，否则侧边栏立刻变、Agent 自己的 prompt 要等重启（"时间维度上的两名制"） |

空名不被接受：`set_instance_name` 忽略空白值，`PUT` 对 `name: ""` 返回 400（"an agent always has a name"）。

### D7 — 记忆 bootstrap 是收敛同步，不是一次性播种

`POST /admin/bootstrap` 每个声明键逐一收敛：

- 键不存在 → 创建（`source = "manifest"`）；
- 存在且 `source = "manifest"`、值不同 → **原地更新**；
- 存在且 `source = "manifest"`、值相同 → 不写（不抬 `updated_at`）；
- 存在但 `source` 非 `manifest`（蒸馏器 `self_evaluation`、legacy 导入）→ **不动**：声明是默认值，观察才是结论；
- 调用方不再声明的键 → **不删除**：bootstrap 拥有它声明的键，不拥有"缺席"的含义。

包的 `[memory]` 策略仍是**仅首次导入**生效（用户动过面板之后，`consolidation.json` 是权威）。

### D8 — 升级保留 `workspace/`

`acowork-node` 升级的保留集合：`workspace/` → 保留，其余替换（回归测试 `upgrade_preserves_instance_workspace`）。

### D9 — 不写兼容代码

项目未上线。安装目录里已有的 `*.overrides.json`、`manifest.toml` 里的 `display_name` 一律**手工清理**，代码侧不留读取分支、不留迁移器。已核实：现网安装目录 0 个 `*.overrides.json` 残留，5 个实例的 `agent_config.json` 已带 `name` / `avatar`，7 个出厂包的 `display_name` 行已删除。

---

## 更新时序（写者与顺序）

改名是一个跨进程的事实变更，顺序固定为：

1. **界面** → `PUT /agents/{id}/avatar-config`（body 只带要改的字段，三态：缺省 / `""` 清空 / 有值）；
2. **Runtime**：写 `workspace/config/agent_config.json`（唯一持久写者），随后 `core.set_instance_name(...)`（同一进程内立即生效）；
3. **Runtime** → Node：`republish_installed_info` 重发 retained inventory，携带新的 `identity`；
4. **Gateway**：`upsert_installed_from_node` 用 `identity` 覆盖实例视图 → 列表/详情/PM 投影同时看到新名字；
5. **下次启动**（或同一次启动）：`bootstrap_autobio` 把新 `name` 声明给记忆服务，D7 的收敛逻辑把身份节点原地刷新。

任一步失败都不会出现"两个名字并存"：文件是权威，其余都是它的投影，下一次事件会重放。

---

## 后果

**正面**

- 名字只有一条链：`agent_config.json → AgentCore.instance_name / Node identity 投影 → Gateway → PM / 桌面 → 记忆身份节点`。任意出口都能用"读同一个值"验证一致性。
- 面板的头像与名字与其它设置项同文件、同写路径，少一个文件、少一个路径推导函数。
- `AgentListResponse` / `AgentInfo` 的 `name` + `package_name` 让"同包多实例"在数据层就可区分，不再靠显示名去猜。
- 升级不再可能删掉实例的会话与配置。

**代价 / 已知边界**

- **执行会话没有 `{{agent_name}}` 通道。** `prompts/system.md` 里的正文按原样进入执行会话的 system prompt，只有 Chat 层（Block A）做占位符替换。当前 7 个出厂包在 `system.md` 里只声明**角色**（"a Document Manager"），不写实例名，所以现状自洽；但包作者若在正文写死名字，就会重新制造漂移。约定：**包内容里不写实例名**，需要名字时用 `{{agent_name}}`（Chat 层）或读记忆身份节点。把替换推广到执行会话需要一套"每轮渲染"的接入点，与 block A 的缓存前缀策略耦合，单独一轮处理。
- **`{{agent_name}}` 只覆盖 Chat 层**：群聊的会话级模板同理；二者的替换点都在 `chat_router`，不在执行路径。
- ADR-073 的 §"用户可重命名走 manifest 的 `display_name`"与 `overrides` 文件相关条款自本 ADR 起失效（见下）。

---

## 被取代的旧决策

| 旧决策 | 出处 | 现状 |
|---|---|---|
| 用户重命名走 `manifest.display_name`，取消 instance alias | ADR-073 §1.3 / §重命名流程 | **失效**：manifest 无此字段；重命名写 `agent_config.json` 的 `name` |
| `InstalledAgentInfo` 携带 `overrides_json` | ADR-073 影响范围 · proto | **失效**：改为 `identity: AgentIdentity` |
| `{instance_id}.overrides.json` 作为实例覆盖层 | ADR-073 安装目录布局 | **失效**：文件与模块一并删除 |
| manifest 基础字段含 `display_name` | ADR-104 §字段分层表 | **失效**：字段删除 |
| `POST /admin/bootstrap` 一次性播种 | ADR-101 P1e（写入入口 D） | **修正**：收敛同步（D7） |

`docs/memory-write-entrypoints.md` 的入口 D 行已同步改述。

---

## 验证

- `cargo test`：core 387 / pm 133 / node 210 / runtime 1961 / gateway 全量 / memory 全量，全绿；新增回归：`an_instance_name_is_shared_across_session_clones`、`upgrade_preserves_instance_workspace`、memory `a_rename_converges_the_declared_name`、`a_fact_from_another_source_survives_the_sync`、`an_unchanged_fact_is_not_rewritten`、`a_rename_rewrites_the_identity_fact_in_place`。
- `cargo clippy --all-targets -- -D warnings`：涉及的 6 个 crate 无告警。
- 前端 `tsc --noEmit` 干净，`vitest` 311 文件 / 3070 用例全过，`check-i18n` OK。
- 线上：`pm_list_agents` 10 个实例全部返回 `name`（实例名）+ `package_name`（包名）；`pm_get_project.members` 同构；`pm_list_my_tasks` 的 `assignee_meta.name` = `SSE.Cassie`。
