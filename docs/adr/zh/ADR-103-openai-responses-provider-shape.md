# ADR-103：支持 OpenAI Responses API 作为 provider shape

- **编号**：ADR-103
- **状态**：📝 草案 v0.2（待架构评审；本文档为讨论稿，未立项实施）
- **日期**：2026-11-XX
- **修订**：**v0.2（2026-11-XX）**——**删除 v0.1 的 D10「本地 shape 覆盖表」**，改为「用户在模型选择 dialog 里显式选协议 + Gateway 侧探测验证」（新 D10）。动因：人工维护的数据文件会腐烂，而探测 + 手选能以零维护成本覆盖 models.dev 尚未标注 `shape` 的厂商（DeepSeek / MiniMax）。连带修改：D1 裁决链剔除探测、D8 裁剪白名单收窄到 1 个字段、D11 协议下拉改为始终可选、§3/§5/§7/§8/§10 同步。
- **决策者**：ACowork 架构组
- **前置文档**：设计 23（`docs/design/zh/23-openai-responses-api.md`，调研报告）——本 ADR **修正并取代**其 §5.2 派生规则与 §5.3 改动清单，其余结论沿用
- **关联**：ADR-012（Per-Session Model/Provider 隔离）、ADR-016（异常处理分层）、ADR-027（会话 token 累计，探测用量须排除）、ADR-034（MQTT/HTTP 边界）、ADR-060（Prompt Cache 友好的上下文块重排）、ADR-066（Provider Cache Tokens）、ADR-055（Node 拓扑）、ADR-085（Agent 生命周期状态机）
- **英文 counterpart**：待本稿定稿后随同一 commit 补 `docs/adr/en/ADR-103-*.md`（依 `docs/adr/en/README.md` 规则）

> ⚠️ 本文档是**讨论稿**。§10 列出必须由人拍板的决策点，未拍板前不进入实施切片。

---

## 摘要

**一句话**：把「用哪个线协议」从 **provider 粒度的静态推导**改成 **(provider, model) 粒度的显式解析**——初值来自 offline_providers.json 的 `provider.shape`，**决定权在用户**（模型选择 dialog 里的协议下拉），**可信度由探测给出**（Gateway 侧真实请求验证）；在此之上新增一个独立的 `ResponsesProvider` 实现 OpenAI Responses API（`POST {base}/responses`），协议无关层（`Provider` trait / `StreamEvent` / `ReliableProvider` / agent loop）零改动；前端对已验证/数据声明支持 Responses 的模型加一个 `R` 标签。

**为什么现在必须改路由粒度**（这是设计 23 没解决的核心问题）：`shape` 在数据里挂在 **model** 上，不是 provider 上。同一个 provider 内部既有 tagged 模型也有 untagged 模型（azure 95 个模型里 27 个 tagged、amazon-bedrock 199 个里 16 个），同一个 provider 甚至可以同时支持两种 shape（DeepSeek 两种都支持；MiniMax 同时支持 Anthropic Messages 和 Responses）。现有 `ProtocolType` 挂在 `ProviderListItem`（provider 级），无法表达这件事。

**推荐方案**：`ProviderModelEntry` 增加可选 `protocol_type` + `base_url`，Runtime 按 (provider, model) 构造 provider；生效协议 = **用户手选 > offline `shape` 初值 > npm 推导**；`shape` 缺失时保持现状（npm 推导 → 默认 OpenAI completions），**绝不因为「Responses 是新协议」就默认切过去**。

**三条塑造全篇的取舍**（否则整份文档会被读偏）：

1. **不依赖 offline 配置，但用它当起点。** `shape` 只覆盖 103/8848 个 provider-model 对（1.2%），DeepSeek / MiniMax **一个都没被标**。v0.1 的解法是人工维护一份覆盖表；v0.2 改为**协议下拉始终可选 + 探测验证**——零维护成本，代价是新用户要自己选一次、点一次测试。
2. **探测结果只影响可信度标记，绝不参与协议裁决。** 否则一次网络抖动就会把用户的 Responses 配置静默切回 completions——那是换了语义还不告诉用户（silent fallback，平台禁止）。见 D10。
3. **前端 `R` 标签表达「支持/已验证」，不是「当前正在用」。** 生效协议在协议下拉里体现，两个字段分开（D11 / Q3）。

---

## 1. 背景

### 1.1 现状：LLM 协议选择链路（实测代码）

| 环节 | 位置 | 事实 |
|---|---|---|
| 协议枚举 | [core/acowork-core/src/protocol.rs:929](../../../core/acowork-core/src/protocol.rs) | `ProtocolType { Anthropic, Google, Ollama, OpenAI }`，`#[default] OpenAI`，`#[serde(rename_all = "lowercase")]` |
| 协议字段挂载点 | `protocol.rs:229`（`ProviderListItem.protocol_type`） | **provider 级**，且字段**没有** `#[serde(default)]` |
| 模型条目 | `protocol.rs:247`（`ProviderModelEntry`） | 只有 `id` / `capabilities` / `max_output_tokens_limit`，**无协议字段** |
| Gateway 派生 | [core/acowork-gateway/src/http/models_api.rs:1511](../../../core/acowork-gateway/src/http/models_api.rs) `derive_protocol_type(npm)` | 纯 npm 子串匹配：`anthropic`/`google`/`ollama` → 对应，其余 → OpenAI |
| model 级 provider 块 | `models_api.rs:1571-1604` `lookup_protocol_info_from_data` | 已经在读 `model.provider.npm` 与 `model.provider.api`，**`shape` 就在同一个对象里，只是没读** |
| 派生时机 | [core/acowork-gateway/src/resource_cache.rs:400](../../../core/acowork-gateway/src/resource_cache.rs) | `build_provider_list_item` 在 **add/update provider 时**派生一次，结果**持久化**进 `provider_list.json`（`resource_cache.rs:5`） |
| 自定义 provider | [core/acowork-gateway/src/http/provider_api.rs:128](../../../core/acowork-gateway/src/http/provider_api.rs) | 注释即结论：「Custom providers always use OpenAI-compatible protocol」——**硬编码** |
| MQTT 契约 | [core/acowork-core/proto/mqtt_payload.proto:339](../../../core/acowork-core/proto/mqtt_payload.proto) | `LlmProtocol` 已用 1–4，`UNSPECIFIED = 0` |
| 跨版本回落 | `protocol.rs:949` `llm_protocol_to_protocol_type` | `_ => ProtocolType::OpenAI`——**未知值静默回落 OpenAI** |
| Runtime 构造 | [core/acowork-runtime/src/providers/router.rs:103](../../../core/acowork-runtime/src/providers/router.rs) | `match protocol_type`；调用点 `agent_core.rs:1104`、`session_core.rs:822` 都传 `provider_meta.protocol_type`（provider 级） |
| OpenAI 实现 | [core/acowork-runtime/src/providers/openai.rs:953](../../../core/acowork-runtime/src/providers/openai.rs) | `format!("{}/chat/completions", self.base_url)`；内置 CompatCache 降级链（`send_with_compat`，`openai.rs:697`） |
| 协议无关层 | `acowork-core/src/providers/traits.rs:819` | `Provider { chat, chat_stream, chat_token_count }`；`StreamEvent` 6 种事件 |
| 工具定义格式 | [core/acowork-runtime/src/agent/context.rs:1592](../../../core/acowork-runtime/src/agent/context.rs) | 产出 **Chat Completions 嵌套格式** `{"type":"function","function":{...}}`，MCP 工具名已下划线化（`mcp_github_list_issues`） |

> 设计 23 的结论仍然成立：**接入 Responses 是「新增一个 Provider 实现 + 打通协议选择链路」，与当初新增 Anthropic provider 同模式**。本 ADR 不推翻这个判断，只推翻它的**路由粒度**和**派生规则**。

### 1.2 新事实：`offline_providers.json` 的 `provider.shape`

数据源：[assets/offline_providers.json](../../../assets/offline_providers.json)（6.1 MB，230 providers / 8848 provider-model 对）。

字段位置：`providers.{pid}.models.{mid}.provider.shape`，取值 `"responses"` | `"completions"`。同对象还可能带 `api`（该 shape 的替代 base URL）与 `npm`。

**覆盖率极低且高度倾斜（实测）**：

| provider | npm | provider 级 `api` | 模型总数 | tagged | shape 分布 |
|---|---|---|---|---|---|
| azure | `@ai-sdk/azure` | — | 95 | 27 | `completions` ×27 |
| azure-cognitive-services | `@ai-sdk/azure` | — | 95 | 27 | `completions` ×27 |
| neon | `@ai-sdk/openai-compatible` | `${NEON_AI_GATEWAY_BASE_URL}/v1` | 57 | 20 | `responses` ×20 |
| amazon-bedrock | `@ai-sdk/amazon-bedrock` | — | 199 | 16 | `responses` ×16 |
| aixy | `@ai-sdk/openai-compatible` | `https://api.aixy-gateway.com/v1` | 13 | 7 | `responses` ×7 |
| sakana | `@ai-sdk/openai-compatible` | `https://api.sakana.ai/v1` | 4 | 3 | `responses` ×3 |
| infer | `@ai-sdk/openai` | `https://infer.flow7.org/v1` | 2 | 2 | `responses` ×2 |
| zenifra | `@ai-sdk/openai-compatible` | `https://ai.zenifra.com/v1` | 1 | 1 | `completions` ×1 |

合计 **103 / 8848 对（1.2%），8 / 230 个 provider**。`shape` 对象键组合：`{shape}` 58 例、`{api, npm, shape}` 43 例、`{npm, shape}` 2 例。

**三条从数据里直接读出来的硬事实**（比任何文档措辞都权威）：

1. **`openai` provider 自身没有任何 shape 标记**（51 个模型全部 untagged）。所以「有 shape 才走 Responses」的默认策略下，官方 OpenAI 的 gpt-5.x 用户**行为不变**——这是好事（见 D1）。
2. **`shape` 与 npm 推导会直接冲突**：`aixy` / `neon` / `sakana` 的 npm 是 `@ai-sdk/openai-compatible`（按现状推导 → Chat Completions），但 shape 标的是 `responses`。反过来 `azure` 系 npm 是 `@ai-sdk/azure`，shape 明确标 `completions`。
3. **shape 缺失 ≠ 不支持 Responses，shape 存在 ≠ 该 provider 只有这一种 shape**。DeepSeek（npm `@ai-sdk/openai-compatible`，`api=https://api.deepseek.com`，4 个模型）**无 shape 标记**，但它实际上两种都支持；MiniMax（npm `@ai-sdk/anthropic`，`api=https://api.minimax.io/anthropic/v1`，8 个模型）同样无标记，而它官方文档把 `/v1/responses` 列为**主接口**。

