//! acowork-keygen CLI

use acowork_sign::keygen::{KeyType, generate_and_save};
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "acowork-keygen")]
#[command(about = "Generate Ed25519 key pairs for .bot package signing")]
struct Cli {
    /// Key type (developer or platform)
    #[arg(short, long, default_value = "developer", value_enum)]
    r#type: KeyType,

    /// Output directory for key files
    #[arg(short, long)]
    output_dir: PathBuf,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    let keypair = generate_and_save(&cli.output_dir, cli.r#type)?;

    println!("Key pair generated successfully");
    println!("  Key type:    {}", cli.r#type);
    println!("  Output dir:  {}", cli.output_dir.display());
    println!("  Fingerprint: {}", keypair.fingerprint());
    println!();
    println!("Files created:");
    println!(
        "  - {}/{}.key   (secret key)",
        cli.output_dir.display(),
        cli.r#type
    );
    println!(
        "  - {}/{}.pub   (public key)",
        cli.output_dir.display(),
        cli.r#type
    );
    println!(
        "  - {}/{}.cert  (certificate)",
        cli.output_dir.display(),
        cli.r#type
    );

    Ok(())
}
