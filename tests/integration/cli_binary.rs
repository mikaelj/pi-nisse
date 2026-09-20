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
    for args in [
        vec!["feedback", "01J8Z", "01J8Y", "--fixed"],
        vec!["records", "pending"],
        vec!["records", "compact"],
        vec!["eval", "routing"],
        vec!["mcp"],
    ] {
        let out = run_nisse(&args);
        assert!(
            !out.status.success(),
            "{args:?} must fail before its milestone"
        );
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            "",
            "{args:?} must not write to stdout"
        );
        assert!(
            String::from_utf8(out.stderr)
                .unwrap()
                .contains("not implemented"),
            "{args:?} must report a typed NotImplemented error on stderr"
        );
    }
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
