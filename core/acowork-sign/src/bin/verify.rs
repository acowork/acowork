//! acowork-verify CLI

use acowork_sign::verify::verify_package;
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "acowork-verify")]
#[command(about = "Verify .bot package signatures")]
struct Cli {
    /// .bot package path to verify
    package: PathBuf,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    println!("Verifying package: {}", cli.package.display());
    println!();

    let result = verify_package(&cli.package)?;

    if result.valid {
        println!("✓ Signature is VALID");
        println!("  Signer:         {}", result.signer);
        println!("  Sections:       {}", result.sections_count);
        println!("  Fingerprint:    {}", result.certificate_fingerprint);
    } else {
        println!("✗ Signature is INVALID");
        std::process::exit(1);
    }

    Ok(())
}
