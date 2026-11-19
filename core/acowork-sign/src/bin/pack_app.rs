//! CLI: build an unsigned `.app` package (design doc 34 §6.12).

use std::path::PathBuf;

use clap::Parser;

use acowork_sign::packager::build_app_package;

#[derive(Parser, Debug)]
#[command(name = "acowork-pack-app", about = "Build an unsigned .app package")]
struct Cli {
    /// Application source directory (must contain app.toml + bin/)
    #[arg(long)]
    app_dir: PathBuf,
    /// Output `.app` path
    #[arg(long)]
    output: PathBuf,
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = build_app_package(&cli.app_dir, &cli.output) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
    println!("wrote {}", cli.output.display());
}
