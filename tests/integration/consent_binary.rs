//! End-to-end tests for the directory-creation consent gate
//! (user-requested plan change).
//!
//! The frozen behaviors proven at the binary level:
//!
//! - without consent (noninteractive) NO Nisse command creates any
//!   persistent directory: cache-needing commands fail with the typed
//!   consent error, `records pending` reports an empty listing, and no
//!   `.git/nisse`, `.git/../nisse`, or `.nisse` appears;
//! - `NISSE_ALLOW_DIR_CREATION=1` restores blanket consent;
//! - `[directories] allow_paths` grants exactly the named paths;
//! - feedback whose record-tree directory is refused but whose local
//!   caches exist defers durably (reason surfaced by `records pending`)
//!   and a later consented run flushes the deferred observation.

use std::path::{Path, PathBuf};

use super::{run_nisse_in_with_env, run_nisse_with_stdin, write_user_config};
use tempfile::TempDir;

/// Opt out of the harness's default blanket consent.
const NO_CONSENT: &[(&str, &str)] = &[("NISSE_ALLOW_DIR_CREATION", "\u{0}remove")];

fn init_repo(label: &str) -> (TempDir, PathBuf, TempDir) {
    let dir = tempfile::Builder::new().prefix(label).tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("src").join("lib.rs"),
        "pub fn version() -> u32 {\n    1\n}\n",
    )
    .unwrap();
    let status = std::process::Command::new("git")
        .args(["init", "--quiet", "-b", "main"])
        .current_dir(&repo)
        .env("LC_ALL", "C")
        .status()
        .unwrap();
    assert!(status.success());
    let status = std::process::Command::new("git")
        .args(["-c", "user.email=t@t", "-c", "user.name=T", "add", "--all"])
        .current_dir(&repo)
        .env("LC_ALL", "C")
        .status()
        .unwrap();
    assert!(status.success());
    let status = std::process::Command::new("git")
        .args([
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=T",
            "commit",
            "--quiet",
            "-m",
            "one",
        ])
        .current_dir(&repo)
        .env("LC_ALL", "C")
        .status()
        .unwrap();
    assert!(status.success());
    let home = TempDir::new().unwrap();
    write_user_config(home.path(), "[contributor]\nid = \"alice\"\n");
    (dir, repo, home)
}

fn git_dir(repo: &Path) -> PathBuf {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--absolute-git-dir"])
        .current_dir(repo)
        .env("LC_ALL", "C")
        .output()
        .unwrap();
    PathBuf::from(String::from_utf8(out.stdout).unwrap().trim())
}

#[test]
fn fresh_repo_without_consent_fails_closed_and_creates_nothing() {
    let (_dir, repo, home) = init_repo("consent-fresh");
    let git = git_dir(&repo);

    let out = run_nisse_in_with_env(&repo, &["query", "version"], home.path(), NO_CONSENT);
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(!out.status.success(), "query must fail closed");
    assert!(
        stderr.contains("directory creation requires consent"),
        "stderr: {stderr}"
    );
    assert!(stderr.contains("nisse doctor"), "stderr: {stderr}");
    assert!(
        stderr.contains("NISSE_ALLOW_DIR_CREATION"),
        "stderr: {stderr}"
    );

    // Read-only listing never demands a directory it does not need.
    let out = run_nisse_in_with_env(&repo, &["records", "pending"], home.path(), NO_CONSENT);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(out.status.success(), "stdout:\n{stdout}");
    assert!(stdout.contains("no pending learning state"));

    // Nothing was created anywhere.
    assert!(!git.join("nisse").exists(), "cache dir must not exist");
    assert!(!repo.join(".nisse").exists(), "record tree must not exist");
}

