//! End-to-end tests for `nisse records list`.
//!
//! These run the compiled binary inside real (temporary) repositories.
//! Records are written directly into `.nisse/` to verify that the
//! command loads durable records strictly — independent of any cache
//! database.

use std::path::{Path, PathBuf};

use super::run_nisse_in;

/// A valid observation record with a fixed ID and timestamp.
fn observation_json() -> serde_json::Value {
    serde_json::json!({
        "schema_version": 1,
        "id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "kind": "observation",
        "author": "alice",
        "created_at": "2026-01-19T12:00:00.000Z",
        "observation_type": "review_outcome",
        "review_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "finding_id": "q-1",
        "reviewer_id": "quality",
        "outcome": "fixed",
        "impact_class": "correctness",
        "category": "race",
        "languages": ["rust"],
        "tags": ["concurrency"],
        "paths": ["src/lib.rs"],
        "snapshot": {
            "mode": "working",
            "base_commit": null,
            "base_tree": null,
            "head_commit": null,
            "head_tree": null
        }
    })
}

/// A valid fact record with a fixed ID and timestamp.
fn fact_json(id: &str, supersedes: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "schema_version": 1,
        "id": id,
        "kind": "fact",
        "author": "alice",
        "created_at": "2026-01-19T12:05:00.000Z",
        "subject": "queue discipline",
        "text": "The sensor queue is single-producer single-consumer.",
        "tags": ["queue"],
        "state": "supported",
        "evidence": [{
            "kind": "source",
            "path": "src/lib.rs",
            "symbol": "enqueue",
            "line_start": 10,
            "line_end": 12,
            "content_key": "git:0123456789abcdef0123456789abcdef01234567",
            "plan_section": null,
            "record_id": null,
            "excerpt": "self.head = (self.head + 1) % len;"
        }],
        "source_review_id": null,
        "source_record_ids": [],
        "supersedes": supersedes.map(|id| vec![id]).unwrap_or_default(),
    })
}

/// Create a temporary repository with one commit and return its root.
fn repo(label: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix(&format!("nisse-records-{label}-"))
        .tempdir()
        .unwrap();
    run_git(dir.path(), &["init", "--quiet", "."]);
    std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
    run_git(dir.path(), &["add", "a.txt"]);
    run_git(
        dir.path(),
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=T",
            "commit",
            "--quiet",
            "-m",
            "one",
        ],
    );
    let root = dir.path().canonicalize().unwrap();
    (dir, root)
}

fn run_git(cwd: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("LC_ALL", "C")
        .output()
        .expect("failed to spawn git");
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Isolated home so the binary cannot pick up user configuration.
fn home() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

/// Write a record into the worktree's `.nisse` tree.
fn write_record(root: &Path, relative: &str, record: &serde_json::Value) -> PathBuf {
    let path = root.join(".nisse").join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let text = format!("{}\n", serde_json::to_string_pretty(record).unwrap());
    std::fs::write(&path, text).unwrap();
    path
}

#[test]
fn records_list_outside_a_repository_fails_on_stderr() {
    let cwd = tempfile::tempdir().unwrap();
    let home = home();
    let out = run_nisse_in(cwd.path(), &["records", "list"], home.path());
    assert!(!out.status.success());
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "");
    assert!(
        String::from_utf8(out.stderr)
            .unwrap()
            .contains("not inside a Git repository"),
        "must explain the missing repository on stderr"
    );
}

