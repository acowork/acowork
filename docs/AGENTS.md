# AGENTS.md — docs/

Public architecture & design docs for ACowork.AI (v3.x).

> **本地归档（不进 git）**：[`docs/_internal/`](./_internal/README.md) — 实施计划 / 评审报告 / 上游项目调研 / 内部诊断报告。由 `.gitignore` 拦截，仅供团队本地查阅。

## OVERVIEW

All public docs are organized by **topic** (architecture, design, module spec, ADR, PRD, protocol, MCP research). Each topic has a `{zh,en}/` subdirectory — Chinese is the source language; English is added incrementally as translations become available.

## STRUCTURE

```
docs/
├── AGENTS.md                # This file (index)
├── design/{zh,en}/          # 架构设计（zh: 23 篇；en: 17 篇，随翻译进度增量）
├── module-design/{zh,en}/   # Rust crate 规格（zh: 8 篇；en: AGENTS.md 占位）
├── adr/{zh,en}/             # 架构决策记录（zh: 81；en: 84）
├── prd/{zh,en}/             # 平台 PRD + Desktop UI/UX（zh: 2；en: 占位）
├── protocols/{zh,en}/       # API 协议参考（zh: HTTP / MQTT / RAG 集成；en: 占位）
├── mcp-server-research/{zh,en}/   # MCP Server 集成调研（en: 1；zh: 占位）
└── prompt-audit/{zh,en}/    # 代码内硬编码 LLM prompt 清单（zh + en: 各 1）
```

每个主题目录均带 `zh/` + `en/` 子目录，使目录结构对称、新主题可零成本复制。

## WHERE TO LOOK

| Need | File |
| --- | --- |
| 平台总览 | [`design/zh/01-overview.md`](./design/zh/01-overview.md) |
| .agent 包格式 | [`design/zh/02-agent-package.md`](./design/zh/02-agent-package.md) |
| Rust crate 结构 | [`module-design/zh/00-overview.md`](./module-design/zh/00-overview.md) |
| Security / 隔离 | [`design/zh/08-security.md`](./design/zh/08-security.md) |
| Gateway 组件 | [`module-design/zh/03-gateway.md`](./module-design/zh/03-gateway.md) |
| 记忆（SQLite） | [`module-design/zh/04-memory-sqlite.md`](./module-design/zh/04-memory-sqlite.md) |
| 平台 PRD | [`prd/zh/prd.md`](./prd/zh/prd.md) |
| Desktop UI/UX | [`prd/zh/prd-ui-ux.md`](./prd/zh/prd-ui-ux.md) |
| HTTP API 协议 | [`protocols/zh/http.md`](./protocols/zh/http.md) |
| MQTT 事件总线 | [`protocols/zh/mqtt.md`](./protocols/zh/mqtt.md) |
| RAG 集成协议 | [`protocols/zh/RAG-protocol-guide.md`](./protocols/zh/RAG-protocol-guide.md) |
| MCP Server 集成 | [`mcp-server-research/en/mcp-server-research.md`](./mcp-server-research/en/mcp-server-research.md) |
| Session 数据模型（持久化终态 + 运行时超集） | [`design/zh/31-session-data-model.md`](./design/zh/31-session-data-model.md) |
| 远程访问：Cloud Hub（托管档，应用层消息中枢） | [`design/zh/26-cloud-sync-hub.md`](./design/zh/26-cloud-sync-hub.md) |
| 远程访问：瘦中继（自托管 BYO 档，协议冻结） | [`design/zh/24-cloud-relay-remote-access.md`](./design/zh/24-cloud-relay-remote-access.md) |
| 架构决策记录 | [`adr/zh/`](./adr/zh/) |
| 包级 LLM prompt 覆盖约定（`prompts/<file>.md`） | [`adr/zh/ADR-063-package-level-prompt-override.md`](./adr/zh/ADR-063-package-level-prompt-override.md) |
| Runtime 内硬编码 prompt 清单 | [`prompt-audit/zh/runtime-prompts-summary.md`](./prompt-audit/zh/runtime-prompts-summary.md) |

## CONVENTIONS (THIS DIR)

- **Primary language**: 所有设计文档以 **中文** 为源语言；English 翻译随项目完成度增量补充
- **Topic + language layout**: 每个主题目录（`design/`、`module-design/`、`adr/`、`prd/`、`protocols/`、`mcp-server-research/`）下统一带 `zh/` 与 `en/` 子目录；尚未翻译的空子目录放 `.gitkeep` 占位
- **File naming**: 同名文件跨 zh/en 对应；ADR 命名 `ADR-NNN-slug.md`
- Version v3.x only — no v2.x terminology
- Rust workspace: 12 crates under `core/acowork-*`，以 [`core/Cargo.toml`](../core/Cargo.toml) `[workspace] members` 为准
- `docs/_internal/` 由 `.gitignore` 拦截，不进入开源仓库

## ADDING A NEW TOPIC

新建一个主题时，按以下结构创建（保持与现有主题一致）：

```bash
mkdir -p docs/<topic>/zh docs/<topic>/en
touch docs/<topic>/en/.gitkeep      # 占位到翻译完成
# 写 docs/<topic>/zh/<file>.md
```

如果主题某语言暂无内容，**保留空目录 + .gitkeep**，不要删除 — 让结构始终对称。