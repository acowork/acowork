//! Copy-text contract tests for the four participant agents (plan §8,
//! `docs/plan/zh/pm-doc-agent-participation-plan.md`).
//!
//! T1–T4 rewrote the participants' prompts, skills and manifests by hand;
//! these three assertions are the last net for that batch — they are also
//! the standing CI guard. A red here means one of the batch cards landed
//! with the wrong declaration, and the fix is a rewrite of the card's
//! output plus one repack, not an incident.
//!
//! The checks (plan §8 items 1, 3, 4):
//!
//! 1. `file_write` is closed to the three `(agent, skill)` pairs named in
//!    plan §4.4 — `senior-engineer/{debugging, refactoring, testing}`:
//!    the only deliverables that are repository code instead of documents.
//!    A fourth occurrence is a red.
//! 2. Every skill's `tool_deps` are declared on the package manifest —
//!    `[[tools]] ∪ [[mcp_servers]].tools`, with the platform-hosted
//!    `mcp_*` reachability rule — i.e. [`AgentManifest::validate_graph`]
//!    must come back with **zero warnings**: the warning level is what
//!    allowed E1/E2 (41+46 lines of unexecutable instructions) to ship,
//!    so for the participants it is an error-level assertion.
//! 3. Every `capabilities.*.skills` entry of the four manifests resolves to
//!    a skill the package actually ships ([`AgentManifest::validate_capability_skills`]
//!    is already an error at packaging time; this pins it for the batch).
//!
//! The packaging guard (plan §8 item 5) is not a test but a `git status`
//! assertion on `agents/agent-packages/` — see the T5 card.

use std::path::{Path, PathBuf};

use acowork_core::manifest::{AgentManifest, SkillDecl};

/// The four participant packages, as repository directories under
/// `agents/`. Excluded on purpose: `github.ponytail` (third-party) and
/// `com.acowork.document-manager` (prior plan, already shipped).
const PARTICIPANTS: [&str; 4] = [
    "product-manager-agent",
    "quality-assurance-agent",
    "senior-engineer-agent",
    "software-architect-agent",
];

/// **Plan §4.4, frozen as a CI constant** (plan §8 item 1): `file_write`
/// is allowed in exactly these three `(agent, skill)` pairs — the SSE
/// skills whose deliverables are repository code, not documents. Adding a
/// fourth pair requires a revision of plan §4.4 first; this list is not
/// "approximately three".
const FILE_WRITE_ALLOWED: [(&str, &str); 3] = [
    ("senior-engineer-agent", "debugging"),
    ("senior-engineer-agent", "refactoring"),
    ("senior-engineer-agent", "testing"),
];

/// Repository `agents/` directory, resolved from the crate root (same
/// layout contract as `example_manifests.rs`).
fn agents_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("agents")
}

/// Every `SKILL.md` of one participant package, sorted for a stable
/// failure order. A missing `skills/` directory is itself a batch error.
fn skill_files(agent: &str) -> Vec<PathBuf> {
    let skills_dir = agents_dir().join(agent).join("skills");
    let entries = std::fs::read_dir(&skills_dir)
        .unwrap_or_else(|e| panic!("{}: cannot read skills/: {e}", skills_dir.display()));
    let mut out: Vec<PathBuf> = entries
        .map(|e| e.expect("skills dir entry").path())
        .filter(|p| p.is_dir())
        .map(|p| p.join("SKILL.md"))
        .filter(|p| p.is_file())
        .collect();
    out.sort();
    out
}

