//! Turn deliverables — what one turn *produced*, as one transcript row.
//!
//! ## Why this exists
//!
//! A turn can run a hundred tool calls; the user reviews the result, not the
//! hundred steps. Screenshots and generated files are the part of that result
//! worth looking at, and the transcript is the wrong place for one entry per
//! step: a real session shows a dozen `Explored (N steps)` folds per screen, so
//! any per-block surface is noise by construction. One row per **turn** is the
//! only granularity that does not scale with the step count.
//!
//! ## Division of responsibility
//!
//! - Tools *declare* what they wrote ([`ProducedFile`]): only the writer knows
//!   whether the path existed before (a `created` flag), and guessing from
//!   mtimes or scanning the workspace is both expensive and wrong (see the
//!   module docs on [`ProducedFile`]).
//! - This module *decides* what counts: it maps node-local absolute paths onto
//!   `(workspace_id, rel_path)`, drops process noise, dedupes, and caps. Pure
//!   functions, so the rules are unit-tested without a session.
//! - The loop writes the row at turn end; the desktop renders it.
//!
//! ## What never leaves the node
//!
//! Only `(workspace_id, rel_path)` — never an absolute path. A node-local path
//! breaks the moment the desktop is on another machine, leaks the node's
//! layout, and cannot be re-addressed after the session is replayed.

use std::path::{Path, PathBuf};

use acowork_core::tools::traits::{ProducedFile, ToolContext};
use serde::Serialize;

use crate::agent::loop_::AgentLoop;
use crate::tools::workspace_resolver::WorkspaceResolver;

/// `metadata.type` of the deliverables turn-part row. The desktop's loader
/// keys off this string to keep the row's metadata, and its renderer keys off
/// it to draw the row (ADR-119).
pub const META_TYPE: &str = "deliverables";

/// Workspace id of the agent home directory — the same virtual id the
/// workspace HTTP routes and the desktop already use.
pub const AGENT_HOME_WORKSPACE: &str = "__agent_home__";

/// How many images / non-image files one row may show. Beyond this the row
/// stops being glanceable, and the full set is still on disk.
pub const MAX_IMAGES: usize = 8;
pub const MAX_OTHER_FILES: usize = 8;

/// Largest file worth offering as an openable deliverable.
pub const MAX_DELIVERABLE_BYTES: u64 = 50 * 1024 * 1024;

/// One deliverable, in the shape the transcript stores and the desktop reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Deliverable {
    /// Workspace the file belongs to (`__agent_home__` for the agent home).
    pub workspace_id: String,
    /// Path relative to that workspace root, forward slashes.
    pub rel_path: String,
    /// MIME type, always known (an unidentifiable file is not offered).
    pub mime: String,
    /// Size in bytes at turn end.
    pub bytes: u64,
}

/// Build / environment noise. Matched against **any** segment: a workspace can
/// nest these (`apps/web/node_modules`), and none of them is ever the thing the
/// user asked for.
const NOISE_SEGMENTS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "__pycache__",
    ".venv",
    "dist",
    "build",
    ".next",
];

/// Agent-home subtrees that are runtime state, never output: transcripts,
/// config, attachment blobs, logs, the memory database.
///
/// Matched on the **leading** segment and only inside the agent home — `files`
/// is a perfectly ordinary directory name in a user's project, and a workspace
/// named that way must still be able to deliver from it.
const AGENT_HOME_INTERNAL_ROOTS: &[&str] = &[
    "conversations",
    "config",
    "logs",
    "memory",
    "files",
    // `artifacts/` is deliberately absent: screenshots a tool returned are
    // written there *by* this module, and they are the row's headline item.
];

/// MIME for the extensions a deliverable row can actually offer. Anything else
/// is dropped: "we could not tell what it is" is not a thumbnail.
///
/// Deliberately narrower than the platform's full type table — this decides
/// whether a click *opens* something, and a wrong guess produces a broken
/// viewer rather than a missing chip.
pub fn mime_for_path(path: &Path) -> Option<&'static str> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())?;
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "json" => "application/json",
        "md" => "text/markdown",
        "txt" | "log" => "text/plain",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        _ => return None,
    })
}

