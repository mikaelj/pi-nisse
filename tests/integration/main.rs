//! Binary-level integration tests.
//!
//! These run the compiled `nisse` binary and verify stdout/stderr behavior.

mod cache_binary;
mod cli_binary;
mod code_review;
mod compact_binary;
mod consent_binary;
mod dir_creation_guard;
mod doctor_binary;
mod eval_binary;
mod fact_staleness;
mod facts_binary;
mod feedback_binary;
mod final_regression;
mod git_env;
mod investigation_binary;
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
    // Delegates so both entry points provide the identical isolated
    // environment (HOME and USERPROFILE point at `home`, so the
    // Windows home resolution also stays isolated).
    run_nisse_in_with_env(cwd, args, home, &[])
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
        .env("USERPROFILE", home)
        .env("XDG_CONFIG_HOME", home.join("xdg"))
        .env("APPDATA", home.join("appdata"))
        .env("RUST_LOG", "off")
        .env("NISSE_PI_HOST_RUNTIME", "fake")
        // Default blanket consent for directory creation: the existing
        // suite exercises behavior, not the consent gate. Consent tests
        // opt out with the `\u{0}remove` sentinel value.
        .env("NISSE_ALLOW_DIR_CREATION", "1");
    for (key, value) in extra_env {
        if *value == "\u{0}remove" {
            command.env_remove(key);
        } else {
            command.env(key, value);
        }
    }
    command.output().expect("failed to spawn nisse binary")
}

/// Canonicalize and strip the Windows verbatim (`\\?\`) device-path
/// prefix: nisse prints plain canonical paths, and raw `canonicalize()`
/// yields verbatim paths on Windows.
pub(crate) fn plain_canonical(path: &std::path::Path) -> std::path::PathBuf {
    let canonical = path.canonicalize().expect("canonicalize fixture path");
    let text = canonical.to_string_lossy().into_owned();
    std::path::PathBuf::from(text.trim_start_matches(r"\\?\").to_string())
}

/// Write the same user config into every platform-native location the
/// `dirs` crate may consult (macOS, Linux/XDG, Windows).
pub(crate) fn write_user_config(home: &Path, contents: &str) {
    // The user config is `$HOME/.nisse/config.toml` on every platform
    // (the harness sets HOME and USERPROFILE to the same isolated
    // directory, so Windows resolves the same dotfile).
    let path = home.join(".nisse").join("config.toml");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// Run the built `nisse` binary like [`run_nisse_in_with_env`] but
/// feeding `input` bytes to the child's stdin (for scripted interactive
/// flows; combine with `NISSE_INTERACTIVE_CONSENT=1`).
pub(crate) fn run_nisse_with_stdin(
    cwd: &Path,
    args: &[&str],
    home: &Path,
    extra_env: &[(&str, &str)],
    input: &[u8],
) -> Output {
    use std::io::Write as _;
    use std::process::Stdio;
    let mut command = Command::new(env!("CARGO_BIN_EXE_nisse"));
    command.args(args).current_dir(cwd);
    command
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_CONFIG_HOME", home.join("xdg"))
        .env("APPDATA", home.join("appdata"))
        .env("RUST_LOG", "off")
        .env("NISSE_PI_HOST_RUNTIME", "fake")
        .env("NISSE_ALLOW_DIR_CREATION", "1");
    for (key, value) in extra_env {
        if *value == "\u{0}remove" {
            command.env_remove(key);
        } else {
            command.env(key, value);
        }
    }
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let mut child = command.spawn().expect("failed to spawn nisse binary");
    child
        .stdin
        .as_mut()
        .expect("piped stdin")
        .write_all(input)
        .expect("write stdin");
    child.wait_with_output().expect("collect nisse output")
}

/// Write a config at the LEGACY platform-native path (the pre-dotfile
/// location, still read as a one-release fallback). All three platform
/// shapes are written so the test works on every OS.
pub(crate) fn write_legacy_user_config(home: &Path, contents: &str) {
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
