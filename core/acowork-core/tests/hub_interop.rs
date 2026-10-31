//! Interop guard: every community agent package must be accepted by the
//! main repo's own manifest contract.
//!
//! The hub's `schema/manifest.schema.json` is a *mirror* of the main repo's
//! `AgentManifest`. A mirror can drift, and a drifted mirror means the hub
//! publishes packages the main repo refuses to install. This test is the
//! drift alarm: it runs the main repo's real parser and validator over the
//! hub's packages, so a divergence fails the build instead of shipping.
//!
//! It is deliberately written as a main-repo test rather than a Python
//! check — only the real Rust implementation can prove the claim.
//!
//! Usage (from the main repo checkout):
//!
//! ```text
//! cp tools/interop-test/main_repo_interop.rs \
//!    core/acowork-core/tests/hub_interop.rs
//! cargo test -p acowork-core --test hub_interop -- --ignored
//! ```
//!
//! The tests are `#[ignore]`d on purpose. CI checks out this repository only,
//! so the hub is absent there and an early `return` would report green while
//! asserting nothing — a drift alarm that never rings is worse than none. The
//! tests are opt-in instead: run them with `--ignored`, and point
//! `ACOWORK_HUB_AGENTS` at the hub's `agents/` directory whenever it is not a
//! sibling of this checkout. Without a hub they fail loudly rather than pass.

use std::path::{Path, PathBuf};

use acowork_core::manifest::{AgentManifest, SkillDecl};

/// The hub repository's `agents/` directory.
///
/// `ACOWORK_HUB_AGENTS` wins when set; otherwise the hub is expected as a
/// sibling of this checkout.
fn hub_agents_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("ACOWORK_HUB_AGENTS") {
        return PathBuf::from(dir);
    }
    // CARGO_MANIFEST_DIR = <main repo>/core/acowork-core
    let main_repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..");
    main_repo
        .join("..")
        .join("ACoworkAgentHub")
        .join("agents")
}

/// Every `agents/<owner>/<name>/manifest.toml` in the hub, skipping `_template`.
///
/// Panics when the hub is missing or empty: a caller that runs these tests
/// means to check the hub, and "no packages found" is a failure of the alarm,
/// not a pass.
fn manifest_paths() -> Vec<PathBuf> {
    let root = hub_agents_dir();
    assert!(
        root.is_dir(),
        "hub checkout not found at {} — set ACOWORK_HUB_AGENTS to its agents/ directory, \
         or run the default test set (these tests are #[ignore]d)",
        root.display()
    );
    let mut out = Vec::new();
    for owner in std::fs::read_dir(&root).expect("read hub agents/").flatten() {
        if !owner.path().is_dir() {
            continue;
        }
        let owner_name = owner.file_name().to_string_lossy().to_string();
        if owner_name.starts_with('_') || owner_name.starts_with('.') {
            continue;
        }
        for agent in std::fs::read_dir(owner.path()).expect("read owner dir").flatten() {
            let manifest = agent.path().join("manifest.toml");
            if manifest.is_file() {
                out.push(manifest);
            }
        }
    }
    out.sort();
    assert!(
        !out.is_empty(),
        "hub checkout at {} ships no packages — the drift alarm would assert nothing",
        root.display()
    );
    out
}

/// The skills a package actually ships, as the main repo's `SkillDecl`.
fn shipped_skills(package_dir: &Path) -> Vec<SkillDecl> {
    let skills_dir = package_dir.join("skills");
    let Ok(entries) = std::fs::read_dir(&skills_dir) else {
        return Vec::new();
    };
    let mut out: Vec<SkillDecl> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let skill_md = e.path().join("SKILL.md");
            if !skill_md.is_file() {
                return None;
            }
            // The main repo only needs the name plus the declared tool
            // dependencies; both come from the SKILL.md frontmatter.
            let tool_deps = std::fs::read_to_string(&skill_md)
                .ok()
                .and_then(|text| {
                    let front = text
                        .strip_prefix("---")
                        .and_then(|r| r.split_once("\n---"))
                        .map(|(f, _)| f.to_string())?;
                    let mut deps = Vec::new();
                    let mut in_deps = false;
                    for line in front.lines() {
                        if line.starts_with("tool_deps:") {
                            in_deps = true;
                            continue;
                        }
                        if in_deps {
                            let trimmed = line.trim();
                            if let Some(item) = trimmed.strip_prefix("- ") {
                                deps.push(item.trim().trim_matches('"').to_string());
                            } else if !trimmed.is_empty() {
                                break;
                            }
                        }
                    }
                    Some(deps)
                })
                .unwrap_or_default();
            Some(SkillDecl { name, tool_deps })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[test]
#[ignore = "needs the ACoworkAgentHub checkout: set ACOWORK_HUB_AGENTS and run --ignored"]
fn every_hub_manifest_parses_and_validates_in_the_main_repo() {
    let paths = manifest_paths();
    for path in paths {
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let manifest = AgentManifest::from_toml(&raw)
            .unwrap_or_else(|e| panic!("{} does not parse in the main repo: {e}", path.display()));
        manifest
            .validate()
            .unwrap_or_else(|e| panic!("{} fails the main repo's validate(): {e}", path.display()));
    }
}

#[test]
#[ignore = "needs the ACoworkAgentHub checkout: set ACOWORK_HUB_AGENTS and run --ignored"]
fn every_hub_capability_points_at_a_shipped_skill() {
    let paths = manifest_paths();
    for path in paths {
        let package_dir = path.parent().expect("manifest has a parent");
        let raw = std::fs::read_to_string(&path).expect("read manifest");
        let manifest = AgentManifest::from_toml(&raw).expect("parse manifest");
        let skills = shipped_skills(package_dir);
        let report = manifest.validate_graph(&skills);
        assert!(
            report.errors.is_empty(),
            "{}: declaration graph errors: {:?}",
            path.display(),
            report.errors
        );
    }
}

#[test]
#[ignore = "needs the ACoworkAgentHub checkout: set ACOWORK_HUB_AGENTS and run --ignored"]
fn every_hub_manifest_declares_no_hub_only_fields() {
    // The main repo re-serialises the parsed manifest on clone/upgrade, so a
    // hub-only field written into manifest.toml would be silently dropped.
    // Hub metadata must live in hub.json — this test pins that rule.
    let paths = manifest_paths();
    for path in paths {
        let raw = std::fs::read_to_string(&path).expect("read manifest");
        let doc: toml::Table = raw.parse().expect("parse as toml");
        for forbidden in [
            "category",
            "tier",
            "tags",
            "publisher",
            "license",
            "repository",
            "homepage",
        ] {
            assert!(
                !doc.contains_key(forbidden),
                "{}: `{forbidden}` is not a main-repo manifest field — hub metadata \
                 belongs in hub.json, or it is lost when the main repo re-serialises \
                 the manifest",
                path.display()
            );
        }
    }
}