> 事实 3 是本 ADR 与用户参考文档之间最大的张力：**光靠 `shape` 字段，DeepSeek 和 MiniMax 一个都接不上**。v0.1 的解法是人工维护一份覆盖表，v0.2 改为「协议下拉始终可选 + Gateway 侧探测验证」（D10）——用一次性交互成本换掉一份会腐烂的数据文件。

### 1.3 对设计 23 的修正清单

| # | 设计 23 的说法 | 实测/文档证据 | 本 ADR 的修正 |
|---|---|---|---|
| C1 | §5.2 白名单：`openai` / `azure` / `azure-cognitive-services` → Responses | 数据里 `openai` 无 shape；`azure` / `azure-cognitive-services` 明确标 **`completions`** | **删除硬编码白名单**，改为「读 `shape` 字段」；设计 23 的白名单方向是**反的** |
| C2 | §5 全套改动基于 provider 级 `ProtocolType` | shape 挂在 model 上；同 provider 内 tagged/untagged 混合；同 provider 可双 shape | **协议解析下沉到 (provider, model)**（D2） |
| C3 | §5.3 注：「Responses 是 OpenAI 一等协议，退化风险低，建议不为其做 compat 链」 | MiniMax：`reasoning.effort: none` → **HTTP 400**（M3.1-Flash）；`tool_choice` 只接受 `auto`；`text.format` 只有 `text`。DeepSeek：`truncation` 不支持、超窗 → **400** | **不能一概而论**：第三方 Responses 实现的裁剪面比 OpenAI 官方小得多。改为「显式错误分类 + 最小裁剪白名单」，并列为待决策点（D8 / Q4） |
| C4 | §6.2 「structured outputs 直接惠及记忆提取」 | MiniMax Responses 的 `text.format.type` 枚举**只有 `text`**；DeepSeek 支持 `json_schema`；OpenAI 支持 | structured outputs 是**逐厂商能力**，不得作为 Responses 的隐含前提（§8 R5） |
| C5 | §5.2 只提 `derive_protocol_type` | `provider.api` 覆盖已存在但**不展开 `${VAR}` 占位符**（实测 `neon` = `${NEON_AI_GATEWAY_BASE_URL}/v1`，bedrock 模型级 api 含 `${AWS_REGION}`） | 带占位符的 `api` **必须 gate 掉**，否则 base_url 直接不可用（D7） |
| C6 | §5.5 「回滚：把 provider 切回 Chat Completions 即回滚，无需改代码」 | `protocol_type` 在 add/update 时**冻结进 `provider_list.json`**；offline 数据刷新不会回写已有行 | 回滚需要一条**显式重解析路径**（D1 的 override + §5 迁移），不是「改配置即可」 |

### 1.4 供应商差异（DeepSeek / MiniMax 官方文档实测摘要）

来源：DeepSeek `POST /responses` + 「使用 Responses API」指南（含完整兼容性明细表）、MiniMax `POST /v1/responses` OpenAPI（`openapi-responses.json`）、MiniMax `POST /v1/responses/input_tokens`。

**两家共同点（对 ACowork 有利）**：

- **无状态**：`previous_response_id` / `conversation` / `store` 全部不支持（DeepSeek 明确「无状态 API」，响应里 `store` 恒 `false`）→ 与 ACowork 自管历史完全一致，设计 23 §1.3 的非目标成立。
- **不支持的参数被静默忽略**（DeepSeek 明文：「不支持的参数会被静默忽略、不会报错」）。→ **不能靠「试错」探测能力**，也不能指望 400 报错来驱动降级（影响 D8）。
- **SSE 无 `data: [DONE]`**，终止事件是 `response.completed` / `response.incomplete` / `response.failed`。两家一致。
- **事件 `type` 在 JSON data 里**（MiniMax 明文：「每个事件的 `data` 为 JSON，`type` 字段标识事件类型」），不能只依赖 SSE `event:` 行。
- **推理增量事件名是 `response.reasoning_text.delta`**（两家一致），而 OpenAI 官方是 `response.reasoning_summary_text.delta` → 解析必须同时接受两种。
- **function 工具是扁平结构** `{"type":"function","name","description","parameters"}`（MiniMax schema `Tool.required = [type, name]`），与 ACowork 现在产出的嵌套格式不同 → 必须转换。
- **usage 结构一致**：`input_tokens` / `output_tokens` / `total_tokens` + `input_tokens_details.cached_tokens` + `output_tokens_details.reasoning_tokens` → 可直接映射到 ADR-066 的 `UsageInfo`。

**两家差异（必须逐厂商对待，不能当成「Responses 通用行为」）**：

| 维度 | OpenAI 官方 | DeepSeek | MiniMax |
|---|---|---|---|
| base / 端点 | `https://api.openai.com/v1/responses` | `https://api.deepseek.com/responses`（**无 `/v1`**） | `https://api.minimax.cn(/.io)/v1/responses` |
| `input` 必填 | `input` 与 `instructions` 至少一个 | 至少一个 | **`input` required**（schema 层） |
| `instructions` 语义 | system | 「作为第一条 system 消息」；`developer` 角色**视同 `user`** | 系统指令 |
| `reasoning.effort` | `minimal/low/medium/high`（+`xhigh`） | `none/low/high/max`；`minimal→low`、`medium/xhigh→high` | `minimal/low/medium/high/xhigh/max/none`，但 **M3.1-Flash 传 `none` → 400**；M2.x 传 `none` 被接收但不生效 |
| `text.format` | `text` / `json_object` / `json_schema` | 三者完整支持 | **只有 `text`** |
| `tool_choice` | `none/auto/required/{name}` | 全支持 | **只有 `auto`** |
| `temperature` | 支持 | 支持（思考模式不生效） | **接受但不生效**（固定 1） |
| `max_output_tokens` | 支持 | 支持 | 支持，**含思维链**；过小 → `status=incomplete` 且 `output` 无 `message` 项 |
| 图片输入 | `input_image` | `input_image`，仅允许在 `user`/`developer` 消息与 `*_output` 里；`system`/`assistant` 带图 → **400** | `input_image` + **`input_video`**（M3 多模态） |
| token 估算 | 无原生端点 | 无 | **`POST /v1/responses/input_tokens`** |
| prompt cache | `prompt_cache_key` | **不支持**（自动 KV 缓存） | 支持 `prompt_cache_key` |
| 工具名约束 | — | `^[a-zA-Z0-9_-]+$`、≤128、唯一；`custom` 工具只允许 `apply_patch`，其他名 → 400 | 仅 `function` |
| 思维链载体 | `summary`（摘要） | `reasoning` item 的 `content[].reasoning_text`（**明文 CoT**） | `content[].reasoning_text`（明文）+ `summary`（可为空数组） |

> 结论：**「Responses API」不是一个协议，是一个协议族**。ACowork 的实现必须以 OpenAI 官方语义为基准、把差异收敛到「错误分类 + 少量 per-provider 裁剪」，而不是假装三家一致。

---

## 2. 决策

### D1 — 生效协议由三级裁决；`shape` 只供初值，探测不参与裁决

协议在 **(provider, model)** 粒度解析，优先级从高到低：

```
1. 用户显式选择   ProviderModelEntry.protocol_type（模型选择 dialog 的协议下拉，含 custom provider）
2. offline shape  providers.{pid}.models.{mid}.provider.shape   ← 新增读取，仅作初值
3. npm 推导       derive_protocol_type(npm) → 默认 ProtocolType::OpenAI（现状行为，不变）

（旁路）探测结果（D10）—— 只写 verified / failed / unverified 状态标记，
        MUST NOT 进入上述三级裁决，MUST NOT 改变生效协议
```

**为什么探测必须被排除在裁决链之外**（v0.2 的核心护栏）：若「生效协议 = 最近一次探测是否成功」，则网络抖动 / 上游 5xx / key 轮换会让系统把用户的 Responses 配置**静默切回 completions**——那是换了一套请求语义（工具定义扁平 vs 嵌套、`instructions` vs `messages`、`max_output_tokens` 含思维链）却不告知用户。这既是平台禁止的 silent fallback，也是不可复现的行为：同一份配置在两个时刻会走两条不同链路。

因此本 ADR 的立场是：**配置决定行为，探测只决定可信度**。探测失败时行为不变，只有 UI 徽标和警告变。

**MUST**：`shape` 只在与 npm 推导**同族**时才覆盖 npm（见下）；`shape` 值不在 `{responses, completions}` 枚举内时 **MUST 忽略并回落第 3 级**，MUST NOT panic、MUST NOT 把未知值当成 Responses。

**shape → 协议的映射表**（`completions` 的含义是「Chat Completions 线格式」，它**不改变** Anthropic/Google/Ollama 这些非 OpenAI 家族的判定）：

| npm 推导结果 | shape | 解析结果 | 说明 |
|---|---|---|---|
| OpenAI | `responses` | **`Responses`** | 唯一的升级路径（aixy / neon / sakana / infer） |
| OpenAI | `completions` | `OpenAI` | 显式确认现状（zenifra） |
| OpenAI | 缺失 | `OpenAI` | 现状不变（openai / deepseek / siliconflow…） |
| Anthropic | `responses` | `Anthropic` + **告警** | 跨家族冲突（minimax 就是这一类）。MUST NOT 静默改判；由用户在下拉里显式选 Responses + 探测验证解决（D10 / D11） |
| Anthropic | `completions` | `Anthropic` + 告警 | 同上，数据脏 |
| Azure(`@ai-sdk/azure`→现状推导 OpenAI) | `completions` | `OpenAI` | azure 系现状即走 `{base}/chat/completions`，数据与实现一致 |
| Google / Ollama | 任意 | 不变 | Responses 不适用于这两个家族（v1 范围外） |

