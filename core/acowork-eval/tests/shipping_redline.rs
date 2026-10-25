//! Shipping red line for the evaluation kernel (ADR-101).
//!
//! `acowork-eval` exists so the memory and runtime harnesses can share one
//! definition of Precision@K / MRR / nDCG without either reaching into the other.
//! That sharing must stay on the test side of the build: if the kernel ever
//! becomes a normal dependency of a crate, a report renderer, a stand-in
//! tokenizer and a metric loop get linked into a shipped binary, and product code
//! gains a path to call measurement code — the exact thing this crate was split
//! out to prevent.
//!
//! The rule checked here:
//!
//! * no workspace crate may declare `acowork-eval` outside a `[dev-dependencies]`
//!   table (`[dependencies]`, `[build-dependencies]`, a `[target.*.dependencies]`
//!   table, or a `[dependencies.acowork-eval]` spec table are all violations);
//! * the workspace root may declare it once, in `[workspace.dependencies]`, as the
//!   path entry the dev-dependencies inherit from;
//! * both harness crates (`acowork-memory`, `acowork-runtime`) must actually
//!   declare it as a dev-dependency — a shared kernel nobody depends on is a
//!   shared kernel that will be duplicated the next time a metric is needed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const KERNEL: &str = "acowork-eval";

fn workspace_root() -> PathBuf {
    // `core/acowork-eval` -> `core`
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .canonicalize()
        .expect("workspace root")
}

/// Every `acowork-eval =` assignment in `manifest`, keyed by the TOML table it
/// sits in. Line numbers are reported with them so a failure is actionable.
fn kernel_edges(manifest: &Path) -> Vec<(String, usize)> {
    let text = std::fs::read_to_string(manifest)
        .unwrap_or_else(|e| panic!("read {}: {e}", manifest.display()));
    let mut edges = Vec::new();
    let mut table = String::from("(top level)");
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            table = line[1..line.len() - 1].to_string();
            continue;
        }
        if line.contains("dep:") && line.contains(KERNEL) {
            edges.push((table.clone(), n + 1));
            continue;
        }
        let Some(rest) = line.strip_prefix(KERNEL).map(str::trim_start) else {
            continue;
        };
        // `acowork-eval` followed by something other than `=` is a different crate
        // whose name merely starts with ours, or a comment.
        if rest.starts_with('=') {
            edges.push((table.clone(), n + 1));
        }
    }
    edges
}

/// True when a table is test-only and therefore an allowed host for the kernel.
fn is_dev_dependencies(table: &str) -> bool {
    let tail = table.rsplit('.').next().unwrap_or(table);
    tail == "dev-dependencies"
}

/// The kernel's own manifest and the workspace root are exempt: the first
/// defines the package, the second declares the inherited path entry.
fn is_exempt(rel: &Path) -> bool {
    rel == Path::new("Cargo.toml") || rel.starts_with(KERNEL)
}

#[test]
fn kernel_is_never_a_shipping_dependency() {
    let root = workspace_root();
    let mut violations = Vec::new();

    for manifest in member_manifests(&root) {
        let rel = manifest
            .strip_prefix(&root)
            .expect("under root")
            .to_path_buf();
        if is_exempt(&rel) {
            continue;
        }
        for (table, line) in kernel_edges(&manifest) {
            if !is_dev_dependencies(&table) {
                violations.push(format!("{}:{} — [{table}]", rel.display(), line));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "{KERNEL} is a test-only evaluation kernel and must not be linked into a \
         shipped binary. Move each of these to [dev-dependencies]:\n  {}",
        violations.join("\n  "),
    );
}

#[test]
fn kernel_is_wired_into_both_harness_crates() {
    let root = workspace_root();
    for harness in ["acowork-memory", "acowork-runtime"] {
        let manifest = root.join(harness).join("Cargo.toml");
        let wired = kernel_edges(&manifest)
            .into_iter()
            .any(|(table, _)| is_dev_dependencies(&table));
        assert!(
            wired,
            "{harness} runs {KERNEL}-based retrieval tests but does not declare it as a \
             dev-dependency — the shared kernel would be unreachable from its tests"
        );
    }
}

/// `core/Cargo.toml` plus every `core/*/Cargo.toml` that is a package manifest.
fn member_manifests(root: &Path) -> Vec<PathBuf> {
    let mut out = vec![root.join("Cargo.toml")];
    let entries: BTreeMap<_, _> = std::fs::read_dir(root)
        .expect("read core/")
        .map(|e| {
            let e = e.expect("dir entry");
            (e.file_name().to_string_lossy().into_owned(), e.path())
        })
        .collect();
    for (name, dir) in entries {
        if name.starts_with("acowork-") || name == "cli" {
            let manifest = dir.join("Cargo.toml");
            if manifest.exists() {
                out.push(manifest);
            }
        }
    }
    out
}

