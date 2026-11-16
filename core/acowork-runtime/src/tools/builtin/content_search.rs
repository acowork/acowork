//! Content search tool — regex search in file contents using ripgrep's ignore crate

use acowork_core::tools::traits::{Tool, ToolResult, ToolSpec};
use async_trait::async_trait;
use ignore::WalkBuilder;
use serde_json::Value;
use std::collections::HashSet;
use std::path::Path;

use crate::tools::output;
use crate::tools::path_utils;
use crate::tools::workspace_resolver::SharedResolver;

const DEFAULT_MAX_RESULTS: usize = 1000;

pub struct ContentSearchTool {
    resolver: SharedResolver,
}

impl ContentSearchTool {
    pub fn new(resolver: &SharedResolver) -> Self {
        Self {
            resolver: resolver.clone(),
        }
    }

    pub fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "content_search".to_string(),
            description: "Search file contents using a regex pattern. Returns matching lines with file paths, context, and summary. \
                 IMPORTANT: Use specific 'include' glob (e.g. '*.rs', '*.tsx') to limit scanned files \
                 and a focused regex pattern to avoid oversized results. Results default to 1000 max; \
                 use 'output_mode': 'files_with_matches' for a compact file list when exploring broadly.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Regex pattern to search for"
                    },
                    "path": {
                        "type": "string",
                        "description": "Optional subdirectory to search in (relative to workspace)"
                    },
                    "output_mode": {
                        "type": "string",
                        "description": "Output format: 'content' (matching lines), 'files_with_matches' (paths only), 'count' (match counts per file)",
                        "enum": ["content", "files_with_matches", "count"],
                        "default": "content"
                    },
                    "include": {
                        "type": "string",
                        "description": "File glob filter, e.g. '*.rs', '*.{ts,tsx}'"
                    },
                    "case_sensitive": {
                        "type": "boolean",
                        "description": "Case-sensitive matching. Defaults to true",
                        "default": true
                    },
                    "context_before": {
                        "type": "integer",
                        "description": "Lines of context before each match (content mode only)",
                        "default": 0
                    },
                    "context_after": {
                        "type": "integer",
                        "description": "Lines of context after each match (content mode only)",
                        "default": 0
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Maximum number of results. Defaults to 1000",
                        "default": 1000
                    }
                },
                "required": ["pattern"]
            }),
        }
    }
}

#[async_trait]
impl Tool for ContentSearchTool {
    fn spec(&self) -> ToolSpec {
        Self::spec_value()
    }

