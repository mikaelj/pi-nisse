//! Binary-level integration tests.
//!
//! These run the compiled `nisse` binary and verify stdout/stderr behavior.

mod cache_binary;
mod cli_binary;
mod code_review;
mod compact_binary;
mod doctor_binary;
mod eval_binary;
mod fact_staleness;
mod facts_binary;
mod feedback_binary;
mod final_regression;
mod git_env;
mod lessons_binary;
mod mcp_binary;
mod needle_binary;
mod performance;
mod pi_host;
mod pi_reviewer;
mod pi_synthesis;
mod pi_tools;
mod plan_review;
mod query_binary;
mod records_binary;
mod security;
mod windows_compat;

use std::path::Path;
use std::process::{Command, Output};

/// Run the built `nisse` binary with the given arguments.
pub(crate) fn run_nisse(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_nisse"))
        .args(args)
        .output()
        .expect("failed to spawn nisse binary")
}

/// Run the built `nisse` binary in `cwd` with an isolated home so that
/// platform-native user config resolution is deterministic and offline.
pub(crate) fn run_nisse_in(cwd: &Path, args: &[&str], home: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_nisse"))
        .args(args)
        .current_dir(cwd)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("xdg"))
        .env("APPDATA", home.join("appdata"))
        .env("RUST_LOG", "off")
        .output()
        .expect("failed to spawn nisse binary")
}

/// Run the built `nisse` binary in `cwd` with an isolated home and
/// extra environment variables (host runtime selection, fault
/// injection).
pub(crate) fn run_nisse_in_with_env(
    cwd: &Path,
    args: &[&str],
    home: &Path,
    extra_env: &[(&str, &str)],
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nisse"));
    command.args(args).current_dir(cwd);
    command
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("xdg"))
        .env("APPDATA", home.join("appdata"))
        .env("RUST_LOG", "off")
        .env("NISSE_PI_HOST_RUNTIME", "fake");
    for (key, value) in extra_env {
        command.env(key, value);
    }
    command.output().expect("failed to spawn nisse binary")
}

/// Write the same user config into every platform-native location the
/// `dirs` crate may consult (macOS, Linux/XDG, Windows).
pub(crate) fn write_user_config(home: &Path, contents: &str) {
    for relative in [
        "Library/Application Support/nisse/config.toml", // macOS
        "xdg/nisse/config.toml",                         // Linux
        "appdata/nisse/config.toml",                     // Windows
    ] {
        let path = home.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
}
