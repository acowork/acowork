# Agent 打包格式（.agent）

> 版本：v3.3 | 更新日期：2026-04-17

---

## 1. 包结构

`.agent` 文件本质是一个 ZIP 压缩包。Agent 包**不含可执行文件**，只包含配置、Prompt 和数据。由 Agent Runtime 二进制加载执行。

```
<agent_id>.agent
├── manifest.toml          # 必需，元数据 + LLM 配置 + 权限 + 工具声明
├── prompts/               # System prompt 模板
│   ├── system.md          # 主系统提示词
│   ├── tools.md           # 工具使用说明
│   └── constraints.md     # 约束和安全规则
├── config/                # 默认配置文件（用户可覆盖）
│   └── settings.toml
├── data/                  # 初始数据（如空 SQLite 记忆层 快照）
├── skills/                # Skill 定义（兼容 Agent Skills 开放标准）
│   └── weather-query/
│       ├── SKILL.md       # YAML frontmatter（---）+ Markdown body
│       └── references/    # 可选：补充文档、模板数据
├── tools/                 # 自定义工具（WASM，可选）
│   └── image_filter.wasm
└── resources/             # 图标、本地化等
```

**约束：**

- 包大小上限 **50 MB**（安装时校验，超出拒绝安装）。
- `skills/*/references/` 仅允许不可执行的数据文件（JSON 模板、Markdown 文档等）。如需动态执行逻辑，应通过 `tools/` 下的 WASM 工具实现。

## 2. 包签名机制

.agent 包必须经过签名才能被 Gateway 安装和执行，类似 Android APK 签名。签名机制保障三个核心安全属性：**完整性**（包未被篡改）、**来源认证**（确认开发者身份）、**更新防护**（防止恶意包覆盖已安装 Agent）。

### 2.1 签名结构——Signing Block

采用类似 APK Signature Scheme v2 的思路，在 ZIP 的 Central Directory 之前插入一个 Signing Block，签名覆盖整个 ZIP 的所有内容（Local Files + Central Directory + End of Directory），而非将签名文件放在 ZIP 内部的 `META-INF/` 目录中（后者可被 strip 攻击绕过）：

```
.agent ZIP 结构：
┌──────────────────────┐
│   ZIP Local Files    │  ← 被签名覆盖
│   (manifest.toml,    │
│    prompts/, skills/)│
├──────────────────────┤
│   Signing Block      │  ← 签名数据（在 Central Dir 之前）
│   ┌────────────────┐ │
│   │  Signer        │ │
│   │  - certificates│ │     X.509 证书链
│   │  - digest list │ │     各 section 的 SHA-256 摘要
│   │  - signature   │ │     对 digest list 的签名
│   └────────────────┘ │
├──────────────────────┤
│   ZIP Central Dir    │  ← 被签名覆盖
├──────────────────────┤
│   ZIP End of Dir     │  ← 被签名覆盖
└──────────────────────┘
```

### 2.2 签名数据结构

```rust
struct SigningBlock {
    signers: Vec<Signer>,
}

struct Signer {
    certificates: Vec<Certificate>,     // X.509 证书链
    digest_algorithm: DigestAlgorithm,  // SHA-256
    digests: Vec<SectionDigest>,        // 各 section 的摘要
    signature: Vec<u8>,                 // 对 digests 的签名
    signed_attrs: SignedAttributes,     // 签名时间戳等属性
}

struct SectionDigest {
    section: Section,       // LocalFiles / CentralDir / EoCD
    hash: [u8; 32],         // SHA-256 digest
}
```

### 2.3 两类签名身份

Phase 1 实现两类签名身份。未来扩展阶段将引入 Distribution Key（商店分发密钥），详见第 5 节。

| 身份类型 | 适用场景 | 说明 | 类比 Android |
|---------|---------|------|-------------|
| Developer | 普通 Agent | 开发者自签名证书，最常见 | 普通应用的 debug/release 签名 |
| Platform | 系统 Agent | ACowork 平台签发的证书，Gateway 内置平台根公钥 | Platform 签名（系统应用） |