**理由**：`shape` 是数据、白名单是代码；把「哪家支持 Responses」写进 Rust 源码，等于每次 models.dev 更新都要发版。设计 23 C1 的白名单方向已被数据证伪。v0.2 进一步把「数据没标怎么办」从人工文件改成用户手选 + 探测，同理：**不新增需要发版才能修正的静态事实**。

**默认行为（回答「shape 缺失时怎么办」）**：**缺失 = 维持现状 = npm 推导 = 绝大多数情况 Chat Completions**。这是本 ADR 最重要的安全阀——它保证 8848 个 provider-model 对里 8745 个（98.8%）的行为**逐字节不变**，官方 `openai` provider 也在其中。用户想升级链路，必须显式做一次动作（选协议 + 探测），这个动作本身就是知情同意。

### D2 — 协议解析下沉到 (provider, model) 粒度

数据模型改动（[core/acowork-core/src/protocol.rs:247](../../../core/acowork-core/src/protocol.rs)）：

```rust
pub struct ProviderModelEntry {
    pub id: String,
    pub capabilities: ModelCapabilitiesInfo,
    #[serde(default = "default_max_output_tokens_limit")]
    pub max_output_tokens_limit: u64,
    /// Effective protocol for this model. `None` = inherit
    /// `ProviderListItem.protocol_type`. Written ONLY by an explicit user
    /// action (protocol dropdown / REST), never by a probe result (D1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_type: Option<ProtocolType>,
    /// Per-model base URL override, used when the model's Responses endpoint
    /// differs from the provider default (e.g. MiniMax: provider base is
    /// `/anthropic/v1`, Responses needs `/v1`). Also written by explicit user
    /// action, and offered as an auto-fill by a successful probe (D10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Probe verdict for `protocol_type` (D10). Advisory only — affects the
    /// `R` badge and warnings, never which provider gets constructed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_probe: Option<ProtocolProbe>,
}
```

**写入来源只有三个**：① 用户在协议下拉里选（D11）；② REST `PUT /api/providers/{id}` 带 `protocol` 字段（供 CLI / 自动化）；③ D1 第 2/3 级算出的**初值**（写盘时固化，之后不随 offline 数据刷新自动改写，见 §4.2）。v0.1 的「本地覆盖表」这条来源已删除。

同时 **MUST** 给 `ProviderListItem.protocol_type` 补 `#[serde(default)]`（现状缺失，见 §1.1 表格）——否则新 Gateway 写出 `"responses"` 后，旧版本读 `provider_list.json` 会**反序列化硬失败**而不是回落。这是跨版本兼容的前提，不是可选项。

Runtime 侧：`build_provider_for(provider_id, model_id, account_id)` 增加 `model_id` 入参，按 `model.protocol_type.or(provider.protocol_type)` 选构造分支；`agent_core.rs:1104` / `session_core.rs:822` / `agent_init.rs:739` 三个调用点同步传模型。找不到 model 条目时**MUST** 回落 provider 级并 `warn!`，MUST NOT 猜测。

**被否决的替代方案**：
- ❌ *把 provider 拆成两行*（`openai` + `openai-responses`）：用户会看到重复 provider、重复 key 配置、重复 logo，且与 ADR-012 的 per-session 模型隔离语义冲突。
- ❌ *provider 级加 `supports_responses: bool` 开关*：无法表达 azure 系「同 provider 内 tagged/untagged 混合」的事实（27/95、16/199）。

### D3 — 新增 `ProtocolType::Responses` 与 `LlmProtocol::Responses = 5`

| 位置 | 改动 |
|---|---|
| `acowork-core/src/protocol.rs:929` | 新增变体 `Responses`，`#[serde(alias = "openai-responses")]`；`FromStr` 接受 `"responses"` |
| `acowork-core/proto/mqtt_payload.proto:339` | `LLM_PROTOCOL_RESPONSES = 5;`（1–4 语义不变） |
| `protocol.rs:949` `llm_protocol_to_protocol_type` | 增加 `Responses => ProtocolType::Responses`；**`_ => OpenAI` 的静默回落 MUST 加 `warn!`**（见 §8 R1） |
| `global_resources_builders.rs:461` `map_protocol_type` | 增加 Responses 分支 |

**跨版本互通结论**：新增枚举值本身向后兼容（不改 1–4），但**回落方向不安全**——旧 Runtime 收到 `5` 会静默当 OpenAI completions，把 Responses-only 模型打到 `/chat/completions` 上，得到 400 而不是「协议不支持」。因此 **MUST 按 §4 的升级顺序发布**（Runtime/Node 先、Gateway 后），并把该回落从静默改为可见。

### D4 — 独立的 `ResponsesProvider`，不塞进 `openai.rs`

新增 `core/acowork-runtime/src/providers/responses.rs`，实现 `Provider` trait 三方法；`router.rs:103` 增加 `ProtocolType::Responses =>` 分支。

**复用**：`openai.rs` 的 reqwest 客户端构造、超时注入、`from_http_parts` 错误解析、SSE 帧切分工具。
**不复用**：`NativeChatRequest` / `NativeChatResponse` / CompatCache 挂载（`install_wiring` 只对 completions provider 生效）。

**MUST NOT** 改动：`Provider` trait、`StreamEvent`、`ReliableProvider`、`loop_llm.rs`、记忆/工具注册/预算/限流。协议无关层零改动是本 ADR 的核心约束，也是「与新增 Anthropic provider 同模式」成立的依据。

**理由**：`openai.rs` 已 ~1900 行且内置写死在 `/chat/completions` 的 CompatCache 键与降级链；两套线格式同存一个 struct 会让 cache key、URL、降级归属全部变含糊（设计 23 方案 B 已否决，本 ADR 维持）。

### D5 — 请求映射（域 → Responses 线格式）

| ACowork 域（`ChatRequest`） | Responses 线格式 | 规则 |
|---|---|---|
| `messages[role=system]` | 顶层 `instructions: string` | **合并**所有 system 消息，`\n\n` 连接。ADR-060 的「稳定前缀」语义由此保持：instructions 是稳定段，历史是追加段 |
| `messages[role=developer]` | `instructions` 追加 **或** `input` 里 `role=developer` | v1 归并进 `instructions`（DeepSeek 把 `developer` 视同 `user`，归并到 instructions 语义更稳） |
| `messages[role=user/assistant]` | `input[]` 的 `message` item | content 为 parts 数组 |
| 文本 part | `input_text` / `output_text` | 按角色选类型 |
| `image_url`（data URI） | `input_image{image_url, detail}` | **MUST** 复用 `traits.rs` 的 `parse_data_uri`；MUST NOT 把 URL 字符串塞进 base64 字段。**`assistant` 消息里的图片 MUST 丢弃并 warn**（DeepSeek 对 `system`/`assistant` 带图返回 400） |
| `assistant.tool_calls[]` | 独立 `function_call` item（`call_id`/`name`/`arguments`） | 一条 assistant 消息可产出 1 个 `message` + N 个 `function_call` item |
| `role=tool` + `tool_call_id` | `function_call_output{call_id, output}` | `output` 用字符串；工具产图时用内容块数组（DeepSeek/MiniMax 均支持在 `*_output` 里放 `input_image`） |
| `tools`（嵌套 function） | **扁平** `{type:"function", name, description, parameters}` | 必须做嵌套→扁平转换（`context.rs:1592` 产出的是嵌套格式） |
| `max_tokens` | `max_output_tokens` | **语义差异**：Responses 的该字段**含思维链 token**（MiniMax 明文）。过小 → `status=incomplete` 且 `output` 无 `message`，MUST 映射为可见截断错误而非空回答 |
| `reasoning_effort` | `reasoning.effort` | `Auto` → **不发该字段**；`Off` → `none`（**注意 MiniMax M3.1-Flash 会 400**，见 D8）；`Low/Medium/High/Max` → 同名，由 provider 侧按各自枚举裁剪（DeepSeek 把 `medium`→`high`） |
| `temperature` | `temperature` | 透传；MiniMax 接受但不生效，不做客户端补偿 |
| `thinking_mode` | **不发** | Anthropic 专属，Responses 无对应概念 |
| — | `stream` | 由 `chat` / `chat_stream` 决定 |
| — | `store:false` / 不发 `previous_response_id` / `conversation` / `background` / `include` / `truncation` | **YAGNI，v1 明确不发**（与 §1.3 非目标一致） |

**MUST**：`input` 永不为空（MiniMax schema 层 required）。若上游只给了 instructions 而无任何用户消息，MUST 报错而不是发空 `input`。

### D6 — 响应与流式映射（Responses → `StreamEvent`）

**非流式 `chat()`**：遍历 `output[]` → `message.content[].output_text` 拼 `content`；`reasoning.content[].reasoning_text` 拼 `reasoning_content`；`function_call` item → `tool_calls[]`。`status` → `finish_reason`：

| `status` / `incomplete_details.reason` | `finish_reason` | 处理 |
|---|---|---|
| `completed` | `stop`（无工具）/ `tool_calls`（有 `function_call`） | 正常 |
| `incomplete` + `max_output_tokens` | `length` | **MUST 可见告警**：思维链吃满预算导致无正文 |
| `incomplete` + `content_filter` | `content_filter` | 透传给前端 |
| `failed` + `error{code,message}` | `error` | 走 `from_http_parts` 同族错误分类（ADR-016） |

**流式 `chat_stream()`**：

```mermaid
flowchart LR
    A["SSE data 帧"] --> B{"JSON.type"}
    B -->|"response.output_text.delta"| C["StreamEvent::Content"]
    B -->|"response.reasoning_text.delta<br/>response.reasoning_summary_text.delta"| D["StreamEvent::ReasoningContent"]
    B -->|"response.output_item.added<br/>item.type=function_call"| E["StreamEvent::ToolCallStart"]
    B -->|"response.function_call_arguments.delta"| F["StreamEvent::ToolCallChunk"]
    B -->|"response.completed / .incomplete"| G["StreamEvent::Finished(ChatResponse)"]
    B -->|"response.failed"| H["StreamEvent::Error(StreamError)"]
    B -->|"其他已知噪声事件"| I["忽略（debug 日志）"]
    B -->|"未知事件"| J["warn! 一次/流，不失败"]
```

