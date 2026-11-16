//! Hosted-application runtime (ADR-116).
//!
//! An app owns its process, HTTP service, front end and data. The Gateway owns
//! registration, supervision, reverse proxying, identity injection, MCP
//! catalog publication and the platform API. This module contains:
//!
//! - `registry` — runtime-state projection shared by every consumer.
//! - `supervisor` — configuration-driven process lifecycle for apps declared
//!   with `process.runtime = "generic"`. Built-in PM/DOC keep their ADR-064
//!   supervisors (`runtime = "legacy"`) because those carry bespoke spawn
//!   arguments and a large identity-injection test suite; both runtimes
//!   project into the same process table.
//! - `proxy` — the only cross-boundary HTTP channel.
//! - `api` / `platform` — discovery and the narrow App → Platform surface.
//! - `ticket` — capability tickets for the sandboxed webview.

pub mod api;
pub mod platform;
pub mod proxy;
pub mod registry;
pub mod supervisor;
pub mod ticket;

#[cfg(test)]
mod framework_tests;
