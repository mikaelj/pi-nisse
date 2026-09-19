//! Nisse binary entry point.
//!
//! stdout is reserved for machine-readable output. All logs and human-facing
//! diagnostics go to stderr.

use std::process::ExitCode;

use clap::Parser;

use nisse::app::App;
use nisse::cli::Cli;

fn main() -> ExitCode {
    // Parse before initializing tracing so `--version`/`--help` never emit
    // log output.
    let cli = Cli::parse();

    nisse::app::init_tracing();

    let app = App::new();
    match app.run(cli.command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("nisse: {err}");
            ExitCode::FAILURE
        }
    }
}