#[test]
fn records_list_in_fresh_repository_reports_zero_records() {
    let (_dir, repo_root) = repo("fresh");
    let home = home();

    let out = run_nisse_in(&repo_root, &["records", "list"], home.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert_eq!(stderr, "", "an empty listing must not write to stderr");
    assert!(stdout.starts_with("nisse records list\n"));
    assert!(stdout.contains(&format!("records: {}", repo_root.join(".nisse").display())));
    assert!(stdout.contains("0 records (0 superseded)"));
    // Listing never creates the record tree.
    assert!(!repo_root.join(".nisse").exists());
}

#[test]
fn records_list_prints_records_of_every_kind_with_provenance() {
    let (_dir, repo_root) = repo("kinds");
    let home = home();

    write_record(
        &repo_root,
        "people/alice/01ARZ3NDEKTSV4RRFFQ69G5FAV.json",
        &observation_json(),
    );
    write_record(
        &repo_root,
        "common/01ARZ3NDEKTSV4RRFFQ69G5FB0.json",
        &fact_json("01ARZ3NDEKTSV4RRFFQ69G5FB0", None),
    );

    let out = run_nisse_in(&repo_root, &["records", "list"], home.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert_eq!(stderr, "");
    assert!(stdout.contains("observation 01ARZ3NDEKTSV4RRFFQ69G5FAV people/alice"));
    assert!(stdout.contains("review_outcome fixed race"));
    assert!(stdout.contains("fact        01ARZ3NDEKTSV4RRFFQ69G5FB0 common"));
    assert!(stdout.contains("supported queue discipline"));
    assert!(stdout.contains("2 records (0 superseded)"));
}

#[test]
fn records_list_marks_superseded_records() {
    let (_dir, repo_root) = repo("superseded");
    let home = home();

    write_record(
        &repo_root,
        "people/alice/01ARZ3NDEKTSV4RRFFQ69G5FAV.json",
        &fact_json("01ARZ3NDEKTSV4RRFFQ69G5FAV", None),
    );
    write_record(
        &repo_root,
        "people/alice/01ARZ3NDEKTSV4RRFFQ69G5FB0.json",
        &fact_json(
            "01ARZ3NDEKTSV4RRFFQ69G5FB0",
            Some("01ARZ3NDEKTSV4RRFFQ69G5FAV"),
        ),
    );

    let out = run_nisse_in(&repo_root, &["records", "list"], home.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(out.status.success(), "stdout:\n{stdout}");
    assert!(
        stdout.contains("01ARZ3NDEKTSV4RRFFQ69G5FAV alice 2026-01-19T12:05:00.000Z supported")
            || stdout.contains("01ARZ3NDEKTSV4RRFFQ69G5FAV"),
        "listing must include the superseded record: {stdout}"
    );
    assert!(stdout.contains("[superseded]"));
    assert!(stdout.contains("2 records (1 superseded)"));
}

#[test]
fn records_list_fails_loudly_on_invalid_records() {
    let (_dir, repo_root) = repo("invalid");
    let home = home();

    let bad = write_record(
        &repo_root,
        "people/alice/01ARZ3NDEKTSV4RRFFQ69G5FAV.json",
        &serde_json::json!({"kind": "note"}),
    );

    let out = run_nisse_in(&repo_root, &["records", "list"], home.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        !out.status.success(),
        "invalid records must fail the listing"
    );
    assert_eq!(stdout, "", "no listing output on failure");
    assert!(
        stderr.contains("invalid record"),
        "stderr must name the invalid record: {stderr}"
    );
    assert!(
        stderr.contains(bad.to_string_lossy().as_ref())
            || stderr.contains("01ARZ3NDEKTSV4RRFFQ69G5FAV.json"),
        "stderr must contain the offending path: {stderr}"
    );
}

#[test]
fn records_list_survives_cache_loss_independently_of_sqlite() {
    let (_dir, repo_root) = repo("no-cache");
    let home = home();

    write_record(
        &repo_root,
        "people/alice/01ARZ3NDEKTSV4RRFFQ69G5FAV.json",
        &observation_json(),
    );

    // No cache database was ever created; records work without them.
    assert!(!repo_root.join(".git").join("nisse").exists());
    let out = run_nisse_in(&repo_root, &["records", "list"], home.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(out.status.success(), "stdout:\n{stdout}");
    assert!(stdout.contains("1 records"));
    assert!(!repo_root.join(".git").join("nisse").exists());
}