硬性要求：

1. **终止判定只看 `response.completed` / `.incomplete` / `.failed`**，MUST NOT 等 `data: [DONE]`（两家都不发）。流结束但未收到终止事件 → **MUST** 产出 `StreamEvent::Error`（截断），MUST NOT 发 `Finished` 假装成功。
2. **事件类型从 JSON `type` 字段取**，SSE `event:` 行仅作可选校验（MiniMax 明确 type 在 data 里）。
3. **reasoning 事件名接受两种别名**：`response.reasoning_text.delta/done`（DeepSeek/MiniMax）与 `response.reasoning_summary_text.delta`（OpenAI 官方）。
4. **`function_call` 的 `call_id` / `name` 来自 `response.output_item.added` 的 item**，arguments 才走 delta → `ToolCallStart` 必须在 `added` 时发出（此时 arguments 为空串），后续 `ToolCallChunk{index}` 按 `output_index` 分配下标，与现有 agent loop 的 index 语义对齐。
5. **usage 只在终止事件的完整 `response` 对象里** → `Finished(ChatResponse)` 的 `UsageInfo` 从 `response.usage` 映射：`input_tokens→prompt_tokens`、`output_tokens→completion_tokens`、`input_tokens_details.cached_tokens→cache_read_tokens`（ADR-066）、`output_tokens_details.reasoning_tokens→reasoning_tokens`。**MUST** 复用 `openai.rs:277` 已有的双命名容忍策略（`prompt_tokens.or(input_tokens)`）。
6. **`sequence_number` 不保证连续语义**，v1 MUST NOT 用它做乱序重排或去重。
7. 事件面约 40 种，v1 实现约 10 种（上图）。`response.content_part.*`、`response.output_item.done` 里的完整 item **MUST 解析但只用于校验**，MUST NOT 二次 emit 文本（否则前端重复渲染）。

> D6 的第 1 条（终止判定与不重试）与第 3 条（reasoning 明文 vs 摘要的呈现）是**待拍板项**，见 Q7 / Q8；其余条目为架构约束，不随评审改变。

### D7 — URL 解析与 base_url 覆盖

`ResponsesProvider` 端点：`{base_url}/responses`（与 `openai.rs:953` 的 `{base_url}/chat/completions` 同构）。

**base_url 解析顺序**：用户显式 `base_url` override > **model 级 `provider.api`** > provider 级 `api` > 内置默认。

三条必须处理的现实：

1. **`/v1` 不一致**：DeepSeek provider `api = https://api.deepseek.com`（→ `/responses` 正确），MiniMax Responses 需要 `https://api.minimax.cn/v1`（而它的 provider `api` 是 `.../anthropic/v1`）。→ **MUST** 支持 model 级 `base_url` 覆盖（D2 的字段正是为此存在），MUST NOT 假设「provider base + /responses」万能。
2. **`${VAR}` 占位符**：`neon` = `${NEON_AI_GATEWAY_BASE_URL}/v1`，bedrock 模型级 api 含 `${AWS_REGION}`。现状代码**不展开**（实测 `models_api.rs` 只做 `as_str()` 直传）。→ **MUST** 在解析处检测 `${`，未展开则**判定该 override 不可用**、回落上一级并 `warn!`。v1 **MUST NOT** 引入 env 展开（会把 Gateway 的 provider 配置变成隐式依赖进程环境，越界；列为 Q6）。
3. **CompatCache 键隔离**：`provider_id` 命名空间 MUST 含 shape，避免 completions 与 responses 两条链路共用一个 key（现状 `openai.rs:117` 用 host 兜底，同 host 两协议会撞键）。

### D8 — 错误处理：显式分类 + 最小裁剪白名单，不做静默跨 shape 降级

**分层归属（ADR-016）**：错误分类在 core、重试编排在 `ReliableProvider`、呈现由前端负责。`ResponsesProvider` 只负责把 HTTP/SSE 错误变成结构化 `StreamError`。

**MUST NOT**：在 provider 内部把 Responses 请求自动重发到 `/chat/completions`。理由：(a) 两协议的 `tools` / `reasoning` / 历史结构不同形，自动换端点等于「换个语义再试一次」，失败时无法归因；(b) DeepSeek 明文「不支持的参数静默忽略」，探测不可靠；(c) 与「无静默回落」的平台原则冲突（AGENTS.md：silent fallback 是反模式）。

**MUST** 实现的错误分类与处置：

| 信号 | 分类 | 处置 |
|---|---|---|
| `404` 且 body 提示 `responses` / unknown endpoint | `EndpointUnavailable` | **不重试**；错误文案明确「该 provider 不支持 Responses API，请在模型设置里把协议切回 Chat Completions」；打 `warn!` 含 provider/model |
| `400` 且 body 指名某字段（`reasoning`/`tool_choice`/`text.format`/`temperature`） | `UnsupportedField{field}` | 进入 **D8 裁剪白名单**（下） |
| `400` 且 body 是内容完整性问题（图片位置、`input` 为空、工具名非法、超窗） | `ContentIntegrity` | **不裁剪、不重试**，直接上抛（对齐 `openai.rs` 2026-09-05 事故教训：`ErrorClass::is_learnable()` 门控） |
| `401/403` | `Auth` | 不重试，走既有 key 失效路径 |
| `429` | `RateLimited` | 交给 `ReliableProvider`（含 429 UX），协议无关 |
| `5xx` | `Transient` | 交给 `ReliableProvider` 退避 |
| SSE `response.failed` | `InBandFailure{code,message}` | → `StreamEvent::Error`，MUST NOT 当空回答 |
| 流断开且无终止事件 | `TruncatedStream` | → `StreamEvent::Error`，不重试（可能已产生副作用） |

**裁剪白名单（ResponsesCompat，v0.2 收窄到 1 个字段）**：与 `CompatCache` 同纪律（**两次确认才固化**、TTL 租约、plain success 重置、错误分类门控、级别必须可见告警），但 v1 **只允许裁剪 `reasoning` 一个字段**，且**不复用 `openai.rs` 的 `StripProfile` 结构**（它是 completions 形状）：

- `reasoning.effort: none` 触发 400 → 省略 `reasoning` 整块重试一次。选它的理由：这是唯一「不裁就完全不可用」的场景（MiniMax M3.1-Flash 明文 400），且裁剪后**语义损失可判定**——用户选的 effort 档位被降级，必须可见告警 + 记入探测状态。

**v1 明确不裁**（改为分类上抛，让用户在 UI 里看到并调整）：

| 字段 | 不裁的理由 |
|---|---|
| `tool_choice` / `text.format` / `temperature` / `top_p` | 裁剪它们会**静默改变模型行为**（强制 `auto`、丢掉结构化输出、改掉采样），而这三项在 MiniMax 上的表现是「接受但不生效」而非报错——客户端补偿只会掩盖差异，不会消除（§1.4 差异表才是正解） |
| `tools` | 工具是 agent 的能力面，剥掉等于静默关闭 function calling——正是 `compat.rs` 头部记录的 2026-09-05 事故 |

**明确不做**：不做 4 步链式盲试；不做「试到 200 为止」。裁剪命中后 **MUST** `warn!`、在 debug panel 可见，并把该 provider-model 标记为 `ProbeState::Degraded`（D10），让 R 标签能表达「能通，但被裁过」。

> 是否连 `reasoning` 也不裁（v1 纯「分类 + 上抛」），仍列为**待决策点 Q4**。

### D9 — 自动跨 shape failover：v1 不做

一个模型同时支持两种 shape（DeepSeek、MiniMax）时，是否「Responses 失败 → 自动用 completions 重试」？**v1 不做**。改为提供**显式可切换的 per-model 协议选择**（D1 第 1 级），把选择权和责任交给人。列为 Q2 讨论。

### D10 — 协议探测：Gateway 侧真实请求，结果只作可信度标记

**动机（v0.2 的立身之本）**：`shape` 只覆盖 1.2% 的数据，DeepSeek / MiniMax 恰好都不在里面（§1.2 事实 3）。v0.1 用一份人工维护的覆盖表补这个洞；v0.2 改为**让用户在 dialog 里显式选协议，然后用一次真实请求验证它通不通**。这样做的直接收益：

- **零维护成本**：不新增需要 owner、会腐烂、会与 models.dev 漂移的数据文件。
- **事实来自上游而不是我们的猜测**：探测打的是真端点，覆盖表写的是「我们以为」。
- **顺带解决 custom provider**：`provider_api.rs:128` 现在硬编码「custom 一律 OpenAI」，自定义网关的协议本来就只能实测。

**已有先例（不是新机制）**：

| 能力 | 位置 | 可复用点 |
|---|---|---|
| `POST /api/models/discover` | [core/acowork-gateway/src/http/models_api.rs:150](../../../core/acowork-gateway/src/http/models_api.rs) | 带 `base_url` + 明文 key、Gateway 发出站请求、5s 超时、结构化错误串 |
| `POST /api/embedding-models/test` | [core/acowork-gateway/src/http/embedding_api.rs:563](../../../core/acowork-gateway/src/http/embedding_api.rs) | 返回形态 `{success, model_id, latency_ms, error}` |
| dialog 内「探测再保存」 | [apps/acowork-desktop/src/components/harness/AddProviderFlow.tsx:334](../../../apps/acowork-desktop/src/components/harness/AddProviderFlow.tsx)（状态区 `:723`） | 交互位置与状态展示直接沿用 |

#### D10.1 执行位置：MUST 在 Gateway，前端只发起

不是风格问题，是边界问题：provider **保存之后**明文 key 只存在于 Vault，经 MQTT 下发给 Runtime，前端拿不到。因此探测端点由 Gateway 自己从 Vault 取 key：

```
POST /api/providers/{id}/probe
  { "model": "deepseek-flash", "protocol": "responses", "tier": "standard" | "deep" }
→ { "success": bool, "tier_passed": ["L1","L3"], "latency_ms": 812,
    "observed_base_url": "https://api.deepseek.com",
    "usage": {"input_tokens": 9, "output_tokens": 1},
    "error": {"class": "EndpointUnavailable", "message": "..."} | null,
    "stripped_fields": ["reasoning"] }
```

