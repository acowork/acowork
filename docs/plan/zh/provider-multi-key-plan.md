# Provider 多 API Key（多 account）实现计划

- 日期：2026-09-20
- 范围：Gateway（Vault / HTTP / MQTT）、Runtime（凭据解析 / session 持久化）、Desktop（model 选择菜单 / Harness）
- 状态：存储与传输层已完成；Runtime 消费层与前端菜单待做

---

## 1. 背景与问题

现状：一个 provider 只能配一个 API key。给同一个 provider（如 `deepseek`）配了第二个 key 后，第一个被覆盖或无法添加。用户诉求：同一个 provider 支持多个 key，用 alias 区分，待配置列表始终完整（已配置 provider 不从列表移除）。

根因（发现于 review）：存储与传输层已经做到 1:N，但 Runtime 消费层仍是 1:1。

```
MQTT ProviderRef[] → extract_provider_keys (透传 account_id)
  → session_manager.rs:2271  vault.insert(entry.provider_id, entry.api_key)   // HashMap 覆盖
  → build_provider_for(provider_id) → vault.get(provider_id)                  // 单键
```

`provider_key_vault` 是 `Arc<RwLock<HashMap<String, String>>>`（以 provider_id 为键），同一 provider 的多条 entry 互相覆盖，只有最后一条 key 生效。`account_id` 在 runtime 中是死字段（除透传与 test fixture 外零处消费）。

---

## 2. 核心设计决策

### 2.1 三层概念分离

| 概念 | 层级 | 存储 |
|---|---|---|
| Provider | catalog 级（`provider_id` 如 `alibaba-cn`）：base_url / 协议 / 模型元数据 | `provider_list.json` |
| Account | 凭据级（`account_id` 全局唯一 UUID + `alias` 用户可改显示名） | Vault `<provider>__<account_id>.enc` |
| manifest / `.bot` 包 | 纯声明意图，**不含** account 信息 | — |

- `account_id`（UUID，系统生成）承担唯一性与寻址；
- `alias` 无唯一性约束，纯显示层，用户可任意重命名；
- account 是实例化产物（使用痕迹），不进分发包；换环境安装后无意义。

### 2.2 选择策略：用户显式选 account

**不在 runtime 自动选/轮转**，而是由前端 model 选择菜单让用户显式指定 account。理由：可控、无隐式行为。

### 2.3 菜单形态：分组列表 `provider → model → account`

- 模型集合是 **provider 级**、account 不改变模型集合。因此 **account 必须放叶子层**，不能放中间层。
- 若按 `provider → alias → model` 展开，长度 = `accounts × models`（30 key × 10 model = 300 行，且大量重复），是结构性问题，滚动救不了。
- `provider → model → account`：任一屏都是单层列表，长度是求和不是乘积。单 key 用户只有两级（不出现 account 层），观感不变。
- 几十项需配：搜索框、滚动 + sticky 分组标题、alias 重名时显示 key 后 4 位 preview、记住每 provider 的 last-used account。

### 2.4 持久化边界（已确认）

| 文件 | 是否改 | 说明 |
|---|---|---|
| `agent_provider.json`（`AgentProviderConfig`） | **不改** | `providers: Vec<ProviderListItem>` 本就是 provider 级（id/base_url/models/compact_model/custom），无 account 维度；provider/model 只维护一份 |
| session `meta.json`（`SessionMeta`） | **改** | 加 `account_id: Option<String>`（已有 `model`/`provider` 字段，紧邻）。**不存 alias**（可变会 stale，显示时用 account_id 解析） |
| `provider_list.json` | 不改 | provider 级元数据 |
| manifest / `.bot` 包 | 不改 | 不含 account |
| runtime `provider_key_vault` | 不落盘 | MQTT 内存态，多 account 仅影响内存结构 |

**前端三级菜单数据源**：Gateway `GET /api/providers`（`list_providers` 已做 vault account × provider_list 的 1:N join，返回 `provider + account_id + alias + models` 展开行）。**不经过 `agent_provider.json`**。前端只需把 N 行折叠成 `provider → {accounts, models}`，模型去重一份。不需要新接口，也不需要 runtime 参与菜单数据。

---

## 3. 已完成（已落盘，编译 + 单测通过）

