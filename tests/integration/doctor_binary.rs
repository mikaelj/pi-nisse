//! End-to-end tests for `nisse doctor`.
//!
//! These tests run the compiled binary with an isolated home directory so
//! user config resolution is deterministic, and with IP-literal endpoints
//! only, so no DNS or other network access is required.

use std::path::Path;

use super::{run_nisse_in, write_legacy_user_config, write_user_config};

const VALID_CONFIG: &str = r#"
[contributor]
id = "tester"

[model_profiles.standard]
model = "local-standard"
endpoint = "http://127.0.0.1:8100/v1"
thinking_level = "low"

[model_profiles.strong]
model = "local-strong"
endpoint = "http://[::1]:8101/v1"
"#;

const PUBLIC_ENDPOINT_CONFIG: &str = r#"
[model_profiles.standard]
model = "cloud-standard"
endpoint = "https://192.0.2.10/v1"

[model_profiles.strong]
model = "local-strong"
endpoint = "http://127.0.0.1:8101/v1"
"#;

fn init_repo(dir: &Path) {
    let output = std::process::Command::new("git")
        .args(["init", "-q", "."])
        .current_dir(dir)
        .output()
        .expect("failed to run git init");
    assert!(
        output.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write_repo_config(repo: &Path, contents: &str) {
    let nisse_dir = repo.join(".git").join("nisse");
    std::fs::create_dir_all(&nisse_dir).unwrap();
    std::fs::write(nisse_dir.join("config.toml"), contents).unwrap();
}

#[test]
fn doctor_fails_without_required_profiles_and_reports_on_stdout() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();

    let out = run_nisse_in(cwd.path(), &["doctor"], home.path());
    assert!(!out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stdout.contains("model profiles: FAIL"));
    assert!(stdout.contains("missing required model profile: `standard`"));
    assert!(stdout.contains("missing required model profile: `strong`"));
    assert!(stdout.contains("git: ok"));
    assert!(stdout.contains("user config: not found"));
    assert!(!stdout.contains("not implemented"));
    assert!(stderr.contains("doctor found problems"), "stderr: {stderr}");
}

#[test]
fn doctor_passes_with_valid_local_config() {
    let home = tempfile::tempdir().unwrap();
    write_user_config(home.path(), VALID_CONFIG);
    let cwd = tempfile::tempdir().unwrap();

    let out = run_nisse_in(cwd.path(), &["doctor"], home.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("config: ok"));
    assert!(stdout.contains("user config loaded"));
    assert!(stdout.contains("model profiles: ok"));
    assert!(stdout.contains("`standard`: model=local-standard"));
    assert!(stdout.contains("http://127.0.0.1:8100/v1"));
    assert!(stdout.contains("endpoint trust: ok"));
    assert!(stdout.contains("classifier: ok"));
    assert!(stdout.contains("not configured (deterministic rules-only routing)"));
    assert_eq!(stderr, "", "a passing doctor run must not write to stderr");
}

#[test]
fn legacy_config_is_loaded_with_a_migration_notice() {
    let home = tempfile::tempdir().unwrap();
    write_legacy_user_config(home.path(), VALID_CONFIG);
    let cwd = tempfile::tempdir().unwrap();

    let out = run_nisse_in(cwd.path(), &["doctor"], home.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    // The legacy settings still apply...
    assert!(stdout.contains("user config loaded"));
    assert!(stdout.contains("`standard`: model=local-standard"));
    // ...and every run says how to migrate.
    assert!(
        stderr.contains("move the file to migrate"),
        "stderr: {stderr}"
    );
    assert!(stderr.contains(".nisse/config.toml"));
}

#[test]
fn dotfile_config_wins_over_the_legacy_fallback() {
    let home = tempfile::tempdir().unwrap();
    write_legacy_user_config(home.path(), VALID_CONFIG);
    write_user_config(
        home.path(),
        &VALID_CONFIG.replace("local-standard", "dotfile-standard"),
    );
    let cwd = tempfile::tempdir().unwrap();

    let out = run_nisse_in(cwd.path(), &["doctor"], home.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("`standard`: model=dotfile-standard"));
    assert!(!stdout.contains("model=local-standard"));
    assert_eq!(
        stderr, "",
        "the migration notice must not print when the dotfile exists"
    );
}

#[test]
fn doctor_reports_malformed_config() {
    let home = tempfile::tempdir().unwrap();
    write_user_config(home.path(), "this is [ not toml");
    let cwd = tempfile::tempdir().unwrap();

    let out = run_nisse_in(cwd.path(), &["doctor"], home.path());
    assert!(!out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("config: FAIL"));
    assert!(stdout.contains("malformed configuration"));
    assert!(stdout.contains("model profiles: skipped"));
}

#[test]
fn repository_config_overrides_user_config() {
    let home = tempfile::tempdir().unwrap();
    write_user_config(home.path(), VALID_CONFIG);
    let repo = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    write_repo_config(
        repo.path(),
        "[model_profiles.standard]\nmodel = \"repo-standard\"\nendpoint = \"http://127.0.0.1:9000/v1\"\n",
    );

    let out = run_nisse_in(repo.path(), &["doctor"], home.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("model=repo-standard"));
    assert!(stdout.contains("http://127.0.0.1:9000/v1"));
    assert!(stdout.contains("repository config loaded"));
    // The user-provided `strong` profile survives the repository override.
    assert!(stdout.contains("`strong`: model=local-strong"));
}

#[test]
fn public_endpoint_is_rejected_unless_explicitly_allowed() {
    let home = tempfile::tempdir().unwrap();
    write_user_config(home.path(), PUBLIC_ENDPOINT_CONFIG);
    let cwd = tempfile::tempdir().unwrap();

    let out = run_nisse_in(cwd.path(), &["doctor"], home.path());
    assert!(!out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("endpoint trust: FAIL"));
    assert!(stdout.contains("non-permitted address"));
    assert!(stdout.contains("192.0.2.10"));

    // The repository layer explicitly permits non-private endpoints.
    let repo = tempfile::tempdir().unwrap();
    init_repo(repo.path());
    write_repo_config(
        repo.path(),
        "[network]\nallow_non_private_endpoints = true\n",
    );
    let out = run_nisse_in(repo.path(), &["doctor"], home.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("endpoint trust: ok"));
    assert!(stdout.contains("permitted by trusted local configuration override"));
}