**MUST NOT** 让前端直连上游（CORS + key 暴露 + Desktop 是纯展示层，AGENTS.md 红线）。**MUST NOT** 让 Runtime 承担探测（探测是配置期行为，不是会话期行为；Runtime 只在被派单时活着）。

#### D10.2 探测分档：L1+L3 是「敢亮 R 标签」的最低线

DeepSeek 官方明文：「不支持的参数会被**静默忽略**、不会报错。」所以单次 ping 能验的是**端点级**事实，验不了**能力级**子集（MiniMax 接受 `/responses` 但 `text.format` 只有 `text`、`tool_choice` 只有 `auto`、`effort:none` → 400）。一次全绿的 ping 探测，真跑 agent 时照样炸。

| 档 | 请求 | 断言 | 结论用途 |
|---|---|---|---|
| **L1** | `POST {base}/responses`，`input:"ping"`、`max_output_tokens:1`、`stream:false` | HTTP 2xx 且 body `object == "response"` | 端点存在 + 鉴权通过 + 请求被接受 |
| **L2** | 复用 L1 响应 | `status`、`usage.input_tokens` / `output_tokens` 存在且能映射到 `UsageInfo` | 用量核算不会瞎（ADR-066） |
| **L3** | 带 1 个 no-op 工具 + `tool_choice` 倾向调用 | 产出 `function_call` item、`call_id` 可回环、`arguments` 是合法 JSON 串 | **工具循环可用** |
| L4 | `stream:true` 跑 L1 | 收到 `response.completed/.incomplete/.failed` 之一；无 `[DONE]` 也能正常收尾 | 流式可用 |

**定案（本轮讨论确认）**：v1 实现 **L1 + L2 + L3**，L4 列为可选后续。理由：ACowork 的 provider 存在的唯一目的是跑 agent 工具循环，不含工具往返的探测**没有决策价值**——它给绿灯，用户就会信，然后在真实任务里炸。L4 与 L1 高度重叠（同一条请求加 `stream:true`），若实现则合并进 L1 的执行路径，不单列一档（避免探测矩阵膨胀）。

**诚实标注**：探测通过 = 「这三件事在这一个模型上此刻成立」，**不等于**「该 provider 的 Responses 实现完整」。UI 文案 MUST NOT 写「已验证兼容」，应写「已验证可用（文本 + 工具调用）」。

#### D10.3 结果语义：advisory，绝不参与裁决

```mermaid
flowchart LR
    U["用户在下拉里选 Responses"] --> W["写入 protocol_type<br/>（生效，立即）"]
    P["探测结果<br/>verified / degraded / failed / unverified"] --> B["R 标签 + 警告条<br/>（仅展示）"]
    P -.->|"MUST NOT"| W
    W --> R["Runtime 构造 ResponsesProvider"]
```

| 探测结果 | 生效协议 | UI |
|---|---|---|
| `verified` | 用户选的 | R 标签实心 + 「已验证可用」 |
| `degraded`（触发过 D8 裁剪） | 用户选的 | R 标签 + 黄标「已降级：reasoning 参数被省略」 |
| `failed` | **仍是用户选的** | 黄标「未验证：<原因>」+ 错误详情；**不切换、不禁用** |
| `unverified`（从未探测） | 用户选的 | R 标签空心/灰（仅当 shape 初值声明支持）或无标签 |

**MUST**：`failed` 时**允许保存并允许使用**。用户可能比我们的三档探测更了解自己的网关（例如他刚在别处验证过、或他的负载不用工具）。禁止的行为是「探测失败 → 系统自动改回 completions」——那正是 D1 排除探测进裁决链的原因。

**MUST**：探测结果**不自动改配置**，但**可以建议**：L1 成功时若发现需要不同的 base_url（例：`{provider base}/responses` 404 而 `{provider base}/v1/responses` 200），MUST 以「建议把 base_url 改为 X」的形式呈现给用户，由他确认写入，MUST NOT 直接落盘。

#### D10.4 成本与副作用约束

- **真实费用**：探测发出 1–2 次真实生成请求（`max_output_tokens:1`）。UI MUST 明示「将发送 1 次真实请求，消耗少量配额」。
- **用量不计入会话/Agent 累计**：探测的 `usage` MUST NOT 进入 ADR-027（SessionTokens）与 ADR-066（cache tokens 累计）的统计，否则预算与成本核算被非任务流量污染。允许单独记一条 Gateway 审计日志。
- **限流**：同一 (provider, model, protocol) 探测结果缓存（建议 TTL 24h），重复点按钮命中缓存直接返回，`?force=true` 才重探。全局探测速率上限走既有 rate limiter（`budget tracker / rate limiter` 在 Gateway 侧）。
- **超时**：沿用 `discover_models` 的 5s 量级，MUST NOT 用 Runtime 的 600s provider 超时（那是会话期的宽松值，探测要的是快失败）。
- **单厂商免费变体**：MiniMax 有 `POST /v1/responses/input_tokens`（不生成、只估算 token）。**MUST NOT** 为此抽象 trait 方法（一家不通用，违反 rule of three）；将来 ≥2 家再抽（§9.2）。

### D11 — 前端：协议下拉 + `R` 标签

**数据来源（唯一真源）**：`GET /api/models` 的 `ModelInfo` 新增**三个**字段，语义严格分开：

- `shape: Option<String>` —— offline 数据对该 (provider, model) 的**声明**（`responses` / `completions` / `null`=未标）。
- `protocol_active: Option<String>` —— **当前实际生效**协议（D1 三级裁决结果：用户手选 > `shape` 初值 > npm 推导）。
- `protocol_probe: Option<ProbeState>` —— 探测状态（`verified` / `degraded` / `failed` / `unverified`，含 `tier_passed` 与失败原因）。D10 的唯一消费者。

三者可以两两不同（例：MiniMax-M3 `shape=null`、用户手选 `protocol_active="responses"`、探测 `verified`）。前端 MUST NOT 自己解析 offline JSON 推导协议（`apps/acowork-desktop/src/components/harness/offlineJson.ts` 现在就在做本地解析，协议字段 MUST 走 Gateway）。

**呈现位置（4 处）**：

| # | 位置 | 文件 | 现状 | v0.2 改动 |
|---|---|---|---|---|
| 1 | Harness → 添加/编辑 provider 的模型列表行 | `components/harness/ModelMultiSelect.tsx:313-315` | `🧠 reasoning` / `🔧 tools` / `🖼️ image` 文本 chip | 追加 `Ⓡ Responses` chip，与现有 chip 同排同字号 |
| 2 | Chat 模型切换下拉的模型行 | `components/chat/ChatPanel.tsx:3251-3253` | `<Wrench size={10}/>` / `<Brain>` / `<Image>` 图标簇 | 追加字母徽标 `R`（`text-[9px]`，`text-text-tertiary`），保持图标簇节奏 |
| 3 | 模型能力编辑面板 | `components/harness/ModelCapEditor.tsx:60-80` | `supports_tool_calling` / `supports_reasoning` 复选框 | **协议下拉**（`跟随数据` / `Chat Completions` / `Responses`）+ **base_url 覆盖输入框**（仅选 Responses 时出现） |
| 4 | 同一面板内的探测按钮 + 状态区 | 新增，交互形态抄 `AddProviderFlow.tsx:723` 的 discovery status 区 | — | 「测试此协议」按钮 → 调 `POST /api/providers/{id}/probe` → 显示 `tier_passed` / 失败原因 / 「将发送 1 次真实请求」提示 |

**规则**：

- **协议下拉 MUST 始终可选，与数据无关。** 这是 v0.2 的关键差异：v0.1 靠覆盖表告诉用户「这个能选 Responses」；v0.2 没有这份表，所以**如果下拉里藏掉 Responses 选项，DeepSeek / MiniMax 的用户根本不知道有这条路**。`shape == null` 时选项照常列出，只是初始值不指向它。
- **R 标签表达「支持」，不表达「当前生效」。** 显示条件：`shape == "responses"`（数据声明）**或** `protocol_probe ∈ {verified, degraded}`（实测过）。理由：标签出现在**选择**模型的界面上，用户需要的是「选了会怎样」。生效协议由位置 3 的下拉体现，两处语义不混用。
- **R 标签的三档视觉强度**（避免「空心/实心」这种需要 legend 才能读懂的设计）：`verified` 实心 R + tooltip「已验证可用（文本 + 工具调用）」；`degraded` 实心 R + 黄点 + tooltip 说明被裁字段；仅 `shape` 声明未探测 → 灰色 R + tooltip「数据声明支持，未验证」。**文案与形态是 Q3。**
- **`failed` 不显示 R**，显示黄标「未验证：<原因>」，且**不禁用、不切换**该模型的协议选择（D10.3）。
- **`completions` 不显示** `C` 标签（默认态无需标注，避免噪音——YAGNI）。
- R 标签是**只读指示器**，MUST NOT 可点击切换（切换走位置 3 的下拉，避免「点了没反应」的假 affordance）。
- 文案与 i18n：新增 `harness.shapeResponses` / `harness.probeButton` / `harness.probeVerified` / `harness.probeFailed` / `chatPanel.modelTagResponses`，zh/en 双语（house 现有 locale 含 ja，需同步）。
- 与现有标签体系整合：沿用 `ModelMultiSelect` 的 `text-xs text-text-tertiary` chip 与 `ChatPanel` 的 `size={10}` 图标簇两套既有节奏，**不引入新的 badge 组件**（`components/common/list/Badge.tsx` 是列表徽标，样式不匹配，不复用）。
- 无障碍：字母 `R` 对读屏无意义，MUST 带 `title`/`aria-label`。

**MUST NOT**：在模型列表加「协议」筛选 chip（现有 filter chips 是 tool_call/reasoning/image 三类能力，协议不是模型能力，是接入方式；混进去会让筛选语义腐化）。

**必须承认的 UX 代价**：v0.2 把「知道这个模型能走 Responses」的责任从数据文件转移给了**用户的主动性**。不主动点开协议下拉的用户，永远看不到 DeepSeek/MiniMax 的 Responses 能力。缓解手段只有一条：位置 4 的探测按钮要足够显眼（放在模型编辑面板而不是藏在二级菜单里），并在 provider 首次配置成功时提示一句「可以为该模型测试 Responses 协议」。若评审不接受这个代价，出路是回到 Q5 的选项 (b)/(c)。

