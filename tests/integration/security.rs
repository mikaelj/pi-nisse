//! Milestone 28: security-hardening regression tests at the binary
//! level.
//!
//! Each test proves one frozen trust boundary holds end to end:
//!
//! - malformed untrusted input (invalid UTF-8 plans, hostile `.nisse`
//!   records) fails closed with a clean error instead of a panic;
//! - revision arguments containing shell metacharacters never execute;
//! - the durable-record store refuses symlinked directory components.

use std::path::{Path, PathBuf};

use super::run_nisse_in;

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

/// A temporary repository with one commit.
fn repo(label: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix(&format!("nisse-security-{label}-"))
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

/// Isolated home so the binary cannot pick up user configuration.
fn home() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn plan_with_invalid_utf8_fails_closed() {
    let (dir, _root) = repo("plan-utf8");
    let plan = dir.path().join("plan.md");
    std::fs::write(&plan, b"# Plan\n\nnot valid utf8: \xff\xfe\n").unwrap();
    let home = home();

    let out = run_nisse_in(
        dir.path(),
        &["review", "plan", "--file", "plan.md"],
        home.path(),
    );
    assert!(!out.status.success(), "must fail");
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(stdout, "", "no report may be produced for a broken plan");
    assert!(
        stderr.contains("not valid UTF-8"),
        "the failure must be the typed UTF-8 error: {stderr}"
    );
    assert!(
        !stderr.contains("panicked"),
        "a broken plan must fail closed, not crash: {stderr}"
    );
}

#[test]
fn revision_metacharacters_never_execute() {
    let (dir, root) = repo("metachar");
    let home = home();
    for hostile in [
        "HEAD; touch pwned.txt",
        "HEAD && touch pwned.txt",
        "$(touch pwned.txt)",
        "`touch pwned.txt`",
    ] {
        let out = run_nisse_in(
            dir.path(),
            &["review", "code", "--commit", hostile],
            home.path(),
        );
        assert!(
            !out.status.success(),
            "the hostile revision {hostile:?} must be rejected"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !stderr.contains("panicked"),
            "hostile revision {hostile:?} must fail cleanly: {stderr}"
        );
    }
    assert!(
        !root.join("pwned.txt").exists(),
        "no metacharacter may ever reach a shell"
    );
}

#[test]
fn records_list_fails_closed_on_hostile_record_tree() {
    let (dir, root) = repo("hostile-records");
    let home = home();

    // An oversized record file (declared size beyond the bound).
    let people = root.join(".nisse").join("people").join("alice");
    std::fs::create_dir_all(&people).unwrap();
    let huge = people.join("01ARZ3NDEKTSV4RRFFQ69G5FAV.json");
    let file = std::fs::File::create(&huge).unwrap();
    file.set_len(1024 * 1024 + 1).unwrap();
    drop(file);

    let out = run_nisse_in(dir.path(), &["records", "list"], home.path());
    assert!(!out.status.success(), "must fail closed");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("byte bound"),
        "expected the size-bound error: {stderr}"
    );
}

#[cfg(unix)]
#[test]
fn record_creation_refuses_symlinked_directories() {
    use std::os::unix::fs::symlink;

    let (dir, root) = repo("symlink-create");
    let home = home();
    let outside = tempfile::tempdir().unwrap();

    // A pre-planted symlinked contributor directory pointing outside
    // the worktree: loading must refuse to traverse it, and nothing may
    // ever be written through the link.
    let people = root.join(".nisse").join("people");
    std::fs::create_dir_all(&people).unwrap();
    symlink(outside.path(), people.join("mallory")).unwrap();

    let out = run_nisse_in(dir.path(), &["records", "list"], home.path());
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("must be directories") || stderr.contains("symlink"),
        "expected the symlink rejection: {stderr}"
    );

    // Nothing may have escaped through the symlink.
    assert_eq!(
        std::fs::read_dir(outside.path()).unwrap().count(),
        0,
        "no record may be written through a symlink"
    );
}