### Vault — `core/acowork-gateway/src/vault/mod.rs`
- `ProviderEntry{provider_id, account_id, alias, api_key}`；存储名 `<provider>__<account_id>.enc`
- `add_account` / `get_account` / `list_accounts` / `update_alias` / `remove_account`
- `unlock` 两遍扫描 + `migrate_legacy_entry`（旧格式 → `<provider>__legacy.enc`，alias 默认 `"{provider}-default"`）
- `get_provider` 三级查找；`store_key`/`remove_key`/`list_keys` 保留单 account 兼容语义
- 6 个新测试（multi_account / update_alias / remove_account / 两种 legacy 迁移 / default_alias）

### HTTP API — `core/acowork-gateway/src/http/provider_api.rs`
- `AddProviderRequest` / `UpdateProviderRequest` 加 `keys: Vec<AddProviderKey{alias, key}>`（旧 `key` 字段保留向后兼容）
- `ProviderEntryResponse` 加 `account_id` / `alias`
- `list_providers` 改按 provider × account 1:N 展开

### Proto / Runtime 传输 — `mqtt_payload.proto`、`protocol.rs`、`global_resources_builders.rs`、`mqtt/client.rs`
- `ProviderRef.account_id`（field 8）、`ProviderKeyEntry.account_id`
- Gateway 按 account 1:N 生成多条 `ProviderRef`

### Desktop — `commands/vault.rs`、`gateway_client.rs`、`types.ts`、`AddProviderFlow.tsx`、`HarnessPage.tsx`
- invoke 参数改为 `keys: Vec<{alias, key}>`；`remove_key` 加 `account_id: Option`
- Add / Edit dialog 均支持 alias + key 行数组（`+`/`-`、`max-h-[200px] overflow-y-auto`、上限 256）

**验证状态**：`cargo test -p acowork-gateway --lib` 489/489；clippy 0 警告；Tauri Rust `cargo check` 干净；tsc 干净。

---

## 4. 实现清单（P0 / P1 / P2 全部完成）

### P0 — Runtime 凭据层（根因）✅

| 文件 | 改动 |
|---|---|
| `agent/session/session_manager.rs:2271` | `provider_key_vault`：`HashMap<String,String>` → `HashMap<String, Vec<ProviderKeyEntry>>`（按 provider 分组，保留全部 account） |
| `agent/agent_core.rs:1554` | `get_provider_api_key` 保留（取该 provider 第一个 account）；新增 `get_provider_account_api_key(provider, account_id)` |
| `agent/session_core.rs:695` | `build_provider_for(provider_id, account_id: Option<&str>)`：有 account 精确取 key，无则第一个 |
| `conversation.rs:288` | `SessionMeta` 加 `account_id: Option<String>`；session 加 `set_provider(provider_id, account_id)` |
| `agent/session_config/llm_effects.rs:133` | `set_provider` / `update_provider` 带 account_id |
| `agent/inbound.rs:184` | `ModelSwitchAction` 加 `account_id: Option<String>`（Option 保兼容） |
| `startup/gateway_loop.rs:1221` | 路由透传 account_id 到 `route_model_switch` |
| `model_confirmed` 回执 | payload 带 account_id，前端 session 状态同步 |

解析规则：`aid = Some(x)` → 按 account_id 精确匹配；`None` → 第一个；`Some(x)` 找不到（account 被删）→ 回退第一个 + `tracing::warn`。老 meta（无 account_id）自动回退，兼容。

> 实际落点：`provider_key_vault` 定义在 `core/acowork-runtime/src/agent/agent_core.rs`（`HashMap<String, Vec<ProviderKeyEntry>>`），填充点是 `session_manager.rs` / `session_init.rs`（`entry(provider).or_default().push(..)`，保留全部 account）。key 解析抽成 `session_core.rs` 的自由函数 `resolve_provider_key(accounts: Option<&[ProviderKeyEntry]>, account_id: Option<&str>)`，带 4 个单测（空/命中/未命中回退/None 取第一个）。`account_id` 走 proto `ModelSwitch` field 5 + `SessionConfig` field 11，贯穿 `control_handler → inbound → gateway_loop → SessionConfigDelta → ConversationSession → SessionMeta`，`llm_effects` 新增 `account_changed` 判断触发 provider rebuild。

> P0 完成即可独立测试：配 2 个 key，手动发 `model_switch{account_id=<第二个>}`，验证请求用第二个 key。

### P1 — 前端菜单 ✅

