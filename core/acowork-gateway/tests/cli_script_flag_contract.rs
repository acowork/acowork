//! ADR-114 — the Gateway CLI's flag surface, and the scripts that drive it.
//!
//! ADR-114 removed the deployment auth mode: `--auth-mode`, the
//! `ACOWORK_GATEWAY_AUTH_MODE` env var and the `[multi_user]` config
//! section are all gone, and the account system is *always* present. The
//! Gateway CLI itself was updated in that commit — but the **build scripts**
//! were not: `dev/common/build_core.sh` and `dev/common/build_core.ps1`
//! still invoked
//!
//! ```text
//! acowork-gateway --auth-mode multi_user admin-setup --password-file <pw>
//! ```
//!
//! clap rejects the unknown flag, `admin-setup` never runs, and a fresh
//! install dead-ends at "Setting admin password..." followed by a
//! misleading "Likely a password-policy violation" message. The operator
//! sees a policy error for what is really a removed argument.
//!
//! This test pins both halves of that contract:
//!
//! 1. **The scripts only pass flags the Gateway actually accepts.** The
//!    valid set is derived from clap itself (`CommandFactory`), never from a
//!    hand-written list — a hand-written list is exactly the kind of copy
//!    that drifts. A flag the Gateway removed therefore fails here the
//!    moment a script still uses it.
//! 2. **The removed knob stays removed.** `--auth-mode` must not come back
//!    as an accepted flag, and the scripts must not mention it.
//!
//! The scripts are shell / PowerShell, so they are scanned as *text* — but
//! only on the lines that actually invoke the Gateway, so the scripts' own
//! flags (`--start`, `--local`, `--release`, …) and cargo's flags are not
//! mistaken for Gateway arguments.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use clap::CommandFactory;

/// The Gateway CLI definition — the single source of truth for which
/// flags exist.
fn gateway_command() -> clap::Command {
    acowork_gateway::cli::Cli::command()
}

/// Repository root, resolved from the crate root (same layout contract as
/// `example_manifests.rs`): `core/acowork-gateway` -> repo root.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("workspace root")
}

/// Every long flag the Gateway accepts, e.g. `--vault-dir`.
///
/// Includes subcommand flags (`--password-file`, `--node`, …) — a script
/// may legitimately pass those, and the point of the contract is "does the
/// Gateway understand this argument", not "is it a top-level flag".
fn accepted_long_flags() -> BTreeSet<String> {
    fn walk(cmd: &clap::Command, out: &mut BTreeSet<String>) {
        for arg in cmd.get_arguments() {
            for long in arg.get_long_and_visible_aliases().unwrap_or_default() {
                out.insert(long.to_string());
            }
        }
        for sub in cmd.get_subcommands() {
            walk(sub, out);
        }
    }

    let mut out = BTreeSet::new();
    walk(&gateway_command(), &mut out);
    out
}

/// The scripts that invoke the Gateway binary.
fn gateway_invoking_scripts() -> Vec<PathBuf> {
    let root = repo_root();
    vec![
        root.join("dev/common/build_core.sh"),
        root.join("dev/common/build_core.ps1"),
    ]
}

/// Whether a line is a Gateway invocation site.
///
/// The scripts pass the Gateway's flags in two ways: directly on the
/// command line (`"$GATEWAY_EXE" --flag …`) and through an argument array
/// (`GATEWAY_ARGS=(--addr … --mqtt-addr …)`). Both are covered by looking
/// for the binary variable or the argument array. Lines that merely *talk*
/// about the Gateway (help text, comments) are excluded, because they also
/// carry the scripts' own flags and cargo's flags.
fn is_gateway_invocation_line(line: &str) -> bool {
    line.contains("GATEWAY_EXE")
        || line.contains("GATEWAY_ARGS")
        || line.contains("gatewayExe")
        || line.contains("GatewayArgs")
}

/// Extract the long flags from one line, as `--name` names.
///
/// Stops at the first character that cannot be part of a flag name, so
/// `"--addr",` yields `addr` and `--no-start).` yields `no-start`.
fn flags_in_line(line: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for token in line.split_whitespace() {
        let Some(rest) = token.strip_prefix("--") else {
            continue;
        };
        let name: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        if !name.is_empty() {
            found.insert(name);
        }
    }
    found
}