### 2.4 签名验证流程

> **Phase 1 说明**：当前阶段签名验证为可选——未签名的 .agent 包允许安装但记录警告日志。严格的签名强制验证将在 Phase 2 实施。

```
用户安装 .agent 包
       │
       ▼
1. 解析 ZIP，提取 Signing Block
       │
       ▼
2. 用证书中的公钥验证签名
   （证明"包确实由持有该私钥的人签名"）
   └─ 验证失败 → 拒绝安装："签名无效"
       │
       ▼
3. 按 SHA-256 重新计算各 section digest，与签名中的 digests 比对
   （证明"包内容未被篡改"）
   └─ 不一致 → 拒绝安装："包已被篡改"
       │
       ▼
4. 验证证书可信性（区分签名身份）:
   │
   ├─ 证书由 Gateway 内置的平台根公钥签发？
   │   └─ 是 → 识别为 Platform 签名
   │
   └─ 否 → 识别为 Developer 自签名
   │        （自签名证书链验证无安全意义，任何人都可以自签名）
       │
       ▼
5. 检查签名身份与 manifest 声明的匹配:
   ├─ manifest 声明 platform.system = true → 必须是 Platform 签名
   │   └─ 非 Platform 签名 → 拒绝安装："系统 Agent 必须由平台签发"
   │
   └─ 已安装同 agent_id 的旧版本 → 比对签名证书指纹
       └─ 证书指纹不一致 → 拒绝更新："签名者与已安装版本不同"
       │
       ▼
6. 全部通过 → 允许安装
```

**两种签名身份的信任模型差异：**

| 验证维度 | Developer 自签名 | Platform Key |
|---------|-----------------|-------------|
| 完整性 | SHA-256 digest 比对 | 同左 |
| 来源认证 | 证书指纹一致性（更新时匹配） | 证书链验证（必须由平台根 CA 签发） |
| 首次安装 | 任何自签名证书都接受 | 必须匹配平台根公钥 |
| 更新安装 | 新包证书指纹 = 已安装指纹 | 同左（平台根 CA 不变） |

Developer 自签名的安全保障靠**指纹锁定**：首次安装时记录证书指纹，后续更新必须一致。这和 Android 早期（v1 签名）对普通应用的做法一致——你不知道开发者是谁，但你能保证更新来自同一个开发者。

### 2.5 签名与权限的关联

Gateway 配置中可定义基于签名者的信任规则，特定签名的 Agent 可自动获得额外权限：

```toml
# ~/.config/agent-gateway/config.toml

[trust]
# 平台签名的 Agent 自动授予的权限
platform_signer_permissions = [
    "identity:admin",       # 可管理用户身份
    "agent:install",        # 可触发安装其他 Agent
    "sandbox:bypass",       # 可跳过沙箱（系统 Agent）
]

# 指定证书指纹的信任规则
[[trusted_signers]]
fingerprint = "sha256:AB:CD:EF:..."
permissions = ["network:*"]
label = "Trusted Weather Developer"
```

### 2.6 签名工具链

```bash
# 生成开发者密钥对
acowork-keygen --alias my-key --output ./keys/

# 签名 .agent 包
acowork-sign \
    --key ./keys/my-key.pem \
    --cert ./keys/my-key.crt \
    --input ./build/com.example.weather.unsigned.agent \
    --output ./build/com.example.weather.agent

# 验证签名
acowork-verify ./build/com.example.weather.agent

# 查看签名信息
acowork-verify --verbose ./build/com.example.weather.agent
# 输出：Signer: CN=Zhang San, O=Example Corp
#       Digest: SHA-256
#       Valid from: 2026-01-01 to 2027-01-01
```

### 2.7 Debug 签名

参考 Android Debug Keystore 机制，为本地开发测试提供便捷签名方式。

首次运行 `acowork` CLI 时自动生成 debug 密钥：

```
~/.config/acowork/debug.key
~/.config/acowork/debug.crt

- 算法: Ed25519
- 有效期: 1 年
- 仅限本地开发测试，不得用于生产分发
```

