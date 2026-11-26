//! acowork-tool-sdk — SDK for building ACowork WASM tools
//!
//! Provides the `#[tool]` declarative macro and `ToolInput`/`ToolOutput`
//! types for building WASM tools that run in the ACowork sandbox.
//!
//! # Quick Start
//!
//! ```ignore
//! use acowork_tool_sdk::{tool, ToolInput, ToolOutput, ToolError};
//!
//! #[tool(name = "image_filter")]
//! fn execute(input: ToolInput) -> Result<ToolOutput, ToolError> {
//!     let filter: String = input.get("filter")?;
//!     Ok(ToolOutput::from(json!({"status": "ok"})))
//! }
//! ```
//!
//! # Building
//!
//! ```bash
//! cargo build --target wasm32-wasip2 --release
//! ```
//!
//! The resulting .wasm file goes into the .bot package's `tools/` directory.

pub mod exports;
pub mod tool;

// Re-export core types
pub use tool::{ToolError, ToolInput, ToolOutput};