| 文件 | 改动 |
|---|---|
| `src/lib/types.ts:353` | 新增 `ProviderAccount{accountId, alias, preview}`（**未**给 `ModelEntry` 加 accountId —— 模型仍是 provider 级一份） |
| `src/components/chat/ChatPanel.tsx` `loadModels` | `list_keys` 先按 provider 去重（多 account 只 fetch 一次 models API），再构建 `providerAccounts: Record<provider, ProviderAccount[]>` 存入 store |
| `src/components/chat/ChatPanel.tsx` `ModelMenu` | 三级：provider sticky 标题 → 模型行 → （多 account provider 点模型后）原位下钻 account 列表；模型/account 数 > 8 时显示搜索框；列表 `max-h-[240px]` 滚动；account 行 = `alias` + key 尾 4 位 preview；已选 account 打勾 |
| `src/stores/chatStore.ts` | `setCurrentModel(model, provider, agentId, accountId?)`；`model_switch` payload 加 `account_id`；新增 `providerAccounts` state |
| last-used account | 存 `localStorage["acowork:last-account:<provider>"]` —— 纯 UI 记忆，不进 store/session；只显示"上次使用"提示，**不**预选 |
| i18n | 5 个 locale 新增 `modelMenuSearchModel` / `modelMenuSearchAccount` / `modelMenuNoMatch` / `modelMenuLastUsed`、harness `accountsConfigured` |

菜单细节：单 account provider 点模型直接提交（观感与改动前一致）；多 account provider 点模型后原位下钻，返回用标题行的 `‹ provider` 面包屑。

### P2 — 收掉 review 发现的 bug ✅

| 文件 | 改动 |
|---|---|
| `http/provider_api.rs` | **补** `DELETE /api/providers/{provider}/keys/{account_id}` 路由 + `remove_provider_account` handler（调 `vault.remove_account`，只删该 key、保留 provider config，并触发 MQTT 重推）。此前 `gateway_client.rs` 已指向该 URL，缺失会 404 |
| `AddProviderFlow.tsx` | 测试连接改为 before/after `list_keys` diff，只删本次测试新增的 account（原来 `remove_key(provider)` 会清空该 provider 全部 account；catch 分支还会残留测试 key）。成功/失败两条路径都清理 |
| `HarnessPage.tsx` | list 行删除传 `account_id` 删单条；行 key 用 `provider::account_id`（同 provider 多行不再是重复 key）；已配置行显示 alias 徽章 |
| `ProviderPicker.tsx` | 待配置列表**不再过滤**已配置 provider（远程/本地/自定义三组都完整显示，符合"待配置列表始终完整"的要求）；已配置行加 "N keys" 徽章，点击仍可继续追加 key |

---

## 5. 验证

**已跑（全绿）：**

| 命令 | 结果 |
|---|---|
| `cargo test -p acowork-gateway --lib` | 489 passed |
| `cargo test -p acowork-core` | 227 passed（含 `_enc_test` 补齐 `SessionConfig.account_id`） |
| `cargo test -p acowork-runtime` | 1505 passed（含 4 个 `resolve_provider_key` 单测） |
| `cargo clippy --workspace --all-targets` | 0 warning |
| `cargo check`（`apps/acowork-desktop/src-tauri`） | 干净 |
| `npx tsc --noEmit` | 干净 |
| `npx vitest run` | 749 passed / 68 files |
| `npm run check:i18n` | OK（5 locale 占位符一致） |

已知无关失败：`acowork-lsp-relay` 5 个 install/idle_timeout（缺 LSP binary）、`git_api_e2e` 3 个（git 环境）。

**待手测清单：** 配 2 个 key → 菜单出现 account 层 → 分别选两个 account 各发一条消息 → 两边走各自 key（看日志 `api_key_prefix`）→ 重启 runtime，session meta 仍记得 account → 删掉选中的 account 再发消息，行为符合回退预期。

## 6. 不做 / 划界

- `agent_provider.json`、`provider_list.json`、manifest 一律不动
- 不做 account 轮转 / 容错切换（用户显式选，不做 fallback）
- `default_compact_model`（`CompactModelRef={provider_id, model_id}`）**不加** account_id —— 留 TODO，当前蒸馏省略 account 取第一个
- provider 级 alias 唯一性约束不做
- `SearchTab` 编辑 dialog 不支持多 key（YAGNI）

## 7. 风险

- 老 session meta（无 account_id）与老前端 payload（无 account_id）都靠 `Option` + 回退兼容，不会硬失败
- 菜单是本次改动最大的 UI，需回归"单 key 用户观感不变"
- 几十 key 场景下 alias 重名较多，必须靠 key 后 4 位 preview 区分（不显示完整 key）
