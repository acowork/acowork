//! acowork-node — the Node Agent (ADR-055).
//!
//! A lightweight per-machine daemon that hosts Agent Runtime processes,
//! manages agent packages locally, and speaks the node control plane
//! (`acowork/nodes/#`) with the Gateway over MQTT.
//!
//! ## Dependency red line (ADR-055 §6.20)
//!
//! This crate MUST NOT depend on `acowork-gateway`. Gateway-internal
//! types (`GatewayState` / `SharedState` / `GatewayError`) carry the
//! whole monolith's coupling; code migrated from the Gateway's
//! `lifecycle/` and `package_manager/` modules must be re-based on
//! [`state::NodeState`] and [`error::NodeError`] instead. Enforced by
//! `tests/dependency_redline.rs` and `dev/ci.sh`.
//!
//! ## Module layout (§6.20 — module boundaries are the migration
//! landing points)
//!
//! ```text
//! src/
//! ├── identity/     # identity.json + enrollment state machine (§6.12)
//! ├── control/      # MQTT node control plane (§6.2): topics,
//! │                 # request_id dedup, idempotent commands, events
//! ├── process/      # Runtime process table + spawn/kill/reap +
//! │                 # re-adopt (Phase 2b — migrated from gateway lifecycle/)
//! ├── package/      # install/uninstall/clone/skills/avatar local ops
//! │                 # (Phase 2b — migrated from gateway package_manager/)
//! ├── proxy/        # :19900 node reverse proxy + node token auth
//! │                 # (Phase 2c)
//! ├── sidecar/      # LSP relay supervisor (Phase 4 — migrated from
//! │                 # gateway lifecycle/lsp_relay_supervisor.rs)
//! ├── fs_browse.rs  # node-local filesystem browsing (Phase 3, L7-1)
//! ├── lsp_registry.rs # LSP server registry — the single writer of
//! │                 # `{node_home}/lsp_servers.json` (ADR-091)
//! ├── lsp_api.rs    # `/lsp/...` registry HTTP API (ADR-091 §5.5)
//! ├── lsp_install.rs # install-script discovery + execution (ADR-091 D15)
//! ├── lsp_converge.rs # relay-probe → persisted install state (ADR-091 §5.8)
//! ├── node_api.rs   # node-local identity HTTP (rename, ADR-075 D4)
//! ├── memory_migration.rs # one-shot hand-over of the legacy workspace
//! │                 # memory store to the memory service (ADR-100 decision 10)
//! ├── state.rs      # NodeState — the GatewayState replacement
//! └── cli.rs        # §6.13.2 command surface (thin orchestration shell)
//! ```

pub mod package_http;
pub mod cli;
pub mod config;
pub mod control;
pub mod error;
pub mod fs_browse;
pub mod identity;
pub mod lsp_api;
pub mod lsp_converge;
pub mod lsp_install;
pub mod lsp_registry;
pub mod memory_migration;
pub mod node_api;
pub mod package;
pub mod process;
pub mod proxy;
pub mod service;
pub mod sidecar;
pub mod state;
