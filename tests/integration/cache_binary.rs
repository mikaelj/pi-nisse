//! End-to-end tests for `nisse cache status` and `nisse cache clear`.
//!
//! These run the compiled binary inside real (temporary) repositories,
//! covering the fresh-repository, ready, corrupt, and linked-worktree
//! cases without any network access.

use std::path::{Path, PathBuf};

use super::run_nisse_in;

/// Create a temporary repository with one commit and return its root.
fn repo(label: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix(&format!("nisse-cache-{label}-"))
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

/// Isolated home so cache commands cannot pick up user configuration.
fn home() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn cache_commands_outside_a_repository_fail_on_stderr() {
    let cwd = tempfile::tempdir().unwrap();
    let home = home();
    for args in [&["cache", "status"][..], &["cache", "clear"][..]] {
        let out = run_nisse_in(cwd.path(), args, home.path());
        assert!(!out.status.success(), "{args:?} must fail outside a repo");
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            "",
            "{args:?} must not write to stdout on failure"
        );
        assert!(
            String::from_utf8(out.stderr)
                .unwrap()
                .contains("not inside a Git repository"),
            "{args:?} must explain the missing repository on stderr"
        );
    }
}

#[test]
fn status_in_fresh_repository_reports_not_created_without_creating() {
    let (_dir, repo_root) = repo("fresh");
    let home = home();

    let out = run_nisse_in(&repo_root, &["cache", "status"], home.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert_eq!(stderr, "", "a healthy status run must not write to stderr");
    assert!(stdout.starts_with("nisse cache status\n"));
    assert!(stdout.contains(&format!(
        "shared cache: {}",
        repo_root.join(".git").join("nisse").join("cache.sqlite").display()
    )));
    assert!(stdout.contains(&format!(
        "worktree cache: {}",
        repo_root
            .join(".git")
            .join("nisse")
            .join("worktree.sqlite")
            .display()
    )));
    assert_eq!(
        stdout.matches("status: not created").count(),
        2,
        "both caches must be reported as not created"
    );
    // Status is diagnostic: it must not create any database.
    assert!(!repo_root.join(".git").join("nisse").exists());
}

#[test]
fn clear_creates_empty_caches_and_status_reports_them() {
    let (_dir, repo_root) = repo("clear");
    let home = home();

    let out = run_nisse_in(&repo_root, &["cache", "clear"], home.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert_eq!(stderr, "");
    assert!(stdout.contains("nisse cache clear"));
    assert!(stdout.contains("cleared shared cache: "));
    assert!(stdout.contains("cleared worktree cache: "));
    assert!(
        repo_root
            .join(".git")
            .join("nisse")
            .join("cache.sqlite")
            .is_file()
    );
    assert!(
        repo_root
            .join(".git")
            .join("nisse")
            .join("worktree.sqlite")
            .is_file()
    );

    let out = run_nisse_in(&repo_root, &["cache", "status"], home.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(out.status.success(), "stdout:\n{stdout}");
    assert_eq!(
        stdout.matches("status: ok").count(),
        2,
        "both caches must be healthy after clear"
    );
    // The shared cache remains at schema version 2; the worktree cache
    // gained the fact-staleness marking column in version 3.
    assert_eq!(stdout.matches("schema version: 2").count(), 1);
    assert_eq!(stdout.matches("schema version: 3").count(), 1);
    assert_eq!(stdout.matches("integrity: ok").count(), 2);
    for table in [
        "content_artifact",
        "parsed_artifact",
        "symbol",
        "reference",
        "text_chunk",
        "embedding",
        "review_event",
        "review_feedback",
        "pending_learning",
        "manifest",
        "path_content",
        "record_index",
        "refresh_state",
    ] {
        assert!(
            stdout.contains(&format!("{table}: 0 rows")),
            "status must list table {table} in {stdout}"
        );
    }
}

#[test]
fn status_surfaces_corruption_and_clear_rebuilds() {
    let (_dir, repo_root) = repo("corrupt");
    let home = home();
    run_nisse_in(&repo_root, &["cache", "clear"], home.path());

    // Corrupt the shared cache on disk.
    let shared_db = repo_root.join(".git").join("nisse").join("cache.sqlite");
    std::fs::write(&shared_db, b"definitely not a sqlite database").unwrap();

    let out = run_nisse_in(&repo_root, &["cache", "status"], home.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        !out.status.success(),
        "corrupt cache must make status exit non-zero"
    );
    assert!(stdout.contains("status: corrupt"), "stdout:\n{stdout}");
    assert!(stdout.contains("nisse cache clear"));
    assert!(stderr.contains("cache status found problems"));
    // Status must not rebuild the corrupt database.
    assert_eq!(
        std::fs::read(&shared_db).unwrap(),
        b"definitely not a sqlite database",
        "status must not modify a corrupt database"
    );

    // Explicit clear rebuilds it; status is healthy again.
    let out = run_nisse_in(&repo_root, &["cache", "clear"], home.path());
    assert!(out.status.success());
    let out = run_nisse_in(&repo_root, &["cache", "status"], home.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(out.status.success(), "stdout:\n{stdout}");
    assert_eq!(stdout.matches("status: ok").count(), 2);
}

#[test]
fn clear_removes_stale_journal_sidecars() {
    let (_dir, repo_root) = repo("sidecars");
    let home = home();
    run_nisse_in(&repo_root, &["cache", "clear"], home.path());

    let shared_db = repo_root.join(".git").join("nisse").join("cache.sqlite");
    let sidecars = [
        {
            let mut os = shared_db.as_os_str().to_os_string();
            os.push("-wal");
            std::path::PathBuf::from(os)
        },
        {
            let mut os = shared_db.as_os_str().to_os_string();
            os.push("-shm");
            std::path::PathBuf::from(os)
        },
    ];
    for sidecar in &sidecars {
        std::fs::write(sidecar, b"stale sidecar").unwrap();
    }

    let out = run_nisse_in(&repo_root, &["cache", "clear"], home.path());
    assert!(out.status.success());
    for sidecar in &sidecars {
        let content = std::fs::read(sidecar).unwrap_or_default();
        assert_ne!(
            content,
            b"stale sidecar",
            "clear must not leave stale sidecar content at {}",
            sidecar.display()
        );
    }
}

#[test]
fn linked_worktrees_share_the_cache_and_keep_private_worktree_databases() {
    let (_dir, repo_root) = repo("worktrees");
    let home = home();

    let wt_path = _dir.path().join("wt");
    run_git(
        &repo_root,
        &[
            "worktree",
            "add",
            "--quiet",
            wt_path.to_str().unwrap(),
            "-b",
            "topic",
        ],
    );

    // Create the caches from the main worktree.
    let out = run_nisse_in(&repo_root, &["cache", "clear"], home.path());
    assert!(out.status.success());

    // The linked worktree sees the same shared cache file but its own
    // private worktree database.
    let wt_root = wt_path.canonicalize().unwrap();
    let out = run_nisse_in(&wt_root, &["cache", "status"], home.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(out.status.success(), "stdout:\n{stdout}");
    let shared_path = repo_root.join(".git").join("nisse").join("cache.sqlite");
    assert!(stdout.contains(&format!("shared cache: {}", shared_path.display())));
    assert!(stdout.contains(&format!(
        "worktree cache: {}",
        repo_root
            .join(".git")
            .join("worktrees")
            .join("wt")
            .join("nisse")
            .join("worktree.sqlite")
            .display()
    )));
    // The shared cache is healthy (created by the main worktree) while
    // the private worktree cache does not exist yet.
    assert!(stdout.contains("status: ok"));
    assert!(stdout.contains("status: not created"));
    assert!(
        !repo_root
            .join(".git")
            .join("worktrees")
            .join("wt")
            .join("nisse")
            .exists()
    );
}
