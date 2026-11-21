//! File write tool — writes content to files within the workspace

use acowork_core::tools::traits::{Tool, ToolContext, ToolResult, ToolSpec};
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

    /// Session-aware entry point: the per-turn deliverables sink lives on
    /// [`ToolContext`], so this is the only place a created file can be
    /// declared to the turn it belongs to.
    async fn execute_with_context(
        &self,
        params: Value,
        ctx: &ToolContext,
    ) -> acowork_core::error::Result<ToolResult> {
        let (result, written) = Self::write_file(params, ctx.work_dir.as_deref()).await;
        if let Some((full_path, existed_before)) = written {
            crate::agent::turn_deliverables::declare_written_file(ctx, &full_path, existed_before);
        }
        result
    }

    async fn execute(
        &self,
        params: Value,
        work_dir: Option<&str>,
    ) -> acowork_core::error::Result<ToolResult> {
        // Direct-call path (tests, non-session callers): no turn to attribute a
        // produced file to, so the second half of the pair is dropped.
        Self::write_file(params, work_dir).await.0
    }
}

impl FileWriteTool {
    /// The write itself, plus the two facts the caller needs to declare the
    /// file to its turn: which absolute path was touched, and whether it
    /// existed beforehand.
    ///
    /// Returned as data rather than reported from here because "who the file
    /// belongs to" is a session question and this function has no session.
    async fn write_file(
        params: Value,
        work_dir: Option<&str>,
    ) -> (
        acowork_core::error::Result<ToolResult>,
        Option<(std::path::PathBuf, bool)>,
    ) {
        let path = params["path"].as_str().unwrap_or("");
        let content = params["content"].as_str().unwrap_or("");
        let mode = params["mode"].as_str().unwrap_or("overwrite");
        let is_append = mode == "append";
        if path.is_empty() {
            return (
                Ok(ToolResult {
                    content_parts: None,
                    ok: false,
                    content: String::new(),
                    error: Some("Missing 'path'".to_string()),
                    token_usage: None,
                }),
                None,
            );
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

        // Read BEFORE the write: after it the answer is always "yes", which
        // would turn every rewrite into a false deliverable.
        let existed_before = full_path.exists();

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
            Ok(()) => (
                Ok(ToolResult {
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
                Some((full_path.clone(), existed_before)),
            ),
            Err(e) => {
                tracing::warn!(
                    work_dir = ?work_dir,
                    input_path = %path,
                    full_path = %full_path.display(),
                    mode,
                    error = %e,
                    "file_write: failed to write file"
                );
                (
                    Ok(ToolResult {
                        content_parts: None,
                        ok: false,
                        content: String::new(),
                        error: Some(format!("Failed to write file: {e}")),
                        token_usage: None,
                    }),
                    None,
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acowork_core::tools::traits::ProducedFileSink;

    fn ctx_with_sink(work_dir: &str) -> (ToolContext, ProducedFileSink) {
        let sink: ProducedFileSink = Default::default();
        let ctx = ToolContext {
            work_dir: Some(work_dir.to_string()),
            produced_files: Some(sink.clone()),
            ..Default::default()
        };
        (ctx, sink)
    }

    fn written(sink: &ProducedFileSink) -> Vec<acowork_core::tools::traits::ProducedFile> {
        sink.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn creating_a_file_declares_it_to_the_turn() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, sink) = ctx_with_sink(dir.path().to_str().unwrap());

        let result = FileWriteTool::new()
            .execute_with_context(
                serde_json::json!({"path": "shot.png", "content": "x"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(result.ok);

        let files = written(&sink);
        assert_eq!(files.len(), 1, "a created file is declared exactly once");
        assert!(files[0].created);
        assert_eq!(files[0].bytes, 1);
        assert_eq!(files[0].mime.as_deref(), Some("image/png"));
        assert!(files[0].abs_path.ends_with("shot.png"));
    }

    #[tokio::test]
    async fn rewriting_an_existing_file_declares_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.ts"), "old").unwrap();
        let (ctx, sink) = ctx_with_sink(dir.path().to_str().unwrap());

        let result = FileWriteTool::new()
            .execute_with_context(serde_json::json!({"path": "a.ts", "content": "new"}), &ctx)
            .await
            .unwrap();
        assert!(result.ok);

        assert!(
            written(&sink).is_empty(),
            "editing what already existed is process, not output"
        );
    }

    #[tokio::test]
    async fn appending_to_a_new_path_still_counts_as_created() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, sink) = ctx_with_sink(dir.path().to_str().unwrap());

        FileWriteTool::new()
            .execute_with_context(
                serde_json::json!({"path": "notes.md", "content": "hi", "mode": "append"}),
                &ctx,
            )
            .await
            .unwrap();

        assert_eq!(written(&sink).len(), 1, "append creating the file is a creation");
    }

    #[tokio::test]
    async fn the_context_free_path_reports_nothing_and_still_writes() {
        let dir = tempfile::tempdir().unwrap();
        let result = FileWriteTool::new()
            .execute(
                serde_json::json!({"path": "b.txt", "content": "yo"}),
                Some(dir.path().to_str().unwrap()),
            )
            .await
            .unwrap();
        assert!(result.ok);
        assert!(dir.path().join("b.txt").exists());
    }
}