```bash
# 使用 --debug 自动选用 debug key
acowork-sign --debug \
    --input ./build/com.example.weather.unsigned.agent \
    --output ./build/com.example.weather.debug.agent
```

Gateway 安装 debug 包时的行为：

1. 验证签名完整性（与正式包相同）
2. 检测到 debug 证书指纹 → 输出警告："Debug 签名，仅限本地开发测试"
3. `platform.system = true` 的 debug 包仍被拒绝（debug key 不是 Platform Key）
4. 生产环境可在 Gateway 配置中禁用 debug 包安装：

```toml
# ~/.config/agent-gateway/config.toml
[debug]
allow_debug_packages = true   # 生产环境设为 false
```

### 2.8 系统 Agent 本地调试

`platform.system = true` 的 Agent 必须由 Platform Key 签名，但开发者本地没有 Platform Key。Phase 1 采用**本地信任配置**解决——开发者在 Gateway 配置中显式信任特定指纹的包以 system 权限运行：

```toml
# ~/.config/agent-gateway/config.toml
[debug]
# 允许指定指纹的包以 system 权限运行（仅开发用）
[[debug_platform_overrides]]
fingerprint = "sha256:12:34:56:..."
agent_id = "com.acowork.system"
note = "本地开发调试用"
```

这是纯本地操作，不依赖在线平台服务。更完善的 Debug Platform Key 机制（在线申请、远程吊销等）留到 Phase 5 实现。

## 3. manifest.toml 格式

```toml
agent_id = "com.example.weather"
version = "1.0.0"
name = "Weather Agent"
display_name = "天气助手"
role = "Weather Specialist"
description = "查询实时天气并建议穿衣"
author = "example@domain.com"
runtime_version = "0.1.0"
system = false
dev = false

# ADR-104：属性标签（小写 kebab，2-32 字符，≤12 个）
labels = ["weather", "daily-life"]

# 头像: 优先使用包内图片(assets/avatar.png), 否则用内置图标("icon-05" / "5")
# 都不设置则首次安装时随机一个内置图标
# avatar = "assets/avatar.png"
# builtin_avatar = "icon-05"

# permissions 使用数组表语法
[[permissions]]
type = "Network"
value = "https://api.weather.com"

[[permissions]]
type = "FilesystemRead"

# 记忆不再是权限域：ADR-101 把记忆剥离为 Gateway 托管的独立服务，
# `memory_store` / `memory_recall` 变成该服务（MCP server id = `memory`）的工具，
# 工具级开关闭在 `agent_mcp_tools.json`（前缀名 `mcp_memory__*`，ADR-069），
# 包声明里已删除的 `MemoryRead` / `MemoryWrite` 会被当作未知类型拒绝。

[[permissions]]
type = "IntentSend"
value = "com.example.calendar"

# 触发器
triggers = []

[llm]
temperature = 0.7

[llm.providers.openai]
model = "gpt-4o"
api_key_ref = "vault:openai_key"
base_url = "https://api.openai.com/v1"

[llm.providers.claude]
model = "claude-sonnet-4-20250514"
api_key_ref = "vault:anthropic_key"

[llm.routing]
strategy = "quality_priority"
fallback_order = ["openai", "claude"]

[llm.budget]
max_output_tokens = 8192
exceeded_action = "warn"

# ADR-104：agent_config.json 的行为默认值。全空则不写本段（= 作者没有意见）。
# 只允许行为旋钮，安全阈值 / 宿主配额 / 用户偏好不在白名单内。
[config]
temperature = 0.7
context_window = 128000
max_output_tokens = 8192
max_iterations = 40
compression_ratio_threshold = 0.9

[memory]
enabled = true
retention_days = 90

identity_deps = ["display_name", "language", "timezone"]

[[tools]]
name = "http_request"

[[tools]]
name = "memory_store"

[[tools]]
name = "memory_recall"

# 企业 RAG 工具（可选）
[[tools]]
type = "rag"
name = "enterprise_knowledge"
[tools.rag]
endpoint = "https://rag.internal.company.com/api/query"
collection = "product_docs"
auth_ref = "vault:company_rag_token"
auth_type = "bearer"
max_results = 5
score_threshold = 0.7

# ADR-104：推荐 MCP 服务器。推荐 ≠ 安装 —— 落地到 agent_mcp.json 的
# recommended 列表，由用户在工具面板确认安装与启用（ADR-090）。
# 三种形态互斥：preset / hosted / transport+url；禁止内联 stdio。

[[mcp_servers]]
preset = "brave-search"                 # 平台预设 id（Gateway 注册表）
why = "联网检索最新天气资料"              # 展示给用户看，原样渲染
required_secrets = ["BRAVE_API_KEY"]     # 缺密钥时该卡片点名提示
tools = ["brave_web_search"]             # 工具子集；启用后成为初始勾选集
required = false                         # 软必需：未满足只提示"能力受限"

[[mcp_servers]]
hosted = "memory"                        # Gateway 托管服务（pm / doc / memory）
why = "记住用户所在城市"

# capabilities 用映射语法
[capabilities.query_weather]
description = "查询天气信息"
# ADR-104：本能力由哪些技能实现（多对多）。引用不存在的技能是 error。
skills = ["weather-query"]

[capabilities.query_weather.input_schema]
type = "object"
properties.city = { type = "string" }
properties.date = { type = "string" }

[resources]
max_memory_mb = 512
idle_timeout_secs = 300

[sandbox]
enabled = false

[skills]
progressive = false
```

