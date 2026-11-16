//! HTTP request tool — perform HTTP requests (GET/POST/PUT/DELETE)
//!
//! Adapted from zeroclaw/src/tools/http_request.rs
//! ACowork deviation: uses acowork_core::Tool trait; no SecurityPolicy dependency;
//! adds PUT/DELETE methods for flexibility; method parameter controls request type
//! with permission-level granularity via Permission enum.
//! SPDX-License-Identifier: MIT OR Apache-2.0

use acowork_core::tools::traits::{Tool, ToolResult, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;

/// HTTP request tool — perform HTTP requests with method selection
///
/// Supported methods: GET, POST, PUT, DELETE.
/// - GET: retrieves data, no body
/// - POST: creates/submits data with body (JSON/form/raw)
/// - PUT: updates data with body
/// - DELETE: removes data, optional body
pub struct HttpRequestTool {
    client: reqwest::Client,
}

impl HttpRequestTool {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "http_request".to_string(),
            description: "Perform an HTTP request. Supports GET, POST, PUT, and DELETE methods. JSON responses are auto-parsed. Requires network permission.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "method": {
                        "type": "string",
                        "enum": ["GET", "POST", "PUT", "DELETE"],
                        "description": "HTTP method (default: GET)",
                        "default": "GET"
                    },
                    "url": {
                        "type": "string",
                        "description": "The URL to send the request to"
                    },
                    "headers": {
                        "type": "object",
                        "description": "Optional custom headers (key-value pairs)",
                        "additionalProperties": { "type": "string" }
                    },
                    "body": {
                        "description": "Request body (JSON object for application/json, or string for raw body). Used with POST/PUT/DELETE."
                    },
                    "content_type": {
                        "type": "string",
                        "enum": ["json", "form", "raw"],
                        "description": "Body format: 'json' (default), 'form' (application/x-www-form-urlencoded), or 'raw' (text/plain). Used with POST/PUT/DELETE.",
                        "default": "json"
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "Request timeout in seconds (default: 30)",
                        "default": 30
                    }
                },
                "required": ["url"]
            }),
        }
    }

    /// Parse the HTTP method from params, defaulting to GET
    fn parse_method(params: &Value) -> reqwest::Method {
        match params["method"]
            .as_str()
            .unwrap_or("GET")
            .to_uppercase()
            .as_str()
        {
            "POST" => reqwest::Method::POST,
            "PUT" => reqwest::Method::PUT,
            "DELETE" => reqwest::Method::DELETE,
            _ => reqwest::Method::GET,
        }
    }

    /// Build the request body based on content_type and body params
    fn build_request_body(
        req: reqwest::RequestBuilder,
        method: reqwest::Method,
        params: &Value,
    ) -> reqwest::RequestBuilder {
        // GET requests don't have a body
        if method == reqwest::Method::GET {
            return req;
        }

        let content_type = params["content_type"].as_str().unwrap_or("json");
        let body = params.get("body");

        match content_type {
            "form" => {
                if let Some(form_data) = body.and_then(|b| b.as_object()) {
                    let form_pairs: Vec<(String, String)> = form_data
                        .iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect();
                    req.form(&form_pairs)
                } else {
                    req
                }
            }
            "raw" => {
                if let Some(raw) = body.and_then(|b| b.as_str()) {
                    req.header("Content-Type", "text/plain")
                        .body(raw.to_string())
                } else {
                    req
                }
            }
            _ => {
                // "json" — default
                if let Some(json_body) = body {
                    match json_body {
                        Value::String(s) => {
                            // Body is a JSON string — parse it first to avoid
                            // double-encoding (req.json() on a Value::String would
                            // serialize the string as a JSON string value, adding
                            // extra quotes and escaping).
                            match serde_json::from_str::<Value>(s) {
                                Ok(parsed) => req.json(&parsed),
                                Err(_) => {
                                    // Not valid JSON — send as raw string with
                                    // application/json content type
                                    req.header("Content-Type", "application/json")
                                        .body(s.clone())
                                }
                            }
                        }
                        _ => req.json(json_body),
                    }
                } else {
                    req
                }
            }
        }
    }

    /// Process the HTTP response into a ToolResult
    async fn process_response(resp: reqwest::Response, method_str: &str) -> ToolResult {
        let status = resp.status();
        let resp_content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        match resp.text().await {
            Ok(body) => {
                // Auto-parse JSON responses
                let content = if resp_content_type.contains("application/json") {
                    if let Ok(formatted) = serde_json::from_str::<Value>(&body) {
                        serde_json::to_string_pretty(&formatted).unwrap_or(body)
                    } else {
                        body
                    }
                } else {
                    body
                };

                let truncated = if content.len() > 50_000 {
                    let trunc: String = content.chars().take(50_000).collect();
                    format!(
                        "{}\n\n... (truncated, {} chars total)",
                        trunc,
                        content.len()
                    )
                } else {
                    content
                };

                ToolResult {
                    content_parts: None,
                    ok: status.is_success(),
                    content: format!(
                        "HTTP {} {} {}",
                        status.as_u16(),
                        status.canonical_reason().unwrap_or("Unknown"),
                        truncated
                    ),
                    error: if status.is_success() {
                        None
                    } else {
                        Some(format!("HTTP {} {}", method_str, status))
                    },
                    token_usage: None,
                }
            }
            Err(e) => ToolResult {
                content_parts: None,
                ok: false,
                content: String::new(),
                error: Some(format!("Failed to read response body: {e}")),
                token_usage: None,
            },
        }
    }
}

