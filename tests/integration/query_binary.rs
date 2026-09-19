//! Binary-level tests for `nisse query`.

use std::path::Path;

use super::run_nisse_in;
use tempfile::TempDir;

/// A temporary repository with one indexed Rust source file and one
/// durable fact record, plus an isolated home directory.
fn repo_with_source_and_fact() -> (TempDir, std::path::PathBuf, TempDir) {
    let dir = TempDir::new().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("src").join("lib.rs"),
        "pub fn frame_parser() -> u16 {\n    7\n}\n",
    )
    .unwrap();

    // One durable, verifiable fact about the queue module.
    let fact = serde_json::json!({
        "schema_version": 1,
        "id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "kind": "fact",
        "author": "kalle",
        "created_at": "2026-01-01T00:00:00Z",
        "subject": "queue capacity",
        "text": "The zephyr queue holds exactly 32 slots at runtime.",
        "tags": ["queue"],
        "state": "supported",
        "evidence": [
            {
                "kind": "source",
                "path": "src/lib.rs",
                "symbol": null,
                "line_start": 1,
                "line_end": 3,
                "content_key": null,
                "plan_section": null,
                "record_id": null,
                "excerpt": "frame_parser"
            }
        ],
        "source_review_id": null,
        "source_record_ids": [],
        "supersedes": []
    });
    let fact_dir = repo.join(".nisse").join("people").join("kalle");
    std::fs::create_dir_all(&fact_dir).unwrap();
    std::fs::write(
        fact_dir.join("01ARZ3NDEKTSV4RRFFQ69G5FAV.json"),
        serde_json::to_string_pretty(&fact).unwrap(),
    )
    .unwrap();

    std::process::Command::new("git")
        .arg("init")
        .arg("--quiet")
        .arg(&repo)
        .status()
        .unwrap();
    let home = TempDir::new().unwrap();
    (dir, repo, home)
}

fn git(repo: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("LC_ALL", "C")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

#[test]
fn query_outside_repository_fails_on_stderr_only() {
    let dir = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let out = run_nisse_in(dir.path(), &["query", "anything"], home.path());
    assert!(!out.status.success());
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "");
    assert!(
        String::from_utf8(out.stderr)
            .unwrap()
            .contains("not inside a Git repository"),
        "expected a repository requirement error"
    );
}

#[test]
fn query_reports_symbols_and_facts_to_stdout() {
    let (_dir, repo, home) = repo_with_source_and_fact();
    git(&repo, &["add", "--all"]);
    git(
        &repo,
        &[
            "-c",
            "user.email=n@t",
            "-c",
            "user.name=t",
            "commit",
            "--quiet",
            "-m",
            "one",
        ],
    );

    let out = run_nisse_in(&repo, &["query", "frame_parser"], home.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert_eq!(stderr, "");
    assert!(stdout.contains("nisse query \"frame_parser\""));
    assert!(
        stdout.contains("src/lib.rs"),
        "symbol path expected:\n{stdout}"
    );
    assert!(stdout.contains("symbol-definition"), "{stdout}");

    // The durable fact participates through its record text.
    let out = run_nisse_in(&repo, &["query", "zephyr queue slots"], home.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(out.status.success(), "{stdout}");
    assert!(
        stdout.contains("project-fact"),
        "fact entry expected:\n{stdout}"
    );
    assert!(stdout.contains("01ARZ3NDEKTSV4RRFFQ69G5FAV"), "{stdout}");
}

#[test]
fn query_json_is_machine_readable() {
    let (_dir, repo, home) = repo_with_source_and_fact();
    git(&repo, &["add", "--all"]);
    git(
        &repo,
        &[
            "-c",
            "user.email=n@t",
            "-c",
            "user.name=t",
            "commit",
            "--quiet",
            "-m",
            "one",
        ],
    );

    let out = run_nisse_in(&repo, &["query", "frame_parser", "--json"], home.path());
    assert!(out.status.success());
    assert_eq!(String::from_utf8(out.stderr).unwrap(), "");
    let stdout = String::from_utf8(out.stdout).unwrap();
    let value: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|err| panic!("stdout is one JSON document ({err}): {stdout}"));
    assert_eq!(value["query"], "frame_parser");
    assert_eq!(value["snapshot"]["mode"], "working");
    let entries = value["entries"].as_array().unwrap();
    assert!(
        entries
            .iter()
            .any(|entry| entry["kind"] == "symbol_definition" && entry["path"] == "src/lib.rs"),
        "entries: {entries:?}"
    );
}

#[test]
fn query_without_matches_is_an_empty_success() {
    let (_dir, repo, home) = repo_with_source_and_fact();
    git(&repo, &["add", "--all"]);
    git(
        &repo,
        &[
            "-c",
            "user.email=n@t",
            "-c",
            "user.name=t",
            "commit",
            "--quiet",
            "-m",
            "one",
        ],
    );

    let out = run_nisse_in(&repo, &["query", "zzzznope", "--json"], home.path());
    assert!(out.status.success());
    let value: serde_json::Value =
        serde_json::from_str(&String::from_utf8(out.stdout).unwrap()).unwrap();
    assert_eq!(value["entries"].as_array().unwrap().len(), 0);
    assert_eq!(value["truncated"], false);
}

#[test]
fn query_handles_fts_metacharacters_safely() {
    let (_dir, repo, home) = repo_with_source_and_fact();
    git(&repo, &["add", "--all"]);
    git(
        &repo,
        &[
            "-c",
            "user.email=n@t",
            "-c",
            "user.name=t",
            "commit",
            "--quiet",
            "-m",
            "one",
        ],
    );

    // Quotes, parens, and operators must never reach FTS syntax.
    let out = run_nisse_in(
        &repo,
        &["query", "frame_parser) \" OR 1=1 --", "--json"],
        home.path(),
    );
    assert!(out.status.success());
    let value: serde_json::Value =
        serde_json::from_str(&String::from_utf8(out.stdout).unwrap()).unwrap();
    assert!(!value["entries"].as_array().unwrap().is_empty());
}
