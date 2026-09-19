//! End-to-end verification that ambient `GIT_*` environment variables
//! cannot hijack repository discovery.
//!
//! `GitRunner` strips `GIT_*` variables from the environment of every Git
//! child process. Observing this requires a controlled *ambient* process
//! environment, so this test re-executes the integration-test binary as a
//! subprocess with `GIT_DIR` pointing at a decoy repository and runs only
//! the dedicated subtest inside it.

use std::path::{Path, PathBuf};
use std::process::Command;

use nisse::git::{GitRunner, RepoContext};

/// Marker set only in the re-executed subtest process.
const SUBTEST_MARKER: &str = "NISSE_TEST_GIT_ENV_SUBTEST";

#[test]
fn ambient_git_dir_cannot_hijack_discovery() {
    let exe = std::env::current_exe().expect("test binary path");

    // A decoy repository that `GIT_DIR` points at.
    let decoy = tempfile::tempdir().unwrap();
    run_git(decoy.path(), &["init", "--quiet", "."]);

    let output = Command::new(&exe)
        .args(["--exact", "subtest_ambient_git_dir_cannot_hijack_discovery"])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("LC_ALL", "C")
        .env("GIT_DIR", decoy.path())
        .env(SUBTEST_MARKER, "1")
        .output()
        .expect("failed to re-exec test binary");

    assert!(
        output.status.success(),
        "subtest failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Runs only when re-executed with [`SUBTEST_MARKER`]; a no-op otherwise.
#[test]
fn subtest_ambient_git_dir_cannot_hijack_discovery() {
    if std::env::var_os(SUBTEST_MARKER).is_none() {
        return;
    }

    let repo = tempfile::tempdir().unwrap();
    run_git(repo.path(), &["init", "--quiet", "."]);
    write(repo.path(), "a.txt", "a\n");
    run_git(repo.path(), &["add", "a.txt"]);
    run_git(
        repo.path(),
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

    // The ambient `GIT_DIR` of this process points at the decoy repository;
    // discovery must still resolve the real worktree.
    let ctx = RepoContext::discover(&GitRunner::new(), repo.path())
        .expect("discovery must ignore ambient GIT_DIR");
    assert_eq!(ctx.worktree_root, canonical(repo.path()));
    assert!(ctx.head.is_some());
}

fn run_git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
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

fn write(root: &Path, relative: &str, contents: &str) {
    std::fs::write(root.join(relative), contents).unwrap();
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap()
}
