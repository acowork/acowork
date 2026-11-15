// Adapted from zeroclaw/src/tools/mcp_tool.rs
// ACowork deviation: implements acowork_core::tools::Tool trait instead
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;

use async_trait::async_trait;

use crate::client::McpRegistry;
use crate::protocol::McpToolDef;

/// Classify an MCP error as permanent, transient, or permission-related.
///
/// This classification is used to prefix error messages returned to the agent,
/// enabling the LLM to make intelligent retry decisions:
/// - `[permanent]` — the tool call will never succeed (unknown tool, bad params)
/// - `[transient]` — the tool call might succeed on retry (connection lost, timeout)
/// - `[permission]` — the tool call failed due to access/auth issues
fn classify_mcp_error(msg: &str) -> &'static str {
    let lower = msg.to_ascii_lowercase();

    // Permanent: the request itself is invalid
    if lower.contains("unknown mcp tool")
        || lower.contains("invalid params")
        || lower.contains("invalid_params")
        || lower.contains("method not found")
        || lower.contains("method_not_found")
        || lower.contains("rejected initialize")
        || lower.contains("no result")
    {
        return "permanent";
    }

    // Permission: auth/access issues
    if lower.contains("unauthorized")
        || lower.contains("forbidden")
        || lower.contains("permission")
        || lower.contains("access denied")
        || lower.contains("authentication")
    {
        return "permission";
    }

    // Transient: connection/transport issues (default for IO failures)
    if lower.contains("disconnected")
        || lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("closed stdout")
        || lower.contains("connection")
        || lower.contains("failed to create transport")
        || lower.contains("error during tool call")
        || lower.contains("reconnect")
        || lower.contains("http 5")
        || lower.contains("http 502")
        || lower.contains("http 503")
        || lower.contains("http 504")
    {
        return "transient";
    }

    // Default: transient — safer to retry than to give up
    "transient"
}

/// A ACowork [`Tool`](acowork_core::tools::Tool) backed by an MCP server tool.
///
/// The `prefixed_name` (e.g. `mcp_filesystem__read_file`) is what the agent sees.
/// The `mcp_` prefix is a stable marker that distinguishes MCP tools from built-in
/// tools in the tool registry, and is used for filtering during rebuilds.
/// The registry knows how to route it to the correct server.
pub struct McpToolWrapper {
    /// Prefixed name: `<server_name>__<tool_name>`.
    prefixed_name: String,
    /// Description extracted from the MCP tool definition.
    description: String,
    /// JSON schema for the tool's input parameters.
    input_schema: serde_json::Value,
    /// Shared registry — used to dispatch actual tool calls.
    registry: Arc<McpRegistry>,
}

impl McpToolWrapper {
    pub fn new(prefixed_name: String, def: McpToolDef, registry: Arc<McpRegistry>) -> Self {
        let description = def.description.unwrap_or_else(|| "MCP tool".to_string());
        Self {
            prefixed_name,
            description,
            input_schema: def.input_schema,
            registry,
        }
    }
}

