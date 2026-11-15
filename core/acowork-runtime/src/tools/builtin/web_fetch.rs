//! Web fetch tool — fetch webpage and convert to plain text

use acowork_core::tools::traits::{Tool, ToolResult, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;
use std::time::Duration;

use crate::config::DEFAULT_TOOL_HTTP_TIMEOUT;

pub struct WebFetchTool {
    client: reqwest::Client,
}

impl WebFetchTool {
    pub fn new() -> Self {
        Self::with_timeout(DEFAULT_TOOL_HTTP_TIMEOUT)
    }

    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .expect("Failed to build WebFetch HTTP client"),
        }
    }
    pub fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "web_fetch".to_string(),
            description: "Fetch a webpage and extract its text content, stripping HTML tags."
                .to_string(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "url": { "type": "string", "description": "URL to fetch" } }, "required": ["url"] }),
        }
    }
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WebFetchTool {
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
                error: Some("Missing 'url'".to_string()),
                token_usage: None,
            });
        }

        match self.client.get(url).send().await {
            Ok(resp) => {
                let html = resp.text().await.unwrap_or_default();
                // Simple HTML-to-text: strip tags
                let text = strip_html_tags(&html);
                let max_chars = 10000;
                let chars: Vec<char> = text.chars().collect();
                let truncated = if chars.len() > max_chars {
                    format!(
                        "{}\n\n[Web page truncated: fetched {} characters, showing first {}.]",
                        chars.iter().take(max_chars).collect::<String>(),
                        chars.len(),
                        max_chars
                    )
                } else {
                    text
                };
                Ok(ToolResult {
                    content_parts: None,
                    ok: true,
                    content: truncated,
                    error: None,
                    token_usage: None,
                })
            }
            Err(e) => Ok(ToolResult {
                content_parts: None,
                ok: false,
                content: String::new(),
                error: Some(format!("Fetch failed: {e}")),
                token_usage: None,
            }),
        }
    }
}

fn strip_html_tags(html: &str) -> String {
    let mut result = String::new();
    let mut in_tag = false;
    let mut in_script = false;
    for ch in html.chars() {
        match ch {
            '<' => {
                in_tag = true;
                if !result.is_empty() && !result.ends_with(' ') && !result.ends_with('\n') {
                    result.push(' ');
                }
            }
            '>' => {
                in_tag = false;
                // Skip script/style content
                if result.to_lowercase().ends_with("<script")
                    || result.to_lowercase().ends_with("<style")
                {
                    in_script = true;
                }
            }
            _ if in_tag => {}
            _ if in_script => {}
            _ => result.push(ch),
        }
        if in_script && result.to_lowercase().ends_with("</script>") {
            in_script = false;
        }
    }
    // Collapse whitespace
    let re = regex::Regex::new(r"\s+").unwrap();
    re.replace_all(&result, " ").trim().to_string()
}
