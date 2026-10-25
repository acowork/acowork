//! Tool security wrappers — decorator pattern for tool security
//!
//! Adapted from ZeroClaw's RateLimitedTool + PathGuardedTool pattern.
//! ACowork deviation: uses manifest-driven permission checking
//! instead of ZeroClaw's config-driven security policy.
//! SPDX-License-Identifier: MIT OR Apache-2.0

use acowork_core::tools::traits::{Tool, ToolContext, ToolResult, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

use crate::tools::output;
use crate::tools::path_utils;

// Re-export for backward compatibility — existing tests import from wrappers
pub use crate::tools::workspace_resolver::{
    SharedResolver, WorkspaceAccess, WorkspaceDir, WorkspaceResolver,
};
use std::time::Instant;

/// Wrap a raw tool with the standard decorator stack:
///   1. `OutputBoundedTool` (outermost) — hard-cap tool output at
///      [`output::MAX_OUTPUT_BYTES`] (32 KB). This is the **last line of
///      defence** against context blow-up: any tool that forgets to
///      truncate its own output, or hits an unexpected blow-up (e.g. LSP
///      `references` returning thousands of Locations), is caught here
///      and a clear `TRUNCATED_OUTPUT_MARKER` is appended so the LLM
///      knows to narrow its next call.
///   2. `RateLimitedTool` — per-tool call rate limiting
///   3. `PathGuardedTool` — filesystem path validation
///
/// Per-tool semantic truncation (shell `head+tail`, `MAX_RESULT_COUNT`
/// arrays, single-line `truncate_line`, etc.) still happens **inside**
/// the inner tool — the wrapper is the final safety net, not a
/// replacement. Each tool owns its own truncation strategy because
/// different output shapes call for different recovery hints ("use
/// grep" vs "use start_line/end_line" vs "showing first 1000 results").
///
/// This is the **single source of truth** for how builtin tools are
/// decorated, used by both:
///   - `ToolRegistry::activate()` at agent startup
///   - `SessionManager::register_dynamic_tool()` for ADR-030 sidecar pushes
///
/// Extracting this function eliminates the duplicated wrapping logic that
/// previously existed in both call sites (ADR-030 review ISSUE-3).
pub(crate) fn wrap_with_security_decorators(
    tool: Arc<dyn Tool>,
    resolver: SharedResolver,
    max_calls_per_minute: u32,
) -> Arc<dyn Tool> {
    let path_guarded = Arc::new(PathGuardedTool::new(tool, resolver)) as Arc<dyn Tool>;
    let rate_limited =
        Arc::new(RateLimitedTool::new(path_guarded, max_calls_per_minute)) as Arc<dyn Tool>;
    Arc::new(OutputBoundedTool::new(rate_limited)) as Arc<dyn Tool>
}

/// Output-bounded tool wrapper.
///
/// Sits at the **outermost** layer of the decorator stack. After the
/// inner tool returns, this wrapper enforces the global
/// [`output::MAX_OUTPUT_BYTES`] (32 KB) hard cap on `ToolResult.content`
/// — a single tool call may never contribute more than this to the LLM
/// context, regardless of what the inner tool did or didn't truncate.
///
/// ## Why this exists as a wrapper, not as per-tool logic
///
/// Per-tool `truncate_output` calls were duplicated in seven builtin
/// tools and easy to forget for new tools (the original `codebase`
/// tool shipped without any cap and could dump megabytes of LSP JSON).
/// Centralising the cap in one wrapper:
/// - **Catches every tool** without per-tool audit, including future
///   sidecar tools added via ADR-030.
/// - **Separates concerns**: tools own *semantic* truncation (which
///   knows how to advise the LLM to recover — `grep`, `start_line`,
///   narrower query), the wrapper owns the *byte-level* safety net.
/// - **Single point of change**: if the global cap ever shifts (e.g.
///   to 64 KB for agents with larger context), one constant edit
///   propagates everywhere.
///
/// ## Why we don't truncate `ok=false` results
///
/// Error messages are diagnostics: the LLM needs the full message to
/// understand what went wrong and how to fix the next call. Truncating
/// a "Failed to read file: ..." would defeat the purpose. Error
/// messages are also typically < 1 KB; if one ever grows past
/// 32 KB, that's a bug worth surfacing, not silently masking.
///
/// ## Performance
///
/// One UTF-8-safe byte slice + one `String` allocation per tool call
/// in the worst case. For the common case (output already under the
/// cap), the `len()` check is O(1) and we copy nothing.
pub struct OutputBoundedTool {
    inner: Arc<dyn Tool>,
}

impl OutputBoundedTool {
    /// Wrap any tool with the 32 KB output cap.
    pub fn new(inner: Arc<dyn Tool>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl Tool for OutputBoundedTool {
    fn spec(&self) -> ToolSpec {
        // Spec is identical to the inner tool — the wrapper is
        // transparent to the LLM, which is the whole point.
        self.inner.spec()
    }

    async fn execute(
        &self,
        params: Value,
        work_dir: Option<&str>,
    ) -> acowork_core::error::Result<ToolResult> {
        self.execute_with_context(
            params,
            &ToolContext {
                work_dir: work_dir.map(str::to_string),
                ..Default::default()
            },
        )
        .await
    }

    /// Forward the [`ToolContext`] to the inner tool — the wrapper must
    /// not be the layer that drops it (session-aware tools like
    /// `chat_history_read` / `intent_send` resolve their state from it).
    /// The cap then applies to whatever the inner tool returned.
    async fn execute_with_context(
        &self,
        params: Value,
        ctx: &ToolContext,
    ) -> acowork_core::error::Result<ToolResult> {
        let mut result = self.inner.execute_with_context(params, ctx).await?;
        // Last-line-of-defence cap. Only kicks in when the inner tool
        // forgot to truncate (or was structurally unable to — e.g. an
        // LSP response with thousands of Locations). Per-tool semantic
        // truncation (head+tail, max_results, truncate_line, fail-fast)
        // happens inside the inner tool and produces much smaller
        // outputs, so this branch is the exception, not the norm.
        if result.ok && result.content.len() > output::MAX_OUTPUT_BYTES {
            let (truncated, _was_truncated) = output::truncate_output(&result.content);
            result.content = truncated;
        }
        Ok(result)
    }
}

/// Rate-limited tool wrapper
///
/// Enforces a maximum number of calls per minute for a tool.
/// Returns an error when the rate limit is exceeded.
pub struct RateLimitedTool {
    inner: Arc<dyn Tool>,
    max_calls_per_minute: u32,
    call_times: parking_lot::Mutex<Vec<Instant>>,
}

impl RateLimitedTool {
    pub fn new(inner: Arc<dyn Tool>, max_calls_per_minute: u32) -> Self {
        Self {
            inner,
            max_calls_per_minute,
            call_times: parking_lot::Mutex::new(Vec::new()),
        }
    }

    fn check_rate_limit(&self) -> Result<(), String> {
        let now = Instant::now();
        let cutoff = now - std::time::Duration::from_secs(60);
        let mut times = self.call_times.lock();
        times.retain(|t| *t > cutoff);

        if times.len() >= self.max_calls_per_minute as usize {
            return Err(format!(
                "Rate limit exceeded for tool '{}': max {} calls/minute",
                self.inner.name(),
                self.max_calls_per_minute
            ));
        }

        times.push(now);
        Ok(())
    }
}

#[async_trait]
impl Tool for RateLimitedTool {
    fn spec(&self) -> ToolSpec {
        self.inner.spec()
    }

    async fn execute(
        &self,
        params: Value,
        work_dir: Option<&str>,
    ) -> acowork_core::error::Result<ToolResult> {
        self.execute_with_context(
            params,
            &ToolContext {
                work_dir: work_dir.map(str::to_string),
                ..Default::default()
            },
        )
        .await
    }

    /// Rate-limit check, then forward the [`ToolContext`] untouched —
    /// dropping it here would silently disable every session-aware tool
    /// behind the decorator stack.
    async fn execute_with_context(
        &self,
        params: Value,
        ctx: &ToolContext,
    ) -> acowork_core::error::Result<ToolResult> {
        if let Err(e) = self.check_rate_limit() {
            return Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some(e),
                token_usage: None,
            });
        }
        self.inner.execute_with_context(params, ctx).await
    }
}

/// Path-guarded tool wrapper
///
/// Restricts filesystem tool access to paths within the agent's
/// allowed working directories. Validates path parameters before
/// executing the inner tool.
///
/// Reads workspace directories fresh from the global resolver on every
/// `execute()` call — no local caching. This ensures hot-reload of
/// workspace access changes (e.g., read-only → read-write toggled
/// by the user via the frontend) takes effect immediately without
/// requiring an agent restart.
pub struct PathGuardedTool {
    inner: Arc<dyn Tool>,
    resolver: SharedResolver,
}

impl PathGuardedTool {
    pub fn new(inner: Arc<dyn Tool>, resolver: SharedResolver) -> Self {
        Self { inner, resolver }
    }

    /// Validate that a path is within any of the allowed directories
    ///
    /// Security: prevents path traversal attacks (e.g., "../../etc/passwd")
    /// and prefix-suffix attacks (e.g., "/tmp/agent-workdir-eval/secret").
    /// Uses path component normalization instead of filesystem canonicalize
    /// so it works for paths that don't exist yet.
    ///
    /// Returns the `WorkspaceAccess` level of the **most specific** (longest
    /// prefix) matching allowed directory. This ensures nested directories
    /// with stricter access take precedence over broader parent dirs.
    fn validate_path(&self, path: &str) -> Result<WorkspaceAccess, String> {
        let allowed_dirs = self.resolver.read().unwrap().allowed_dirs().to_vec();

        if allowed_dirs.is_empty() {
            return Err("No workspace directories configured for this agent".to_string());
        }

        let target = std::path::Path::new(path);

        // Track the best (most specific) match: (prefix_length, access)
        let mut best_match: Option<(usize, WorkspaceAccess)> = None;

        // Check against each allowed directory
        for dir in &allowed_dirs {
            let allowed = std::path::Path::new(&dir.path);

            // Resolve relative paths against this allowed dir
            let target_normalized = if target.is_absolute() {
                target.to_path_buf()
            } else {
                allowed.join(target)
            };

            // Normalize path components to resolve ".." and reject traversal
            let normalized = Self::normalize_path(&target_normalized);

            // Reject if normalization failed (e.g., ".." escaped root)
            if normalized.is_none() {
                continue; // Try next allowed dir
            }

            let normalized = normalized.unwrap();

            // Also normalize the allowed dir for consistent comparison
            let allowed_normalized =
                Self::normalize_path(allowed).unwrap_or_else(|| allowed.to_path_buf());

            // Convert to string and normalize separators for cross-platform comparison
            let target_str = path_utils::normalize_separators(&normalized.to_string_lossy());
            let allowed_str =
                path_utils::normalize_separators(&allowed_normalized.to_string_lossy());

            // LOG-001: fires once per candidate allowed-dir in the loop
            // (N lines per file-tool call) — raw iteration noise. Demoted
            // to TRACE; the final allow/reject decision is logged once
            // after the loop (see below).
            tracing::trace!(
                target_path = %target_str,
                allowed_path = %allowed_str,
                "PathGuardedTool: validating path"
            );

            // Ensure target starts with allowed dir + separator to prevent
            // prefix-suffix attacks (e.g., "/tmp/agent-workdir-eval" matching "/tmp/agent-workdir")
            if target_str.starts_with(&allowed_str) {
                // Verify the prefix match ends at a path boundary
                let suffix = &target_str[allowed_str.len()..];
                if suffix.is_empty() || suffix.starts_with('/') || suffix.starts_with('\\') {
                    // This is a valid match — keep the most specific (longest) prefix
                    let current_len = allowed_str.len();
                    let is_better = best_match
                        .as_ref()
                        .is_none_or(|(prev_len, _)| current_len > *prev_len);
                    if is_better {
                        best_match = Some((current_len, dir.access.clone()));
                    }
                }
            }
        }

        // LOG-001: single per-call decision log replacing the N per-candidate
        // lines — carries the resolved access level for debugging path guards.
        let decision = best_match.as_ref().map(|(len, access)| (len, access));
        tracing::debug!(
            target_path = %path,
            matched = decision.is_some(),
            allowed_len = decision.map(|(l, _)| *l).unwrap_or(0),
            "PathGuardedTool: path access resolved"
        );

        // Return the access level of the best match, or error if no match
        best_match.map(|(_, access)| access).ok_or_else(|| {
            format!(
                "Path '{}' is outside all allowed workspace directories",
                path
            )
        })
    }

    /// Normalize a path by resolving ".." components without touching the filesystem.
    /// Returns None if ".." escapes the path root (i.e., path traversal).
    fn normalize_path(path: &std::path::Path) -> Option<std::path::PathBuf> {
        let mut components = Vec::new();
        for comp in path.components() {
            match comp {
                std::path::Component::Prefix(p) => components.push(std::path::Component::Prefix(p)),
                std::path::Component::RootDir => components.push(std::path::Component::RootDir),
                std::path::Component::Normal(c) => components.push(std::path::Component::Normal(c)),
                std::path::Component::CurDir => { /* skip current dir */ }
                std::path::Component::ParentDir => {
                    // Pop the last normal component; fail if we'd escape root
                    let popped = components
                        .iter()
                        .rposition(|c| matches!(c, std::path::Component::Normal(_)));
                    let pos = popped?;
                    components.truncate(pos);
                }
            }
        }
        Some(components.iter().collect())
    }

    /// Check if the wrapped tool is a filesystem tool that needs path validation
    fn is_filesystem_tool(&self) -> bool {
        matches!(
            self.inner.name().as_str(),
            "file_read"
                | "file_write"
                | "file_edit"
                | "glob_search"
                | "content_search"
                | "doc_reader"
        )
    }

    /// Check if the wrapped tool performs write operations
    fn is_write_tool(&self) -> bool {
        matches!(self.inner.name().as_str(), "file_write" | "file_edit")
    }
}

#[async_trait]
impl Tool for PathGuardedTool {
    fn spec(&self) -> ToolSpec {
        self.inner.spec()
    }

    async fn execute(
        &self,
        params: Value,
        work_dir: Option<&str>,
    ) -> acowork_core::error::Result<ToolResult> {
        self.execute_with_context(
            params,
            &ToolContext {
                work_dir: work_dir.map(str::to_string),
                ..Default::default()
            },
        )
        .await
    }

    /// Path guard, then forward the [`ToolContext`] untouched. The guard
    /// itself only reads `ctx.work_dir` (same as the old `work_dir`
    /// argument), so session-aware inner tools keep every other field.
    async fn execute_with_context(
        &self,
        params: Value,
        ctx: &ToolContext,
    ) -> acowork_core::error::Result<ToolResult> {
        let work_dir = ctx.work_dir.as_deref();
        let mut params = params;
        if self.is_filesystem_tool() {
            // Check path parameter
            if let Some(path) = params["path"].as_str().map(|s| s.to_string()) {
                match self.validate_path(&path) {
                    Ok(access) => {
                        // Write tools require ReadWrite access
                        if self.is_write_tool() && access != WorkspaceAccess::ReadWrite {
                            return Ok(ToolResult {
                                ok: false,
                                content: String::new(),
                                error: Some(format!(
                                    "Write access denied for path '{}': directory is read-only",
                                    path
                                )),
                                token_usage: None,
                            });
                        }
                        // Rewrite relative paths to absolute using work_dir.
                        // All filesystem tools route through `path_utils::resolve`
                        // for this exact step, so behaviour stays in lock-step.
                        let abs_path_str = acowork_core::path_utils::resolve(&path, work_dir)
                            .to_string_lossy()
                            .into_owned();
                        // Re-validate the rewritten absolute path to ensure it
                        // still falls within an allowed directory and has the
                        // correct access level for the actual target location.
                        match self.validate_path(&abs_path_str) {
                            Ok(resolved_access) => {
                                if self.is_write_tool()
                                    && resolved_access != WorkspaceAccess::ReadWrite
                                {
                                    return Ok(ToolResult {
                                        ok: false,
                                        content: String::new(),
                                        error: Some(format!(
                                            "Write access denied for resolved path '{}': directory is read-only",
                                            abs_path_str
                                        )),
                                        token_usage: None,
                                    });
                                }
                                params["path"] = serde_json::Value::String(abs_path_str);
                            }
                            Err(e) => {
                                return Ok(ToolResult {
                                    ok: false,
                                    content: String::new(),
                                    error: Some(e),
                                    token_usage: None,
                                });
                            }
                        }
                    }
                    Err(e) => {
                        return Ok(ToolResult {
                            ok: false,
                            content: String::new(),
                            error: Some(e),
                            token_usage: None,
                        });
                    }
                }
            }
        }
        self.inner.execute_with_context(params, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acowork_core::tools::traits::ToolSpec;
    use std::sync::RwLock;

    /// Helper: build a SharedResolver from a Vec of WorkspaceDir for testing.
    fn test_resolver(dirs: Vec<WorkspaceDir>) -> SharedResolver {
        Arc::new(RwLock::new(WorkspaceResolver::new_for_test(dirs)))
    }

    /// A simple test tool for testing wrappers
    struct EchoTool;
    #[async_trait]
    impl Tool for EchoTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "echo".to_string(),
                description: "Echo tool".to_string(),
                input_schema: serde_json::json!({"type": "object"}),
            }
        }
        async fn execute(
            &self,
            params: Value,
            _work_dir: Option<&str>,
        ) -> acowork_core::error::Result<ToolResult> {
            Ok(ToolResult {
                ok: true,
                content: params.to_string(),
                error: None,
                token_usage: None,
            })
        }
    }

    struct FileEchoTool;
    #[async_trait]
    impl Tool for FileEchoTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "file_read".to_string(),
                description: "File read echo".to_string(),
                input_schema: serde_json::json!({"type": "object"}),
            }
        }
        async fn execute(
            &self,
            params: Value,
            _work_dir: Option<&str>,
        ) -> acowork_core::error::Result<ToolResult> {
            Ok(ToolResult {
                ok: true,
                content: format!("Read: {}", params["path"].as_str().unwrap_or("")),
                error: None,
                token_usage: None,
            })
        }
    }

    #[tokio::test]
    async fn test_rate_limited_allows_within_limit() {
        let inner = Arc::new(EchoTool);
        let tool = RateLimitedTool::new(inner, 3);
        for _ in 0..3 {
            let result = tool
                .execute(serde_json::json!({"msg": "hi"}), None)
                .await
                .unwrap();
            assert!(result.ok);
        }
    }

    #[tokio::test]
    async fn test_rate_limited_blocks_over_limit() {
        let inner = Arc::new(EchoTool);
        let tool = RateLimitedTool::new(inner, 2);
        let _ = tool.execute(serde_json::json!({}), None).await;
        let _ = tool.execute(serde_json::json!({}), None).await;
        let result = tool.execute(serde_json::json!({}), None).await.unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("Rate limit"));
    }

    #[tokio::test]
    async fn test_path_guarded_allows_within_dir() {
        let inner = Arc::new(FileEchoTool);
        let tool = PathGuardedTool::new(
            inner,
            test_resolver(vec![WorkspaceDir {
                id: "test-ws".to_string(),
                path: "/tmp/agent-workdir".to_string(),
                access: WorkspaceAccess::ReadWrite,
                last_active: false,
                prompt_file: None,
            }]),
        );
        let result = tool
            .execute(
                serde_json::json!({ "path": "/tmp/agent-workdir/file.txt" }),
                None,
            )
            .await
            .unwrap();
        assert!(result.ok);
    }

    #[tokio::test]
    async fn test_path_guarded_blocks_outside_dir() {
        let inner = Arc::new(FileEchoTool);
        let tool = PathGuardedTool::new(
            inner,
            test_resolver(vec![WorkspaceDir {
                id: "test-ws".to_string(),
                path: "/tmp/agent-workdir".to_string(),
                access: WorkspaceAccess::ReadWrite,
                last_active: false,
                prompt_file: None,
            }]),
        );
        let result = tool
            .execute(serde_json::json!({ "path": "/etc/passwd" }), None)
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(
            result
                .error
                .unwrap()
                .contains("outside all allowed workspace directories")
        );
    }

    #[tokio::test]
    async fn test_path_guarded_relative_path() {
        let inner = Arc::new(FileEchoTool);
        let tool = PathGuardedTool::new(
            inner,
            test_resolver(vec![WorkspaceDir {
                id: "test-ws".to_string(),
                path: "/tmp/agent-workdir".to_string(),
                access: WorkspaceAccess::ReadWrite,
                last_active: false,
                prompt_file: None,
            }]),
        );
        let result = tool
            .execute(serde_json::json!({ "path": "subdir/file.txt" }), None)
            .await
            .unwrap();
        assert!(result.ok); // relative path resolved within allowed dir
    }

    #[tokio::test]
    async fn test_path_guarded_non_filesystem_tool() {
        let inner = Arc::new(EchoTool);
        let tool = PathGuardedTool::new(
            inner,
            test_resolver(vec![WorkspaceDir {
                id: "test-ws".to_string(),
                path: "/tmp/agent-workdir".to_string(),
                access: WorkspaceAccess::ReadWrite,
                last_active: false,
                prompt_file: None,
            }]),
        );
        // echo is not a filesystem tool, so no path check
        let result = tool
            .execute(serde_json::json!({ "path": "/etc/passwd" }), None)
            .await
            .unwrap();
        assert!(result.ok); // Not checked because not a filesystem tool
    }

    #[tokio::test]
    async fn test_path_guarded_blocks_traversal() {
        let inner = Arc::new(FileEchoTool);
        let tool = PathGuardedTool::new(
            inner,
            test_resolver(vec![WorkspaceDir {
                id: "test-ws".to_string(),
                path: "/tmp/agent-workdir".to_string(),
                access: WorkspaceAccess::ReadWrite,
                last_active: false,
                prompt_file: None,
            }]),
        );
        // Path traversal via ".." resolves to /etc/passwd which is outside allowed dir
        let result = tool
            .execute(
                serde_json::json!({ "path": "/tmp/agent-workdir/../../etc/passwd" }),
                None,
            )
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(
            result
                .error
                .unwrap()
                .contains("outside all allowed workspace directories")
        );
    }

    #[tokio::test]
    async fn test_path_guarded_blocks_prefix_suffix_attack() {
        let inner = Arc::new(FileEchoTool);
        let tool = PathGuardedTool::new(
            inner,
            test_resolver(vec![WorkspaceDir {
                id: "test-ws".to_string(),
                path: "/tmp/agent-workdir".to_string(),
                access: WorkspaceAccess::ReadWrite,
                last_active: false,
                prompt_file: None,
            }]),
        );
        // Prefix-suffix attack: "/tmp/agent-workdir-eval" starts with "/tmp/agent-workdir"
        let result = tool
            .execute(
                serde_json::json!({ "path": "/tmp/agent-workdir-eval/secret" }),
                None,
            )
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(
            result
                .error
                .unwrap()
                .contains("outside all allowed workspace directories")
        );
    }

    #[tokio::test]
    async fn test_readonly_allows_read() {
        // A file_read tool should be allowed in a ReadOnly directory
        let inner = Arc::new(FileEchoTool);
        let tool = PathGuardedTool::new(
            inner,
            test_resolver(vec![WorkspaceDir {
                id: "test-ws".to_string(),
                path: "/tmp/agent-pkg".to_string(),
                access: WorkspaceAccess::ReadOnly,
                last_active: false,
                prompt_file: None,
            }]),
        );
        let result = tool
            .execute(
                serde_json::json!({ "path": "/tmp/agent-pkg/manifest.toml" }),
                None,
            )
            .await
            .unwrap();
        assert!(result.ok);
    }

    #[tokio::test]
    async fn test_readonly_blocks_write() {
        // A file_write tool should be blocked in a ReadOnly directory
        struct FileWriteEchoTool;
        #[async_trait]
        impl Tool for FileWriteEchoTool {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: "file_write".to_string(),
                    description: "File write echo".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                }
            }
            async fn execute(
                &self,
                params: Value,
                _work_dir: Option<&str>,
            ) -> acowork_core::error::Result<ToolResult> {
                Ok(ToolResult {
                    ok: true,
                    content: format!("Wrote: {}", params["path"].as_str().unwrap_or("")),
                    error: None,
                    token_usage: None,
                })
            }
        }

        let inner = Arc::new(FileWriteEchoTool);
        let tool = PathGuardedTool::new(
            inner,
            test_resolver(vec![WorkspaceDir {
                id: "test-ws".to_string(),
                path: "/tmp/agent-pkg".to_string(),
                access: WorkspaceAccess::ReadOnly,
                last_active: false,
                prompt_file: None,
            }]),
        );
        let result = tool
            .execute(
                serde_json::json!({ "path": "/tmp/agent-pkg/manifest.toml" }),
                None,
            )
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("read-only"));
    }

    #[tokio::test]
    async fn test_readonly_blocks_file_edit() {
        // A file_edit tool should be blocked in a ReadOnly directory
        struct FileEditEchoTool;
        #[async_trait]
        impl Tool for FileEditEchoTool {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: "file_edit".to_string(),
                    description: "File edit echo".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                }
            }
            async fn execute(
                &self,
                params: Value,
                _work_dir: Option<&str>,
            ) -> acowork_core::error::Result<ToolResult> {
                Ok(ToolResult {
                    ok: true,
                    content: format!("Edited: {}", params["path"].as_str().unwrap_or("")),
                    error: None,
                    token_usage: None,
                })
            }
        }

        let inner = Arc::new(FileEditEchoTool);
        let tool = PathGuardedTool::new(
            inner,
            test_resolver(vec![WorkspaceDir {
                id: "test-ws".to_string(),
                path: "/tmp/agent-pkg".to_string(),
                access: WorkspaceAccess::ReadOnly,
                last_active: false,
                prompt_file: None,
            }]),
        );
        let result = tool
            .execute(
                serde_json::json!({ "path": "/tmp/agent-pkg/prompts/system.md" }),
                None,
            )
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("read-only"));
    }

    #[tokio::test]
    async fn test_nested_readwrite_overrides_readonly() {
        // When a ReadOnly parent and ReadWrite child both match,
        // the more specific (longest prefix) ReadWrite should win.
        // This simulates: package_root=ReadOnly, workspace=ReadWrite
        let inner = Arc::new(FileEchoTool);
        let tool = PathGuardedTool::new(
            inner,
            test_resolver(vec![
                WorkspaceDir {
                    id: "rw".to_string(),
                    path: "/tmp/agent-pkg".to_string(),
                    access: WorkspaceAccess::ReadOnly,
                    last_active: false,
                    prompt_file: None,
                },
                WorkspaceDir {
                    id: "ws".to_string(),
                    path: "/tmp/agent-pkg/workspace".to_string(),
                    access: WorkspaceAccess::ReadWrite,
                    last_active: false,
                    prompt_file: None,
                },
            ]),
        );
        // Read within workspace should succeed (ReadWrite wins)
        let result = tool
            .execute(
                serde_json::json!({ "path": "/tmp/agent-pkg/workspace/file.txt" }),
                None,
            )
            .await
            .unwrap();
        assert!(result.ok);
    }

    #[tokio::test]
    async fn test_nested_readonly_overrides_readwrite() {
        // When a ReadWrite parent and ReadOnly child both match,
        // the more specific (longest prefix) ReadOnly should win.
        let inner = Arc::new(FileEchoTool);
        let _tool = PathGuardedTool::new(
            inner,
            test_resolver(vec![
                WorkspaceDir {
                    id: "rw".to_string(),
                    path: "/tmp/agent-pkg".to_string(),
                    access: WorkspaceAccess::ReadWrite,
                    last_active: false,
                    prompt_file: None,
                },
                WorkspaceDir {
                    id: "ro".to_string(),
                    path: "/tmp/agent-pkg/readonly".to_string(),
                    access: WorkspaceAccess::ReadOnly,
                    last_active: false,
                    prompt_file: None,
                },
            ]),
        );
        // A write tool should be blocked in the nested ReadOnly directory
        struct FileWriteEchoTool2;
        #[async_trait]
        impl Tool for FileWriteEchoTool2 {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: "file_write".to_string(),
                    description: "File write echo".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                }
            }
            async fn execute(
                &self,
                params: Value,
                _work_dir: Option<&str>,
            ) -> acowork_core::error::Result<ToolResult> {
                Ok(ToolResult {
                    ok: true,
                    content: format!("Wrote: {}", params["path"].as_str().unwrap_or("")),
                    error: None,
                    token_usage: None,
                })
            }
        }

        let write_inner = Arc::new(FileWriteEchoTool2);
        let write_tool = PathGuardedTool::new(
            write_inner,
            test_resolver(vec![
                WorkspaceDir {
                    id: "rw".to_string(),
                    path: "/tmp/agent-pkg".to_string(),
                    access: WorkspaceAccess::ReadWrite,
                    last_active: false,
                    prompt_file: None,
                },
                WorkspaceDir {
                    id: "ro".to_string(),
                    path: "/tmp/agent-pkg/readonly".to_string(),
                    access: WorkspaceAccess::ReadOnly,
                    last_active: false,
                    prompt_file: None,
                },
            ]),
        );
        // Write within /tmp/agent-pkg/readonly should be blocked
        let result = write_tool
            .execute(
                serde_json::json!({ "path": "/tmp/agent-pkg/readonly/secret.txt" }),
                None,
            )
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("read-only"));
    }

    // ── OutputBoundedTool wrapper E2E ─────────────────────────────────
    //
    // These tests verify the outermost decorator enforces the global
    // 32 KB hard cap on `ToolResult.content` — the last line of defence
    // against context blow-up. They cover the four behaviours the
    // wrapper must get right:
    //
    //   1. **Oversized content gets capped** with an actionable marker.
    //   2. **Small content passes through verbatim** — no marker added.
    //   3. **Inner tools that already truncated keep their own marker** —
    //      wrapper does NOT clobber a more specific recovery hint.
    //   4. **Error results (ok=false) are NOT capped** — diagnostics
    //      must stay whole for the LLM to understand failure.
    //
    // Plus one static cross-constraint test pinning the wrapper's cap
    // to `output::MAX_OUTPUT_BYTES` (32 KB) — the single source of
    // truth for the global tool output budget.

    /// Mock tool that ignores its params and returns whatever content
    /// its `content_for` field holds. Lets each test parametrise both
    /// success payload size and the ok/error branch without writing a
    /// fresh struct per case.
    struct MockTool {
        name: &'static str,
        content: String,
        ok: bool,
        error: Option<String>,
    }

    #[async_trait]
    impl Tool for MockTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.name.to_string(),
                description: format!("Mock {}", self.name),
                input_schema: serde_json::json!({"type": "object"}),
            }
        }

        async fn execute(
            &self,
            _params: Value,
            _work_dir: Option<&str>,
        ) -> acowork_core::error::Result<ToolResult> {
            Ok(ToolResult {
                ok: self.ok,
                content: self.content.clone(),
                error: self.error.clone(),
                token_usage: None,
            })
        }
    }

    fn mock(content: &str) -> Arc<dyn Tool> {
        Arc::new(MockTool {
            name: "mock",
            content: content.to_string(),
            ok: true,
            error: None,
        })
    }

    #[tokio::test]
    async fn e2e_wrapper_caps_oversized_output_with_marker() {
        // 1 MB of payload — the wrapper must catch this even though no
        // inner tool truncated anything. This is the headline safety net.
        let huge = "x".repeat(1_000_000);
        let wrapped = OutputBoundedTool::new(mock(&huge));
        let result = wrapped.execute(serde_json::json!({}), None).await.unwrap();

        assert!(result.ok, "wrapper must not flip ok=true to false");
        // Wrapper caps at MAX_OUTPUT_BYTES then appends TRUNCATED_OUTPUT_MARKER
        // (~280 bytes). Total content length must be within that budget.
        let total = result.content.len();
        assert!(
            total <= output::MAX_OUTPUT_BYTES + output::TRUNCATED_OUTPUT_MARKER.len() + 16,
            "wrapper exceeded its own budget: {total} bytes"
        );
        // The head (first MAX_OUTPUT_BYTES bytes) must be the original
        // 'x' payload — wrapper preserves content, only cuts + appends.
        let head = &result.content[..output::MAX_OUTPUT_BYTES];
        assert!(
            head.chars().all(|c| c == 'x'),
            "head must be the original payload"
        );
        // And the marker must be present and teach the LLM how to recover.
        assert!(
            result.content.contains("OUTPUT TRUNCATED"),
            "missing truncation marker: {:?}",
            &result.content[output::MAX_OUTPUT_BYTES..]
        );
        assert!(
            result.content.contains("narrow") || result.content.contains("targeted"),
            "marker must give actionable guidance"
        );
    }

    #[tokio::test]
    async fn e2e_wrapper_passes_through_small_output_untouched() {
        // Fast path: small content → no allocation, no marker, byte-for-byte.
        let payload = "shell stdout: 5 KB total\nfinal result: ok\n";
        let wrapped = OutputBoundedTool::new(mock(payload));
        let result = wrapped.execute(serde_json::json!({}), None).await.unwrap();

        assert_eq!(
            result.content, payload,
            "wrapper must not mutate small output"
        );
        assert!(
            !result.content.contains("TRUNCATED"),
            "wrapper must not add a marker to small output"
        );
    }

    #[tokio::test]
    async fn e2e_wrapper_preserves_inner_tool_truncation_marker() {
        // Inner tool (e.g. shell with head+tail) already truncated to
        // 30 KB and added its own marker. Wrapper sees content < 32 KB
        // and must NOT add its generic TRUNCATED_OUTPUT_MARKER on top —
        // the inner marker is more specific (carries a concrete re-query
        // command like `grep -n`).
        let mut content = "x".repeat(30 * 1024);
        content.push_str(
            "\n[... 1024 bytes omitted from middle. \
            Use 'grep -n PATTERN file' to query the missing section.]\n",
        );
        assert!(content.len() < output::MAX_OUTPUT_BYTES);

        let wrapped = OutputBoundedTool::new(mock(&content));
        let result = wrapped.execute(serde_json::json!({}), None).await.unwrap();

        // Inner marker preserved verbatim.
        assert!(
            result.content.contains("omitted from middle"),
            "wrapper clobbered inner tool's marker"
        );
        assert!(
            result.content.contains("grep -n"),
            "inner marker lost its recovery hint"
        );
        // Wrapper's generic marker NOT present (because content was under cap).
        assert!(
            !result
                .content
                .contains("OUTPUT TRUNCATED: exceeded tool-level"),
            "wrapper added its own marker on top of inner marker"
        );
    }

    #[tokio::test]
    async fn e2e_wrapper_does_not_truncate_error_results() {
        // Diagnostics must stay whole — the LLM needs the full message to
        // understand what went wrong. The wrapper only caps ok=true; for
        // ok=false results it leaves content (and error) untouched even if
        // they're enormous.
        let huge_diag = "stack trace line\n".repeat(20_000); // ~400 KB
        let mock_err: Arc<dyn Tool> = Arc::new(MockTool {
            name: "failing",
            content: huge_diag.clone(),
            ok: false,
            error: Some("Failed: out of memory".to_string()),
        });
        let wrapped = OutputBoundedTool::new(mock_err);
        let result = wrapped.execute(serde_json::json!({}), None).await.unwrap();

        assert!(!result.ok);
        // Content untouched.
        assert_eq!(
            result.content.len(),
            huge_diag.len(),
            "wrapper must not truncate ok=false content"
        );
        assert!(
            !result.content.contains("TRUNCATED"),
            "wrapper must not add marker to error result"
        );
        // Error field also untouched.
        assert_eq!(result.error.as_deref(), Some("Failed: out of memory"));
    }

    #[tokio::test]
    async fn e2e_wrapper_exactly_at_cap_is_not_truncated() {
        // Boundary: content at exactly MAX_OUTPUT_BYTES bytes must NOT
        // be truncated. The wrapper uses strict `> MAX`, not `>= MAX`,
        // so a payload that exactly fills the budget passes through.
        let payload = "y".repeat(output::MAX_OUTPUT_BYTES);
        let wrapped = OutputBoundedTool::new(mock(&payload));
        let result = wrapped.execute(serde_json::json!({}), None).await.unwrap();

        assert_eq!(
            result.content.len(),
            output::MAX_OUTPUT_BYTES,
            "exact-cap content must pass through unchanged"
        );
        assert!(
            !result.content.contains("TRUNCATED"),
            "exact-cap content must not trigger the marker"
        );
    }

    #[tokio::test]
    async fn e2e_wrapper_one_byte_over_cap_triggers_truncation() {
        // Boundary inverse: MAX_OUTPUT_BYTES + 1 must be cut down. This
        // pins down the strict-greater-than semantics so a future
        // `>= MAX` off-by-one bug is caught.
        let payload = "y".repeat(output::MAX_OUTPUT_BYTES + 1);
        let wrapped = OutputBoundedTool::new(mock(&payload));
        let result = wrapped.execute(serde_json::json!({}), None).await.unwrap();

        assert!(result.content.contains("OUTPUT TRUNCATED"));
        // First MAX_OUTPUT_BYTES bytes preserved.
        assert!(
            result
                .content
                .starts_with(&"y".repeat(output::MAX_OUTPUT_BYTES))
        );
    }

    #[test]
    fn e2e_wrapper_cap_equals_max_output_bytes_constant() {
        // Cross-constraint: the wrapper's hard cap is the same constant
        // every other tool reads. If this drifts, the LLM context
        // budget is silently violated (e.g. content_search thinks the
        // cap is 100 KB but the wrapper still cuts at 32 KB, or vice
        // versa). The single source of truth is `output::MAX_OUTPUT_BYTES`.
        assert_eq!(
            output::MAX_OUTPUT_BYTES,
            32 * 1024,
            "MAX_OUTPUT_BYTES must stay at 32 KB to match the LLM context budget"
        );
        // And the marker is sized to fit on top of the cap without
        // overshooting it by more than a small constant. If we ever
        // lengthen the marker significantly, this guard catches it.
        assert!(
            output::TRUNCATED_OUTPUT_MARKER.len() < 512,
            "TRUNCATED_OUTPUT_MARKER grew unexpectedly large: {} bytes",
            output::TRUNCATED_OUTPUT_MARKER.len()
        );
    }

    // ── Decorator stack integration tests ───────────────────────────
    //
    // The single-wrapper tests above verify each layer in isolation.
    // These tests verify the *three-layer stack* as a whole — the
    // ordering, the error-message preservation, and the cross-layer
    // interactions that a per-wrapper unit test cannot catch. Bugs
    // like "wrapper truncates a rate-limit error" or "wrapper runs
    // before path-guard" only show up at the integration level.

    /// Wrap a mock inner tool with the full three-layer decorator stack
    /// (OutputBoundedTool → RateLimitedTool → PathGuardedTool → inner).
    /// `allowed` is the list of workspace directories the path-guard
    /// will accept; tools with `file_*`/`content_search`/etc. names
    /// that pass paths outside this list get rejected before reaching
    /// the inner tool.
    fn full_stack(
        inner: Arc<dyn Tool>,
        allowed: &[&str],
        max_calls_per_minute: u32,
    ) -> Arc<dyn Tool> {
        wrap_with_security_decorators(
            inner,
            test_resolver(
                allowed
                    .iter()
                    .enumerate()
                    .map(|(i, p)| WorkspaceDir {
                        id: format!("ws-{i}"),
                        path: (*p).to_string(),
                        access: WorkspaceAccess::ReadOnly,
                        last_active: false,
                        prompt_file: None,
                    })
                    .collect(),
            ),
            max_calls_per_minute,
        )
    }

    /// A mock inner tool that returns a configurable result. Used by
    /// the integration tests as the "innermost" layer of the stack.
    fn mock_inner(content: &str) -> Arc<dyn Tool> {
        mock(content)
    }

    /// A mock inner tool that overrides `execute_with_context` and
    /// echoes the session-scoped fields back — the exact shape of
    /// `chat_history_read` / `intent_send` / `memory_*`. Used to prove
    /// the decorator stack forwards the [`ToolContext`] instead of
    /// dropping it at the outermost layer (the bug that made those
    /// tools hit their context-less `execute()` fallback in production).
    struct CtxEchoTool;
    #[async_trait]
    impl Tool for CtxEchoTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "ctx_echo".to_string(),
                description: "Echo the ToolContext back".to_string(),
                input_schema: serde_json::json!({"type": "object"}),
            }
        }
        async fn execute(
            &self,
            _params: Value,
            _work_dir: Option<&str>,
        ) -> acowork_core::error::Result<ToolResult> {
            // Same contract as the session-aware builtins: reachable only
            // without context — the test below must NOT land here.
            Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some("ctx_echo requires session context".to_string()),
                token_usage: None,
            })
        }
        async fn execute_with_context(
            &self,
            _params: Value,
            ctx: &ToolContext,
        ) -> acowork_core::error::Result<ToolResult> {
            Ok(ToolResult {
                ok: true,
                content: format!(
                    "{}|{}",
                    ctx.session_id.clone().unwrap_or_default(),
                    ctx.chat_session_id.clone().unwrap_or_default()
                ),
                error: None,
                token_usage: None,
            })
        }
    }

    #[tokio::test]
    async fn full_stack_forwards_tool_context_to_inner_tool() {
        // Regression: the decorator stack must dispatch through
        // `execute_with_context` end to end. Before the fix, none of the
        // three wrappers overrode it, so the trait default dropped the
        // context at the outermost layer and session-aware tools only
        // ever saw their bare `execute()` fallback.
        let tool = full_stack(Arc::new(CtxEchoTool), &["/allowed"], 60);
        let ctx = ToolContext {
            work_dir: None,
            session_id: Some("sess-1".to_string()),
            current_user_message: None,
            chat_session_id: Some("cs-1".to_string()),
        };
        let result = tool
            .execute_with_context(serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert!(result.ok, "inner tool never saw the context: {:?}", result.error);
        assert_eq!(result.content, "sess-1|cs-1");
    }

    #[tokio::test]
    async fn bare_execute_through_stack_still_reaches_inner() {
        // The delegation direction must not invert: a wrapper's `execute`
        // funnels into its own `execute_with_context` with a default ctx,
        // so the inner tool is still reached — just with empty session
        // fields. (Session-aware tools must therefore tolerate a default
        // ctx, exactly as `memory_recall` already does.)
        let tool = full_stack(Arc::new(CtxEchoTool), &["/allowed"], 60);
        let result = tool.execute(serde_json::json!({}), None).await.unwrap();
        assert!(result.ok);
        assert_eq!(result.content, "|");
    }

    /// A mock inner tool that returns a 1 MB payload — the headline
    /// scenario the wrapper must catch even with the full stack
    /// (rate-limit + path-guard) ahead of it.
    fn mock_huge_1mb() -> Arc<dyn Tool> {
        mock(&"x".repeat(1_000_000))
    }

    #[tokio::test]
    async fn integration_full_stack_passes_normal_through() {
        // Happy path: small content, no rate limit, no path issue.
        // All three layers must pass and content comes back unchanged.
        let stack = full_stack(mock_inner("hello from inner"), &["/tmp/ws"], 60);
        let r = stack.execute(serde_json::json!({}), None).await.unwrap();
        assert!(r.ok);
        assert_eq!(r.content, "hello from inner");
    }

    #[tokio::test]
    async fn integration_full_stack_caps_huge_inner_output() {
        // Headline scenario: inner dumps 1 MB. The outermost layer
        // (OutputBoundedTool) must catch it and append the marker.
        let stack = full_stack(mock_huge_1mb(), &["/tmp/ws"], 60);
        let r = stack.execute(serde_json::json!({}), None).await.unwrap();
        assert!(r.ok);
        assert!(
            r.content.contains("OUTPUT TRUNCATED"),
            "wrapper at outermost layer must catch 1 MB output"
        );
        // Cap + marker size sanity check.
        assert!(
            r.content.len() <= output::MAX_OUTPUT_BYTES + 400,
            "got {} bytes",
            r.content.len()
        );
    }

    #[tokio::test]
    async fn integration_path_guard_blocks_before_wrapper() {
        // A "file_read"-named tool gets path validation. A path
        // outside the allowed directories is rejected by
        // PathGuardedTool (innermost security layer). The wrapper
        // then sees ok=false and MUST NOT truncate the error message.
        struct FsTool;
        #[async_trait]
        impl Tool for FsTool {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: "file_read".to_string(), // name triggers PathGuardedTool
                    description: "fs tool".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                }
            }
            async fn execute(
                &self,
                _params: Value,
                _work_dir: Option<&str>,
            ) -> acowork_core::error::Result<ToolResult> {
                // If we get here, path-guard failed to block — panic
                // so the test fails loudly.
                panic!("inner must not be called when path-guard rejects")
            }
        }
        let stack = full_stack(Arc::new(FsTool), &["/tmp/allowed"], 60);
        let r = stack
            .execute(serde_json::json!({ "path": "/etc/passwd" }), None)
            .await
            .unwrap();
        assert!(!r.ok);
        let err = r.error.as_deref().expect("path-guard must set error");
        assert!(
            err.contains("outside") || err.contains("allowed"),
            "expected path-guard error, got: {err}"
        );
        // Content field is empty — wrapper did NOT touch it.
        assert!(
            r.content.is_empty(),
            "wrapper must not write to error result"
        );
        // And the wrapper's TRUNCATED_OUTPUT_MARKER must NOT appear.
        assert!(!r.content.contains("OUTPUT TRUNCATED"));
    }

    #[tokio::test]
    async fn integration_rate_limit_blocks_before_wrapper() {
        // Inner tool that increments a counter every time it's
        // called. After max_calls_per_minute, RateLimitedTool (the
        // middle layer) returns ok=false and the inner is never
        // invoked again. The wrapper must see ok=false and skip
        // truncation.
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct CountingTool {
            counter: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl Tool for CountingTool {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: "counter".to_string(),
                    description: "counter".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                }
            }
            async fn execute(
                &self,
                _params: Value,
                _work_dir: Option<&str>,
            ) -> acowork_core::error::Result<ToolResult> {
                let n = self.counter.fetch_add(1, Ordering::SeqCst);
                Ok(ToolResult {
                    ok: true,
                    content: format!("call #{n}"),
                    error: None,
                    token_usage: None,
                })
            }
        }
        let counter = Arc::new(AtomicUsize::new(0));
        let inner = Arc::new(CountingTool {
            counter: counter.clone(),
        });
        let stack = full_stack(inner, &["/tmp/ws"], 2); // 2 calls/min

        // First two calls succeed; inner counter goes 0 → 2.
        for i in 0..2 {
            let r = stack.execute(serde_json::json!({}), None).await.unwrap();
            assert!(r.ok, "call {i} should succeed");
            assert_eq!(r.content, format!("call #{i}"));
        }
        // Third call: rate-limit blocks at RateLimitedTool.
        let r = stack.execute(serde_json::json!({}), None).await.unwrap();
        assert!(!r.ok, "3rd call must be rate-limited");
        assert!(
            r.error.as_deref().unwrap().contains("Rate limit"),
            "expected rate-limit error, got: {:?}",
            r.error
        );
        assert!(
            r.content.is_empty(),
            "wrapper must not write to error result"
        );
        assert!(!r.content.contains("OUTPUT TRUNCATED"));
        // Counter confirms the inner tool was called exactly twice —
        // rate-limit really did block the third call before inner ran.
        assert_eq!(
            counter.load(Ordering::SeqCst),
            2,
            "rate-limit must block before inner is called"
        );
    }

    #[tokio::test]
    async fn integration_spec_is_transparent_through_full_stack() {
        // The wrapper stack must be transparent to the LLM: spec()
        // and name() come from the innermost tool, unchanged through
        // three layers of decoration. If any layer ever rewrote the
        // spec (e.g. renamed the tool or trimmed the description),
        // the LLM's tool-call schema would silently drift from what
        // the runtime actually dispatches — a nasty failure mode.
        let stack = full_stack(
            Arc::new(MockTool {
                name: "chain_check",
                content: "x".into(),
                ok: true,
                error: None,
            }),
            &["/tmp/ws"],
            60,
        );
        assert_eq!(stack.name(), "chain_check");
        assert_eq!(stack.spec().name, "chain_check");
        // Schema must also pass through untouched.
        assert_eq!(
            stack.spec().input_schema,
            serde_json::json!({"type": "object"})
        );
        assert_eq!(stack.spec().description, format!("Mock {}", "chain_check"));
    }

    #[tokio::test]
    async fn integration_outermost_wrapper_runs_last_on_return_path() {
        // On the return path, the inner tool produces content first,
        // then rate-limit, then path-guard, then the wrapper. This
        // means the wrapper sees the *final* ToolResult. We verify
        // by having the inner tool produce exactly MAX_OUTPUT_BYTES
        // bytes — no marker should appear (boundary case), and the
        // wrapper must NOT add its own marker either.
        let boundary_payload = "y".repeat(output::MAX_OUTPUT_BYTES);
        let inner = Arc::new(MockTool {
            name: "boundary",
            content: boundary_payload.clone(),
            ok: true,
            error: None,
        });
        let stack = full_stack(inner, &["/tmp/ws"], 60);
        let r = stack.execute(serde_json::json!({}), None).await.unwrap();
        assert_eq!(r.content.len(), output::MAX_OUTPUT_BYTES);
        assert!(
            !r.content.contains("OUTPUT TRUNCATED"),
            "exact-cap content must pass through all layers untouched"
        );
    }

    // ── Real-builtin-tool integration tests ─────────────────────────
    //
    // The mock-based tests above prove the wrapper behaves correctly
    // for ANY Arc<dyn Tool>. These tests prove the wrapper still works
    // when the inner tool is a REAL builtin whose per-tool
    // truncate_output was removed in this refactor. That matters
    // because a mock can't catch interaction bugs: a tool that reads a
    // different param than the path-guard validates, a tool whose
    // semantic truncation silently regressed, a tool that errors in a
    // way the wrapper mis-handles.
    //
    // Covered tools are the ones whose per-tool truncate_output was
    // deleted (file_read, doc_reader paged path, glob_search,
    // content_search). codebase needs a live LSP relay mock — which would require
    // spawning network servers for marginal coverage over the mock
    // tests above (the wrapper is inner-type-agnostic), so they are
    // intentionally not duplicated here. Their *semantic* truncation
    // logic is separately unit-tested (see codebase.rs shape_lsp_result
    // tests).
    //
    // Each test constructs a real tempdir + file big enough to exceed
    // 32 KB at the wrapper boundary, then asserts the wrapper caps it.

    /// Write `lines` of `width` bytes each (plus a short label) into
    /// `path`, producing a text file large enough to trip the 32 KB
    /// wrapper cap when fully read.
    fn write_big_text_file(path: &std::path::Path, lines: usize, width: usize) {
        use std::io::Write;
        let mut f = std::fs::File::create(path).expect("create big file");
        for i in 0..lines {
            writeln!(f, "line {i} {}", "x".repeat(width)).expect("write line");
        }
    }

    #[tokio::test]
    async fn integration_real_file_read_capped_by_wrapper() {
        // file_read's truncate_output was removed — 100 lines × ~410
        // bytes ≈ 41 KB must be caught by the wrapper, not returned
        // raw to the LLM. (When MAX_LINES_PER_CALL was 400 the scenario
        // used 400×200; we kept the spirit of the test by widening the
        // line payload so 100 lines still exceed the 32 KB cap.)
        let dir = tempfile::TempDir::new().expect("tempdir");
        let file_path = dir.path().join("big.txt");
        write_big_text_file(&file_path, 100, 400);
        assert!(std::fs::metadata(&file_path).unwrap().len() > output::MAX_OUTPUT_BYTES as u64);

        let stack = full_stack(
            Arc::new(crate::tools::builtin::file_read::FileReadTool::new()),
            &[dir.path().to_str().unwrap()],
            60,
        );
        let r = stack
            .execute(
                serde_json::json!({
                    "path": file_path.to_string_lossy(),
                    "start_line": 1,
                    "end_line": 100,
                }),
                None,
            )
            .await
            .unwrap();
        assert!(r.ok, "file_read should succeed: {:?}", r.error);
        assert!(
            r.content.contains("OUTPUT TRUNCATED"),
            "file_read ~41 KB output must be capped by wrapper"
        );
        assert!(
            r.content.len() <= output::MAX_OUTPUT_BYTES + 400,
            "got {} bytes",
            r.content.len()
        );
    }

    #[tokio::test]
    async fn integration_real_doc_reader_paged_capped_by_wrapper() {
        // doc_reader's paged path had truncate_output removed. A paged
        // read of 100 wide lines ≈ 41 KB must be capped by the wrapper.
        let dir = tempfile::TempDir::new().expect("tempdir");
        let file_path = dir.path().join("big.md");
        write_big_text_file(&file_path, 100, 400);

        let stack = full_stack(
            Arc::new(crate::tools::builtin::doc_reader::DocReaderTool::new()),
            &[dir.path().to_str().unwrap()],
            60,
        );
        let r = stack
            .execute(
                serde_json::json!({
                    "path": file_path.to_string_lossy(),
                    "start_line": 1,
                    "end_line": 100,
                }),
                None,
            )
            .await
            .unwrap();
        assert!(r.ok, "doc_reader paged read should succeed: {:?}", r.error);
        assert!(
            r.content.contains("OUTPUT TRUNCATED"),
            "doc_reader paged ~41 KB output must be capped by wrapper"
        );
    }

    #[tokio::test]
    async fn integration_real_glob_search_capped_by_wrapper() {
        // glob_search's truncate_output was removed. 1200 files with
        // long names → 1000 results (MAX_RESULT_COUNT semantic cap) ×
        // ~64-byte paths ≈ 64 KB → wrapper must cap.
        let dir = tempfile::TempDir::new().expect("tempdir");
        // Filenames: "file_<i>_<60 a's>.log" → ~75 chars each, so 1000
        // results ≈ 75 KB, comfortably over 32 KB.
        for i in 0..1200usize {
            let name = format!("file_{i}_{}.log", "a".repeat(60));
            std::fs::write(dir.path().join(name), b"x").expect("write file");
        }

        // glob_search needs a resolver at construction but searches
        // from work_dir, so an empty resolver is fine here.
        let empty = Arc::new(std::sync::RwLock::new(
            crate::tools::workspace_resolver::WorkspaceResolver::new_for_test(vec![]),
        ));
        let stack = full_stack(
            Arc::new(crate::tools::builtin::glob_search::GlobSearchTool::new(
                &empty,
            )),
            &[dir.path().to_str().unwrap()],
            60,
        );
        let work_dir = dir.path().to_str().unwrap();
        let r = stack
            .execute(serde_json::json!({ "pattern": "*.log" }), Some(work_dir))
            .await
            .unwrap();
        assert!(r.ok, "glob_search should succeed: {:?}", r.error);
        assert!(
            r.content.contains("OUTPUT TRUNCATED"),
            "glob_search 64 KB output must be capped by wrapper"
        );
        assert!(
            r.content.len() <= output::MAX_OUTPUT_BYTES + 400,
            "got {} bytes",
            r.content.len()
        );
    }

    #[tokio::test]
    async fn integration_real_content_search_capped_by_wrapper() {
        // content_search's truncate_output was removed. A single log
        // file with 1000 matching lines × ~90 chars ≈ 90 KB → wrapper
        // must cap. (max_results semantic cap stops at 1000 entries;
        // the byte cap is the wrapper's job.)
        let dir = tempfile::TempDir::new().expect("tempdir");
        let file_path = dir.path().join("app.log");
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&file_path).expect("create log");
            for i in 0..1000usize {
                writeln!(f, "line {i} ERROR {}", "z".repeat(80)).expect("write line");
            }
        }

        let empty = Arc::new(std::sync::RwLock::new(
            crate::tools::workspace_resolver::WorkspaceResolver::new_for_test(vec![]),
        ));
        let stack = full_stack(
            Arc::new(crate::tools::builtin::content_search::ContentSearchTool::new(&empty)),
            &[dir.path().to_str().unwrap()],
            60,
        );
        let work_dir = dir.path().to_str().unwrap();
        let r = stack
            .execute(serde_json::json!({ "pattern": "ERROR" }), Some(work_dir))
            .await
            .unwrap();
        assert!(r.ok, "content_search should succeed: {:?}", r.error);
        assert!(
            r.content.contains("OUTPUT TRUNCATED"),
            "content_search 90 KB output must be capped by wrapper"
        );
        assert!(
            r.content.len() <= output::MAX_OUTPUT_BYTES + 400,
            "got {} bytes",
            r.content.len()
        );
    }

    #[tokio::test]
    async fn e2e_wrapper_empty_content_passes_through_untouched() {
        // Empty payload: below the cap, so the fast path must return it
        // byte-for-byte with no marker and no allocation surprises.
        let wrapped = OutputBoundedTool::new(mock(""));
        let result = wrapped.execute(serde_json::json!({}), None).await.unwrap();

        assert!(result.ok);
        assert_eq!(result.content, "", "empty content must stay empty");
        assert!(
            !result.content.contains("OUTPUT TRUNCATED"),
            "empty content must not carry a truncation marker"
        );
    }

    #[tokio::test]
    async fn e2e_wrapper_utf8_multibyte_truncation_is_char_safe() {
        // 20_000 CJK chars × 3 bytes ≈ 60 KB — the 32 KB cut lands in
        // the middle of many multibyte sequences. truncate_utf8 must
        // cut on a char boundary; the wrapper must neither panic nor
        // emit half a character.
        let huge = "字".repeat(20_000);
        let wrapped = OutputBoundedTool::new(mock(&huge));
        let result = wrapped.execute(serde_json::json!({}), None).await.unwrap();

        assert!(result.ok);
        // Budget: cap + ASCII marker + slack.
        assert!(
            result.content.len()
                <= output::MAX_OUTPUT_BYTES + output::TRUNCATED_OUTPUT_MARKER.len() + 16,
            "got {} bytes",
            result.content.len()
        );
        // The marker is ASCII and starts on its own line — find it and
        // verify everything before it is a complete prefix of the CJK
        // payload (no half-cut 3-byte sequence).
        let marker = output::TRUNCATED_OUTPUT_MARKER;
        let marker_at = result
            .content
            .find(marker)
            .expect("oversized multibyte output must carry the marker");
        let head = &result.content[..marker_at];
        assert!(
            head.len() <= output::MAX_OUTPUT_BYTES,
            "head {} bytes must respect the cap",
            head.len()
        );
        assert!(
            head.chars().all(|c| c == '字'),
            "head must be intact CJK chars, got {:?}",
            &head[head.len().saturating_sub(12)..]
        );
        assert_eq!(
            head.len() % 3,
            0,
            "CJK truncation must stay aligned to the 3-byte char width"
        );
    }

    #[tokio::test]
    async fn e2e_wrapper_propagates_inner_panic() {
        // A panicking inner tool must NOT be converted into Ok/Err by
        // the wrapper — a panic is a programming error and must surface
        // to the caller for debugging, never be swallowed.
        struct PanickingTool;
        #[async_trait]
        impl Tool for PanickingTool {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: "panicking".to_string(),
                    description: "always panics".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                }
            }

            async fn execute(
                &self,
                _params: Value,
                _work_dir: Option<&str>,
            ) -> acowork_core::error::Result<ToolResult> {
                panic!("inner tool exploded");
            }
        }

        let wrapped = OutputBoundedTool::new(Arc::new(PanickingTool));
        // tokio::spawn gives us a JoinHandle: if the panic propagates
        // out of the wrapper the task dies and join returns Err(panic).
        let joined =
            tokio::spawn(async move { wrapped.execute(serde_json::json!({}), None).await }).await;

        match joined {
            Err(e) => assert!(
                e.is_panic(),
                "inner panic must surface as a task panic, got {e:?}"
            ),
            Ok(_) => panic!("wrapper swallowed the inner panic — it must propagate"),
        }
    }
}