/// True when `(workspace_id, rel_path)` is process noise rather than output.
fn is_excluded(workspace_id: &str, rel_path: &str) -> bool {
    if rel_path
        .split('/')
        .any(|segment| NOISE_SEGMENTS.contains(&segment))
    {
        return true;
    }
    match rel_path.split_once('/') {
        Some((first, _)) => {
            workspace_id == AGENT_HOME_WORKSPACE
                && AGENT_HOME_INTERNAL_ROOTS.contains(&first)
        }
        // A file sitting directly in the agent home root (`report.md` at the
        // top level) is fine; the guard above is about *subtrees*.
        None => false,
    }
}

/// Workspace roots a produced file may belong to: the agent home plus every
/// user-configured directory, in no particular order ([`collect`] decides which
/// root a path belongs to).
///
/// The roots come from the same resolver the workspace HTTP routes use, not
/// from a second read of `agent_workspaces.json`: a deliverable is addressed by
/// `(workspace_id, rel_path)` on the wire, so the id must be the one the file
/// routes accept, and two readers of one file are two ways to disagree.
pub fn roots_from_resolver(resolver: &WorkspaceResolver) -> Vec<(String, PathBuf)> {
    let mut roots = vec![(
        AGENT_HOME_WORKSPACE.to_string(),
        PathBuf::from(resolver.agent_home()),
    )];
    for dir in resolver.allowed_dirs() {
        roots.push((dir.id.clone(), PathBuf::from(&dir.path)));
    }
    roots
}

/// Normalize a path to forward slashes. The wire shape (and the workspace HTTP
/// routes) use `/` on every platform.
fn to_wire_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Decide what of `produced` becomes a deliverable.
///
/// Rules, in order: only `created` files, never inside an excluded segment, must
/// fall under a known workspace root, must have an identifiable MIME, must be
/// under the size cap. Then dedupe by `(workspace_id, rel_path)` keeping the
/// first occurrence, and cap images and other files independently.
///
/// Returning partial results (rather than an error) is deliberate: a turn that
/// wrote one unidentifiable file still has deliverables.
pub fn collect(produced: &[ProducedFile], roots: &[(String, PathBuf)]) -> Vec<Deliverable> {
    // Innermost root wins, and this function owns that rule rather than
    // trusting its caller to have sorted: a caller that forgets would silently
    // attribute every project file to the agent home, and the link would open
    // the wrong workspace. Sorting a handful of roots per turn is free.
    let mut roots: Vec<&(String, PathBuf)> = roots.iter().collect();
    roots.sort_by_key(|(_, root)| std::cmp::Reverse(root.as_os_str().len()));

    let mut out: Vec<Deliverable> = Vec::new();
    let mut images = 0usize;
    let mut others = 0usize;

    for file in produced {
        if !file.created || file.bytes > MAX_DELIVERABLE_BYTES {
            continue;
        }
        let abs = Path::new(&file.abs_path);
        let mime = match file.mime.as_deref() {
            Some(declared) if !declared.is_empty() => declared.to_string(),
            _ => match mime_for_path(abs) {
                Some(known) => known.to_string(),
                None => continue,
            },
        };

        // Longest-first root list: the first match is the innermost workspace.
        let Some((workspace_id, rel_path)) = roots.iter().find_map(|(id, root)| {
            abs.strip_prefix(root)
                .ok()
                .map(|rel| (id.clone(), to_wire_path(rel)))
        }) else {
            continue;
        };
        if rel_path.is_empty() || is_excluded(&workspace_id, &rel_path) {
            continue;
        }

        if out
            .iter()
            .any(|d| d.workspace_id == workspace_id && d.rel_path == rel_path)
        {
            continue;
        }

        let is_image = mime.starts_with("image/");
        if is_image && images >= MAX_IMAGES {
            continue;
        }
        if !is_image && others >= MAX_OTHER_FILES {
            continue;
        }

        if is_image {
            images += 1;
        } else {
            others += 1;
        }
        out.push(Deliverable {
            workspace_id,
            rel_path,
            mime,
            bytes: file.bytes,
        });
    }

    out
}