#[test]
fn interactive_always_prompts_persists_and_is_honored_next_run() {
    let (_dir, repo, home) = init_repo("consent-interactive");
    let git = git_dir(&repo);
    let cache_dir = git.join("nisse");

    // Scripted interactive run without any pre-existing consent: the
    // prompt appears on stderr, "a" persists the canonical path.
    let out = run_nisse_with_stdin(
        &repo,
        &["query", "version"],
        home.path(),
        &[
            ("NISSE_ALLOW_DIR_CREATION", "\u{0}remove"),
            ("NISSE_INTERACTIVE_CONSENT", "1"),
        ],
        b"a\n",
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stderr.contains("Nisse wants to create") && stderr.contains("[y]es / [a]lways / [n]o"),
        "stderr: {stderr}"
    );
    assert!(cache_dir.exists(), "the consented dir should be created");

    // "a" persisted the exact canonical path into the user config while
    // preserving the pre-existing contributor identity.
    let config = std::fs::read_to_string(home.path().join(".nisse").join("config.toml")).unwrap();
    let table: toml::Table = toml::from_str(&config).unwrap();
    assert_eq!(
        table["contributor"]["id"].as_str(),
        Some("alice"),
        "{config}"
    );
    let allowed: Vec<&str> = table["directories"]["allow_paths"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(toml::Value::as_str)
        .collect();
    let canonical_cache = std::fs::canonicalize(&cache_dir).unwrap();
    assert!(
        allowed.contains(&canonical_cache.to_string_lossy().as_ref()),
        "allowed={allowed:?}, canonical={}",
        canonical_cache.display()
    );

    // A later noninteractive run without the env honors the remembered
    // consent (no blanket, no prompt).
    let probe_repo_dir = git_dir(&repo); // unchanged
    let _ = probe_repo_dir;
    let out = run_nisse_in_with_env(
        &repo,
        &["query", "version"],
        home.path(),
        &[("NISSE_ALLOW_DIR_CREATION", "\u{0}remove")],
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");

    // And "no" refuses without creating anything.
    let (_dir2, repo2, home2) = init_repo("consent-interactive-no");
    let out = run_nisse_with_stdin(
        &repo2,
        &["query", "version"],
        home2.path(),
        &[
            ("NISSE_ALLOW_DIR_CREATION", "\u{0}remove"),
            ("NISSE_INTERACTIVE_CONSENT", "1"),
        ],
        b"n\n",
    );
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(!out.status.success(), "refusal must fail closed");
    assert!(!git_dir(&repo2).join("nisse").exists());
    assert!(!repo2.join(".nisse").exists());
    let _ = stderr;
}

#[test]
fn blanket_env_restores_previous_behavior() {
    let (_dir, repo, home) = init_repo("consent-blanket");
    let git = git_dir(&repo);

    let out = run_nisse_in_with_env(&repo, &["query", "version"], home.path(), &[]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(git.join("nisse").exists(), "cache dir should be created");
}

#[test]
fn allow_paths_grant_exactly_the_named_paths() {
    let (_dir, repo, home) = init_repo("consent-allow");
    let git = git_dir(&repo);
    let cache_dir = git.join("nisse");
    let config = format!(
        "[contributor]\nid = \"alice\"\n\n[directories]\nallow_paths = [\"{}\"]\n",
        cache_dir.display()
    );
    write_user_config(home.path(), &config);

    let out = run_nisse_in_with_env(&repo, &["query", "version"], home.path(), NO_CONSENT);
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    let doctor = run_nisse_in_with_env(&repo, &["doctor"], home.path(), NO_CONSENT);
    let doctor_out = String::from_utf8_lossy(&doctor.stdout);
    let doctor_err = String::from_utf8_lossy(&doctor.stderr);
    assert!(
        out.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}\ndoctor:\n{doctor_out}\n{doctor_err}"
    );
    assert!(
        cache_dir.exists(),
        "the allowed cache dir should be created"
    );
}

#[test]
fn feedback_defers_when_record_dir_is_refused_but_caches_exist() {
    let (_dir, repo, home) = init_repo("consent-defer");
    // Modify the working tree so a code review has a real diff.
    std::fs::write(
        repo.join("src").join("lib.rs"),
        "pub fn version() -> u32 {\n    2\n}\n",
    )
    .unwrap();

    // One review WITH blanket consent: local caches are created, no
    // record tree exists yet.
    let out = run_nisse_in_with_env(
        &repo,
        &["review", "code", "--working", "--json"],
        home.path(),
        &[],
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    let result: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let review_id = result["review_id"].as_str().unwrap().to_string();
    let finding_id = result["findings"][0]["id"].as_str().unwrap().to_string();
    assert!(!repo.join(".nisse").exists());

    // Feedback WITHOUT consent: the caches exist, the record tree would
    // need creation — the outcome defers durably instead of failing.
    let out = run_nisse_in_with_env(
        &repo,
        &["feedback", &review_id, &finding_id, "--fixed"],
        home.path(),
        NO_CONSENT,
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stdout.contains("observations pending directory consent"),
        "stdout: {stdout}"
    );
    assert!(!repo.join(".nisse").exists(), "record tree must not exist");

    // The deferral is listed with its reason.
    let out = run_nisse_in_with_env(&repo, &["records", "pending"], home.path(), NO_CONSENT);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("deferred observation (fixed)"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("directory creation requires consent"),
        "stdout: {stdout}"
    );

    // A later consented run flushes the deferred observation durably.
    let out = run_nisse_in_with_env(
        &repo,
        &["feedback", &review_id, &finding_id, "--fixed"],
        home.path(),
        &[],
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("already recorded"), "stdout: {stdout}");
    assert!(repo.join(".nisse").join("people").join("alice").exists());

    // And nothing is pending anymore.
    let out = run_nisse_in_with_env(&repo, &["records", "pending"], home.path(), &[]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(!stdout.contains("deferred observation"), "stdout: {stdout}");
}

#[test]
fn refused_config_dir_save_degrades_to_unsaved_identity() {
    let (_dir, repo, home) = init_repo("consent-identity");
    // No user config at all: the identity prompt fires, the answer's
    // save needs $HOME/.nisse creation, and consent is refused — the
    // save is skipped (unsaved for this run) instead of failing the
    // command with a consent error.
    std::fs::remove_dir_all(home.path().join(".nisse")).unwrap();

    let out = run_nisse_with_stdin(
        &repo,
        &["records", "compact"],
        home.path(),
        &[
            ("NISSE_ALLOW_DIR_CREATION", "\u{0}remove"),
            ("NISSE_INTERACTIVE_CONSENT", "1"),
        ],
        b"alice\n",
    );
    let stderr = String::from_utf8(out.stderr).unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        !stderr.contains("directory creation requires consent"),
        "stderr:\n{stderr}\nstdout:\n{stdout}"
    );
    assert!(
        stderr.contains("contributor identity") || stdout.contains("contributor identity"),
        "the typed no-identity outcome must surface, stderr:\n{stderr}\nstdout:\n{stdout}"
    );
    // The config directory was never created.
    assert!(!home.path().join(".nisse").exists());
}
