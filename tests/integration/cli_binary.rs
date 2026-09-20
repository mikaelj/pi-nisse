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
fn unimplemented_review_subcommand_fails_on_stderr_only() {
    // `eval routing` and `eval retrieval` (milestone 23) are
    // implemented; the MCP server remains a future command and must
    // fail with a typed NotImplemented error.
    let out = run_nisse(&["mcp"]);
    assert!(!out.status.success(), "mcp must fail before its milestone");
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        "",
        "mcp must not write to stdout"
    );
    assert!(
        String::from_utf8(out.stderr)
            .unwrap()
            .contains("not implemented"),
        "mcp must report a typed NotImplemented error on stderr"
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