**关键字段说明：**

- `runtime_version`：声明兼容的 Agent Runtime 版本。当前为 `"0.1.0"`。
- `dev`：是否为开发者模式。用于本地开发测试。
- `display_name` / `role`：UI 展示用的短名称和角色标题。`display_name` 默认为 `name`。
- `avatar`：可选，包内头像图片路径（如 `"assets/avatar.png"`）。优先级最高。
- `builtin_avatar`：可选，内置头像索引（如 `"icon-05"` 或裸数字 `"5"`），用于在没有 `avatar` 时给首次安装的客户端一个确定的默认图标。客户端按字母数字顺序归一化为 `"icon-XX"`，并对照自身捆绑的图标集校验；无法识别时回退到随机。

#### 头像解析优先级（客户端契约）

Agent 客户端在渲染 Agent 头像时必须遵循以下优先级（与 [Avatar resolution priority policy](../development_practice/agent-avatar-priority.md) 一致）：

1. **`manifest.avatar` 包内头像**（最高优先级）。Desktop App 通过 Gateway 的 `GET /api/agents/:id/avatar` 端点获取，URL 拼接 `?v=<manifest.version>` 作为 HTTP 缓存击穿键。当包重新安装（`agent_id` 不变但 `version` 变化）时，URL 自动变化，浏览器/WebView 重新下载。
2. **Profile store 里的 `avatarIconId`**（本地持久化）。覆盖两类来源：`AgentSetupTab` 中用户手动挑选的图标；以及首次安装时 `ensureBuiltinAvatars` 后台分配的默认图标。Profile store 会在每次 `fetchAgents` 后自愈：若 manifest 新加了 `avatar`，则清除残留的 `avatarIconId`（让包内头像生效）。
3. **确定性随机内置图标**。根据 `agentId` 做哈希选择 `BUILTIN_ICONS[id]`，与 `ensureBuiltinAvatars` 即将分配的图标一致，避免首次渲染闪动。