    async fn execute(
        &self,
        params: Value,
        work_dir: Option<&str>,
    ) -> acowork_core::error::Result<ToolResult> {
        let resolver_ref = self.resolver.read().unwrap();
        let effective_work_dir = work_dir.unwrap_or(resolver_ref.agent_home());
        let pattern = params["pattern"].as_str().unwrap_or("");
        if pattern.is_empty() {
            return Ok(ToolResult {
                content_parts: None,
                ok: false,
                content: String::new(),
                error: Some("Missing 'pattern'".to_string()),
                token_usage: None,
            });
        }

        let output_mode = params["output_mode"].as_str().unwrap_or("content");
        if !matches!(output_mode, "content" | "files_with_matches" | "count") {
            return Ok(ToolResult {
                content_parts: None,
                ok: false,
                content: String::new(),
                error: Some(format!(
                    "Invalid output_mode '{output_mode}'. Allowed: content, files_with_matches, count."
                )),
                token_usage: None,
            });
        }

        let case_sensitive = params["case_sensitive"].as_bool().unwrap_or(true);

        let re = if case_sensitive {
            match regex::Regex::new(pattern) {
                Ok(r) => r,
                Err(e) => {
                    return Ok(ToolResult {
                        content_parts: None,
                        ok: false,
                        content: String::new(),
                        error: Some(format!("Invalid regex: {e}")),
                        token_usage: None,
                    });
                }
            }
        } else {
            match regex::RegexBuilder::new(pattern)
                .case_insensitive(true)
                .build()
            {
                Ok(r) => r,
                Err(e) => {
                    return Ok(ToolResult {
                        content_parts: None,
                        ok: false,
                        content: String::new(),
                        error: Some(format!("Invalid regex: {e}")),
                        token_usage: None,
                    });
                }
            }
        };

        let context_before = params["context_before"].as_u64().unwrap_or(0) as usize;
        let context_after = params["context_after"].as_u64().unwrap_or(0) as usize;

        let max_results = params["max_results"]
            .as_u64()
            .map(|v| v as usize)
            .unwrap_or(DEFAULT_MAX_RESULTS)
            .min(DEFAULT_MAX_RESULTS);

        // Build file filter glob if specified
        let include_glob = params["include"].as_str();
        let file_filter = match include_glob {
            Some(glob_str) if !glob_str.is_empty() => {
                match path_utils::build_glob_set(glob_str, true) {
                    Ok(gs) => Some(gs),
                    Err(e) => {
                        return Ok(ToolResult {
                            content_parts: None,
                            ok: false,
                            content: String::new(),
                            error: Some(format!("Invalid 'include' glob: {e}")),
                            token_usage: None,
                        });
                    }
                }
            }
            _ => None,
        };

        // Determine search directories:
        // - If user specified `path`, resolve it against current_dir
        // - Otherwise, search current_dir only (respecting workspace setting)
        let user_path = params["path"].as_str();
        let search_roots: Vec<std::path::PathBuf> = if let Some(p) = user_path {
            // User specified a subdirectory — resolve against work_dir
            let resolved = Path::new(effective_work_dir).join(p);
            if resolved.exists() {
                vec![resolved]
            } else {
                // Fallback: try resolving against each search_dir
                resolver_ref
                    .search_dirs()
                    .iter()
                    .map(|d| Path::new(d).join(p))
                    .filter(|p| p.exists())
                    .collect()
            }
        } else {
            // No path specified — search current workspace only
            vec![Path::new(effective_work_dir).to_path_buf()]
        };

        if search_roots.is_empty() {
            return Ok(ToolResult {
                content_parts: None,
                ok: true,
                content: "No search directories found".to_string(),
                error: None,
                token_usage: None,
            });
        }

        // Use a HashSet to deduplicate files_with_matches and track unique files
        let mut file_set = HashSet::new();
        let mut results: Vec<String> = Vec::new();
        let mut total_matches: usize = 0;
        let mut truncated = false;

        tracing::info!(
            current_dir = %effective_work_dir,
            user_path = ?user_path,
            search_roots = ?search_roots.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
            include = ?include_glob,
            pattern = %pattern,
            case_sensitive = case_sensitive,
            "content_search: starting walk"
        );

        let mut files_scanned: usize = 0;
        let mut files_skipped_by_glob: usize = 0;
        let mut files_read_failed: usize = 0;

        'outer: for search_root in &search_roots {
            let walker = WalkBuilder::new(search_root)
                .hidden(true)
                .git_ignore(true)
                .git_global(true)
                .git_exclude(true)
                .build();

            for entry in walker {
                let entry = match entry {
                    Ok(e) => e,
                    Err(err) => {
                        tracing::debug!("content_search: walk error: {err}");
                        continue;
                    }
                };

                if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                    continue;
                }

                let path = entry.path();
                // Compute relative path from this search root
                let rel_str =
                    path_utils::normalize_separators(&path_utils::relative_path(path, search_root));
                files_scanned += 1;

                // Apply file filter if specified
                // Match against the file name only, not the full relative path.
                // This way `include="*.rs"` matches `src/main.rs` — users mean
                // "any .rs file", not ".rs files in the root only".
                if let Some(ref filter) = file_filter {
                    let file_name = path
                        .file_name()
                        .map(|n| n.to_string_lossy())
                        .unwrap_or(std::borrow::Cow::Borrowed(""));
                    if !filter.is_match(file_name.as_ref()) {
                        files_skipped_by_glob += 1;
                        if files_skipped_by_glob <= 5 {
                            tracing::debug!(
                                path = %path.display(),
                                rel = %rel_str,
                                file_name = %file_name,
                                "content_search: file skipped by include glob"
                            );
                        }
                        continue;
                    }
                }

                // Log first 5 matched files
                if files_scanned <= 10 {
                    tracing::debug!(
                        path = %path.display(),
                        rel = %rel_str,
                        "content_search: file passed glob filter"
                    );
                }

                let content = match std::fs::read_to_string(path) {
                    Ok(c) => c,
                    Err(_) => {
                        files_read_failed += 1;
                        continue;
                    }
                };

                match output_mode {
                    "files_with_matches" => {
                        if content.lines().find(|line| re.is_match(line)).is_some()
                            && file_set.insert(rel_str.clone())
                        {
                            results.push(rel_str);
                            total_matches += 1;
                            if results.len() >= max_results {
                                truncated = true;
                                break 'outer;
                            }
                        }
                    }
                    "count" => {
                        let count = content.lines().filter(|line| re.is_match(line)).count();
                        if count > 0 {
                            results.push(format!("{}:{}", rel_str, count));
                            file_set.insert(rel_str);
                            total_matches += count;
                            if results.len() >= max_results {
                                truncated = true;
                                break 'outer;
                            }
                        }
                    }
                    _ => {
                        // content mode
                        let lines: Vec<&str> = content.lines().collect();
                        let mut match_indexes = Vec::new();
                        for (i, line) in lines.iter().enumerate() {
                            if re.is_match(line) {
                                match_indexes.push(i);
                            }
                        }

                        if match_indexes.is_empty() {
                            continue;
                        }

                        file_set.insert(rel_str.clone());

                        // Build output with context
                        for &match_line in &match_indexes {
                            if results.len() >= max_results {
                                truncated = true;
                                break 'outer;
                            }

                            let before_start = match_line.saturating_sub(context_before);
                            let after_end = (match_line + context_after + 1).min(lines.len());

                            // For context lines, show as path-line:content.
                            // Non-matching (context) lines are shown raw for
                            // readability; matching lines go through
                            // truncate_line() to cap single-line blow-up.
                            for ctx_line in before_start..after_end {
                                let marker = if ctx_line == match_line { ":" } else { "-" };
                                let content_ref = lines.get(ctx_line).unwrap_or(&"");
                                let content_str = if ctx_line == match_line {
                                    output::truncate_line(content_ref)
                                } else {
                                    content_ref.to_string()
                                };
                                results.push(format!(
                                    "{}{}{}:{}",
                                    rel_str,
                                    marker,
                                    ctx_line + 1,
                                    content_str.trim_end()
                                ));
                            }

                            // Add separator between non-adjacent match blocks in the same file
                            let next_idx =
                                match_indexes
                                    .iter()
                                    .position(|&x| x == match_line)
                                    .map(|pos| {
                                        match_indexes.get(pos + 1).copied().unwrap_or(usize::MAX)
                                    });
                            if let Some(next) = next_idx
                                && next != usize::MAX
                                && next > match_line + context_after + context_before + 1
                                && results.len() < max_results
                            {
                                results.push("--".to_string());
                            }

                            total_matches += 1;
                        }
                    }
                } // end match output_mode
            } // end for entry in walker
        } // end for search_root

        // Build output
        tracing::info!(
            files_scanned,
            files_skipped_by_glob,
            files_read_failed,
            total_matches,
            result_count = results.len(),
            "content_search: walk complete"
        );

        if results.is_empty() {
            return Ok(ToolResult {
                content_parts: None,
                ok: true,
                content: "No matches found".to_string(),
                error: None,
                token_usage: None,
            });
        }

        use std::fmt::Write;
        let mut output = results.join("\n");

        if truncated {
            let _ = write!(
                output,
                "\n\n[Results truncated: showing first {max_results} results]"
            );
        }

        match output_mode {
            "files_with_matches" => {
                let _ = write!(output, "\n\nTotal: {} files", file_set.len());
            }
            "count" => {
                let _ = write!(
                    output,
                    "\n\nTotal: {} matches in {} files",
                    total_matches,
                    file_set.len()
                );
            }
            _ => {
                let _ = write!(
                    output,
                    "\n\nTotal: {} matching lines in {} files",
                    total_matches,
                    file_set.len()
                );
            }
        }

        // Semantic truncation happens here: max_results caps the array
        // and `truncate_line` caps any single minified-JSON line. The
        // byte-level 32 KB hard cap is the OutputBoundedTool wrapper's
        // job — kept centralised so a future change to the cap
        // propagates everywhere without per-tool edits.

        Ok(ToolResult {
            content_parts: None,
            ok: true,
            content: output,
            error: None,
            token_usage: None,
        })
    }
}
