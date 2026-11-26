//! WASM tool sandbox module
//!
//! Provides Wasmtime-based sandboxed execution for custom tools.
//! WASM tools are loaded from .bot packages and run in isolated
//! instances with fuel metering, memory limits, and WASI capabilities.
//!
//! Architecture:
//! - `engine` — WasmEngine singleton (Engine + Config management)
//! - `instance` — WasmToolInstance lifecycle (load, execute, destroy)
//! - `host` — Host function registration (execute, schema)
//! - `wasi_mapper` — Permission → WASI capability mapping
//! - `sandbox` — WASI sandbox configuration builder
//! - `wit` — WIT component model interface (Phase 3+)
//! - `component` — Component model loader (Phase 3+)

pub mod component;
pub mod engine;
pub mod host;
pub mod instance;
pub mod sandbox;
pub mod wasi_mapper;
pub mod wit;