---

## 3. 接口契约与数据模型变更汇总

| 层 | 位置 | 变更 | 兼容性 |
|---|---|---|---|
| 域枚举 | `acowork-core/src/protocol.rs:929` | `ProtocolType::Responses`（serde `"responses"`，alias `openai-responses`） | 新值；旧 reader 遇未知字符串**当前会硬失败** → 必须先补 `#[serde(default)]`（D2） |
| 域结构 | `protocol.rs:229` | `ProviderListItem.protocol_type` 补 `#[serde(default)]` | 纯加固，无行为变化 |
| 域结构 | `protocol.rs:247` | `ProviderModelEntry` + `protocol_type: Option<ProtocolType>` + `base_url: Option<String>` + `protocol_probe: Option<ProtocolProbe>` | `#[serde(default, skip_serializing_if)]`，旧数据可读 |
| MQTT | `proto/mqtt_payload.proto:339` | `LLM_PROTOCOL_RESPONSES = 5` | 1–4 不变；旧 Runtime 遇 5 → 回落 OpenAI（**需加 warn**） |
| MQTT | `global_resources_builders.rs:461` / `ProviderRef.protocol_type` | 增加映射分支 | provider 级仍发**默认协议**；model 级新增字段走 `ProviderModelRef`（需同步扩 proto：`optional LlmProtocol protocol_type = 4; optional string base_url = 5;`）。**探测状态不下发**——它是配置期信息，Runtime 不需要 |
| Gateway REST | `GET /api/models` → `ModelInfo` | + `shape`（数据声明）+ `protocol_active`（生效协议）+ `protocol_probe`（探测状态） | 新增可选字段，前端旧版本忽略即可 |
| Gateway REST | **新增 `POST /api/providers/{id}/probe`** | 入参 `{model, protocol, tier?}`；返回 `{success, tier_passed, latency_ms, observed_base_url, usage, error{class,message}, stripped_fields}`（D10.1）。实现抄 `models_api.rs:150` 的出站请求骨架 | 新端点，无兼容负担；MUST 走 Vault 取 key，MUST 限流 + 结果缓存（D10.4） |
| Gateway REST | `POST/PUT /api/providers`（`provider_api.rs`） | `AddProviderRequest` + `protocol?: "openai"|"responses"|"anthropic"|...`；`model_capabilities` 旁新增 `model_protocols: {model: protocol}` 与 `model_base_urls: {model: url}` | 缺省 = 走 D1 解析，行为不变 |
| Gateway 持久化 | `resource_cache.rs:5` `provider_list.json` | 行内新增三个可选字段（含探测状态） | 需 §4.2 迁移约束 |
| Gateway 数据 | `models_api.rs:1511` `derive_protocol_type` | 保持签名不变；新增 `resolve_protocol(provider_id, model_id) -> (ProtocolType, Option<String> base_url)` | 新函数，旧函数不动（降低 blast radius） |
| Gateway 数据 | `provider_api.rs:128`「custom 一律 OpenAI」 | 允许 custom provider 显式选 `responses` + 探测 | 默认仍是 OpenAI |
| Runtime | `providers/responses.rs`（新） | `ResponsesProvider` | 新文件 |
| Runtime | `providers/router.rs:103` | + Responses 分支；`create_provider_with_wiring` 增 `model_id` 参数（或新增 `..._for_model`） | 保留旧签名做兼容 wrapper，避免一次性改所有调用点 |
| Runtime | `token/counter.rs:34` `estimate_image_tokens` | Responses 分支（公式同 OpenAI：`85 + 170*tiles`，`low`=85） | 纯新增 |
| Desktop | `lib/types.ts:1043` `ModelInfo` | + `shape?: string` + `protocol_active?: string` + `protocol_probe?: ProbeState` | 可选字段 |
| Desktop | `cacheHitRate.ts` | Responses 归入 `"openai"` 缓存族 | 设计 23 §5.4 结论沿用 |

**契约红线**：

1. 协议的**裁决**只在 Gateway 发生一次，Runtime 与前端都消费裁决结果。MUST NOT 出现「Gateway 按 A 判、Runtime 按 B 判」的双源解析——这正是 §1.1 里 `derive_protocol_type` 与 `lookup_protocol_info` 已有的雏形风险。
2. **探测状态是 Gateway 侧的展示元数据，不是 Runtime 的输入。** Runtime MUST NOT 读 `protocol_probe` 来决定构造哪个 provider（D1 / D10.3）。
3. 探测用量 MUST NOT 进入 ADR-027 / ADR-066 的累计链路（D10.4），因此探测请求 MUST NOT 复用会话侧的 usage 上报通道。

---

## 4. 兼容性、迁移与数据回写

### 4.1 发布顺序（硬约束）

```mermaid
sequenceDiagram
    participant R as Runtime / Node（读方）
    participant G as Gateway（写方）
    participant D as Desktop（展示）
    Note over R: 第 1 步：先升级读方
    R->>R: 认识 LLM_PROTOCOL_RESPONSES=5<br/>认识 ProviderModelEntry 新字段<br/>补 serde default 属性
    Note over G: 第 2 步：再升级写方
    G->>R: 才开始可能发出 "responses"
    Note over D: 第 3 步：前端上 R 标签
    D->>D: 读 ModelInfo.shape / protocol_active（旧前端忽略）
```

**理由**：新枚举值 + 无 `#[serde(default)]` 的 `protocol_type` 字段，意味着「新 Gateway 写、旧 Runtime 读」是**硬失败**；反过来「新 Runtime 写不了、旧 Gateway 不写」是安全的。顺序不可颠倒。

### 4.2 存量数据迁移

`protocol_type` 在 add/update provider 时**冻结进 `provider_list.json`**（§1.1）。因此：

1. **不自动回写存量行**。首次启动**MUST NOT** 批量重算已有 provider 的协议——否则 offline 数据一刷新，用户已验证可用的 provider 悄悄换协议，是典型的不可回滚变更。
2. **用户手选优先于重解析**。`re-resolve` MUST NOT 覆盖 `protocol_type` 已被用户显式设置过的模型（判据：该字段非 `None`）。这条是 v0.2 新增的硬约束：手选是知情决定，重解析只是数据同步。
3. 提供**显式重解析入口**：Gateway 新增 `POST /api/providers/{id}/re-resolve`（或 Harness 面板「重新检测协议」按钮），逐 provider 重跑 D1，产出 diff 预览（哪些模型协议将变化、哪些因用户手选被跳过）→ 用户确认后才写盘。
4. **探测状态与配置同生命周期但可过期**：provider 的 `base_url` / key / 模型列表任一变更后，其下所有 `protocol_probe` MUST 标记为 `stale`（下次进面板时提示重测），但 **MUST NOT** 因此改变 `protocol_type`（D10.3）。
5. 新增 provider 走新解析路径（自然生效）。
6. `provider_list.json` 的 `version` 递增沿用现有 `expected_version` 乐观并发机制（`provider_api.rs:141` ADR-059 §7.3），MUST NOT 绕过。

### 4.3 灰度

| 阶段 | 范围 | 门禁 |
|---|---|---|
| G0 | 仅 unit/integration 测试 + fake SSE 服务 | 无真实流量 |
| G1 | **只对主动点了「测试此协议」并探测成功的模型生效**。含两类：offline `shape` 声明的（aixy / neon / sakana / infer / bedrock tagged）与用户手选的（deepseek / minimax / 任意 custom 网关） | 每条探测与每次真实调用打 `info!`：协议、`status`、usage 三元组、工具循环轮数、探测 `tier_passed` |
| G2 | 允许对 `shape` 声明的 provider 开「数据驱动默认」（shape → 初值即 Responses，无需手选） | 观测 2 周：400 率、`TruncatedStream` 率、`incomplete` 率、cache_read 归属正确性 |
| G3 | 官方 `openai` provider 是否默认切 Responses | **独立决策，本 ADR 不预授权**（Q1/Q2） |

> v0.2 的灰度天然比 v0.1 安全：**G1 阶段没有任何自动路径**——用户不手选 + 不探测，就没有流量走 Responses。这使 G0→G1 的风险接近零，代价是铺开慢。

### 4.4 回滚

- **单 provider 级**：模型协议下拉切回 `Chat Completions` → 立即生效，无需发版。**这是主回滚路径**，比设计 23 §5.5 的「改配置即回滚」更真实——它需要 D1 第 1 级的手动覆盖入口（D11 位置 3）存在才成立。
- **全局级**：Gateway 配置开关 `llm.responses_api_enabled: bool`（默认 `false`）。关闭时 D1 解析结果强制回落 npm 推导，`Responses` 分支不可达，且**探测端点一并禁用**（避免关功能却还能发出真实计费请求）。比逐个改 provider 快，且能一键止血。
- **版本级**：读方（Runtime）先于写方（Gateway）发布，因此回滚 Gateway 不需要回滚 Runtime。

---

## 5. 测试要点

**单元 — 协议解析（最高价值，纯函数）**
- `resolve_protocol` 全表覆盖：D1 映射表 7 行 × {有/无 shape} × {有/无用户手选}，断言优先级与「未知 shape 值忽略」。
- **探测结果不参与裁决**：构造 `protocol_probe = failed` 且 `protocol_type = Responses` 的条目，断言 `resolve_protocol` 仍返回 `Responses`（这条测试是 D10.3 护栏的锁）。
- `${VAR}` 占位符 override 被判定不可用并回落（D7.2）。
- serde：`ProviderListItem` 缺 `protocol_type` 字段可反序列化（回归 `#[serde(default)]`）；`ProviderModelEntry` 缺新字段可读旧数据。

**单元 — 探测（D10）**
- L1 断言：非 `object == "response"` 的 2xx（例如网关返回聊天补全格式）MUST 判 `failed`，不能因为 200 就算通过——这是**虚假绿灯**的主要来源。
- L3 断言：无 `function_call` item / `call_id` 缺失 / `arguments` 非法 JSON → `failed` 且原因可区分。
- 探测用量**不进入** usage 上报通道（断言 `SessionTokens` / agent totals 不变，ADR-027/066）。
- 结果缓存命中 → 不发第二次真实请求；`?force=true` → 重发。
- 限流：并发点按钮不产生 N 次上游请求。
- `base_url` 建议路径：`{base}/responses` 404 而 `{base}/v1/responses` 200 → 返回建议但**不改配置**。
- provider 配置变更后 `protocol_probe` 标记 `stale`，且 `protocol_type` 不变（§4.2.4）。

