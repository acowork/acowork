//! The single naming convention the PM and Doc services share (design 32 §5.5).
//!
//! PM owns project identity, the Doc service owns the document tree; the only
//! thing both must agree on is *where a project's deliverables live*. That
//! agreement is one string, and it lives here — not copied into each agent's
//! prompt — because a convention duplicated across prompts is a convention
//! that will drift the first time one of them is edited.
//!
//! The coupling is deliberately one-way: PM **renders** the path into its tool
//! responses (so every agent — including the ones we did not rewrite — reads
//! the convention out of a tool result instead of out of its instructions),
//! and Doc never learns that PM exists. Doc-side code may reference
//! [`PROJECT_DOC_ROOT`] to recognise the tree it is serving, but must not
//! derive anything from it.
//!
//! Directory name is the `project_id`, not a slug of the title: it is
//! derivable from a single value (no lookup, works in any session), and a
//! retitled project does not break the link (`pm_update_project` makes
//! retitling routine). Human readability is carried by `README.md` inside the
//! directory — the directory is a machine handle, the README is the human
//! face.

/// Root directory of every project's deliverables inside the document
/// library. The vocabulary of file names under it is closed and specified in
/// design 32 §5.5; agents must not invent new names.
pub const PROJECT_DOC_ROOT: &str = "项目管理";

/// The document directory of one project, as a Doc path
/// (`项目管理/p-1a2b3c/`-style, **without** a trailing slash so it can be
/// joined with `/`).
///
/// Callers pass the value straight through to `doc_mkdir` /
/// `doc_write(reference = format!("{dir}/PRD.md"))`; it is never a filesystem
/// path.
pub fn project_doc_dir(project_id: &str) -> String {
    format!("{PROJECT_DOC_ROOT}/{project_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_dir_is_derived_from_the_id_alone() {
        assert_eq!(project_doc_dir("p-1a2b"), "项目管理/p-1a2b");
        // No trailing slash: callers join with `/` themselves.
        assert!(!project_doc_dir("p-1a2b").ends_with('/'));
    }

    #[test]
    fn root_is_a_single_segment() {
        assert!(!PROJECT_DOC_ROOT.contains('/'));
    }
}
