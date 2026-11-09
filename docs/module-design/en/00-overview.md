# ACowork-AI Module Design — Overview

> Version: v1.2 | Last Updated: 2026-04-16

---

## 1. Design Principles

### 1.1 Workspace Splitting Principles

1. **Binary boundary = Crate boundary**: Gateway and Agent Runtime are different processes, must have independent crates
2. **Shared types in independent crate**: Protocol messages, manifest structures and other types used by multiple crates, placed in `acowork-core`
3. **Heavy dependency isolation**: WASM runtime (wasmtime), ONNX Runtime and other heavy dependencies, encapsulated in independent crates for conditional compilation and cross-compilation
4. **Testability**: Each crate can be tested independently, doesn't depend on other crates' runtimes

---

## 2. Workspace Structure

```
acowork-ai/
├── Cargo.toml                    # workspace root
├── crates/
│   ├── acowork-core/            # shared types, protocol, tool traits
│   ├── acowork-memory/          # MemoryStore trait + shared memory types (v3.4 new)
│   ├── acowork-runtime/         # Agent Runtime binary + library
│   ├── acowork-gateway/         # Gateway binary + library
│   ├── acowork-sqlite/          # SQLite memory backend (implements MemoryProvider trait)
│   ├── acowork-vault/           # encrypted key storage
│   └── acowork-sign/            # .agent package sign/verify tool
├── apps/
│   └── acowork-desktop/         # Tauri v2 desktop app (Phase 5)
│       ├── src-tauri/            # Rust backend (Gateway/Debug client + tray)
│       └── web/                  # React frontend (four-column layout UI)
├── docs/                         # design documents (public)
├── tests/                        # integration tests
└── agents/                     # example Agent packages
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
# async runtime
tokio = { version = "1.50", default-features = false, features = ["rt-multi-thread", "macros", "time", "net", "io-util", "sync", "process", "io-std", "fs", "signal"] }

# serialization
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"

# error handling
anyhow = "1.0"
thiserror = "2.0"

# logging
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["fmt", "ansi", "env-filter"] }

# async trait
async-trait = "0.1"

# encryption
chacha20poly1305 = "0.10"
rand = "0.10"

# config
toml = "1.0"
directories = "6.0"

# time
chrono = { version = "0.4", features = ["clock", "std", "serde"] }

# CLI
clap = { version = "4.5", features = ["derive"] }

# database
rusqlite = { version = "0.37", features = ["bundled"] }

# HTTP
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls", "stream"] }

# ZIP
zip = { version = "8.1", default-features = false, features = ["deflate-flate2"] }

# concurrency primitives
parking_lot = "0.12"

# UUID
uuid = { version = "1.22", features = ["v4", "std"] }

# internal crate references
acowork-core = { path = "crates/acowork-core" }
acowork-sqlite = { path = "crates/acowork-sqlite" }
acowork-vault = { path = "crates/acowork-vault" }
acowork-sign = { path = "crates/acowork-sign" }
```

---

> **Detailed design per Crate** in sub-documents:
> - [01-core.md](01-core.md) — acowork-core: shared types and protocols
> - [02-runtime.md](02-runtime.md) — acowork-runtime: Agent Runtime
> - [03-gateway.md](03-gateway.md) — acowork-gateway: Gateway
> - [04-sqlite.md](04-sqlite.md) — acowork-sqlite: SQLite graph DB engine
> - [05-vault-sign.md](05-vault-sign.md) — acowork-vault + acowork-sign
> - [06-architecture.md](06-architecture.md) — dependencies, data flow, roadmap, build artifacts, testing strategy