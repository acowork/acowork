//! acowork-sign CLI

use acowork_sign::keygen::KeyType;
use acowork_sign::sign::sign_package;
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "acowork-sign")]
#[command(about = "Sign .bot packages")]
struct Cli {
    /// Input .bot package path (unsigned ZIP)
    #[arg(short, long)]
    input: PathBuf,

    /// Key directory path (contains developer.key, developer.pub, developer.cert)
    #[arg(short, long)]
    key: PathBuf,

    /// Output signed package path
    #[arg(short, long)]
    output: PathBuf,

    /// Key type to use for signing
    #[arg(long, default_value = "developer", value_enum)]
    key_type: KeyType,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    println!("Signing package...");
    println!("  Input:     {}", cli.input.display());
    println!("  Key dir:   {}", cli.key.display());
    println!("  Key type:  {}", cli.key_type);
    println!("  Output:    {}", cli.output.display());
    println!();

    sign_package(&cli.input, &cli.output, &cli.key, cli.key_type)?;

    println!("Package signed successfully");
    println!("  Output: {}", cli.output.display());

    Ok(())
}
