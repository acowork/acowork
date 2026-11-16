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

/// The doc service's **internal** route for batch doc-reference validation
/// (`POST /refs/validate`) — the endpoint `pm_submit_task` calls to learn
/// whether a submitted `doc_refs` entry resolves (design 32 §3, plan §7 G4).
///
/// It lives here, in the crate both the Doc service and PM already depend
/// on, because it is a **wire contract between two services**: the Doc
/// service registers the route with this constant, PM builds its URL from
/// this constant, and the Gateway's doc proxy strips only its own `/api/doc`
/// public prefix. A second copy of the string is a bug waiting to happen —
/// and it *was* one: PM shipped `/api/doc/api/refs/validate` (the Doc
/// service's route with an extra `/api` glued on) while the Doc service
/// registered `/refs/validate`, so every `pm_submit_task` carrying
/// `doc_refs` failed with a 404 that no unit test caught, because the PM
/// test stub mocked the same wrong path the implementation used.
///
/// Note the shape: **no `/api` prefix**. The doc router mounts its REST
/// routes without one (`core/acowork-doc/src/api/router.rs`), and the
/// Gateway's doc proxy forwards `/api/doc/{rest}` → `{doc}/{rest}`.
///
/// The public (Gateway-facing) form is [`DOC_REFS_VALIDATE_GATEWAY_PATH`].
pub const DOC_REFS_VALIDATE_ROUTE: &str = "/refs/validate";

/// The same endpoint as seen through the Gateway's doc reverse proxy:
/// `POST /api/doc/refs/validate` — what PM actually calls.
///
/// The Gateway mounts the doc proxy at `/api/doc/{*rest}` and forwards
/// `{rest}` to the doc process unchanged, so the public path is the doc
/// route with the Gateway's own `/api/doc` prefix in front of it.
pub const DOC_REFS_VALIDATE_GATEWAY_PATH: &str = "/api/doc/refs/validate";

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

/// The five participant deliverable areas under [`project_doc_dir`] — the
/// controlled revision of design 32 §5.5's closed vocabulary (plan
/// `pm-doc-agent-participation-plan` §6, decision D-2).
///
/// A participant writes files into exactly one of these areas and never
/// invents a sixth. The areas are created by the PM's `project-setup`
/// (one pass, root first); a participant that finds an area missing reports
/// the gap through `pm_add_activity` instead of creating it — the project
/// tree has exactly one governor.
///
/// These are the *documented* vocabulary, not yet a consumed one: the PM
/// bootstrap that creates the directories is agent-side prose in the
/// `project-setup` skill, so today the only Rust readers are the tests in
/// this module. The constants exist so that "a participant artifact has a
/// place in the tree" is a compiled fact rather than a sentence — the first
/// Rust consumer (a bootstrap pass, or a Doc-side assertion) reads these
/// instead of re-typing the names.
pub const DOC_AREA_TECH_DESIGN: &str = "技术方案";
pub const DOC_AREA_REVIEW: &str = "评审";
pub const DOC_AREA_TESTING: &str = "测试";
pub const DOC_AREA_ACCEPTANCE: &str = "验收";
pub const DOC_AREA_RESEARCH: &str = "调研";

/// All five areas, in tree order — the enumeration the vocabulary tests walk
/// (and any future Rust-side bootstrap).
pub const DOC_AREAS: [&str; 5] = [
    DOC_AREA_TECH_DESIGN,
    DOC_AREA_REVIEW,
    DOC_AREA_TESTING,
    DOC_AREA_ACCEPTANCE,
    DOC_AREA_RESEARCH,
];

/// The document directory of one area of one project, as a Doc path
/// (`项目管理/p-1a2b3c/技术方案`-style, **without** a trailing slash, same
/// join rule as [`project_doc_dir`]).
///
/// `area` is a single tree segment. The five participant areas are the
/// constants above; PM's own subdirectories (`周报`, `会议纪要`) are equally
/// valid inputs — this function composes any segment under the project
/// directory and does not validate the vocabulary, which is why the closed
/// set lives here as constants rather than as a runtime check.
pub fn project_doc_area(project_id: &str, area: &str) -> String {
    format!("{}/{}", project_doc_dir(project_id), area)
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

    /// **Design 32 §5.5 controlled revision (plan §6):** the five
    /// participant areas compose with the project directory into
    /// `项目管理/{project_id}/{area}` — the shape the PM `project-setup`
    /// bootstrap builds and the participant `project-participation` skill
    /// writes into. The composition must go through [`project_doc_dir`]
    /// (one join rule), and every area must stay a single tree segment.
    #[test]
    fn participant_areas_compose_under_the_project_directory() {
        for area in DOC_AREAS {
            assert!(
                !area.contains('/'),
                "area `{area}` is not a single tree segment"
            );
            assert_eq!(
                project_doc_area("p-1a2b", area),
                format!("{}/p-1a2b/{}", PROJECT_DOC_ROOT, area)
            );
            // The area directory never invents its own join rule.
            assert_eq!(
                project_doc_area("p-1a2b", area),
                format!("{}/{}", project_doc_dir("p-1a2b"), area)
            );
        }
        assert_eq!(project_doc_area("p-1a2b", DOC_AREA_TECH_DESIGN), "项目管理/p-1a2b/技术方案");
    }

    #[test]
    fn the_area_vocabulary_is_closed_and_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for area in DOC_AREAS {
            assert!(seen.insert(area), "area `{area}` is declared twice");
        }
        assert_eq!(DOC_AREAS.len(), 5);
    }

    /// The doc route carries **no** `/api` prefix: the doc router mounts
    /// its REST routes bare, and the Gateway's doc proxy strips only its
    /// own `/api/doc` public prefix. A stray `/api` here is exactly the
    /// 404 bug (PM called `/api/doc/api/refs/validate`).
    #[test]
    fn the_refs_validate_route_carries_no_api_prefix() {
        assert_eq!(DOC_REFS_VALIDATE_ROUTE, "/refs/validate");
        assert!(
            !DOC_REFS_VALIDATE_ROUTE.starts_with("/api"),
            "the doc router mounts REST routes without an `/api` prefix: {}",
            DOC_REFS_VALIDATE_ROUTE
        );
    }

    /// The Gateway-facing path is the doc route behind the Gateway's own
    /// `/api/doc` prefix — and **only** that prefix.
    #[test]
    fn the_gateway_path_is_the_doc_route_behind_the_gateway_prefix() {
        assert_eq!(DOC_REFS_VALIDATE_GATEWAY_PATH, "/api/doc/refs/validate");
        assert_eq!(
            DOC_REFS_VALIDATE_GATEWAY_PATH,
            format!("/api/doc{DOC_REFS_VALIDATE_ROUTE}"),
            "the Gateway strips exactly `/api/doc`; the rest reaches the doc service verbatim"
        );
    }
}
