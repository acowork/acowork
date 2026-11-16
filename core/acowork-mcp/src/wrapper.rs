// Adapted from zeroclaw/src/tools/mcp_tool.rs
// ACowork deviation: implements acowork_core::tools::Tool trait instead
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;

use acowork_core::providers::traits::{ContentPart, ImageUrlPart};
use async_trait::async_trait;
use base64::Engine;

use crate::client::McpRegistry;
use crate::protocol::McpToolDef;

/// Maximum decoded image payload exposed from an MCP result.
const MAX_MCP_IMAGE_BYTES: usize = 8 * 1024 * 1024;
/// Maximum number of images accepted from one MCP tool result.
const MAX_MCP_IMAGES_PER_RESULT: usize = 6;
/// Maximum combined decoded image payload exposed from one MCP result.
const MAX_MCP_IMAGE_BYTES_PER_RESULT: usize = 24 * 1024 * 1024;
/// MCP image MIME types supported by the provider image pipeline.
const SUPPORTED_IMAGE_MIME_TYPES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

#[derive(Clone, Copy)]
struct ImageLimits {
    per_image_bytes: usize,
    total_bytes: usize,
    count: usize,
}

const MCP_IMAGE_LIMITS: ImageLimits = ImageLimits {
    per_image_bytes: MAX_MCP_IMAGE_BYTES,
    total_bytes: MAX_MCP_IMAGE_BYTES_PER_RESULT,
    count: MAX_MCP_IMAGES_PER_RESULT,
};

/// Convert a raw MCP tool result to the tool's text and multimodal channels.
/// Text blocks are joined in their original relative order; image parts retain
/// their relative order and are injected by the runtime immediately afterward.
fn parse_tool_result(output: &str) -> (String, Option<Vec<ContentPart>>) {
    parse_tool_result_with_limits(output, MCP_IMAGE_LIMITS)
}

