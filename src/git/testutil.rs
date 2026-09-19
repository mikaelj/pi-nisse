//! Shared fixtures for Git tests: temporary real repositories built with the
//! installed `git` executable.

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

/// A temporary real Git repository.
pub(crate) struct Fixture {
    /// Keeps the temporary directory alive.
    _dir: TempDir,
    /// Canonical worktree root (Git resolves symlinks, so must the tests).
    pub root: PathBuf,
    /// Name of the initially checked-out branch, read from the `HEAD` file
    /// so it is independent of the installed Git's default branch name.
    pub branch_name: String,
}

impl Fixture {
    /// Create an empty repository named `label` with a deterministic identity.
    pub fn repo(label: &str) -> Self {
        let dir = tempdir(label);
        git_in(dir.path(), &["init", "--quiet", "repo"]);

        let root = dir.path().join("repo").canonicalize().unwrap();
        git_in(&root, &["config", "user.email", "nisse@test.invalid"]);
        git_in(&root, &["config", "user.name", "Nisse Test"]);
        git_in(&root, &["config", "commit.gpgsign", "false"]);
        git_in(&root, &["config", "tag.gpgsign", "false"]);
        // Insulate fixtures from machine-global hook configuration by
        // pointing back at the repository's (sample-only) hooks directory.
        git_in(&root, &["config", "core.hooksPath", ".git/hooks"]);

        let branch_name = read_head_branch(&root);

        Fixture {
            _dir: dir,
            root,
            branch_name,
        }
    }

    /// Path of a sibling directory next to the repository.
    pub fn sibling(&self, name: &str) -> PathBuf {
        self.root
            .parent()
            .unwrap_or_else(|| panic!("no parent of {}", self.root.display()))
            .join(name)
    }
}

/// Run `git` in `cwd`, asserting success.
pub(crate) fn git_in(cwd: &Path, args: &[&str]) -> std::process::Output {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("LC_ALL", "C")
        .output()
        .expect("failed to spawn git");
    assert!(
        output.status.success(),
        "git {} failed in {}: {}",
        args.join(" "),
        cwd.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// Write a file under `root`, creating parent directories as needed.
pub(crate) fn write_file(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().expect("path has a parent")).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// Stage everything and commit; returns the new commit OID.
pub(crate) fn commit_all(root: &Path, message: &str) -> String {
    git_in(root, &["add", "--all"]);
    git_in(root, &["commit", "--quiet", "-m", message]);
    stdout_trimmed(git_in(root, &["rev-parse", "HEAD"]))
}

/// Trimmed stdout of a completed `git` invocation.
pub(crate) fn stdout_trimmed(output: std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// Create a uniquely named temporary directory.
fn tempdir(label: &str) -> TempDir {
    tempfile::Builder::new()
        .prefix(&format!("nisse-git-{label}-"))
        .tempdir()
        .expect("failed to create temp dir")
}

/// Read the branch name from the `HEAD` file of a freshly initialized repo.
fn read_head_branch(root: &Path) -> String {
    let head = std::fs::read_to_string(root.join(".git").join("HEAD"))
        .expect("HEAD file readable in fresh repository");
    let ref_name = head
        .trim()
        .strip_prefix("ref: refs/heads/")
        .expect("unborn HEAD points at a branch");
    ref_name.to_string()
}