/// The `tool_deps` of one `SKILL.md`, from its YAML frontmatter.
///
/// Deliberately minimal (no full frontmatter model): the contract is about
/// *declared tool dependencies*, and the frontmatter `name` is already
/// cross-checked against the directory by the packaging pipeline.
fn frontmatter_tool_deps(skill_md: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(skill_md)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", skill_md.display()));
    let lines: Vec<&str> = text.lines().collect();
    // No frontmatter, or a file that does not open with `---`: the skill
    // simply declares no tool dependencies.
    let Some(open) = lines.first() else {
        return Vec::new();
    };
    if !open.trim_end().starts_with("---") {
        return Vec::new();
    }
    let Some(end) = lines[1..]
        .iter()
        .position(|l| l.trim_end() == "---")
    else {
        panic!("{}: frontmatter is not closed by a `---` line", skill_md.display());
    };
    let block = lines[1..=end].join("\n");
    let value: serde_yaml::Value = serde_yaml::from_str(&block)
        .unwrap_or_else(|e| panic!("{}: frontmatter is not valid YAML: {e}", skill_md.display()));
    value
        .get("tool_deps")
        .and_then(|v| v.as_sequence())
        .map(|seq| {
            seq.iter()
                .map(|v| v
                    .as_str()
                    .unwrap_or_else(|| {
                        panic!("{}: tool_deps entry is not a string: {v:?}", skill_md.display())
                    })
                    .to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// The package's shipped skills as [`SkillDecl`]s, as `validate_graph`
/// expects (directory name + frontmatter `tool_deps`).
fn shipped_skills(agent: &str) -> Vec<SkillDecl> {
    skill_files(agent)
        .into_iter()
        .map(|path| SkillDecl {
            tool_deps: frontmatter_tool_deps(&path),
            name: path
                .parent()
                .expect("SKILL.md has a parent")
                .file_name()
                .expect("skill dir has a name")
                .to_string_lossy()
                .into_owned(),
        })
        .collect()
}

fn load_manifest(agent: &str) -> AgentManifest {
    let path = agents_dir().join(agent).join("manifest.toml");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    AgentManifest::from_toml(&raw)
        .unwrap_or_else(|e| panic!("{} does not parse: {e}", path.display()))
}

#[test]
fn file_write_is_closed_to_the_three_sse_skills() {
    // Plan §8 item 1: scan the DEPENDENCY declarations (frontmatter
    // `tool_deps`) of all four participants. Prose mentions of `file_write`
    // in a skill body are out of scope — plan §4.2 deliberately keeps the
    // role line "code goes to the repository" in SSE's
    // `project-participation`; what must stay closed is the executable
    // declaration.
    let mut offenders = Vec::new();
    for agent in PARTICIPANTS {
        for path in skill_files(agent) {
            let skill = path
                .parent()
                .expect("SKILL.md has a parent")
                .file_name()
                .expect("skill dir has a name")
                .to_string_lossy();
            let skill = skill.as_ref();
            let deps = frontmatter_tool_deps(&path);
            if !deps.iter().any(|d| d == "file_write") {
                continue;
            }
            let allowed = FILE_WRITE_ALLOWED
                .iter()
                .any(|(a, s)| *a == agent && *s == skill);
            if !allowed {
                offenders.push(format!(
                    "{}/skills/{skill} declares `file_write` but is not in the §4.4 allowlist",
                    agent
                ));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "`file_write` leaked outside the three §4.4 skills:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn every_skill_tool_dep_is_declared_on_the_manifest() {
    // Plan §8 item 3: `tool_deps ⊆ [[tools]] ∪ [[mcp_servers]].tools`
    // (plus the platform-hosted `mcp_*` reachability rule). `validate_graph`
    // reports the gap as a *warning* — packaging still succeeds — and that
    // is exactly how E1/E2 (41+46 unexecutable instruction lines) shipped.
    // For the participants the warning is an error: zero warnings allowed.
    for agent in PARTICIPANTS {
        let manifest = load_manifest(agent);
        let skills = shipped_skills(agent);
        assert!(
            !skills.is_empty(),
            "{agent}: the participant ships no skills — the corpus moved, fix this test"
        );
        let report = manifest.validate_graph(&skills);
        assert!(
            report.errors.is_empty(),
            "{agent}: packaging errors:\n{}",
            report.errors.join("\n")
        );
        assert!(
            report.warnings.is_empty(),
            "{agent}: undeclared tool dependencies (error-level, plan §8):\n{}",
            report.warnings.join("\n")
        );
    }
}

#[test]
fn every_capability_skill_resolves_inside_the_package() {
    // Plan §8 item 4: a `capabilities.*.skills` entry that names a skill
    // the package does not ship is a dangling reference. `acowork-sign`
    // checks this at packaging time (error); this pins it for the batch
    // so the check runs on every build, not only at pack time.
    for agent in PARTICIPANTS {
        let manifest = load_manifest(agent);
        let skills = shipped_skills(agent);
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        let errors = manifest.validate_capability_skills(&names);
        assert!(
            errors.is_empty(),
            "{agent}: capability → skill references do not resolve:\n{}",
            errors.join("\n")
        );
    }
}
