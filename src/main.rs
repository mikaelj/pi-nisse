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

    // Directory-creation consent may prompt interactively exactly when
    // stdin is a terminal (user-requested plan change; plan section 10
    // amendments). Subprocess callers stay noninteractive and fail
    // closed or defer instead of blocking.
    // `NISSE_INTERACTIVE_CONSENT=1` forces the interactive consent
    // path for scripted/pty-less runs (tests, automation); it grants no
    // consent by itself — prompts still require real answers.
    let interactive = std::io::IsTerminal::is_terminal(&std::io::stdin())
        || std::env::var("NISSE_INTERACTIVE_CONSENT").is_ok_and(|value| value == "1");
    nisse::config::consent::set_interactive(interactive);
    nisse::config::consent::load_policy_from_user_config();

    let app = App::new();
    match app.run(cli.command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("nisse: {err}");
            ExitCode::FAILURE
        }
    }
}
