//! Shipped `.agent` packages must match the source they were built from.
//!
//! `agents/agent-packages/*.agent` is a build artifact (ADR-104), and the
//! repository has already paid for that twice: once when the packages still
//! said "No shell tools" after the manifests stopped saying it, and once when
//! the two benchmark agents were rewritten while users kept installing the
//! pre-rewrite copies. Both were found by a human unpacking a zip by hand,
//! which is the most expensive way to find it and the least likely to happen
//! again.
//!
//! So the check is executable. For every example agent that ships a package,
//! the packaged `manifest.toml`, `prompts/**` and `skills/**` must be
//! byte-identical to the source tree, and the package must carry no file the
//! source no longer has (a deleted skill lingering in the zip is just as
//! wrong as a stale body).
//!
//! The fix is always the same:
//! `dev/common/build-agent.sh agents/<agent-dir>` (or `.ps1` on Windows),
//! then commit the rebuilt `.agent`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use zip::ZipArchive;

/// Repository `agents/` directory, resolved from the crate root.
fn agents_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("agents")
}

/// Directories whose contents belong to the agent and must be published
/// verbatim. `assets/` is deliberately excluded from the comparison: images
/// may be re-encoded by tooling without changing any behaviour, and a
/// mismatch there would produce a failure nobody acts on.
const COMPARED_DIRS: &[&str] = &["prompts", "skills"];

/// `agent_id` from a manifest, read the same way the build script reads it.
fn agent_id(manifest: &str) -> Option<String> {
    manifest.lines().find_map(|line| {
        let rest = line.strip_prefix("agent_id")?;
        let value = rest.split('"').nth(1)?;
        Some(value.to_string())
    })
}

/// Every regular file under `dir`, keyed by its `/`-separated relative path.
fn walk(dir: &Path, base: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, base, out);
        } else if path.is_file() {
            let rel = path
                .strip_prefix(base)
                .expect("walked path is under base")
                .to_string_lossy()
                .replace('\\', "/");
            let bytes = std::fs::read(&path)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
            out.insert(rel, bytes);
        }
    }
}

#[test]
fn shipped_packages_match_their_source_tree() {
    let root = agents_dir();
    let packages = root.join("agent-packages");

    let mut checked = 0usize;
    let mut entries: Vec<_> = std::fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", root.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.join("manifest.toml").is_file())
        .collect();
    entries.sort();

    for agent_dir in entries {
        let source_manifest = std::fs::read_to_string(agent_dir.join("manifest.toml")).unwrap();
        let Some(id) = agent_id(&source_manifest) else {
            panic!(
                "{} has no agent_id — the packaging script reads it too",
                agent_dir.display()
            );
        };
        let package = packages.join(format!("{id}.agent"));
        if !package.is_file() {
            // Not every source agent ships a package (internal examples do
            // not); only the ones that do are held to this contract.
            continue;
        }
        checked += 1;

        let file = std::fs::File::open(&package)
            .unwrap_or_else(|e| panic!("cannot open {}: {e}", package.display()));
        let mut zip = ZipArchive::new(file)
            .unwrap_or_else(|e| panic!("{} is not a readable package: {e}", package.display()));

        // What the package carries, for the whole-tree comparison.
        let mut packaged: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i).unwrap();
            if entry.is_dir() {
                continue;
            }
            let name = entry.name().to_string();
            if name != "manifest.toml"
                && !COMPARED_DIRS
                    .iter()
                    .any(|d| name.starts_with(&format!("{d}/")))
            {
                continue;
            }
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut bytes).unwrap();
            packaged.insert(name, bytes);
        }

        assert_eq!(
            packaged.get("manifest.toml").map(|b| b.as_slice()),
            Some(source_manifest.as_bytes()),
            "{}: packaged manifest.toml differs from the source — rebuild with \
             dev/common/build-agent.sh {}",
            package.display(),
            agent_dir
                .strip_prefix(&root)
                .unwrap_or(&agent_dir)
                .display()
        );

        let mut source: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        for dir in COMPARED_DIRS {
            let full = agent_dir.join(dir);
            if full.is_dir() {
                walk(&full, &agent_dir, &mut source);
            }
        }

        let missing: Vec<&String> = source
            .keys()
            .filter(|k| !packaged.contains_key(*k))
            .collect();
        assert!(
            missing.is_empty(),
            "{}: package is missing {missing:?} — rebuild it, the source has files \
             the published package does not",
            package.display()
        );

        let stale: Vec<&String> = packaged
            .keys()
            .filter(|k| *k != "manifest.toml" && !source.contains_key(*k))
            .collect();
        assert!(
            stale.is_empty(),
            "{}: package carries {stale:?}, which the source tree no longer has — \
             a deleted skill is still being shipped",
            package.display()
        );

        let differing: Vec<&String> = source
            .iter()
            .filter(|(k, v)| packaged.get(*k) != Some(v))
            .map(|(k, _)| k)
            .collect();
        assert!(
            differing.is_empty(),
            "{}: {differing:?} differ from the source — the package is stale",
            package.display()
        );
    }

    assert!(
        checked > 0,
        "no example package was compared — the corpus moved, fix this test"
    );
}