该优先级在远端 Gateway 场景下也必须保持一致（详见 [04-gateway.md § 9.6](./04-gateway.md#96-安全设计)）。
- `permissions`：使用 TOML 数组表语法，每条包含 `type` 和可选的 `value`。支持 `Network`、`FilesystemRead`、`FilesystemWrite`、`IntentSend` 等类型（`MemoryRead` / `MemoryWrite` 已随 ADR-101 删除——记忆改由服务承载，不再有权限域）。
- `triggers`：激活触发器数组。支持 `cron`、`event`、`manual` 类型。`cron` 使用标准 5 段表达式（UTC 时区），不支持秒级精度和特殊宏。
- `llm.providers`：支持配置多个 LLM Provider，每个引用 Vault 中的密钥。
- `llm.routing.strategy`：LLM 路由策略（`cost_priority` / `quality_priority` / `latency_priority`）。
- `llm.budget`：Token 和费用预算，超限后的动作（`stop` / `fallback_to_local` / `warn`）。
- `memory`：记忆系统配置。`enabled` 是否启用，`retention_days` 保留天数。
- `identity_deps`：声明启动时需要的用户身份字段（如 `display_name`、`language`、`timezone`），Gateway 在握手时通过 UserProfile 注入。
- `tools`：工具声明数组。支持 `builtin`（默认）和 `rag` 两种类型。RAG 工具需在 `[tools.xxx.rag]` 中配置端点、认证方式等。
- `capabilities`：声明本 Agent 可被其他 Agent 通过 Intent 调用的能力。使用映射语法（action name → 描述 + schema）。
- `skills`：技能系统配置。`progressive` 为 `true` 时启用渐进式技能注入（system prompt 中仅注入摘要，完整指令按需加载）。

### 3.1 identity_deps 注入细节

`identity_deps` 是一个字符串数组，声明 Agent 启动时需要的用户身份字段。Gateway 在握手阶段（AgentHello → AgentHelloResult）将 UserProfile 注入 Runtime，不再经过系统 Agent。详见 [18-user-identity-simplified.md](./18-user-identity-simplified.md)。

**字段名约定**：

| 字段名 | 语义 | 来源 | Phase 1 默认值（Gateway 无数据时） |
|--------|------|------|--------------------------------------|
| `display_name` | 用户希望被怎么称呼 | Onboarding 必填 | `""`（空字符串，Agent 应在 prompt 中优雅降级） |
| `language` | 用户语言偏好（BCP 47） | Onboarding 必填 | `"en-US"`（安全回退到英语） |
| `timezone` | 用户时区（IANA） | Onboarding 必填 | `"UTC"`（安全回退到 UTC） |
| `city` | 所在城市 | Onboarding 选填 | `null`（未知，不猜测） |
| `occupation` | 职业/领域 | 对话沉淀 | `null` |
| `communication_style` | 沟通偏好 | 对话沉淀 | `null` |
| `custom:*` | 开放扩展字段 | 各来源 | `null` |

**required vs optional 语义**：

identity_deps 数组中的字段**全部视为 optional**——即使 Agent 声明了 `identity_deps = ["display_name", "city"]`，如果 Gateway 没有这些字段的数据（用户未提供），Runtime 仍然正常启动，只是注入的 UserProfile 中该字段值为 null。

这种设计基于以下考量：
- Agent 不应因为用户缺少某个身份信息就拒绝工作（如天气 Agent 不知道用户城市，可以主动询问）
- 必填 vs 选填的控制权在 Onboarding 侧（哪些字段强制采集），而非 Agent 声明侧
- 如果未来确实需要 required 语义，可在字段名后加 `!` 后缀（如 `"city!"`），但 Phase 1 不实现

**identity_delivery 中字段缺失的处理**：

```json
// Agent 声明 identity_deps = ["display_name", "city", "occupation"]
// Gateway 只知道 display_name

{
    "type": "user_identity",
    "fields": {
        "display_name": "张三",
        "city": null,
        "occupation": null
    }
}
```

- 已知字段：返回实际值
- 未知字段：值为 `null`
- Gateway 不认识的字段名：仍返回 `null`（不会报错）

### 3.2 权限匹配语义

permissions 数组中的每条权限字符串遵循统一的模式语法，Gateway 和 Runtime 据此判断工具调用的授权状态。

**权限格式**：

```
<domain>:<resource>[:<qualifier>]
```

| 组成 | 说明 | 示例 |
|------|------|------|
| `domain` | 权限域（大类别） | `network`, `filesystem`, `memory`, `intent` |
| `resource` | 具体资源或操作 | `https://api.weather.com`, `read`, `write`, `send` |
| `qualifier` | 可选限定词 | `~/Documents`, `com.example.calendar` |

**通配符规则**：

| 模式 | 含义 | 匹配示例 | 不匹配 |
|------|------|---------|--------|
| `network:https://api.weather.com` | 精确匹配 | `https://api.weather.com` | `https://api.other.com` |
| `network:https://*.weather.com` | 子域名通配 | `https://api.weather.com`, `https://v2.weather.com` | `https://weather.com`（裸域不匹配） |
| `network:*` | 整个 network 域 | 任意 HTTPS URL | — |
| `filesystem:read:*` | 整个 filesystem:read 域 | `~/Documents`, `/tmp` | — |
| `filesystem:*:*` | 整个 filesystem 域 | read/write 任意路径 | — |
| `intent:send:*` | 可向任何 Agent 发 Intent | `com.example.calendar`, `com.acowork.system` | — |

> 旧版此处还有一行 `memory:read`；ADR-101 之后 memory 不再是权限域，通配规则也不再涉及它。

**匹配算法**：

```rust
fn matches_permission(declared: &str, requested: &str) -> bool {
    let decl_parts: Vec<&str> = declared.splitn(3, ':').collect();
    let req_parts: Vec<&str> = requested.splitn(3, ':').collect();

    // 1. 域必须精确匹配
    if decl_parts[0] != req_parts[0] { return false; }

    // 2. 资源匹配（支持 * 通配和子域名 *.example.com）
    if decl_parts[1] == "*" { return true; }  // 整个域通配
    if !wildcard_match(decl_parts[1], req_parts[1]) { return false; }

    // 3. qualifier 匹配（如果声明了 qualifier）
    if decl_parts.len() == 3 && req_parts.len() == 3 {
        wildcard_match(decl_parts[2], req_parts[2])
    } else if decl_parts.len() == 3 && req_parts.len() == 2 {
        false  // 声明了 qualifier 但请求没有，不匹配
    } else {
        true   // 都没有 qualifier，匹配
    }
}
```

**权限检查流程**：

1. Agent 发起工具调用（如 `http_request({"method":"GET","url":"https://api.weather.com/..."})`)
2. Runtime 构造请求权限字符串（如 `network:https://api.weather.com`）
3. 遍历 manifest.permissions，调用 `matches_permission`
4. 任一声明权限匹配 → 允许执行
5. 无匹配 → 拒绝执行，返回 PermissionDenied 错误