/// Declare one file a tool just wrote, sizing it from disk.
///
/// This is the **only** way a workspace file becomes a deliverable, and it
/// takes the one fact only the writer has: whether the path existed before the
/// call. Everything else (exclusions, workspace mapping, caps) is decided later
/// in [`collect`], so a writer cannot accidentally smuggle a rule in.
///
/// Idempotent and infallible by design: a metadata read that fails means the
/// file is not there, which is not an error worth failing a tool call over.
pub fn declare_written_file(ctx: &ToolContext, full_path: &Path, existed_before: bool) {
    // A rewrite is the turn editing something that already existed — process,
    // never output. This single line is the whole of the "created" rule.
    if existed_before {
        return;
    }
    let Ok(meta) = std::fs::metadata(full_path) else {
        return;
    };
    if !meta.is_file() {
        return;
    }
    ctx.record_produced_file(ProducedFile {
        abs_path: full_path.to_string_lossy().to_string(),
        created: true,
        bytes: meta.len(),
        mime: mime_for_path(full_path).map(str::to_string),
    });
}

/// Directory holding this session's persisted tool artifacts.
pub fn artifacts_dir(agent_home: &Path, session_id: &str) -> PathBuf {
    agent_home.join("artifacts").join(session_id)
}

/// Persist one image artifact and describe it as a [`ProducedFile`].
///
/// The file name is derived from the tool call that produced it, not from the
/// image's own metadata: the runtime is the one writing, so it can be
/// deterministic about it instead of parsing whatever a server called the
/// picture. `created` is `true` by construction — the directory is ours and the
/// name carries a per-call index.
pub fn write_image_artifact(
    dir: &Path,
    tool_call_id: &str,
    index: usize,
    mime: &str,
    bytes: &[u8],
) -> std::io::Result<ProducedFile> {
    let extension = match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "bin",
    };
    // Tool call ids come from the model; keep only characters that cannot
    // escape the directory or upset a filesystem.
    let safe_id: String = tool_call_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let name = format!("{safe_id}-{index}.{extension}");

    std::fs::create_dir_all(dir)?;
    let path = dir.join(name);
    std::fs::write(&path, bytes)?;

    Ok(ProducedFile {
        abs_path: path.to_string_lossy().to_string(),
        created: true,
        bytes: bytes.len() as u64,
        mime: Some(mime.to_string()),
    })
}

/// Split a `data:` URI into `(mime, base64 payload)`.
///
/// Only `data:` URIs are decoded. An `http(s)` image part is deliberately left
/// alone: fetching it would make a turn's deliverables depend on someone else's
/// server staying up, and the row is supposed to describe what *this node*
/// produced.
fn parse_data_uri(uri: &str) -> Option<(&str, &str)> {
    let rest = uri.strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    // Parameters after the media type (`;charset=…`) are not the type.
    let mime = meta.split(';').next()?.trim();
    if !mime.starts_with("image/") || !meta.contains("base64") {
        return None;
    }
    Some((mime, payload))
}

