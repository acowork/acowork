# ADR Glossary — English Rendering Conventions

> **Scope.** Every English ADR under `docs/adr/en/` renders the same Chinese source
> terms using the English names fixed in this file. One table means a reader who
> learns "compaction" in ADR-010 still understands ADR-083.
>
> **Rule 0 — never translate an identifier.** File paths, module names, struct
> fields, config keys, CLI flags, MQTT topics, proto field names, and log strings
> are copied byte-for-byte. If a Chinese sentence contains `emergency_trim()`, the
> English sentence contains `emergency_trim()`.
>
> **Rule 1 — proper nouns stay as-is.** `Gateway`, `Runtime`, `Node Agent`,
> `Desktop`, `Grafeo`, `ACowork` are component names, not words to translate. They
> are never lowercased, pluralized, or localized.
>
> **Rule 2 — keep decision numbering.** Sources number decisions as `决策 1` /
> `决策 12 v2`. English versions keep the number and translate only the prose, so
> cross-references such as "Decision 12 v2" stay resolvable.

---

## 1. Platform components

These are the processes in the architecture diagram. They are proper nouns.

| Chinese | English | Notes |
|---------|---------|-------|
| Gateway | Gateway | keep-alive process; HTTP :19876, embedded MQTT broker :19875 |
| 网关 | Gateway | |
| Runtime | Runtime | the universal agent runtime binary |
| 运行时 | the Runtime | |
| Node Agent | Node Agent | per-machine daemon (ADR-055); `acowork-node` |
| 节点代理 | Node Agent | |
| Desktop | Desktop | the Tauri v2 desktop app |
| 桌面端 | Desktop | |
| System Agent | System Agent | |
| 用户 Agent | user agent | |
| 智能体 / Agent | agent | lowercase for the concept; capitalized only inside a proper name |
| 用户身份 | user identity | |
| 身份 | identity | |
| 生命周期管理 | lifecycle management | |
| 监督器 | supervisor | |

## 2. Package & distribution

| Chinese | English | Notes |
|---------|---------|-------|
| 包（.agent） | package / `.agent` package | never "APK" outside Android-analogy sections |
| 包管理器 | package manager | |
| 安装 / 卸载 / 升级 / 发布 | install / uninstall / upgrade / publish | |
| 签名块 | Signing Block | verbatim when naming the container spec |
| 签名 | signing / signature | |
| 技能 | skill | |
| 提示词 | prompt | |
| 系统提示词 | system prompt | |
| 清单 | manifest | `manifest.toml` |
| 安装路径 | install path / `install_path` | |
| 私有 Grafeo | private Grafeo | |

## 3. Agent runtime — context & compaction

The most frequently translated cluster; 压缩 must render consistently.

| Chinese | English | Notes |
|---------|---------|-------|
| 上下文 | context | |
| 上下文窗口 | context window | |
| 上下文压缩 | context compression | |
| 压缩 | compaction (noun) / compact (verb) | |
| 摘要 | summary (noun) / summarize (verb) | |
| 蒸馏 | distillation / distill | |
| 折叠 | folding / fold | `fold_tool_results` stays verbatim |
| 裁剪 | trim | `emergency_trim` verbatim |
| 紧急裁剪 | emergency trim | |
| 完整上下文 | full context | |
| 中间段 | middle segment | |
| 保留首尾 | protect head and tail | |
| 预算 | budget | |
| 字节预算 | byte budget | ADR-061 |
| 阈值 | threshold | |
| 告警 | warn | the 70% stage is "warn" |
| 紧急溢出 | ContextOverflow | keep the API error name verbatim |
| 使用率 | usage percent | `usage_percent` |
| Token 计数 | token counting | |
| 归档 | archive | |
| 召回 | recall | memory recall is a feature name |

## 4. Memory subsystem

