//! End-to-end install guard for hub-published packages.
//!
//! The hub publishes `.bot` packages that the app installs through the
//! node's `install_package`. That path demands a valid signature outside dev
//! mode, so a package the hub signs but the node refuses would compile green,
//! pass every unit test and still fail for every user at install time.
//!
//! This test takes the packages the hub actually built (pointed at by
//! `ACOWORK_HUB_DIST`) and runs them through the real install path with
//! `dev_mode = false` — the production mode. It asserts the install
//! succeeded, the manifest landed, and the skills the manifest declares are
//! present on disk.
//!
//! Usage:
//!
//! ```text
//! ACOWORK_HUB_DIST=E:/projects/tranxon/ACoworkBotHub/dist-verify \
//!   cargo test -p acowork-node --test hub_package_install -- --ignored
//! ```
//!
//! The test is `#[ignore]`d for the same reason `hub_interop` is: CI checks
//! out this repository only, so the hub's packages are absent there. A guard
//! that reports green while checking nothing is worse than no guard, so it
//! fails loudly instead of skipping — run it with `--ignored` when a hub
//! checkout is at hand.

use std::path::{Path, PathBuf};

use acowork_node::package::install::install_package;
use acowork_node::state::NodeState;

/// The directory holding the hub-built `.bot` packages.
fn hub_dist_dir() -> PathBuf {
    let dir = std::env::var_os("ACOWORK_HUB_DIST")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
            manifest_dir
                .join("..")
                .join("..")
                .join("..")
                .join("ACoworkBotHub")
                .join("dist-verify")
        });
    assert!(
        dir.is_dir(),
        "hub dist directory not found at {} — set ACOWORK_HUB_DIST to the directory \
         holding the hub-built .bot packages",
        dir.display()
    );
    dir
}

/// Every official package the hub publishes, by agent id.
const OFFICIAL_AGENTS: &[&str] = &[
    "com.acowork.android-node",
    "com.acowork.document-manager",
    "com.acowork.product-manager",
    "com.acowork.project-manager",
    "com.acowork.quality-assurance",
    "com.acowork.senior-engineer",
    "com.acowork.software-architect",
];

#[test]
#[ignore = "needs the hub's built packages: set ACOWORK_HUB_DIST and run --ignored"]
fn every_official_hub_package_installs_in_production_mode() {
    let dist = hub_dist_dir();
    let temp_root = std::env::temp_dir().join(format!("acowork-hub-install-{}", std::process::id()));
    std::fs::create_dir_all(&temp_root).expect("create temp root");

    for agent_id in OFFICIAL_AGENTS {
        let pkg = dist.join(format!("{agent_id}.bot"));
        assert!(
            pkg.is_file(),
            "hub did not publish {agent_id}: {} is missing",
            pkg.display()
        );

        let install_dir = temp_root.join(agent_id);
        let mut state = NodeState::new(16);

        // dev_mode = false: the production path. A package that only installs
        // in dev mode is the exact shape of the hole this test exists for.
        let result = install_package(&pkg, &install_dir, &mut state, false, agent_id);
        let info = match result {
            Ok(info) => info,
            Err(e) => panic!("{agent_id} failed to install in production mode: {e}"),
        };

        assert_eq!(info.agent_id, *agent_id);
        assert!(!info.version.is_empty(), "{agent_id} installed with no version");

        // The manifest's declared skills must be on disk: a capability that
        // points at a skill the package does not ship is a capability that
        // silently does nothing at runtime.
        let install_path = PathBuf::from(&info.install_path);
        for (capability_name, capability) in &info.manifest.capabilities {
            for skill_name in &capability.skills {
                let skill_dir = install_path.join("skills").join(skill_name);
                assert!(
                    skill_dir.join("SKILL.md").is_file(),
                    "{agent_id}: capability '{capability_name}' points at skill '{skill_name}', \
                     which the installed package does not ship"
                );
            }
        }

        // The prompts the runtime loads must be present too.
        assert!(
            install_path.join("prompts").is_dir(),
            "{agent_id} installed without a prompts/ directory"
        );
    }

    let _ = std::fs::remove_dir_all(&temp_root);
}
