//! File write tool — writes content to files within the workspace

use acowork_core::tools::traits::{Tool, ToolResult, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;

pub struct FileWriteTool;

impl Default for FileWriteTool {
    fn default() -> Self {
        Self::new()
    }
}

impl FileWriteTool {
    pub fn new() -> Self {
        Self
    }

    pub fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "file_write".to_string(),
            description: "Write content to a file. Use mode='overwrite' to create or replace a file, mode='append' to add content to the end of an existing file for chunked writes.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative path to the file" },
                    "content": { "type": "string", "description": "Content to write" },
                    "mode": {
                        "type": "string",
                        "enum": ["overwrite", "append"],
                        "default": "overwrite",
                        "description": "Write mode: 'overwrite' creates or replaces entire file, 'append' adds content to end of existing file"
                    }
                },
                "required": ["path", "content"]
            }),
        }
    }
}

#[async_trait]
impl Tool for FileWriteTool {
    fn spec(&self) -> ToolSpec {
        Self::spec_value()
    }

    async fn execute(
        &self,
        params: Value,
        work_dir: Option<&str>,
    ) -> acowork_core::error::Result<ToolResult> {
        let path = params["path"].as_str().unwrap_or("");
        let content = params["content"].as_str().unwrap_or("");
        let mode = params["mode"].as_str().unwrap_or("overwrite");
        let is_append = mode == "append";
        if path.is_empty() {
            return Ok(ToolResult {
                content_parts: None,
                ok: false,
                content: String::new(),
                error: Some("Missing 'path'".to_string()),
                token_usage: None,
            });
        }

        let full_path = acowork_core::path_utils::resolve(path, work_dir);
        tracing::debug!(
            work_dir = ?work_dir,
            input_path = %path,
            full_path = %full_path.display(),
            exists = full_path.exists(),
            mode,
            "file_write: resolving path"
        );

        if let Some(parent) = full_path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }

        let write_result = if is_append {
            use tokio::io::AsyncWriteExt;
            match tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&full_path)
                .await
            {
                Ok(mut f) => f.write_all(content.as_bytes()).await,
                Err(e) => Err(e),
            }
        } else {
            tokio::fs::write(&full_path, content).await
        };

        match write_result {
            Ok(()) => Ok(ToolResult {
                content_parts: None,
                ok: true,
                content: format!(
                    "{} {} bytes to {path}",
                    if is_append { "Appended" } else { "Written" },
                    content.len()
                ),
                error: None,
                token_usage: None,
            }),
            Err(e) => {
                tracing::warn!(
                    work_dir = ?work_dir,
                    input_path = %path,
                    full_path = %full_path.display(),
                    mode,
                    error = %e,
                    "file_write: failed to write file"
                );
                Ok(ToolResult {
                    content_parts: None,
                    ok: false,
                    content: String::new(),
                    error: Some(format!("Failed to write file: {e}")),
                    token_usage: None,
                })
            }
        }
    }
}