impl AgentLoop {
    /// Persist the image parts a tool result came back with, and declare them to
    /// this turn.
    ///
    /// This is the runtime's whole involvement in browser automation, and it
    /// does not know that phrase: a tool returned image bytes, so the bytes are
    /// written where a human can open them. Any MCP server that answers with an
    /// image lands here; a tool that stops doing so disappears from the row
    /// without a line changing.
    ///
    /// Bytes never travel over the message channel. The desktop re-fetches them
    /// through the ordinary workspace file route, which is why only
    /// `(workspace_id, rel_path)` is stored.
    pub(crate) fn absorb_image_parts(
        &self,
        tool_call: &acowork_core::providers::traits::ToolCall,
        parts: &[acowork_core::providers::traits::ContentPart],
    ) {
        use acowork_core::providers::traits::ContentPart;

        if parts.is_empty() {
            return;
        }
        let Some(session_id) = self.session_core.session_id.as_ref() else {
            return;
        };
        let Some(resolver) = self.core.workspace_resolver() else {
            return;
        };
        let agent_home = {
            let guard = match resolver.read() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            PathBuf::from(guard.agent_home())
        };
        let dir = artifacts_dir(&agent_home, session_id);

        for (index, part) in parts.iter().enumerate() {
            let ContentPart::ImageUrl { image_url } = part else {
                continue;
            };
            let Some((mime, payload)) = parse_data_uri(&image_url.url) else {
                continue;
            };
            let decoded = match base64::Engine::decode(
                &base64::engine::general_purpose::STANDARD,
                payload,
            ) {
                Ok(bytes) => bytes,
                Err(e) => {
                    tracing::warn!(
                        tool = %tool_call.function.name,
                        error = %e,
                        "deliverables: image part is not valid base64, skipped"
                    );
                    continue;
                }
            };
            match write_image_artifact(&dir, &tool_call.id, index, mime, &decoded) {
                Ok(file) => self.record_produced_file(file),
                Err(e) => tracing::warn!(
                    tool = %tool_call.function.name,
                    dir = %dir.display(),
                    error = %e,
                    "deliverables: failed to persist image artifact"
                ),
            }
        }
    }

    /// Append one already-written file to this turn's list.
    fn record_produced_file(&self, file: ProducedFile) {
        match self.produced_files.lock() {
            Ok(mut sink) => sink.push(file),
            Err(poisoned) => poisoned.into_inner().push(file),
        }
    }
}

/// Write this turn's deliverables row, then forget the turn's list.
///
/// Called once per turn from the one funnel every terminal path passes through
/// (`AgentLoop::close_turn_round`), which is what makes "one row per turn, no
/// matter how the turn ended" a property of the code: the four `ChunkEvent::Done`
/// exits, a cancel, and an error all arrive here.
///
/// Three conditions each end the turn with no row, all of them ordinary:
/// no tool declared a file, no declared file survived the rules in [`collect`],
/// or the session has no transcript writer (unit-test loops).
///
/// The list is **drained** even when no row is written: leaving last turn's
/// files in the sink would have the next turn inherit them.
pub(crate) fn write_turn_row(agent_loop: &AgentLoop) {
    let produced: Vec<ProducedFile> = {
        let mut sink = match agent_loop.produced_files.lock() {
            Ok(sink) => sink,
            Err(poisoned) => poisoned.into_inner(),
        };
        std::mem::take(&mut *sink)
    };
    if produced.is_empty() {
        return;
    }
    let Some(conversation) = agent_loop.session.conversation.as_ref() else {
        return;
    };
    // No resolver bound (a loop built without startup wiring) means no
    // workspace ids, hence no addressable deliverable. Dropping the row is the
    // honest outcome; inventing `__agent_home__` ids for files that may live
    // elsewhere would produce links that 404.
    let Some(resolver) = agent_loop.core.workspace_resolver() else {
        tracing::debug!(
            files = produced.len(),
            "deliverables: no workspace resolver bound, skipping row"
        );
        return;
    };
    let roots = {
        let guard = match resolver.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        roots_from_resolver(&guard)
    };
    let deliverables = collect(&produced, &roots);
    if deliverables.is_empty() {
        return;
    }
    conversation.append_message_with_id(
        "system",
        "",
        Some(meta_json(&deliverables)),
        None,
    );
    // The row is not a message for the model: it is a projection of what the
    // turn already did, so it never enters `history`. Persisting it (rather
    // than only emitting an event) is what keeps the thumbnails in a replayed
    // session — the requirement that made this a transcript row at all.
    tracing::debug!(
        files = deliverables.len(),
        "deliverables: wrote turn row"
    );
}

