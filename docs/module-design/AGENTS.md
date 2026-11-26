# AGENTS.md — docs/module-design/

Detailed Rust crate specifications for ACowork.AI implementation.

## OVERVIEW

7 markdown files (Chinese) defining the 6-crate Rust workspace structure, module responsibilities, and data flows.

## STRUCTURE

```
module-design/
├── zh/                       # 中文规格文档（源语言）
│   ├── 00-overview.md        # Workspace layout, Cargo.toml deps, crate list
│   ├── 01-core.md            # acowork-core: shared types, protocol, traits
│   ├── 02-runtime.md         # acowork-runtime: Bot Runtime binary
│   ├── 03-gateway.md         # acowork-gateway: IPC gateway, lifecycle mgmt
│   ├── 04-sqlite.md          # acowork-sqlite: graph DB + HNSW + BM25
│   ├── 05-vault-sign.md      # acowork-vault + acowork-sign: secrets, signing
│   ├── 06-architecture.md    # Dependency graph, data flows, compilation
│   └── 07-ask-user-question-tool.md  # AskUserQuestion tool design
└── en/                       # English agent instructions
    └── AGENTS.md             # This file
```

## WHERE TO LOOK

| Crate                 | Spec File (zh)          |
| --------------------- | ----------------------- |
| acowork-core         | `zh/01-core.md`         |
| acowork-runtime      | `zh/02-runtime.md`      |
| acowork-gateway      | `zh/03-gateway.md`      |
| acowork-sqlite       | `zh/04-sqlite.md`       |
| acowork-vault + sign | `zh/05-vault-sign.md`   |
| Architecture overview | `zh/06-architecture.md` |

## KEY CONVENTIONS

- **Trait-driven**: Tool, Provider, Memory, Channel all use Rust traits
- **Multi-crate**: Gateway and Runtime are separate binaries (process boundary = crate boundary)
- **Feature flags**: Keep under 10 feature flags
- **Config per crate**: Each crate has own config struct, not a monolithic schema
- **IPC**: Gateway↔Runtime via gRPC (not legacy Unix Socket)
