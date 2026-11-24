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
//! - This module supplies the *facts* the frontend cannot derive itself: it maps
//!   node-local absolute paths onto `(workspace_id, rel_path)` ([`to_coordinates`]),
//!   the one step that must run on the node. The loop stamps the result on the
//!   producing tool's own `tool_result` row as `metadata.produced`.
//! - The frontend *decides* what counts and how to show it (created-only, dedupe,
//!   caps, image-vs-file): display policy lives there, shared by every client.
//!   [`collect`] retains the legacy turn-row policy for older sessions that still
//!   carry a `deliverables` system row.
//!
//! ## Why facts ride on `tool_result`, not a turn-end row
//!
//! A deliverable is a file on disk with an address; the address is a fact the
//! producing tool already holds. Stamping it on that tool's result keeps one
//! source of truth (the transcript's tool calls) and lets the frontend derive the
//! whole banner — files, images and doc-service documents alike — with a single
//! pure function. The metadata never enters the model context: the session
//! restorer reads only `content` + `tool_call_id` / `tool_name` from a tool_result
//! row, so `produced` is invisible to the LLM.
//!
//! ## What never leaves the node
//!
//! Only `(workspace_id, rel_path)` — never an absolute path. A node-local path
//! breaks the moment the desktop is on another machine, leaks the node's
//! layout, and cannot be re-addressed after the session is replayed.
//!
//! ## The name a human reads
//!
//! An artifact's file name is `<stem>-<content tag>.<ext>`: the stem is the
//! file the tool was pointed at (or, when it named nothing, the tool itself),
//! and the tag is a digest of the bytes. Both are computed here from facts the
//! runtime already holds, so the name is deterministic without being
//! meaningless — the previous scheme (tool-call id plus index) produced
//! `call_01a2095c7dba7734ae7a-0.png`, which is unreadable in a chip, in an
//! editor tab, and in the OS file manager, and which turned two reads of the
//! same picture into two deliverables.

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
///
/// Stamped on the producing tool's own `tool_result` row as `metadata.produced`
/// (an array). The backend supplies the *facts* — addressable coordinates, MIME,
/// size, and whether the write created the file — and the frontend owns the
/// display policy (which of these to show, dedupe, caps). It never enters the
/// model context: the session restorer reads only `content` + `tool_call_id` /
/// `tool_name` from a tool_result row, so extra metadata keys are invisible to
/// the LLM.
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
    /// `true` when the write created the file (it did not exist before). The
    /// frontend decides whether a rewrite counts as a deliverable; the backend
    /// only reports the fact the writer knows.
    pub created: bool,
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

/// Map one produced file onto addressable `(workspace_id, rel_path)` coordinates.
///
/// This is the *fact* half of the pipeline, and the only half that must run on the
/// node: resolving a node-local absolute path against the workspace roots is the
/// one thing the frontend cannot do. It keeps the writer's `created` flag and
/// applies only addressability rules — a known root, an identifiable MIME, a sane
/// size, and not process noise. It does **not** decide what counts as a
/// deliverable (created-only, dedupe, caps): that is display policy and lives in
/// the frontend. `roots` must already be sorted innermost-first.
fn map_one(file: &ProducedFile, roots: &[&(String, PathBuf)]) -> Option<Deliverable> {
    if file.bytes > MAX_DELIVERABLE_BYTES {
        return None;
    }
    let abs = Path::new(&file.abs_path);
    let mime = match file.mime.as_deref() {
        Some(declared) if !declared.is_empty() => declared.to_string(),
        _ => mime_for_path(abs)?.to_string(),
    };
    // Longest-first root list: the first match is the innermost workspace.
    let (workspace_id, rel_path) = roots.iter().find_map(|(id, root)| {
        abs.strip_prefix(root)
            .ok()
            .map(|rel| (id.clone(), to_wire_path(rel)))
    })?;
    if rel_path.is_empty() || is_excluded(&workspace_id, &rel_path) {
        return None;
    }
    Some(Deliverable {
        workspace_id,
        rel_path,
        mime,
        bytes: file.bytes,
        created: file.created,
    })
}

/// Sort roots innermost-first so the first prefix match is the deepest workspace.
///
/// Owned here rather than trusted of the caller: a caller that forgets would
/// silently attribute every project file to the agent home, and the link would
/// open the wrong workspace. Sorting a handful of roots per call is free.
fn sorted_roots(roots: &[(String, PathBuf)]) -> Vec<&(String, PathBuf)> {
    let mut roots: Vec<&(String, PathBuf)> = roots.iter().collect();
    roots.sort_by_key(|(_, root)| std::cmp::Reverse(root.as_os_str().len()));
    roots
}