**Phase 1 实际权限列表**：

| 域 | 权限示例 | 对应工具 |
|----|---------|---------|
| network | `network:https://api.example.com` | http_request |
| filesystem | `filesystem:read:~/Documents`, `filesystem:write:~/Documents` | file_read, file_write |
| filesystem | `filesystem:read:/tmp` | file_read |
| intent | `intent:send:com.example.calendar` | intent_send |
| shell | （shell 工具无需声明 permission，由 Approval Gate 控制） | shell |
| identity | （identity_store 仅系统 Agent 可用，通过 platform.system 声明授权） | identity_store |

### 3.3 能力声明面（ADR-104）

manifest 是**能力声明面**，四类声明各有明确语义与落地路径（详见 [ADR-104](../adr/zh/ADR-104-agent-manifest-declaration-surface.md)）：

| 声明 | 语义 | 落地 |
|---|---|---|
| `[[tools]]` | 内置工具 | 该 Agent 的内置工具开关（`builtin_tools`） |
| `[[mcp_servers]]` | **推荐** 的 MCP 服务器 | `agent_mcp.json.recommended`，由用户确认安装/启用 |
| `[capabilities.<action>].skills` | 该能力由哪些技能实现 | intent 寻址 + 声明图校验 |
| `[config]` | `agent_config.json` 的行为默认值 | 只回填 `None` 字段，永不覆盖用户值 |

