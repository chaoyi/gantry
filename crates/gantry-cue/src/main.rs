mod error;
mod generator;

use std::path::PathBuf;
use std::process;

use clap::Parser;

/// CUE-based configuration generator for Gantry.
///
/// Exports a CUE setup directory into docker-compose.yml + gantry.yaml,
/// ready for `docker compose up`.
#[derive(Parser)]
#[command(name = "gantry-cue", version)]
struct Cli {
    /// Path to the setup directory (e.g. setups/demo)
    setup_dir: PathBuf,
}

fn main() {
    let cli = Cli::parse();

    if let Err(e) = generator::generate(&cli.setup_dir) {
        eprintln!("error: {e}");
        process::exit(1);
    }
}
