//! Regression guard for the shipped example agent packages (ADR-104).
//!
//! `examples/` is documentation *and* a smoke corpus: every example manifest
//! must stay parseable, pass `AgentManifest::validate()`, and keep its
//! declaration references honest — a `capabilities.*.skills` entry must name a
//! skill directory the package actually ships, and every `[[mcp_servers]]`
//! entry must resolve to a single source kind.
//!
//! This lives in `tests/` rather than beside the unit tests because it depends
//! on the repository layout (`../../examples`), not on the crate itself.

use std::path::Path;

use acowork_core::manifest::{AgentManifest, McpSourceKind};

/// Repository `examples/` directory, resolved from the crate root.
fn examples_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples")
}

/// Every `examples/<package>/manifest.toml`, sorted for a stable failure order.
fn manifest_paths() -> Vec<std::path::PathBuf> {
    let root = examples_dir();
    let mut out: Vec<_> = std::fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", root.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.path().join("manifest.toml"))
        .filter(|p| p.is_file())
        .collect();
    out.sort();
    assert!(
        !out.is_empty(),
        "no example manifests found under {} — the corpus moved, fix this test",
        root.display()
    );
    out
}

#[test]
fn every_example_manifest_parses_and_validates() {
    for path in manifest_paths() {
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let manifest = AgentManifest::from_toml(&raw)
            .unwrap_or_else(|e| panic!("{} does not parse: {e}", path.display()));
        manifest
            .validate()
            .unwrap_or_else(|e| panic!("{} fails validate(): {e}", path.display()));
        assert!(!manifest.name.is_empty(), "{}: name is empty", path.display());
    }
}

#[test]
fn every_example_capability_points_at_a_shipped_skill() {
    for path in manifest_paths() {
        let package_dir = path.parent().expect("manifest has a parent");
        let raw = std::fs::read_to_string(&path).expect("read manifest");
        let manifest = AgentManifest::from_toml(&raw).expect("parse manifest");
        for (action, def) in &manifest.capabilities {
            for skill in &def.skills {
                let dir = package_dir.join("skills").join(skill);
                assert!(
                    dir.is_dir(),
                    "{}: capability `{action}` references skill `{skill}`, but {} is not a directory",
                    path.display(),
                    dir.display()
                );
            }
        }
    }
}

#[test]
fn every_example_mcp_declaration_has_one_source_kind() {
    for path in manifest_paths() {
        let raw = std::fs::read_to_string(&path).expect("read manifest");
        let manifest = AgentManifest::from_toml(&raw).expect("parse manifest");
        for decl in &manifest.mcp_servers {
            let key = decl
                .key()
                .unwrap_or_else(|| panic!("{}: a [[mcp_servers]] entry has no key", path.display()));
            let kind = decl.source_kind().unwrap_or_else(|| {
                panic!(
                    "{}: `{key}` does not resolve to a source kind (validate() should have caught this)",
                    path.display()
                )
            });
            // An inline remote server must keep its endpoint; a preset/hosted
            // reference must not smuggle one in.
            match kind {
                McpSourceKind::InlineRemote => assert!(
                    decl.url.is_some(),
                    "{}: inline `{key}` has no url",
                    path.display()
                ),
                McpSourceKind::Preset | McpSourceKind::Hosted => assert!(
                    decl.url.is_none() && decl.transport.is_none(),
                    "{}: `{key}` mixes a reference form with inline fields",
                    path.display()
                ),
            }
        }
    }
}

#[test]
fn every_example_config_default_is_in_range() {
    for path in manifest_paths() {
        let raw = std::fs::read_to_string(&path).expect("read manifest");
        let manifest = AgentManifest::from_toml(&raw).expect("parse manifest");
        manifest.config.validate().unwrap_or_else(|e| {
            panic!("{}: [config] out of range: {e}", path.display())
        });
        // The projection must never invent a value the author did not write.
        let defaults = manifest.config_defaults();
        if manifest.config.temperature.is_none() {
            assert_eq!(
                defaults.temperature,
                manifest.llm.temperature,
                "{}: temperature fallback must come from [llm]",
                path.display()
            );
        }
    }
}