/// The row payload for the transcript metadata.
pub fn meta_json(deliverables: &[Deliverable]) -> serde_json::Value {
    serde_json::json!({
        "type": META_TYPE,
        "files": deliverables,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn produced(path: &str, created: bool) -> ProducedFile {
        ProducedFile {
            abs_path: path.to_string(),
            created,
            bytes: 128,
            mime: None,
        }
    }

    fn roots() -> Vec<(String, PathBuf)> {
        vec![
            (AGENT_HOME_WORKSPACE.to_string(), PathBuf::from("/home/agent")),
            ("ws-1".to_string(), PathBuf::from("/home/agent/project")),
        ]
    }

    #[test]
    fn only_created_files_are_deliverables() {
        let files = vec![produced("/home/agent/project/a.ts", false)];
        assert!(collect(&files, &roots()).is_empty(), "a rewrite is process, not output");
    }

    #[test]
    fn a_created_file_becomes_one_deliverable_with_a_relative_path() {
        let files = vec![produced("/home/agent/project/shot.png", true)];
        let out = collect(&files, &roots());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].workspace_id, "ws-1");
        assert_eq!(out[0].rel_path, "shot.png");
        assert_eq!(out[0].mime, "image/png");
    }

    #[test]
    fn the_innermost_workspace_owns_a_nested_path() {
        // `/home/agent` also contains the file; the project root must win.
        let files = vec![produced("/home/agent/project/docs/report.md", true)];
        let out = collect(&files, &roots());
        assert_eq!(out[0].workspace_id, "ws-1");
        assert_eq!(out[0].rel_path, "docs/report.md");
    }

    #[test]
    fn a_path_outside_every_root_is_dropped() {
        let files = vec![produced("/tmp/elsewhere/shot.png", true)];
        assert!(collect(&files, &roots()).is_empty());
    }

    #[test]
    fn agent_internals_and_build_output_are_not_deliverables() {
        for path in [
            "/home/agent/logs/run.log",
            "/home/agent/conversations/s.jsonl",
            "/home/agent/files/blob.png",
            "/home/agent/project/node_modules/x/index.js",
            "/home/agent/project/.git/HEAD",
            "/home/agent/project/target/debug/app",
        ] {
            assert!(
                collect(&[produced(path, true)], &roots()).is_empty(),
                "{path} must be excluded"
            );
        }
    }

    #[test]
    fn an_artifact_under_the_agent_home_is_kept() {
        let files = vec![produced("/home/agent/artifacts/s-1/call-1-0.png", true)];
        let out = collect(&files, &roots());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].workspace_id, AGENT_HOME_WORKSPACE);
        assert_eq!(out[0].rel_path, "artifacts/s-1/call-1-0.png");
    }

    #[test]
    fn a_file_with_no_identifiable_type_is_dropped() {
        let files = vec![produced("/home/agent/project/build.tar", true)];
        assert!(collect(&files, &roots()).is_empty());
    }

    #[test]
    fn a_declared_mime_wins_over_the_extension() {
        let mut file = produced("/home/agent/project/shot.bin", true);
        file.mime = Some("image/png".to_string());
        let out = collect(&[file], &roots());
        assert_eq!(out[0].mime, "image/png");
    }

    #[test]
    fn duplicates_keep_the_first_occurrence() {
        let files = vec![
            produced("/home/agent/project/a.png", true),
            produced("/home/agent/project/a.png", true),
            produced("/home/agent/project/b.png", true),
        ];
        let out = collect(&files, &roots());
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].rel_path, "a.png");
        assert_eq!(out[1].rel_path, "b.png");
    }

    #[test]
    fn images_and_other_files_have_separate_caps() {
        let mut files: Vec<ProducedFile> = (0..MAX_IMAGES + 3)
            .map(|i| produced(&format!("/home/agent/project/p{i}.png"), true))
            .collect();
        files.extend(
            (0..MAX_OTHER_FILES + 2).map(|i| produced(&format!("/home/agent/project/n{i}.pdf"), true)),
        );

        let out = collect(&files, &roots());
        let images = out.iter().filter(|d| d.mime.starts_with("image/")).count();
        let others = out.len() - images;
        assert_eq!(images, MAX_IMAGES);
        assert_eq!(others, MAX_OTHER_FILES);
    }

    #[test]
    fn an_oversized_file_is_dropped() {
        let mut file = produced("/home/agent/project/big.png", true);
        file.bytes = MAX_DELIVERABLE_BYTES + 1;
        assert!(collect(&[file], &roots()).is_empty());
    }

    #[test]
    fn windows_separators_become_forward_slashes() {
        let roots = vec![(
            AGENT_HOME_WORKSPACE.to_string(),
            PathBuf::from(r"C:\agent"),
        )];
        let out = collect(&[produced(r"C:\agent\artifacts\s-1\a.png", true)], &roots);
        assert_eq!(out[0].rel_path, "artifacts/s-1/a.png");
    }

    #[test]
    fn an_artifact_write_names_the_file_after_its_tool_call() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_image_artifact(
            &dir.path().join("s-1"),
            "call:1/../x",
            0,
            "image/png",
            &[0x89u8, b'P', b'N', b'G'],
        )
        .unwrap();
        assert!(file.created);
        assert_eq!(file.bytes, 4);
        let name = Path::new(&file.abs_path).file_name().unwrap().to_string_lossy().to_string();
        // Every character that is not alphanumeric / `-` / `_` becomes `_`, so
        // a `..` segment cannot survive in the name even as a substring.
        assert_eq!(
            name, "call_1____x-0.png",
            "unsafe characters are replaced, not honoured"
        );
        assert!(Path::new(&file.abs_path).exists());
    }

    #[test]
    fn only_base64_data_uris_are_artifacts() {
        assert_eq!(
            parse_data_uri("data:image/png;base64,iVBORw0="),
            Some(("image/png", "iVBORw0="))
        );
        assert_eq!(
            parse_data_uri("data:image/jpeg;charset=utf-8;base64,AAA="),
            Some(("image/jpeg", "AAA="))
        );
        // A remote image is not this node's output, and a fetch would make the
        // row depend on someone else's uptime.
        assert_eq!(parse_data_uri("https://example.com/a.png"), None);
        // Neither is a non-image payload or an unencoded data URI.
        assert_eq!(parse_data_uri("data:application/pdf;base64,AAA="), None);
        assert_eq!(parse_data_uri("data:image/svg+xml,<svg/>"), None);
    }

    #[test]
    fn a_screenshot_artifact_reaches_the_row_as_its_own_deliverable() {
        // The artifact store sits *inside* the agent home, so this pins the one
        // exclusion rule that must not swallow it.
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::create_dir_all(home.join("artifacts").join("s-1")).unwrap();
        let file = write_image_artifact(
            &artifacts_dir(home, "s-1"),
            "call_1",
            0,
            "image/png",
            &[0x89u8, b'P', b'N', b'G'],
        )
        .unwrap();

        let roots = vec![(AGENT_HOME_WORKSPACE.to_string(), home.to_path_buf())];
        let out = collect(&[file], &roots);

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].workspace_id, AGENT_HOME_WORKSPACE);
        assert_eq!(out[0].rel_path, "artifacts/s-1/call_1-0.png");
        assert_eq!(out[0].mime, "image/png");
    }

    #[test]
    fn a_project_directory_named_files_is_not_agent_internals() {
        let files = vec![produced("/home/agent/project/files/logo.png", true)];
        let out = collect(&files, &roots());
        assert_eq!(
            out.len(),
            1,
            "`files/` is only runtime state inside the agent home"
        );
        assert_eq!(out[0].workspace_id, "ws-1");
    }

    #[test]
    fn the_meta_row_carries_the_type_tag_the_desktop_keys_off() {
        let out = collect(&[produced("/home/agent/project/shot.png", true)], &roots());
        let meta = meta_json(&out);
        assert_eq!(meta["type"], META_TYPE);
        assert_eq!(meta["files"][0]["workspace_id"], "ws-1");
        assert_eq!(meta["files"][0]["rel_path"], "shot.png");
    }
}