**单元 — ResponsesProvider 序列化**
- system 多条 → `instructions` 合并；`input` 为空时**报错**（D5 红线）。
- tools 嵌套 → 扁平转换的字节级 golden test（`context.rs:1592` 的真实产物做 fixture）。
- `assistant` 消息带图 → 图片被丢弃 + warn，MUST NOT 进 payload。
- `reasoning_effort: Auto` → 不发 `reasoning` 字段；`Off` → 发 `none`。

**单元 — 错误分类与裁剪（D8）**
- `reasoning` 触发 400 → 省略 `reasoning` 重试一次成功 → 记 `degraded` + `warn!`；**两次确认才固化**（抄 `CompatCache` 纪律的回归用例）。
- `tool_choice` / `text.format` / `temperature` 触发 400 → **不裁剪**，直接上抛（断言白名单收窄生效）。
- `ContentIntegrity` 类 400 → 永不进裁剪链（2026-09-05 事故回归）。

**单元 — SSE 解析（fake 流）**
- 正常文本流、含 `function_call` 的多 item 流、reasoning 两种事件名别名各一例。
- **无 `[DONE]` 也能正常收尾**（终止事件驱动）。
- 流中断且无终止事件 → `StreamEvent::Error`，**MUST NOT** 出现 `Finished`。
- `response.incomplete(max_output_tokens)` 且 `output` 无 `message` → 可见截断，不是空回答。
- `response.failed` → `StreamError` 且 `is_retryable == false`。
- `content_part.done` / `output_item.done` 不产生重复文本（D6.7）。
- usage 映射断言 `cache_read_tokens` / `reasoning_tokens`（ADR-066 累计链路不丢）。

**契约/集成**
- protobuf 双向：`5 ↔ Responses`；旧值 1–4 不变（回归）。
- Gateway→Runtime 端到端：fake `/responses` 服务跑**完整工具循环**（function_call → 本地执行 → function_call_output → 续流），这是 agent 语义的最低验证线。
- 跨版本互通：旧 Runtime + 新 Gateway（发 5）→ 断言**有 warn 日志**（不再是静默 OpenAI）。
- 真机冒烟（不进 CI）：对 aixy / neon（shape 声明）与 deepseek / minimax（手选 + 探测）各跑一次 L1+L2+L3，记录差异到 §1.4 表。

**前端**
- R 标签三态渲染：`verified` 实心 / `degraded` 实心+黄点 / 仅 `shape` 声明未探测 → 灰色 / `failed` → 不渲染 R 而渲染黄标。
- `shape` 缺失 / `completions` 且未探测 → 不渲染 R，但**协议下拉仍必须能选到 Responses**（D11 关键差异，回归用例）。
- `shape="responses"` 但用户选了 `anthropic` → 仍渲染 R（能力语义），下拉初始值为 anthropic。
- 探测按钮：显示「将发送 1 次真实请求」提示；`failed` 时错误原因可见且**保存按钮仍可用**。
- 位置 1/2 的标签不撑破行宽（长模型名 + 4 个 chip 的截断用例）。
- 协议下拉写回 → `expected_version` 冲突路径。

**回归红线（必须证明「没影响」）**
- 现有 OpenAI / Anthropic / Google / Ollama 四条链路的 golden 请求体不变。
- `CompatCache` 行为与键空间不变（D7.3）。
- 未 opt-in 的 provider（含官方 `openai`）请求 URL 仍是 `{base}/chat/completions`。

---

## 6. 后果

**正面**
1. 协议选择从「代码里的 npm 子串猜测」变成「数据给初值、用户做决定、探测给证据」的 (provider, model) 级解析，后续接任何新线协议都是同一根管线的增量。
2. 98.8% 的 provider-model 对行为逐字节不变（shape 缺失 = 现状），风险面被数据覆盖率天然限制住；v0.2 更叠加了「G1 阶段无自动路径」，不手选不探测就没有 Responses 流量。
3. **DeepSeek / MiniMax / custom 网关的 Responses 能力可用，且不需要我们维护任何静态事实**——这是 v0.2 相对 v0.1 的实质收益：少一份会腐烂、需要 owner、会与上游漂移的数据文件。
4. 探测顺带修好了 custom provider 的长期空白（`provider_api.rs:128` 硬编码），因为自定义网关的协议本来就只能实测。
5. 前端 R 标签给用户可解释的信号，且「已验证」比「数据说」更强——用户看得见证据来源。

**负面 / 代价**
1. `ProviderModelEntry` 长出三个字段，provider 级与 model 级协议**并存**，读代码的人必须理解「override 语义」——复杂度真实增加，但被 D1 的三级裁决表 + §3 三条红线压住。
2. **「知道能选」的责任转移给了用户的主动性**（D11 末段）。不主动点开协议下拉的人，永远看不到 DeepSeek/MiniMax 的 Responses 能力。这是 v0.2 用维护成本换discoverability 的交易，必须被评审知情同意（Q5）。
3. **探测会发出真实计费请求**，并引入一个新的成本与配额面；需要限流、缓存、用量排除（D10.4）三件套才可控。
4. **探测给出的是「此刻这三件事成立」，不是「实现完整」**。若 UI 文案越界成「已验证兼容」，就是虚假承诺（R4）。
5. Responses 的实现面（约 10 种事件 + item 结构）比 Chat Completions 大，`responses.rs` 首版预计 800–1200 行，接近 `openai.rs` 的一半——不便宜；探测端点另需 200–300 行。
6. 协议差异从「一个 bool」变成「逐厂商能力矩阵」（§1.4），任何想加 Responses 特性的后续 ADR 都必须重查这张表。

**明确不做（YAGNI，留给后续）**
`previous_response_id` / 服务端会话、内置工具（web_search / file_search / code_interpreter / computer_use / apply_patch）、`background` 模式、WebSocket 流、`include` 明细、音频输入、`prompt_cache_key` 与 ADR-060 的联动优化、探测结果自动改配置、跨 shape 自动 failover。

---

## 7. 实施切片（每片独立可测、可回滚）

| 片 | 内容 | 可交付判据 |
|---|---|---|
| S1 | D2 数据模型（含 `protocol_probe` 字段）+ D1 解析函数 + D3 枚举/proto + `resolve_protocol` 单测；`ResponsesProvider` **空实现**（返回 `EndpointUnavailable`） | 链路可路由、可观测、可回滚；真实流量零变化 |
| S2 | D5 请求映射 + 非流式 `chat()` + usage 映射 | fake 服务下文本 + 工具调用 + 推理三例通过 |
| S3 | D6 流式 SSE | D6 全部 7 条硬性要求单测通过 |
| S4 | D7 URL 解析 + D8 错误分类（**不含** `reasoning` 裁剪） | 404/400 分类与文案正确 |
| S5 | **D10 探测端点（L1+L2+L3）+ 结果缓存与限流 + 用量排除** | 对 fake 服务能区分 `verified` / `failed(EndpointUnavailable)` / `failed(L3 no function_call)`；探测用量不进 usage 通道 |
| S6 | D11 前端协议下拉（**始终可选**）+ 探测按钮与状态区 + R 标签三态 + `re-resolve` 入口 | G1 灰度就绪；回归用例「shape 缺失仍能选 Responses」通过 |
| S7（可选，待 Q4） | `reasoning` 单字段裁剪 + `degraded` 状态 | 两次确认门控 + 可见告警 + debug panel 可查 |

> S5 与 S6 有依赖（下拉需要探测状态才能显示三态），但 S5 可独立交付为「后端可用、前端未接」，中途停下来的系统是自洽的。

---

## 8. 风险

| # | 风险 | 影响 | 缓解 | 残余 |
|---|---|---|---|---|
| R1 | 旧 Runtime 把 `LLM_PROTOCOL_RESPONSES=5` 静默当 OpenAI，打到 `/chat/completions` → 400，用户看到「模型不可用」而非「协议不支持」 | 中（跨版本混部窗口） | §4.1 顺序 + `_ =>` 回落加 `warn!` | 中：日志是唯一线索，前端文案在旧版本上无法改 |
| R2 | `protocol_type` 无 `#[serde(default)]`，新 Gateway 写、旧读方**硬失败**（provider_list.json 解析炸） | **高**（可能起不来） | D2 先补 default 再发新枚举；S1 单测锁死 | 低 |
| R3 | 第三方 Responses 实现是「Responses 形状的方言」：事件名、必填项、能力子集都不同（§1.4） | 高（每接一家都要重查） | §1.4 差异表进文档 + D8 显式分类 + 裁剪白名单收窄到 1 字段 + L3 工具往返探测 | **高**：这是长期成本，不是能一次解决的 |
| **R4** | **虚假绿灯**：L1 通过但能力子集不全（MiniMax `text.format` 只有 `text`），或网关返回 200 但不是 response 对象；用户看到 R 标签就信了 | **高**（v0.2 特有，因为可信度由探测而非数据背书） | D10.2 强制 **L1+L2+L3**；L1 断言 `object=="response"` 而非仅 2xx；UI 文案禁写「已验证兼容」；`degraded` 单列一档 | 中：探测覆盖不到的能力（如 `json_schema`）仍可能缺失，R5 承接 |
| **R5** | 把 Responses 当成「有 structured outputs」，在 MiniMax 上静默退化 | 中（记忆提取/蒸馏质量下降且无感） | §1.4 差异表；**MUST NOT** 在 provider 层伪造 `json_schema`；能力缺失显式报错；不写进探测承诺范围 | 低 |
| **R6** | **探测成本失控**：用户反复点、批量 provider 一起测、或探测用量混进 ADR-027/066 累计导致预算与成本核算失真 | 中 | D10.4：结果缓存 TTL + `?force` + rate limiter 复用 + **用量走独立不上报路径**（§3 红线 3）；`llm.responses_api_enabled=false` 时连探测一起禁 | 低 |
| R7 | `max_output_tokens` 含思维链 → 预算被 CoT 吃满，`incomplete` 且无正文，用户看到空回答 | 中 | D6 `finish_reason=length` + 前端可见截断提示；`estimate` 侧沿用 `token/counter.rs` | 中 |
| R8 | 前端把 R 标签当「能力」而 Gateway 当「接入方式」，语义漂移；或 `failed` 被实现成「自动切回 completions」 | 中 | D11 规则 + §3 红线 2（Runtime 不读 `protocol_probe`）+ §5 的裁决链锁定测试 | 低 |
| R9 | 一次性改 `create_provider_with_wiring` 签名牵动 3 个调用点 + 测试 | 低 | S1 保留旧签名做 wrapper，分片迁移 | 低 |
| R10 | 探测端点需要 Vault 明文 key，是一个新的敏感面（错误信息可能回显 key） | 中 | 错误文案 MUST 脱敏；探测请求只在 Gateway 进程内构造，MUST NOT 把 key 写进日志或返回体；沿用 `discover_models` 的 key 处理路径 | 低 |
| R11 | 网关侧 fs 红线：协议解析读 offline 数据是 Gateway 自有资产，合法；用户手选与探测都走 REST | 低 | 无新增 fs 访问面（v0.2 反而比 v0.1 少一个资产文件） | 低 |

