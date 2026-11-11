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
//! Two structural preconditions ride along, because everything above is
//! vacuous without them: every `SKILL.md` must have frontmatter (no
//! frontmatter = the Runtime never registers the skill), and its `name` must
//! equal the skill directory (the packager keys capabilities on the
//! directory, the Runtime keys the registry on the frontmatter name).
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

/// The frontmatter fields this contract cares about.
struct SkillFrontmatter {
    /// The Runtime registers skills by this name (`skills/parser.rs`).
    name: String,
    /// Declared tool dependencies — the executable half of the contract.
    tool_deps: Vec<String>,
}

/// One `SKILL.md` frontmatter, parsed from the YAML block between the
/// opening and closing `---`.
///
/// Frontmatter is **required**, and a missing one is a failure rather than
/// an empty result: the Runtime keys the skill registry on the frontmatter
/// `name`, so a file without it does not load at all — skipping it would
/// make every assertion below pass vacuously on a dead skill.
///
/// No full frontmatter model is built: the contract is `name` + `tool_deps`,
/// which is also exactly what `manifest.rs` documents as "the part whoever
/// owns the package contents must supply" (`SkillDecl`).
fn frontmatter(skill_md: &Path) -> SkillFrontmatter {
    let text = std::fs::read_to_string(skill_md)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", skill_md.display()));
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines.first().is_some_and(|l| l.trim_end() == "---"),
        "{}: no YAML frontmatter — the Runtime cannot load this skill",
        skill_md.display()
    );
    let Some(end) = lines[1..].iter().position(|l| l.trim_end() == "---") else {
        panic!(
            "{}: frontmatter is not closed by a `---` line",
            skill_md.display()
        );
    };
    let block = lines[1..=end].join("\n");
    let value: serde_yaml::Value = serde_yaml::from_str(&block).unwrap_or_else(|e| {
        panic!("{}: frontmatter is not valid YAML: {e}", skill_md.display())
    });
    let name = value
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| {
            panic!(
                "{}: frontmatter has no string `name` — the Runtime cannot register this skill",
                skill_md.display()
            )
        })
        .to_string();
    let tool_deps = value
        .get("tool_deps")
        .and_then(|v| v.as_sequence())
        .map(|seq| {
            seq.iter()
                .map(|v| {
                    v.as_str()
                        .unwrap_or_else(|| {
                            panic!(
                                "{}: tool_deps entry is not a string: {v:?}",
                                skill_md.display()
                            )
                        })
                        .to_string()
                })
                .collect()
        })
        .unwrap_or_default();
    SkillFrontmatter { name, tool_deps }
}

/// The directory name of a `SKILL.md`'s skill, and the file itself.
fn skill_id(skill_md: &Path) -> (String, SkillFrontmatter) {
    let dir_name = skill_md
        .parent()
        .expect("SKILL.md has a parent")
        .file_name()
        .expect("skill dir has a name")
        .to_string_lossy()
        .into_owned();
    (dir_name, frontmatter(skill_md))
}

/// The package's shipped skills as [`SkillDecl`]s, as `validate_graph`
/// expects (skill name + frontmatter `tool_deps`).
fn shipped_skills(agent: &str) -> Vec<SkillDecl> {
    skill_files(agent)
        .into_iter()
        .map(|path| {
            let (dir_name, fm) = skill_id(&path);
            // Two different layers name a skill: the packager links
            // capabilities by the `skills/<dir>/` name (`acowork-sign`
            // `validate_declaration_graph`, which cannot read frontmatter),
            // the Runtime registers by the frontmatter `name`
            // (`skills/parser.rs`). Divergence is invisible to both — the
            // package installs, the capability panel lists the skill, and
            // `load_skill` resolves nothing. Assert it here, where both
            // halves are in hand.
            assert_eq!(
                dir_name,
                fm.name,
                "{}: frontmatter `name` must equal the skill directory name (packager keys on \
                 the directory, the Runtime keys on the frontmatter)",
                path.display()
            );
            SkillDecl {
                tool_deps: fm.tool_deps,
                name: fm.name,
            }
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
            let (skill, fm) = skill_id(&path);
            if !fm.tool_deps.iter().any(|d| d == "file_write") {
                continue;
            }
            let allowed = FILE_WRITE_ALLOWED
                .iter()
                .any(|(a, s)| *a == agent && s == &skill);
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

    // The other direction: an entry that no longer declares the tool is
    // stale config. "Exactly three" has to mean exactly three — a removed
    // dependency and its allowlist line move in the same change.
    for (agent, skill) in FILE_WRITE_ALLOWED {
        let path = agents_dir()
            .join(agent)
            .join("skills")
            .join(skill)
            .join("SKILL.md");
        assert!(path.is_file(), "{} is in the allowlist but ships no such skill", path.display());
        assert!(
            frontmatter(&path).tool_deps.iter().any(|d| d == "file_write"),
            "{agent}/skills/{skill} is in the §4.4 allowlist but no longer declares `file_write` \
             — remove the stale entry"
        );
    }
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
