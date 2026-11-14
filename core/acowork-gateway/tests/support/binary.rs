//! Locating sibling service binaries from an integration test.
//!
//! A test that spawns a real Gateway / Runtime / doc service needs the path of
//! a binary that lives in the *build* directory, while its own executable lives
//! somewhere else in that tree. Cargo's layout is not one shape:
//!
//! - `target/<profile>/deps/<test-target>-<hash>` — the usual home of a test
//!   executable, with the sibling binaries one level above it;
//! - `target/<profile>/build/<package>/<metadata>/out/<test-target>-<hash>` —
//!   used when the same package is compiled more than once in one invocation
//!   (a dependent unifies features differently than the package builds for
//!   itself). The binaries are still in `target/<profile>/`, now three or four
//!   levels up instead of two.
//!
//! "Go up two directories" is therefore a guess about Cargo's internals, and it
//! fails in exactly the place that cannot afford guesses: a clean CI checkout
//! with a warm cache builds a different graph than a developer directory that
//! has been incrementally rebuilt for weeks. Walk the ancestors and take the
//! first directory that actually contains the binary — a fact about the artifact
//! tree, not about layout rules.
//!
//! For a binary of the *same* package, do not use this: Cargo exports the exact
//! path as `env!("CARGO_BIN_EXE_<name>")` and guarantees it was built.

use std::path::PathBuf;

/// Path to `<name>[.exe]` in the profile directory this test was built into.
///
/// `name` is the Cargo target name, without the platform executable suffix.
#[allow(dead_code)] // each test crate includes what it needs; the rest is unused there
pub(crate) fn locate(name: &str) -> PathBuf {
    let exe_name = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    let mut dir = std::env::current_exe()
        .expect("current_exe")
        .parent()
        .expect("test executable has no parent directory")
        .to_path_buf();
    loop {
        let candidate = dir.join(&exe_name);
        if candidate.exists() {
            return candidate;
        }
        // Stop at `target/`: above it there is nothing Cargo built for us, and
        // searching further would let an unrelated binary on disk answer for
        // the artifact.
        if dir.file_name().map(|f| f == "target").unwrap_or(false) || dir.parent().is_none() {
            panic!(
                "{exe_name} not found at or below {} — build it first (`cargo build -p \
                 <package>`) or run the tests from the workspace (`cargo test --workspace`)",
                dir.display()
            );
        }
        dir = dir
            .parent()
            .expect("ancestor walk escaped the filesystem root")
            .to_path_buf();
    }
}
