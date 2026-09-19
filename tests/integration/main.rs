//! Binary-level integration tests.
//!
//! These run the compiled `nisse` binary and verify stdout/stderr behavior.

mod cli_binary;

use std::process::{Command, Output};

/// Run the built `nisse` binary with the given arguments.
pub(crate) fn run_nisse(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_nisse"))
        .args(args)
        .output()
        .expect("failed to spawn nisse binary")
}