/// Stamp-ready coordinates for one tool call's produced files.
///
/// The backend fact-provider path: every addressable file the call wrote, with
/// its `created` flag intact, in declaration order. The frontend derives the
/// banner from these (filtering, dedupe, caps), so this applies no display policy.
pub fn to_coordinates(produced: &[ProducedFile], roots: &[(String, PathBuf)]) -> Vec<Deliverable> {
    let roots = sorted_roots(roots);
    produced.iter().filter_map(|f| map_one(f, &roots)).collect()
}

/// Decide what of `produced` becomes a deliverable (the legacy turn-row policy).
///
/// Rules, in order: only `created` files, never inside an excluded segment, must
/// fall under a known workspace root, must have an identifiable MIME, must be
/// under the size cap. Then dedupe by `(workspace_id, rel_path)` keeping the
/// first occurrence, and cap images and other files independently.
///
/// Retained for the legacy `deliverables` turn row that older sessions still
/// carry; the live path now stamps [`to_coordinates`] per tool_result and lets
/// the frontend own this policy. Returning partial results (rather than an
/// error) is deliberate: a turn that wrote one unidentifiable file still has
/// deliverables.
pub fn collect(produced: &[ProducedFile], roots: &[(String, PathBuf)]) -> Vec<Deliverable> {
    let roots = sorted_roots(roots);
    let mut out: Vec<Deliverable> = Vec::new();
    let mut images = 0usize;
    let mut others = 0usize;

    for file in produced {
        if !file.created {
            continue;
        }
        let Some(d) = map_one(file, &roots) else {
            continue;
        };
        if out
            .iter()
            .any(|x| x.workspace_id == d.workspace_id && x.rel_path == d.rel_path)
        {
            continue;
        }
        let is_image = d.mime.starts_with("image/");
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
        out.push(d);
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

/// Longest stem an artifact name may take from its source. Long enough to
/// read, short enough that the name still fits in a chip and in the 255-byte
/// limit of every filesystem we run on.
const MAX_STEM_CHARS: usize = 48;

/// Names Windows refuses as file names on any filesystem, extension included.
/// A source called `nul.png` would otherwise produce a name `fs::write` fails
/// on with a confusing `STATUS_INVALID_NAME`, only on one platform. (`CLOCK$`
/// is not listed: `$` never survives [`sanitize_stem`], so it cannot be hit.)
const WINDOWS_RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "CLOCK$", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
    "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Reduce one model-supplied path segment to a safe, readable stem.
///
/// Returns `None` when nothing readable survives — the caller then falls back
/// to another name source rather than writing a file called `-`.
fn sanitize_stem(raw: &str) -> Option<String> {
    // Last segment only, on either separator: the path comes from the model,
    // so `..` and separators must not survive even as substrings.
    let last = raw.rsplit(['/', '\\']).next().unwrap_or("");
    // Drop the source's extension — the artifact's own comes from the MIME.
    // A leading dot (`.gitignore`) is not an extension, so it is kept.
    let stem = match last.rfind('.') {
        Some(i) if i > 0 => &last[..i],
        _ => last,
    };

    let mut out = String::with_capacity(stem.len());
    let mut pending_sep = false;
    for ch in stem.chars() {
        if ch.is_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            // Alphanumeric covers non-ASCII letters on purpose: a Chinese
            // source file name is friendlier to a human than `_-_-`.
            out.push(ch);
            pending_sep = false;
        } else if !pending_sep {
            // Anything else (space, `@`, `#`, control characters) becomes one
            // space, and runs collapse: `report final v2` reads better than
            // `report__final__v2`, and a space is legal on every filesystem
            // we run on — the desktop percent-encodes each path segment.
            out.push(' ');
            pending_sep = true;
        }
    }
    let out = out.trim_matches(|c| c == '.' || c == '_' || c == ' ');
    if out.is_empty() {
        return None;
    }
    // Truncate by characters (not bytes) and keep the head: the head is what
    // distinguishes `icon-home` from `icon-settings`.
    let capped: String = out.chars().take(MAX_STEM_CHARS).collect();
    let capped = capped.trim_matches(|c| c == '.' || c == '_' || c == ' ');
    if capped.is_empty() {
        return None;
    }
    if WINDOWS_RESERVED.contains(&capped.to_ascii_uppercase().as_str()) {
        return Some(format!("_{capped}"));
    }
    Some(capped.to_string())
}

/// The readable half of an artifact name: what the tool was *about*.
///
/// Falls back to the tool's own name when the call carried no path (an MCP
/// server that returns a screenshot it never named), then to `image`. Never
/// empty, never a path separator.
pub fn artifact_stem(source_path: Option<&str>, tool_name: &str) -> String {
    source_path
        .and_then(sanitize_stem)
        .or_else(|| sanitize_stem(tool_name))
        .unwrap_or_else(|| "image".to_string())
}

/// The disambiguating half of an artifact name: 8 hex of the content digest.
///
/// Content, not call order, is what makes the name deterministic *and* stable:
/// re-reading the same file at the same detail rewrites the same path with the
/// same bytes (so the row shows one thumbnail, not four), while two different
/// sources that share a stem — or one source read at two details — get
/// different names instead of overwriting each other. A truncated digest is
/// fine here: this is a filename, not an integrity claim, and a 32-bit
/// collision inside one session directory is not a threat model.
fn content_tag(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    digest[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// Persist one image artifact and describe it as a [`ProducedFile`].
///
/// The name is `<stem>-<content tag>.<ext>`: the stem says what the picture
/// *is* (the file the tool was pointed at, or the tool itself), the tag says
/// which bytes. Both halves are computed by the runtime from facts it already
/// holds, so the same input always produces the same file — and a human can
/// read the result without a transcript. `created` is `true` by construction:
/// the directory is ours.
pub fn write_image_artifact(
    dir: &Path,
    tool_name: &str,
    source_path: Option<&str>,
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
    let name = format!(
        "{}-{}.{}",
        artifact_stem(source_path, tool_name),
        content_tag(bytes),
        extension
    );

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
    ///
    /// `source_path` is the file the call was *about* when the tool knew one
    /// (the `path` argument of `image_read`, a browser screenshot's save path);
    /// it names the artifact so the row is readable. `None` falls back to the
    /// tool name.
    ///
    /// Returns the artifacts written, so the caller can stamp them on THIS tool
    /// call's `tool_result` metadata. Persisting the bytes is the runtime's job
    /// (the frontend cannot write node-local files); deciding to show them is
    /// the frontend's.
    pub(crate) fn absorb_image_parts(
        &self,
        tool_call: &acowork_core::providers::traits::ToolCall,
        parts: &[acowork_core::providers::traits::ContentPart],
        source_path: Option<&str>,
    ) -> Vec<ProducedFile> {
        use acowork_core::providers::traits::ContentPart;

        let mut produced = Vec::new();
        if parts.is_empty() {
            return produced;
        }
        let Some(session_id) = self.session_core.session_id.as_ref() else {
            return produced;
        };
        let Some(resolver) = self.core.workspace_resolver() else {
            return produced;
        };
        let agent_home = {
            let guard = match resolver.read() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            PathBuf::from(guard.agent_home())
        };
        let dir = artifacts_dir(&agent_home, session_id);

        for part in parts {
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
            match write_image_artifact(
                &dir,
                &tool_call.function.name,
                source_path,
                mime,
                &decoded,
            ) {
                Ok(file) => produced.push(file),
                Err(e) => tracing::warn!(
                    tool = %tool_call.function.name,
                    dir = %dir.display(),
                    error = %e,
                    "deliverables: failed to persist image artifact"
                ),
            }
        }
        produced
    }
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
    fn to_coordinates_keeps_rewrites_and_the_created_flag() {
        // The fact-provider path applies no display policy: a rewrite is still a
        // fact (created=false), and the frontend decides whether to show it.
        let files = vec![
            produced("/home/agent/project/a.md", false),
            produced("/home/agent/project/b.md", true),
        ];
        let out = to_coordinates(&files, &roots());
        assert_eq!(out.len(), 2, "no created-only filter on the fact path");
        assert!(!out[0].created);
        assert!(out[1].created);
        assert_eq!(out[0].rel_path, "a.md");
    }

    #[test]
    fn to_coordinates_does_not_dedupe_or_cap() {
        // Dedupe and caps are display policy (frontend). The fact path reports
        // every addressable declaration, duplicates included.
        let files = vec![
            produced("/home/agent/project/a.png", true),
            produced("/home/agent/project/a.png", true),
        ];
        let out = to_coordinates(&files, &roots());
        assert_eq!(out.len(), 2, "duplicates survive on the fact path");
    }

    #[test]
    fn to_coordinates_still_drops_unaddressable_and_noise() {
        // Addressability is a fact, not policy: a path under no root cannot be
        // opened, and process noise is never a deliverable.
        let files = vec![
            produced("/elsewhere/a.png", true),
            produced("/home/agent/project/node_modules/a.png", true),
            produced("/home/agent/project/ok.png", true),
        ];
        let out = to_coordinates(&files, &roots());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].rel_path, "ok.png");
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
    fn an_artifact_name_reads_as_the_picture_it_holds() {
        let dir = tempfile::tempdir().unwrap();
        let write = |src: Option<&str>, bytes: &[u8]| {
            write_image_artifact(&dir.path().join("s-1"), "image_read", src, "image/png", bytes)
                .unwrap()
        };
        let name = |file: &ProducedFile| {
            Path::new(&file.abs_path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string()
        };

        let a = write(Some("/proj/assets/logo.png"), &[0x89u8, b'P', b'N', b'G']);
        let b = write(Some("/other/dir/logo.png"), &[0x89u8, b'P', b'N', b'G']);
        let c = write(Some("/proj/assets/logo.png"), &[0u8, 1, 2, 3]);

        assert!(a.created);
        assert_eq!(a.bytes, 4);
        // Same stem, same bytes ⇒ the very same path. Two reads of one file at
        // one detail must not become two deliverables.
        assert_eq!(a.abs_path, b.abs_path);
        // Same stem, different bytes ⇒ different names. A re-encode at another
        // detail overwrites nothing.
        assert_ne!(a.abs_path, c.abs_path);
        assert!(Path::new(&a.abs_path).exists());

        let stem = name(&a).split('-').next().unwrap().to_string();
        assert_eq!(stem, "logo", "the stem is the source file, not the call");
        assert!(
            name(&a).ends_with(".png"),
            "the extension comes from the MIME: {}",
            name(&a)
        );
    }

    #[test]
    fn a_source_path_cannot_smuggle_structure_into_the_name() {
        let dir = tempfile::tempdir().unwrap();
        let name = write_image_artifact(
            dir.path(),
            "image_read",
            Some("../../etc/passwd"),
            "image/png",
            b"png-bytes",
        )
        .unwrap()
        .abs_path;
        let base = Path::new(&name).file_name().unwrap().to_string_lossy().to_string();
        assert!(base.starts_with("passwd-"), "only the last segment: {base}");
        assert!(!base.contains('/') && !base.contains('\\'));
        assert_eq!(Path::new(&name).parent().unwrap(), dir.path());
    }

    #[test]
    fn a_friendly_source_name_stays_friendly() {
        // The point of the stem is that a human can read it: non-ASCII and
        // spaces survive as a readable name, not as a wall of underscores.
        assert_eq!(artifact_stem(Some("素材/首页 icon v2.png"), "image_read"), "首页 icon v2");
        // Windows refuses these even with an extension.
        assert_eq!(artifact_stem(Some("nul.png"), "image_read"), "_nul");
        assert_eq!(artifact_stem(Some("C:/a/b/CON.txt"), "image_read"), "_CON");
        // Nothing readable in the path ⇒ the tool name, then a last resort.
        assert_eq!(artifact_stem(Some("///"), "browser_screenshot"), "browser_screenshot");
        assert_eq!(artifact_stem(None, "..."), "image");
        // Long names are capped so the chip still fits.
        let long = artifact_stem(Some(&"x".repeat(300)), "image_read");
        assert_eq!(long.chars().count(), MAX_STEM_CHARS);
    }

    #[test]
    fn an_unnamed_tool_result_is_named_after_the_tool() {
        // An MCP server that answers with a screenshot it never wrote to disk
        // has no source path; the call id told the user nothing, the tool name
        // at least says where the picture came from.
        let dir = tempfile::tempdir().unwrap();
        let file = write_image_artifact(dir.path(), "take_screenshot", None, "image/jpeg", b"j")
            .unwrap();
        let name = Path::new(&file.abs_path)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert!(name.starts_with("take_screenshot-"), "{name}");
        assert!(name.ends_with(".jpg"), "{name}");
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
            "image_read",
            Some("assets/logo.png"),
            "image/png",
            &[0x89u8, b'P', b'N', b'G'],
        )
        .unwrap();

        let roots = vec![(AGENT_HOME_WORKSPACE.to_string(), home.to_path_buf())];
        let out = collect(&[file], &roots);

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].workspace_id, AGENT_HOME_WORKSPACE);
        assert!(
            out[0].rel_path.starts_with("artifacts/s-1/logo-")
                && out[0].rel_path.ends_with(".png"),
            "readable name, still inside the artifact dir: {}",
            out[0].rel_path
        );
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