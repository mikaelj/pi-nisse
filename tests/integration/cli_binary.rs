//! Tests for the `nisse` binary's stdout/stderr behavior.

use super::run_nisse;

#[test]
fn version_prints_to_stdout_with_zero_exit() {
    let out = run_nisse(&["--version"]);
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert_eq!(stderr, "", "--version must not write to stderr");
    assert_eq!(
        stdout.trim(),
        concat!("nisse ", env!("CARGO_PKG_VERSION")),
        "--version must print `nisse <version>` to stdout"
    );
}

#[test]
fn mcp_with_immediate_stdin_eof_exits_cleanly_and_silently() {
    // `nisse mcp` is implemented (milestone 26). With no client
    // connected (stdin at immediate EOF), the server must start,
    // shut down cleanly, and write nothing to either stream: stdout
    // is reserved for protocol messages and there was no exchange,
    // logs belong to stderr (RUST_LOG=off here). The full protocol
    // lifecycle is covered by `mcp_binary.rs`.
    use std::process::Stdio;

    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_nisse"));
    command
        .arg("mcp")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("RUST_LOG", "off");
    let out = command.output().expect("spawn nisse mcp");
    assert!(
        out.status.success(),
        "the server must exit cleanly on stdin EOF: {out:?}"
    );
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        "",
        "no protocol messages were exchanged, so stdout must be empty"
    );
    assert_eq!(
        String::from_utf8(out.stderr).unwrap(),
        "",
        "the server must not log without a reason"
    );
}

#[test]
fn feedback_outside_a_repository_requires_repository_context() {
    // `nisse feedback` is implemented (milestone 19); outside a
    // repository it fails with the typed repository error on stderr.
    let dir = tempfile::Builder::new()
        .prefix("nisse-cli-feedback-norepo-")
        .tempdir()
        .unwrap();
    let out = super::run_nisse_in(
        dir.path(),
        &["feedback", "01J8Z", "01J8Y", "--fixed"],
        dir.path(),
    );
    assert!(!out.status.success());
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "");
    assert!(
        String::from_utf8(out.stderr)
            .unwrap()
            .contains("not inside a Git repository"),
        "stderr must require repository context"
    );
}

#[test]
fn invalid_usage_reports_on_stderr_with_usage_exit_code() {
    let out = run_nisse(&["review", "code"]);
    assert!(!out.status.success());
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "");
    assert_ne!(String::from_utf8(out.stderr).unwrap(), "");
    // clap signals usage errors with exit code 2.
    assert_eq!(out.status.code(), Some(2));
}