impl Default for HttpRequestTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for HttpRequestTool {
    fn spec(&self) -> ToolSpec {
        Self::spec_value()
    }

    async fn execute(
        &self,
        params: Value,
        _work_dir: Option<&str>,
    ) -> acowork_core::error::Result<ToolResult> {
        let url = params["url"].as_str().unwrap_or("");
        if url.is_empty() {
            return Ok(ToolResult {
                content_parts: None,
                ok: false,
                content: String::new(),
                error: Some("Missing 'url' parameter".to_string()),
                token_usage: None,
            });
        }

        // Validate URL scheme
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Ok(ToolResult {
                content_parts: None,
                ok: false,
                content: String::new(),
                error: Some("URL must start with http:// or https://".to_string()),
                token_usage: None,
            });
        }

        let method = Self::parse_method(&params);
        let method_str = method.to_string().to_uppercase();
        let timeout_secs = params["timeout_secs"].as_u64().unwrap_or(30);

        let req = self
            .client
            .request(method.clone(), url)
            .timeout(std::time::Duration::from_secs(timeout_secs));

        // Add custom headers
        let req = if let Some(headers) = params.get("headers").and_then(|h| h.as_object()) {
            let mut req = req;
            for (key, val) in headers {
                if let Some(v) = val.as_str() {
                    req = req.header(key, v);
                }
            }
            req
        } else {
            req
        };

        // Build body
        let req = Self::build_request_body(req, method, &params);

        // Send request
        match req.send().await {
            Ok(resp) => Ok(Self::process_response(resp, &method_str).await),
            Err(e) => Ok(ToolResult {
                content_parts: None,
                ok: false,
                content: String::new(),
                error: Some(format!("HTTP {method_str} request failed: {e}")),
                token_usage: None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_http_request_spec() {
        let spec = HttpRequestTool::spec_value();
        assert_eq!(spec.name, "http_request");
        assert!(spec.input_schema["properties"]["method"].is_object());
        assert!(spec.input_schema["properties"]["url"].is_object());
        assert!(spec.input_schema["properties"]["body"].is_object());
        assert!(spec.input_schema["properties"]["content_type"].is_object());
    }

    #[test]
    fn test_http_request_default() {
        let tool = HttpRequestTool::default();
        assert_eq!(tool.spec().name, "http_request");
    }

    #[test]
    fn test_parse_method_default() {
        assert_eq!(
            HttpRequestTool::parse_method(&serde_json::json!({})),
            reqwest::Method::GET
        );
    }

    #[test]
    fn test_parse_method_post() {
        assert_eq!(
            HttpRequestTool::parse_method(&serde_json::json!({ "method": "POST" })),
            reqwest::Method::POST
        );
    }

    #[test]
    fn test_parse_method_put() {
        assert_eq!(
            HttpRequestTool::parse_method(&serde_json::json!({ "method": "PUT" })),
            reqwest::Method::PUT
        );
    }

    #[test]
    fn test_parse_method_delete() {
        assert_eq!(
            HttpRequestTool::parse_method(&serde_json::json!({ "method": "DELETE" })),
            reqwest::Method::DELETE
        );
    }

    #[test]
    fn test_parse_method_case_insensitive() {
        assert_eq!(
            HttpRequestTool::parse_method(&serde_json::json!({ "method": "post" })),
            reqwest::Method::POST
        );
    }

    #[tokio::test]
    async fn test_http_request_missing_url() {
        let tool = HttpRequestTool::new();
        let result = tool.execute(serde_json::json!({}), None).await.unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("Missing 'url'"));
    }

    #[tokio::test]
    async fn test_http_request_invalid_scheme() {
        let tool = HttpRequestTool::new();
        let result = tool
            .execute(serde_json::json!({ "url": "ftp://example.com" }), None)
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("http:// or https://"));
    }

    #[test]
    fn test_build_request_body_get_no_body() {
        // GET requests should not have a body
        let tool = HttpRequestTool::new();
        let req = tool.client.get("http://example.com");
        let req = HttpRequestTool::build_request_body(
            req,
            reqwest::Method::GET,
            &serde_json::json!({ "body": { "key": "value" } }),
        );
        // Request builder with no body set — just verify it doesn't panic
        drop(req);
    }
}