/// Every flag the scripts pass to the Gateway must exist in the Gateway's
/// clap definition.
///
/// This is the assertion that would have caught the `--auth-mode`
/// regression: the flag is absent from the Gateway's CLI, and the scripts
/// still passed it.
#[test]
fn scripts_only_pass_flags_the_gateway_accepts() {
    let accepted = accepted_long_flags();
    let mut failures = Vec::new();

    for script in gateway_invoking_scripts() {
        let text = std::fs::read_to_string(&script).expect("read script");
        for (idx, line) in text.lines().enumerate() {
            if !is_gateway_invocation_line(line) {
                continue;
            }
            for flag in flags_in_line(line) {
                if !accepted.contains(&flag) {
                    failures.push(format!(
                        "{}:{}: passes `--{flag}`, which the Gateway CLI does not accept \
                         (an unknown flag aborts the command before it does any work)",
                        script.display(),
                        idx + 1
                    ));
                }
            }
        }
    }

    assert!(
        failures.is_empty(),
        "the build scripts invoke the Gateway with flags it no longer accepts:\n  - {}",
        failures.join("\n  - ")
    );
}

/// ADR-114: the deployment auth mode is gone, and must stay gone.
///
/// Two halves: the Gateway must not accept the flag, and the scripts must
/// not pass it to the Gateway. The scan covers the invocation lines only —
/// a prose comment cannot break a build, and the comments that *document*
/// the removal are worth keeping.
#[test]
fn the_removed_auth_mode_flag_stays_removed() {
    let accepted = accepted_long_flags();
    assert!(
        !accepted.contains("auth-mode"),
        "`--auth-mode` is back in the Gateway CLI; ADR-114 removed the deployment auth mode"
    );

    let mut failures = Vec::new();
    for script in gateway_invoking_scripts() {
        let text = std::fs::read_to_string(&script).expect("read script");
        for (idx, line) in text.lines().enumerate() {
            if !is_gateway_invocation_line(line) {
                continue;
            }
            if line.contains("--auth-mode") {
                failures.push(format!(
                    "{}:{}: passes `--auth-mode` (removed by ADR-114): {}",
                    script.display(),
                    idx + 1,
                    line.trim()
                ));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "`--auth-mode` was removed by ADR-114 but the build scripts still pass it:\n  - {}",
        failures.join("\n  - ")
    );
}

/// The Gateway's `admin-setup` must be reachable with the flags the
/// scripts actually use: `--password-file` (and `--password-stdin`).
///
/// Without this the first-boot setup path is unreachable, and the install
/// dead-ends before the Gateway ever starts.
#[test]
fn admin_setup_accepts_the_password_input_flags_the_scripts_use() {
    let accepted = accepted_long_flags();
    for flag in ["password-file", "password-stdin"] {
        assert!(
            accepted.contains(flag),
            "`--{flag}` is missing from the Gateway CLI — the first-boot \
             admin-setup path cannot be driven"
        );
    }
}

/// The exact argv the scripts build must parse, and the argv that broke it
/// must not.
///
/// This is the assertion closest to the reported symptom: the scripts'
/// `admin-setup` invocation is parsed by the real clap definition, so a
/// flag the Gateway does not know fails here with the same "unexpected
/// argument" the operator saw.
#[test]
fn the_scripts_admin_setup_argv_parses() {
    use clap::Parser;

    // The argv the scripts build today.
    acowork_gateway::cli::Cli::parse_from([
        "acowork-gateway",
        "admin-setup",
        "--password-file",
        "/tmp/pw",
    ]);

    // The argv that shipped with the regression: the Gateway rejects the
    // whole line, so `admin-setup` never runs.
    let rejected = acowork_gateway::cli::Cli::try_parse_from([
        "acowork-gateway",
        "--auth-mode",
        "multi_user",
        "admin-setup",
        "--password-file",
        "/tmp/pw",
    ]);
    assert!(
        rejected.is_err(),
        "the Gateway accepts `--auth-mode` again; ADR-114 removed the deployment auth mode"
    );
}
