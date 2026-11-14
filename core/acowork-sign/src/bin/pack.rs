//! acowork-pack CLI — the platform's own `.agent` packer.
//!
//! This exists because the build scripts used to shell out to whatever zip
//! tool the host had: `zip` on POSIX, `Compress-Archive` on Windows. Both
//! wrote a different archive from the same sources — different entry order,
//! different metadata — and the signature covers the archive, so "same commit,
//! same bytes" was not a property anyone could test. The Node publish path
//! already used `acowork_sign::packager::build_agent_package`; this makes the
//! repo's bundled packages go through the same code, which is also where the
//! ADR-104 declaration-graph gate lives. A hand-rolled zip skips that gate.

use acowork_core::packaging::PackageOptions;
use acowork_sign::packager::build_agent_package;
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "acowork-pack")]
#[command(about = "Build an unsigned .agent package from an agent directory")]
struct Cli {
    /// Agent directory (must contain manifest.toml)
    #[arg(short, long)]
    agent_dir: PathBuf,

    /// Output path for the unsigned .agent archive
    #[arg(short, long)]
    output: PathBuf,

    /// Unix timestamp to stamp every archive entry with, instead of the build
    /// clock. A ZIP entry's time is part of the archive's bytes and the
    /// signature covers the archive, so without a pin, rebuilding one commit
    /// yields a different package. `SOURCE_DATE_EPOCH` does the same for
    /// callers that cannot pass a flag; the flag wins.
    #[arg(long)]
    epoch: Option<i64>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    if !cli.agent_dir.join("manifest.toml").is_file() {
        return Err(format!(
            "no manifest.toml in {} — that is not an agent directory",
            cli.agent_dir.display()
        )
        .into());
    }

    println!("Packing agent...");
    println!("  Source: {}", cli.agent_dir.display());
    println!("  Output: {}", cli.output.display());

    // Runs the ADR-104 gate before writing anything: a package whose
    // capabilities name skills it does not ship must fail here, in front of
    // the author, not at a user's install.
    let options = cli.epoch.map(|epoch| PackageOptions {
        timestamp_epoch: Some(epoch),
        ..PackageOptions::default()
    });
    build_agent_package(&cli.agent_dir, &cli.output, options.as_ref())?;

    let entries = zip::ZipArchive::new(std::io::BufReader::new(std::fs::File::open(&cli.output)?))?;
    println!(
        "  {} entr{} written",
        entries.len(),
        if entries.len() == 1 { "y" } else { "ies" }
    );
    Ok(())
}
