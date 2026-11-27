//! ACowork Embedding Runtime — ONNX-based embedding service
//! with OpenAI-compatible API.
//!
//! Library crate exposing the core components for reuse and testing.

pub mod config;
pub mod download;
pub mod event_bus;
pub mod model;
pub mod pool;
pub mod registry;
pub mod server;
pub mod shutdown;
pub mod storage;