---

## 9. 开放问题（工程侧，非拍板项）

1. `ProviderModelRef`（proto）扩字段后，`ProviderRef` 的 provider 级 `protocol_type` 是否应改为「默认协议」语义并在文档里正名？倾向：**是**，但需同步 `docs/protocols/zh`。
2. `chat_token_count`：MiniMax 有原生 `POST /v1/responses/input_tokens`。是否值得为单厂商实现一个可选 trait 方法（会破坏 trait 的窄接口原则）？倾向：**v1 不做**，用本地估算；将来 ≥2 家再抽 `TokenCounter` capability trait（rule of three）。
3. `detail` 图片档位：DeepSeek 有 `original`、OpenAI 有 `low/high/auto`。token 估算公式（`counter.rs`）是否需要 per-vendor 分支？倾向：先按 OpenAI 公式，偏差进观测。
4. ADR-060 的稳定前缀在 Responses 下的对应物是 `instructions`（单字符串），丢失了「分段 cache breakpoint」表达力。是否需要 `prompt.cache_breakpoints` 的后续 ADR？倾向：**是，独立立项**，本 ADR 不夹带。
5. 多账号（`account_id`）与协议 override 是否交叉？即同一 provider 不同账号能否用不同协议？倾向：**不能**，协议是 provider×model 属性，与账号无关。
6. **探测该在何时自动触发？** v0.2 目前是「用户点按钮才测」。可选的自动化时机：保存 provider 时、切换协议时、进 Harness 面板时（若 `stale`）。倾向：**只在用户显式动作时测**（保存/切协议各一次），其余时机只标 `stale` 不发请求——自动发计费请求需要单独的用户授权。
7. **探测结果要不要随 agent 包导出？** `provider_list.json` 里的 `protocol_probe` 是设备本地事实（取决于 key、网络、上游版本），倾向：**MUST NOT** 进任何跨机同步，否则一台机器的探测结论会污染另一台。

---

## 10. 需要与人拍板的关键决策点

v0.2 相对 v0.1 的变化：**Q5 已被本轮讨论定案**（删覆盖表、改探测），**探测深度已定案**（L1+L2+L3）。剩余 7 项：

| Q | 问题 | 选项 | 推荐 |
|---|---|---|---|
| **Q1** | **shape 缺失时的默认行为**（本 ADR 安全阀的核心） | (a) 缺失 = 维持现状（npm 推导 → 多为 Chat Completions），**永不默认 Responses**；(b) 缺失 + npm=`@ai-sdk/openai` = 默认 Responses；(c) 全局开关决定 | **(a)**。理由：官方 `openai` 自身无 shape 标记，(b) 会把 gpt-5 全量用户推进未验证路径，且违反「回滚优先」。v0.2 下 (a) 更自洽——用户手选 + 探测就是升级路径的门槛 |
| **Q2** | **一个模型同时支持两种 shape 时，谁选？** | (a) 只有用户手选（模型协议下拉）；(b) 手选 + Responses 失败自动回落 completions；(c) 手选 + 数据驱动默认（不自动回落） | **(a) 起步，(c) 作为 G2 目标**。反对 (b)：自动换端点等于换语义重试，失败无法归因，且 DeepSeek「静默忽略不支持参数」使探测不可靠 |
| **Q3** | **R 标签的文案、形态与语义** | 形态：(a) 字母 `R`；(b) chip `Ⓡ Responses`；(c) 换字母 `RS`/`Resp`（避免与 reasoning 混淆）。语义：(d) 表达「**支持/已验证**」；(e) 表达「**当前走 Responses**」 | 形态：**位置 1 用 (b)、位置 2 用 (a) + tooltip**；语义：D11 暂取 (d)。**新增子问**：v0.2 的三档强度（`verified` 实心 / `degraded` 实心+黄点 / 未探测灰色）是否太细？若嫌复杂，退化为「只有 `verified` 才显示 R」——更保守但会低估 shape 声明的 provider |
| **Q4** | **Responses 的错误处理边界**：v1 是否包含 D8 的 `reasoning` 单字段裁剪 | (a) 含（MiniMax `effort:none` 会 400，不含就不可用）；(b) 不含，只分类上抛，裁剪留给用户配置 | **(a)**，且已按本轮讨论从 4 字段收窄到 1 字段。裁剪越少越可信；`degraded` 状态让裁剪对用户可见，不构成静默行为 |
| **Q5** | ~~本地覆盖表是否接受~~ → **已定案：删除**（v0.2）。遗留子问：**是否接受「discoverability 代价」**——不主动点开协议下拉的用户，永远看不到 DeepSeek/MiniMax 能走 Responses | (a) 接受，靠 D11 的按钮显眼度 + 首次配置提示缓解；(b) 不接受，回到覆盖表（v0.1 方案）；(c) 不接受，改为「探测一次即把 `shape` 回写进本地缓存」，让后续用户看到 R | **(a)**。(c) 看似聪明但会把「本机一次探测」变成「持久事实」，与 §9.7 冲突，且探测失败时回写什么？不引入 |
| **Q6** | **`${VAR}` 占位符（neon / bedrock）怎么处理** | (a) v1 判定不可用并回落（本文档立场）；(b) 实现 env 展开；(c) 引导用户在 UI 手填 base_url | **(a) + (c)**。反对 (b)：把 provider 配置变成隐式依赖 Gateway 进程环境，跨 Node 部署时语义不清（ADR-055）。v0.2 下 (c) 更顺——base_url 输入框已经在 D11 位置 3 |
| **Q7** | **流式响应处理策略**（D6 的三条待选项） | (a) 终止事件驱动：无 `response.completed/.incomplete/.failed` 即 `Error(TruncatedStream)`，**不自动重试**；(b) 同上但允许「未产出任何 delta 时」重试一次；(c) 兼容模式：额外接受 `data: [DONE]` 视为正常收尾 | **(a) 起步**。反对 (c)：`[DONE]` 收尾意味着「协议说结束了」，把它当成功会掩盖半截响应；若实测有厂商这么发，再单独加白名单。反对 (b)：工具调用可能已产生副作用。**注**：D10 的 L4（若实现）正是用来暴露这类厂商差异的 |
| **Q8** | **思维链（reasoning）在 Responses 下的呈现** | DeepSeek / MiniMax 返回**明文 CoT**（`reasoning_text`），OpenAI 官方返回**摘要**（`summary`）。(a) 统一进 `StreamEvent::ReasoningContent` → 现有 `ThinkBlock` 折叠展示；(b) 明文 CoT 默认**不展示**（隐私/合规与「用户看到模型内心独白」的产品风险）；(c) 按 provider 分档 | **(a)**，与现有 completions 的 `reasoning_content` 行为一致（不新增差异）。但需产品确认：**明文 CoT 是否要落进会话持久化**（ADR-024 / ADR-100 事件日志）——若担心留痕，(b) 只在流内展示、不落盘 |

**另外两项需要确认的范围问题**：

- **Q9**：本 ADR 是否覆盖 **Azure OpenAI**？数据里 azure 系明确标 `completions`，但 Azure 也有 Responses API（端点与版本参数不同）。倾向：**v1 不覆盖**，azure 系保持 completions，另立 ADR。v0.2 下这条更容易执行——用户真想试，可以自己手选 + 探测，不需要架构先承诺。
- **Q10**：`docs/design/zh/23-openai-responses-api.md` 的处置——本 ADR 修正了它的 C1–C6 六处。倾向：**在设计 23 顶部加「已被 ADR-103 取代/修正」横幅并保留原文**（调研报告是决策过程记录，不删）。

---

## 附录 A：与既有 ADR 的边界

| ADR | 关系 |
|---|---|
| ADR-012（Per-Session Model/Provider 隔离） | 本 ADR 把协议下沉到 model 级，与「session 选定 (provider, model)」天然对齐；不冲突 |
| ADR-016（异常处理分层） | D8 的分类归属严格遵循：分类在 core、重试在 Reliable、呈现在前端。探测错误同族分类，但**不走 Reliable**（配置期行为） |
| ADR-027（会话 token 累计）/ ADR-066（Provider Cache Tokens） | D6.5 的 usage 映射是其直接下游，字段不新增；**D10.4 要求探测用量排除在两者之外**（R6） |
| ADR-060（Prompt Cache 稳定前缀） | `instructions` 是稳定前缀的线格式载体；breakpoint 表达力缺失见 §9.4 |
| ADR-034（MQTT/HTTP 边界） | 协议解析结果经 `ProviderRef`/`ProviderModelRef` 走既有 MQTT 契约，不新增通道；探测走 HTTP REST，不经 MQTT |
| ADR-055（Node 拓扑） | offline 数据是 Gateway 资产；用户手选与探测都走 Gateway REST，不引入 Gateway 侧 fs 访问（AGENTS.md 红线）。**注意**：探测由 Gateway 发出，若 provider 实际部署在远端 Node，探测的网络位置与 Runtime 调用不同——v1 接受此差异（探测只验端点可达性，不验网络路径一致性），见 R4 残余 |