**推荐 ≠ 安装。** `[[mcp_servers]]` 只写 `recommended` / `dismissed`，从不写 `catalog` / `local` / `active_names`。用户确认后，服务器进入**本机共享的 MCP 目录**（带 `origin`：`preset:<id>` / `agent-package:<agent_id>#<key>` / `user`），其他 Agent 看得到但不会自动激活。

**能力 ≠ 技能。** 技能是内部流程，不对外暴露；能力是对外契约，其他 Agent 通过 intent 寻址。一个能力可由多个技能实现，一个技能可服务多个能力。

**声明图校验**（`AgentManifest::validate_graph`）：

1. `capabilities.*.skills` 必须能在包内解析 → **error**，打包（`acowork-sign`）与安装（`acowork-node`）都拒绝。
2. 技能的 `tool_deps` 必须可从 `[[tools]]` ∪ `[[mcp_servers]].tools` 到达，或属于平台托管 MCP（`mcp_<server>__<tool>`，server ∈ {memory, pm, doc}）→ 否则 **warning**，不阻断。

**默认值 ≠ 覆盖。** `[config]` 的投影点是 `AgentManifest::config_defaults()`（`[config]` 优先、缺省回退 `[llm].temperature` / `[llm].context_window`），由 `fill_if_empty()` 只填 `None` 字段且幂等。`[config].max_output_tokens` 是**配置级上限**，与 `[llm].max_tokens` 的**请求级覆盖**语义是两条独立通道。


## 4. 设计决策记录

| 决策 | 选择 | 理由 |
|------|------|------|
| 签名方案 | v2 风格（单签名者） | Phase 1 最小实现，密钥轮换等复杂特性延迟到 Phase 5 |
| 签名信息位置 | Signing Block 元数据 | 签名验证数据不属于开发者声明层，不放在 manifest.toml 中 |
| `system` 字段位置 | manifest 顶级字段 | 安全敏感属性独立声明 |
| 包大小上限 | 50 MB | 防止包含过大 SQLite 记忆层 快照或 WASM 工具导致安装问题 |
| capabilities 语法 | 映射（非数组） | action 名称天然唯一，映射比数组更直观 |
| 平台兼容性模型 | Android uses-feature（required/optional） | shell 在移动端不可用、文件操作受限，需要声明式降级机制 |
| target_platforms | 当前未实现，留待 Phase 5+ | 移动端兼容性声明 |
| RAG 工具声明 | 独立 type="rag" + rag_config 节 | 企业 RAG 是外部服务，需要 endpoint/auth/collection 等独立配置 |

## 5. 未来扩展（Phase 5+）

以下特性将在云端与生态阶段实现，当前仅记录设计方向，不在 Phase 1 中实现：

### 5.1 双密钥模型（类比 Play App Signing）

引入 Upload Key（开发者持有）和 Distribution Key（商店持有）的分离。开发者用 Upload Key 签名提交给商店，商店用 Distribution Key 重新签名后分发给用户。好处：开发者丢失 Upload Key 可重置，不影响已安装用户的更新。

### 5.2 密钥轮换（Proof-of-Rotation）

参考 APK Signature Scheme v3，支持签名密钥轮换。Signing Block 中可包含多个 Signer，通过 `proof_of_rotation` 字段建立旧密钥到新密钥的链式信任。旧版本 Runtime 仍可通过历史 Signer 验证包。

### 5.3 Distribution Key（商店分发密钥）

新增第三类签名身份。商店/平台持有 Distribution Key，用于重新签名开发者提交的包。安装时 Gateway 根据 Distribution Key 的商店根 CA 验证。

### 5.4 证书吊销与 CRL

商店维护证书吊销列表（CRL），Gateway 安装时在线查询。开发者可申请吊销并重置 Upload Key，商店颁发新证书并包含 proof_of_rotation。

### 5.5 Debug Platform Key（在线申请）

开发者通过平台账号认证申请 Debug Platform Key（有效期 30 天，可远程吊销），用于在本地调试 system=true 的 Agent。替代当前 Phase 1 的本地信任配置方案。

