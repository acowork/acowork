# ACowork-AI 模块设计 — 总览

> 版本：v1.2 | 更新日期：2026-04-16

---

## 1. 设计原则

### 1.1 Workspace 拆分原则

1. **二进制边界 = Crate 边界**：Gateway 和 Agent Runtime 是不同进程，必须有独立 crate
2. **共享类型独立 crate**：协议消息、manifest 结构等被多个 crate 使用的类型，放入 `acowork-core`
3. **重型依赖隔离**：WASM 运行时（`wasmtime`）、ONNX Runtime（`acowork-embed`）等重型依赖，封装在独立 crate 中以便条件编译和交叉编译
4. **可测试性**：每个 crate 可独立测试，不依赖其他 crate 的运行时

---

## 2. Workspace 结构

```
acowork-ai/
├── Cargo.toml                    # workspace root
├── crates/
│   ├── acowork-core/            # 共享类型、协议、工具 trait
│   ├── acowork-memory/          # MemoryStore trait + 共享记忆类型（v3.4 新增）
│   ├── acowork-runtime/         # Agent Runtime 二进制 + 库
│   ├── acowork-gateway/         # Gateway 二进制 + 库
│   ├── acowork-sqlite/          # SQLite 记忆层 图数据库引擎（实现 MemoryStore trait）
│   ├── acowork-vault/           # 密钥加密存储
│   └── acowork-sign/            # .agent 包签名/验签工具
├── apps/
│   └── acowork-desktop/         # Tauri v2 桌面应用（Phase 5）
│       ├── src-tauri/            # Rust backend (Gateway/Debug 客户端 + 托盘)
│   └── web/                  # React 前端 (四栏布局 UI)
├── docs/                         # 设计文档（公开）
├── docs/_internal/               # 本地归档（gitignored：plan / review / reference / 内部诊断）
├── tests/                        # 集成测试
└── agents/                     # 示例 Agent 包
```

### 2.1 Workspace Cargo.toml

```toml
[workspace]
members = [
    "crates/acowork-core",
    "crates/acowork-memory",
    "crates/acowork-runtime",
    "crates/acowork-gateway",
    "crates/acowork-sqlite",
    "crates/acowork-vault",
    "crates/acowork-sign",
    "apps/acowork-desktop",
]
resolver = "2"

[workspace.package]
version = "0.1.0"
edition = "2024"
license = "Apache-2.0"
rust-version = "1.95"

[workspace.dependencies]
# 异步运行时
tokio = { version = "1.50", default-features = false, features = ["rt-multi-thread", "macros", "time", "net", "io-util", "sync", "process", "io-std", "fs", "signal"] }

# 序列化
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"

# 错误处理
anyhow = "1.0"
thiserror = "2.0"

# 日志
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["fmt", "ansi", "env-filter"] }

# 异步 trait
async-trait = "0.1"

# 加密
chacha20poly1305 = "0.10"
rand = "0.10"

# 配置
toml = "1.0"
directories = "6.0"

# 时间
chrono = { version = "0.4", features = ["clock", "std", "serde"] }

# CLI
clap = { version = "4.5", features = ["derive"] }

# 数据库
rusqlite = { version = "0.37", features = ["bundled"] }

# HTTP
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls", "stream"] }

# ZIP
zip = { version = "8.1", default-features = false, features = ["deflate-flate2"] }

# 并发原语
parking_lot = "0.12"

# UUID
uuid = { version = "1.22", features = ["v4", "std"] }

# 内部 crate 引用
acowork-core = { path = "crates/acowork-core" }
acowork-sqlite = { path = "crates/acowork-sqlite" }
acowork-vault = { path = "crates/acowork-vault" }
acowork-sign = { path = "crates/acowork-sign" }
```

---

> **各 Crate 详细设计**见子文档：
> - [01-core.md](01-core.md) — acowork-core：共享类型与协议
> - [02-runtime.md](02-runtime.md) — acowork-runtime：Agent Runtime
> - [03-gateway.md](03-gateway.md) — acowork-gateway：Gateway
> - [04-sqlite.md](04-sqlite.md) — acowork-sqlite：SQLite 记忆层 图数据库引擎
> - [05-vault-sign.md](05-vault-sign.md) — acowork-vault + acowork-sign
> - [06-architecture.md](06-architecture.md) — 依赖关系、数据流、路线图、编译产物、测试策略