#[async_trait]
impl acowork_core::tools::Tool for McpToolWrapper {
    fn spec(&self) -> acowork_core::tools::ToolSpec {
        acowork_core::tools::ToolSpec {
            name: self.prefixed_name.clone(),
            description: self.description.clone(),
            input_schema: self.input_schema.clone(),
        }
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _work_dir: Option<&str>,
    ) -> acowork_core::error::Result<acowork_core::tools::ToolResult> {
        // Strip the `approved` field before forwarding to the MCP server.
        // ACowork's security model may inject `approved: bool` into tool calls
        // for approval flows. MCP servers are unaware of this field.
        let args = match params {
            serde_json::Value::Object(mut map) => {
                map.remove("approved");
                serde_json::Value::Object(map)
            }
            other => other,
        };

        match self.registry.call_tool(&self.prefixed_name, args).await {
            Ok(output) => Ok(acowork_core::tools::ToolResult {
                content_parts: None,
                ok: true,
                content: output,
                error: None,
                token_usage: None,
            }),
            Err(e) => {
                let msg = e.to_string();
                let classified = classify_mcp_error(&msg);
                Ok(acowork_core::tools::ToolResult {
                    content_parts: None,
                    ok: false,
                    content: String::new(),
                    error: Some(format!("[{}] {}", classified, msg)),
                    token_usage: None,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acowork_core::tools::Tool;
    use serde_json::json;

    fn make_def(name: &str, description: Option<&str>, schema: serde_json::Value) -> McpToolDef {
        McpToolDef {
            name: name.to_string(),
            description: description.map(str::to_string),
            input_schema: schema,
        }
    }

    async fn empty_registry() -> Arc<McpRegistry> {
        let (registry, _failures) = McpRegistry::connect_all(&[])
            .await
            .expect("empty connect_all should succeed");
        Arc::new(registry)
    }

    #[tokio::test]
    async fn spec_returns_prefixed_name_and_description() {
        let registry = empty_registry().await;
        let def = make_def("read_file", Some("Reads a file"), json!({}));
        let wrapper = McpToolWrapper::new("mcp_filesystem__read_file".to_string(), def, registry);
        let spec = wrapper.spec();
        assert_eq!(spec.name, "mcp_filesystem__read_file");
        assert_eq!(spec.description, "Reads a file");
    }

    #[tokio::test]
    async fn description_falls_back_to_default_when_none() {
        let registry = empty_registry().await;
        let def = make_def("mystery", None, json!({}));
        let wrapper = McpToolWrapper::new("mcp_srv__mystery".to_string(), def, registry);
        let spec = wrapper.spec();
        assert_eq!(spec.description, "MCP tool");
    }

    #[tokio::test]
    async fn spec_returns_input_schema_as_parameters() {
        let registry = empty_registry().await;
        let schema = json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"]
        });
        let def = make_def("read_file", Some("Read"), schema.clone());
        let wrapper = McpToolWrapper::new("mcp_fs__read_file".to_string(), def, registry);
        let spec = wrapper.spec();
        assert_eq!(spec.input_schema, schema);
    }

    #[tokio::test]
    async fn execute_returns_non_fatal_error_for_unknown_tool() {
        let registry = empty_registry().await;
        let def = make_def("ghost", Some("Ghost tool"), json!({}));
        let wrapper = McpToolWrapper::new("mcp_nowhere__ghost".to_string(), def, registry);
        let result = wrapper
            .execute(json!({}), None)
            .await
            .expect("execute should be non-fatal");
        assert!(!result.ok);
        let err_msg = result.error.expect("error message should be present");
        assert!(
            err_msg.contains("unknown MCP tool"),
            "unexpected error: {err_msg}"
        );
        // Unknown tool errors should be classified as permanent
        assert!(
            err_msg.starts_with("[permanent]"),
            "expected [permanent] prefix, got: {err_msg}"
        );
        assert!(result.content.is_empty());
    }

    #[tokio::test]
    async fn execute_strips_approved_field_from_object_args() {
        let registry = empty_registry().await;
        let def = make_def("do_thing", Some("Do a thing"), json!({}));
        let wrapper = McpToolWrapper::new("mcp_srv__do_thing".to_string(), def, registry);
        let result = wrapper
            .execute(json!({ "approved": true, "param": "value" }), None)
            .await
            .expect("execute must be non-fatal even with approved field");
        assert!(!result.ok);
        let err = result.error.unwrap_or_default();
        assert!(
            !err.to_lowercase().contains("approved"),
            "approved field should have been stripped, but got: {err}"
        );
    }

    #[tokio::test]
    async fn execute_handles_non_object_args_without_panic() {
        let registry = empty_registry().await;
        let def = make_def("noop", None, json!({}));
        let wrapper = McpToolWrapper::new("mcp_srv__noop".to_string(), def, registry);
        for non_obj in [json!(null), json!("a string"), json!([1, 2, 3])] {
            let result = wrapper
                .execute(non_obj.clone(), None)
                .await
                .expect("non-object args must not propagate Err");
            assert!(!result.ok, "expected non-fatal failure for {non_obj}");
        }
    }

    #[test]
    fn classify_unknown_tool_as_permanent() {
        assert_eq!(classify_mcp_error("unknown MCP tool `foo`"), "permanent");
    }

    #[test]
    fn classify_timeout_as_transient() {
        assert_eq!(
            classify_mcp_error("MCP server timed out after 30s"),
            "transient"
        );
    }

    #[test]
    fn classify_disconnect_as_transient() {
        assert_eq!(
            classify_mcp_error("MCP server is disconnected"),
            "transient"
        );
    }

    #[test]
    fn classify_unauthorized_as_permission() {
        assert_eq!(
            classify_mcp_error("Unauthorized access to resource"),
            "permission"
        );
    }

    #[test]
    fn classify_unknown_error_as_transient_default() {
        assert_eq!(
            classify_mcp_error("something unexpected happened"),
            "transient"
        );
    }
}