| Chinese | English | Notes |
|---------|---------|-------|
| 记忆 | memory | |
| 经历层 | episode layer | **not** "experience layer" |
| 沉淀层 | consolidation layer | |
| 巩固 | consolidation (offline) | ADR-071 |
| 检索 | retrieval | |
| 记忆质量 | memory quality | |
| 门禁 | gate / quality gate | |
| 向量 / 向量库 | vector / vector store | |
| 全文检索 | full-text search (FTS) | ADR-082 |
| 记忆写入 | memory write | |

## 5. Session & conversation

| Chinese | English | Notes |
|---------|---------|-------|
| 会话 | session | |
| 会话级 | per-session | |
| 轮次 | turn | not "round" |
| 消息 / 消息角色 | message / message role | |
| 附件 | attachment | |
| 统一附件条目 | unified attachment entries | ADR-046 |
| 索引 | index | |
| 合并元数据 | merged metadata | ADR-024 |
| 流式 / 刷盘 | streaming / flush to disk | |
| 会话关闭 | session close | |

## 6. Transport & RPC

| Chinese | English | Notes |
|---------|---------|-------|
| 反向代理 | reverse proxy | |
| 生命周期 | lifecycle | |
| 心跳 | heartbeat | |
| 重连 / 断链 | reconnect / disconnection | |
| 主题 / 订阅 / 发布 | topic / subscription / publish | |
| 保活 | keep-alive | |
| 会话（MQTT 语境） | session (MQTT) | distinct from agent session |

## 7. MCP

| Chinese | English | Notes |
|---------|---------|-------|
| 工具 / 工具集 | tool / toolset | |
| 按工具逐个启用 | per-tool opt-in | ADR-069 |
| 目录 / 目录项 | catalog / catalog entry | |
| 安装包规范 | package spec | ADR-072 |
| 包装 | wrap / wrapper | |

## 8. Deployment, processes & security

| Chinese | English | Notes |
|---------|---------|-------|
| 独立进程 | standalone process | the ADR-064/070/084 pattern |
| 常驻进程 | resident process | |
| 内嵌 | embed / embedded | |
| 零业务铁律 | the zero-business rule for Gateway | Gateway carries no domain logic |
| 主机 IP | host IP | |
| 广告地址 | advertise address / `advertise_host` | |
| 看门狗 | watchdog | |
| 权限 / 属主 / 访问控制 | permission / owner / access control | |
| 账号 / 凭据 / 角色 | account / credential / role | |
| 头像 | avatar | |
| 成本 / 预算跟踪 / 限流 | cost / budget tracking / rate limiting | |

## 9. Hosted applications

| Chinese | English | Notes |
|---------|---------|-------|
| 托管应用 | hosted application | ADR-116; not a plug-in |
| 用户画布 | user canvas | left-nav webview; MCP does not attach |
| Agent 工作态 / Agent 产物 | agent working state / agent artifact | a file in that agent's workspace; not conversation memory |
| 聊天预览 | chat preview | open that workspace file in the editor pane; never a transcript-row iframe |
| 共编 | co-edit | only inside the app's own UI |
| hosted adapter | hosted adapter | ACowork glue binary, e.g. `figcraft-acowork` |
| `library` / `visual` / `none` | `library` / `visual` / `none` | `ui.canvas.kind` (ADR-118) |

## 10. Section headings & normative words

| Chinese | English |
|---------|---------|
| 背景 | Context |
| 核心洞察 | Key Insight |
| 决策 | Decision |
| 后果 | Consequences |
| 目标 | Goals |
| 可选方案 | Alternatives |
| 方案 A / 方案 B | Option A / Option B |
| 影响 | Impact |
| 决策者 | Decision Makers |
| 日期 | Date |
| 状态 | Status |
| 关联 / 前置 / 细化 | Related / Supersedes / Refines |
| 已接受 / 提议中 / 已决策 / 已废弃 | Accepted / Proposed / Decided / Deprecated |
| 决策 N | Decision N (number preserved) |
| 必须 / 应当 / 可以 / 禁止 | MUST / SHOULD / MAY / MUST NOT |
| 保留 / 放弃 | Keep / Drop |
| 即 / 见 | i.e. / see |
| 铁律 | iron rule |
| 正面 / 负面 | Upside / Downside |