fn parse_tool_result_with_limits(
    output: &str,
    limits: ImageLimits,
) -> (String, Option<Vec<ContentPart>>) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(output) else {
        return (output.to_string(), None);
    };
    let Some(items) = value.get("content").and_then(serde_json::Value::as_array) else {
        return (output.to_string(), None);
    };

    let mut text_blocks = Vec::new();
    let mut image_parts = Vec::new();
    let mut warnings = Vec::new();
    let mut total_image_bytes = 0usize;

    for item in items {
        match item.get("type").and_then(serde_json::Value::as_str) {
            Some("text") => {
                if let Some(text) = item.get("text").and_then(serde_json::Value::as_str) {
                    text_blocks.push(text.to_string());
                }
            }
            Some("image") => {
                let mime_type = item
                    .get("mimeType")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                let Some(data) = item.get("data").and_then(serde_json::Value::as_str) else {
                    warnings.push(
                        "MCP image omitted: image data is missing or invalid; ask the MCP tool to return a valid image".to_string(),
                    );
                    continue;
                };
                if data.is_empty() {
                    warnings.push(
                        "MCP image omitted: image data is empty; ask the MCP tool to return a valid image".to_string(),
                    );
                    continue;
                }

                if !SUPPORTED_IMAGE_MIME_TYPES.contains(&mime_type) {
                    warnings.push(format!(
                        "MCP image omitted: unsupported mimeType `{}`; ask the MCP tool for PNG, JPEG, GIF, or WebP output",
                        if mime_type.is_empty() { "(missing)" } else { mime_type }
                    ));
                    continue;
                }

                // Reject by the encoded size before allocating a decoded copy.
                let encoded_limit = limits.per_image_bytes.div_ceil(3) * 4;
                if data.len() > encoded_limit {
                    warnings.push(
                        "MCP image omitted: image too large (> 8 MiB); ask the MCP tool to resize, crop, or re-encode it".to_string(),
                    );
                    continue;
                }

                let decoded = match base64::engine::general_purpose::STANDARD.decode(data) {
                    Ok(decoded) if decoded.len() <= limits.per_image_bytes => decoded,
                    Ok(_) => {
                        warnings.push(
                            "MCP image omitted: image too large (> 8 MiB); ask the MCP tool to resize, crop, or re-encode it".to_string(),
                        );
                        continue;
                    }
                    Err(_) => {
                        warnings.push(
                            "MCP image omitted: invalid base64 data; ask the MCP tool to return a valid image".to_string(),
                        );
                        continue;
                    }
                };
                if image_parts.len() >= limits.count {
                    warnings.push(format!(
                        "MCP image omitted: result exceeds the limit of {} images; ask the MCP tool to return fewer images",
                        limits.count
                    ));
                    continue;
                }
                if total_image_bytes.saturating_add(decoded.len()) > limits.total_bytes {
                    warnings.push(format!(
                        "MCP image omitted: combined image payload exceeds {} MiB; ask the MCP tool to return fewer or smaller images",
                        limits.total_bytes / (1024 * 1024)
                    ));
                    continue;
                }
                total_image_bytes += decoded.len();
                drop(decoded);
                image_parts.push(ContentPart::ImageUrl {
                    image_url: ImageUrlPart {
                        url: format!("data:{mime_type};base64,{data}"),
                        detail: None,
                        width: None,
                        height: None,
                    },
                });
            }
            _ => {}
        }
    }

    let content_parts = (!image_parts.is_empty()).then_some(image_parts);
    // Keep the legacy serialized result byte-for-byte for text-only tools.
    // Once an image is present (or an image needs an actionable downgrade),
    // expose the MCP text blocks directly alongside the image channel.
    if content_parts.is_none() && warnings.is_empty() {
        return (output.to_string(), None);
    }

    text_blocks.extend(warnings);
    (text_blocks.join("\n"), content_parts)
}

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
            Ok(output) => {
                let (content, content_parts) = parse_tool_result(&output);
                Ok(acowork_core::tools::ToolResult {
                    content_parts,
                    ok: true,
                    content,
                    error: None,
                    token_usage: None,
                })
            }
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

    #[tokio::test]
    async fn execute_preserves_image_content_from_mock_mcp_transport() {
        let image_data = base64::engine::general_purpose::STANDARD.encode([1_u8, 2, 3]);
        let registry = crate::client::registry_with_mock_tool_result(json!({
            "content": [
                { "type": "text", "text": "rendered" },
                { "type": "image", "mimeType": "image/png", "data": image_data }
            ]
        }));
        let def = make_def("render", Some("Test image tool"), json!({}));
        let wrapper = McpToolWrapper::new("mcp_test__render".to_string(), def, registry);
        let result = wrapper.execute(json!({}), None).await.unwrap();

        assert!(result.ok);
        assert_eq!(result.content, "rendered");
        let parts = result.content_parts.expect("MCP image must become a part");
        assert_eq!(parts.len(), 1);
        match &parts[0] {
            ContentPart::ImageUrl { image_url } => {
                assert_eq!(image_url.url, format!("data:image/png;base64,{image_data}"))
            }
            other => panic!("expected ImageUrl part, got {other:?}"),
        }
    }

    #[test]
    fn parses_mixed_text_and_image_content_in_stable_order() {
        let image_data = "aGVsbG8=";
        let (content, parts) = parse_tool_result(
            &json!({
                "content": [
                    { "type": "text", "text": "first" },
                    { "type": "image", "mimeType": "image/png", "data": image_data },
                    { "type": "text", "text": "second" }
                ]
            })
            .to_string(),
        );

        assert_eq!(content, "first\nsecond");
        let parts = parts.expect("image part should be preserved");
        assert_eq!(parts.len(), 1);
        match &parts[0] {
            ContentPart::ImageUrl { image_url } => {
                assert_eq!(image_url.url, format!("data:image/png;base64,{image_data}"));
            }
            other => panic!("expected image URL part, got {other:?}"),
        }
    }

    #[test]
    fn parses_text_only_content_without_parts() {
        let text_result = json!({
            "content": [
                { "type": "text", "text": "hello" },
                { "type": "text", "text": "world" }
            ]
        })
        .to_string();
        let (content, parts) = parse_tool_result(&text_result);
        assert_eq!(content, text_result);
        assert!(parts.is_none());
    }

    #[test]
    fn reports_unsupported_and_oversized_images_in_text() {
        let too_large = "A".repeat((MAX_MCP_IMAGE_BYTES.div_ceil(3) * 4) + 4);
        let (content, parts) = parse_tool_result(
            &json!({
                "content": [
                    { "type": "image", "mimeType": "image/svg+xml", "data": "PHN2Zz4=" },
                    { "type": "image", "mimeType": "image/png", "data": too_large }
                ]
            })
            .to_string(),
        );
        assert!(parts.is_none());
        assert!(content.contains("unsupported mimeType"));
        assert!(content.contains("too large"));
        assert!(content.contains("resize, crop, or re-encode"));
    }

    #[test]
    fn rejects_missing_empty_and_non_string_image_data() {
        for image in [
            json!({ "type": "image", "mimeType": "image/png" }),
            json!({ "type": "image", "mimeType": "image/png", "data": "" }),
            json!({ "type": "image", "mimeType": "image/png", "data": 42 }),
        ] {
            let (content, parts) = parse_tool_result(&json!({ "content": [image] }).to_string());
            assert!(parts.is_none());
            assert!(
                content.contains("image data is"),
                "unexpected output: {content}"
            );
        }
    }

    #[test]
    fn enforces_combined_size_and_image_count_limits_per_result() {
        let limits = ImageLimits {
            per_image_bytes: 4,
            total_bytes: 5,
            count: 4,
        };
        let data_4 = base64::engine::general_purpose::STANDARD.encode([1, 2, 3, 4]);
        let data_2 = base64::engine::general_purpose::STANDARD.encode([5, 6]);
        let output = json!({
            "content": [
                { "type": "image", "mimeType": "image/png", "data": data_4 },
                { "type": "image", "mimeType": "image/png", "data": data_2 },
                { "type": "image", "mimeType": "image/png", "data": data_2 },
                { "type": "image", "mimeType": "image/png", "data": data_2 }
            ]
        })
        .to_string();
        let (content, parts) = parse_tool_result_with_limits(&output, limits);
        assert!(content.contains("combined image payload exceeds"));
        assert_eq!(
            parts.expect("only first image fits the byte budget").len(),
            1
        );

        let count_limited = ImageLimits {
            per_image_bytes: 4,
            total_bytes: 100,
            count: 2,
        };
        let (count_content, count_parts) = parse_tool_result_with_limits(&output, count_limited);
        assert!(count_content.contains("limit of 2 images"));
        assert_eq!(
            count_parts
                .expect("two images should fit the count limit")
                .len(),
            2
        );
    }

    #[test]
    fn reports_invalid_image_base64() {
        let (content, parts) = parse_tool_result(
            &json!({
                "content": [{ "type": "image", "mimeType": "image/png", "data": "not base64!" }]
            })
            .to_string(),
        );
        assert!(parts.is_none());
        assert!(content.contains("invalid base64 data"));
    }

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
