//! Binary-level integration tests for `nisse review code` (plan
//! milestone 16).
//!
//! The complete code-review pipeline is exercised end to end through
//! the real `nisse` binary and the real Pi review host child process
//! with the deterministic fake reviewer runtime: every snapshot mode
//! (`--working`, `--staged`, `--commit`, `--range`, `--branch`), the
//! real unified diff as the leading reviewer context, deterministic
//! routing from the change, evidence validation against the exact
//! review scope, zero snapshot leakage (historical commits, linked
//! worktrees with different same-path content), the persisted local
//! review event, and the reviewer failure policy.

use std::path::{Path, PathBuf};

use crate::write_user_config;

use super::run_nisse_in_with_env;
use tempfile::TempDir;

/// A rust source file whose change routes to the concurrency
/// specialist (path token `queue`, language `rust`).
const QUEUE_RS: &str = "pub struct Queue {\n    slots: [u8; 32],\n}\n\npub fn enqueue(queue: &mut Queue, item: u8) {\n    queue.slots[0] = item;\n}\n";

const QUEUE_RS_V2: &str = "pub struct Queue {\n    slots: [u8; 32],\n}\n\npub fn enqueue(queue: &mut Queue, item: u8) {\n    queue.slots[0] = item;\n}\n\npub fn drain(queue: &mut Queue) -> u8 {\n    queue.slots[0]\n}\n";

/// Create a repository with one committed rust source file.
fn rust_repo(label: &str) -> (TempDir, PathBuf, TempDir) {
    let dir = tempfile::Builder::new()
        .prefix(label)
        .tempdir()
        .expect("temp dir");
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join("src")).expect("create src");
    std::fs::write(repo.join("src").join("queue.rs"), QUEUE_RS).expect("write source");
    git(&repo, &["init", "--quiet", "-b", "main"]);
    git(&repo, &["config", "user.email", "t@t"]);
    git(&repo, &["config", "user.name", "T"]);
    let home = TempDir::new().expect("home");
    (dir, repo, home)
}

fn git(repo: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("LC_ALL", "C")
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed");
}

/// Commit all current changes; returns the new commit OID.
fn commit_all(repo: &Path, message: &str) -> String {
    git(repo, &["add", "--all"]);
    git(repo, &["commit", "--quiet", "-m", message]);
    rev_parse_head(repo)
}

fn rev_parse_head(repo: &Path) -> String {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .env("LC_ALL", "C")
        .output()
        .expect("spawn git");
    String::from_utf8(out.stdout)
        .expect("oid utf8")
        .trim()
        .to_string()
}

/// Run a code review and return (exit-success, stdout, stderr).
fn review_code(
    repo: &Path,
    home: &TempDir,
    args: &[&str],
    extra_env: &[(&str, &str)],
) -> (bool, String, String) {
    let out = run_nisse_in_with_env(repo, args, home.path(), extra_env);
    (
        out.status.success(),
        String::from_utf8(out.stdout).expect("stdout utf8"),
        String::from_utf8(out.stderr).expect("stderr utf8"),
    )
}

/// Every persisted review-event payload of the repository's shared
/// cache, in insertion order.
fn review_events(repo: &Path) -> Vec<serde_json::Value> {
    let db = repo.join(".git/nisse/cache.sqlite");
    let conn = rusqlite::Connection::open(&db).expect("open shared cache");
    let mut stmt = conn
        .prepare("SELECT payload FROM review_event ORDER BY rowid")
        .expect("prepare review event query");
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query review events");
    rows.map(|row| serde_json::from_str(&row.expect("read payload")).expect("payload is JSON"))
        .collect()
}

/// The single persisted review event of `repo`.
fn review_event(repo: &Path) -> serde_json::Value {
    let events = review_events(repo);
    assert_eq!(events.len(), 1, "expected exactly one review event");
    events.into_iter().next().unwrap()
}

fn jury_ids(event: &serde_json::Value) -> Vec<String> {
    event["jury"]["seats"]
        .as_array()
        .expect("jury seats")
        .iter()
        .map(|seat| seat["reviewer_id"].as_str().expect("seat id").to_string())
        .collect()
}

fn changed_paths(event: &serde_json::Value) -> Vec<String> {
    event["source"]["code_source"]["changed_paths"]
        .as_array()
        .expect("changed paths")
        .iter()
        .map(|path| path.as_str().expect("path string").to_string())
        .collect()
}

/// The evidence excerpt of the first completed reviewer report.
fn first_report_excerpt(event: &serde_json::Value) -> String {
    for outcome in event["outcomes"].as_array().expect("outcomes") {
        if outcome["outcome"] == "completed" {
            return outcome["report"]["findings"][0]["evidence"][0]["excerpt"]
                .as_str()
                .expect("excerpt")
                .to_string();
        }
    }
    panic!("no completed reviewer report in {event}");
}

#[test]
fn working_mode_reviews_the_final_state_end_to_end() {
    let (_dir, repo, home) = rust_repo("code-working");
    commit_all(&repo, "Add the queue");
    // Final working-tree state: modification plus untracked addition.
    std::fs::write(repo.join("src").join("queue.rs"), QUEUE_RS_V2).expect("modify");
    std::fs::write(repo.join("src").join("caller.rs"), "pub fn call() {}\n").expect("untracked");

    let (ok, stdout, stderr) = review_code(&repo, &home, &["review", "code", "--working"], &[]);
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert_eq!(stderr, "");
    assert!(stdout.contains("nisse code review "), "{stdout}");
    assert!(stdout.contains("snapshot: working"), "{stdout}");
    assert!(stdout.contains("findings (1)"), "{stdout}");
    // The fake reviewers agree on the same finding: cross-validated.
    assert!(stdout.contains("cross-validated"), "{stdout}");

    let event = review_event(&repo);
    // Routing starts from the real change: the queue path and the rust
    // language select the concurrency specialist; quality always leads.
    assert_eq!(jury_ids(&event), vec!["quality", "concurrency"]);
    assert_eq!(
        changed_paths(&event),
        vec!["src/caller.rs".to_string(), "src/queue.rs".to_string()]
    );
    assert_eq!(event["source"]["code_source"]["mode"], "working");
    assert_eq!(
        event["routing_input"]["languages"],
        serde_json::json!(["rust"])
    );
    // The changed symbols of the diff feed routing.
    assert!(
        event["routing_input"]["descriptors"]
            .as_array()
            .expect("descriptors")
            .iter()
            .any(|d| d == "src caller.rs")
    );
    // Every reviewer received the real unified diff as leading context:
    // the fake reviewer cites the first diff entry as evidence, and the
    // evidence validates against the working-tree snapshot.
    let excerpt = first_report_excerpt(&event);
    assert!(excerpt.contains("+++ b/src/caller.rs"), "{excerpt}");
    assert!(excerpt.contains("+pub fn call()"), "{excerpt}");
    assert_eq!(
        event["validated"].as_array().expect("validated").len(),
        2,
        "the evidence of both fake reviewers validated"
    );
    assert_eq!(event["result"]["kind"], "code");
    assert_eq!(event["result"]["snapshot"]["mode"], "working");
}

#[test]
fn staged_mode_reviews_only_the_index() {
    let (_dir, repo, home) = rust_repo("code-staged");
    std::fs::write(repo.join("notes.txt"), "note\n").expect("write notes");
    commit_all(&repo, "base");
    // Staged changes.
    std::fs::write(repo.join("src").join("queue.rs"), QUEUE_RS_V2).expect("stage modify");
    git(&repo, &["add", "src/queue.rs"]);
    std::fs::write(repo.join("staged-new.txt"), "staged\n").expect("stage add");
    git(&repo, &["add", "staged-new.txt"]);
    // Unstaged noise: worktree modification and untracked file.
    std::fs::write(repo.join("src").join("queue.rs"), "WORKTREE NOISE\n").expect("dirt");
    std::fs::write(repo.join("worktree-only.txt"), "untracked\n").expect("untracked");

    let (ok, stdout, stderr) = review_code(&repo, &home, &["review", "code", "--staged"], &[]);
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("snapshot: staged"), "{stdout}");

    let event = review_event(&repo);
    // Only the staged paths are in scope.
    assert_eq!(
        changed_paths(&event),
        vec!["src/queue.rs".to_string(), "staged-new.txt".to_string()]
    );
    // The staged diff contains the index content, never the unstaged
    // working-tree noise.
    let excerpt = first_report_excerpt(&event);
    assert!(excerpt.contains("+pub fn drain"), "{excerpt}");
    assert!(!excerpt.contains("WORKTREE NOISE"), "{excerpt}");
}

#[test]
fn commit_mode_reviews_history_with_zero_snapshot_leakage() {
    let (_dir, repo, home) = rust_repo("code-commit");
    commit_all(&repo, "Add the queue");
    // The commit under review.
    std::fs::write(repo.join("src").join("queue.rs"), QUEUE_RS_V2).expect("v2");
    let reviewed = commit_all(&repo, "Add drain to the queue");
    // Later history moves the same path on.
    std::fs::write(
        repo.join("src").join("queue.rs"),
        "HISTORICAL LEAK LINE\npub fn later() {}\n",
    )
    .expect("v3");
    commit_all(&repo, "Rewrite everything");
    // The working tree is dirty on the same path too.
    std::fs::write(repo.join("src").join("queue.rs"), "WORKTREE LEAK CONTENT\n").expect("dirty");

    let (ok, stdout, stderr) = review_code(
        &repo,
        &home,
        &["review", "code", "--commit", &reviewed],
        &[],
    );
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("snapshot: commit"), "{stdout}");

    let event = review_event(&repo);
    assert_eq!(changed_paths(&event), vec!["src/queue.rs".to_string()]);
    // The review input is exactly the historical change: the diff of
    // the reviewed commit, never later commits and never the current
    // working tree.
    let excerpt = first_report_excerpt(&event);
    assert!(excerpt.contains("+pub fn drain"), "{excerpt}");
    assert!(!excerpt.contains("HISTORICAL LEAK LINE"), "{excerpt}");
    assert!(!excerpt.contains("WORKTREE LEAK"), "{excerpt}");
    // Evidence validated against the commit snapshot: the old content
    // of the reviewed change reads back from the historical tree.
    assert_eq!(
        event["validated"].as_array().expect("validated").len(),
        2,
        "both fake reviewers' evidence validated against the commit tree"
    );
    assert_eq!(event["result"]["snapshot"]["mode"], "commit");
}

#[test]
fn range_mode_compares_the_two_named_revisions() {
    let (_dir, repo, home) = rust_repo("code-range");
    commit_all(&repo, "Add the queue");
    std::fs::write(repo.join("notes.txt"), "note\n").expect("write notes");
    let base = commit_all(&repo, "mark base");
    let _ = base;
    std::fs::write(repo.join("src").join("queue.rs"), QUEUE_RS_V2).expect("v2");
    std::fs::write(repo.join("docs.md"), "# docs\n").expect("docs");
    let head = commit_all(&repo, "Extend the queue");

    let spec = format!("{base}..{head}");
    let (ok, stdout, stderr) =
        review_code(&repo, &home, &["review", "code", "--range", &spec], &[]);
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("snapshot: range"), "{stdout}");

    let event = review_event(&repo);
    assert_eq!(
        changed_paths(&event),
        vec!["docs.md".to_string(), "src/queue.rs".to_string()]
    );
    // Commit subjects of the range are deterministic descriptors.
    assert!(
        event["routing_input"]["descriptors"]
            .as_array()
            .expect("descriptors")
            .iter()
            .any(|d| d == "Extend the queue"),
        "descriptors: {}",
        event["routing_input"]["descriptors"]
    );
}

#[test]
fn branch_mode_diffs_against_the_merge_base() {
    let (_dir, repo, home) = rust_repo("code-branch");
    commit_all(&repo, "base");
    git(&repo, &["checkout", "--quiet", "-b", "topic"]);
    std::fs::write(repo.join("topic.txt"), "topic work\n").expect("topic file");
    commit_all(&repo, "topic");
    git(&repo, &["checkout", "--quiet", "main"]);
    std::fs::write(repo.join("src").join("queue.rs"), QUEUE_RS_V2).expect("main change");
    commit_all(&repo, "main work");

    // Review main's work against the topic branch's tip: the merge base
    // (the original base commit) defines the comparison, so only the
    // main-side change is in scope.
    let (ok, stdout, stderr) =
        review_code(&repo, &home, &["review", "code", "--branch", "topic"], &[]);
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("snapshot: branch"), "{stdout}");

    let event = review_event(&repo);
    assert_eq!(changed_paths(&event), vec!["src/queue.rs".to_string()]);
    // The topic-side change is not part of this branch's review scope.
    assert!(!changed_paths(&event).contains(&"topic.txt".to_string()));
    assert_eq!(event["result"]["snapshot"]["mode"], "branch");
}

#[test]
fn linked_worktrees_review_their_own_final_state_only() {
    let (_dir, repo, home) = rust_repo("code-worktree");
    commit_all(&repo, "base");

    let wt_path = _dir.path().join("wt");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "--quiet",
            wt_path.to_str().unwrap(),
            "-b",
            "topic",
        ],
    );

    // Both worktrees diverge on the SAME path and add their own
    // untracked file (named to sort after `src/` so the shared path's
    // diff is the reviewers' leading evidence).
    std::fs::write(repo.join("src").join("queue.rs"), "MAIN DIRTY\n").expect("main dirty");
    std::fs::write(repo.join("zz-main-untracked.rs"), "m\n").expect("main untracked");
    std::fs::write(wt_path.join("src").join("queue.rs"), "TOPIC DIRTY\n").expect("wt dirty");
    std::fs::write(wt_path.join("zz-wt-untracked.rs"), "w\n").expect("wt untracked");

    // Main worktree review.
    let (ok, stdout, stderr) = review_code(&repo, &home, &["review", "code", "--working"], &[]);
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    let event = review_event(&repo);
    assert_eq!(
        changed_paths(&event),
        vec![
            "src/queue.rs".to_string(),
            "zz-main-untracked.rs".to_string()
        ],
        "main sees only its own final state"
    );
    let excerpt = first_report_excerpt(&event);
    assert!(excerpt.contains("+MAIN DIRTY"), "{excerpt}");
    assert!(!excerpt.contains("TOPIC DIRTY"), "{excerpt}");

    // Linked-worktree review: its own dirty content and its own
    // untracked file; the main worktree's content never leaks in. The
    // shared cache accumulates both events.
    let (ok, stdout, stderr) = review_code(
        wt_path.as_path(),
        &home,
        &["review", "code", "--working"],
        &[],
    );
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    let events = review_events(&repo);
    assert_eq!(events.len(), 2, "both events share the common-dir cache");
    let wt_event = events
        .iter()
        .find(|event| changed_paths(event).contains(&"zz-wt-untracked.rs".to_string()))
        .expect("the worktree event names its own untracked file");
    assert_eq!(
        changed_paths(wt_event),
        vec!["src/queue.rs".to_string(), "zz-wt-untracked.rs".to_string()]
    );
    let excerpt = first_report_excerpt(wt_event);
    assert!(excerpt.contains("+TOPIC DIRTY"), "{excerpt}");
    assert!(!excerpt.contains("MAIN DIRTY"), "{excerpt}");

    // Historical review works from inside the linked worktree against
    // shared commit objects.
    let base = commit_all(&wt_path, "topic work");
    let (ok, stdout, stderr) = review_code(
        wt_path.as_path(),
        &home,
        &["review", "code", "--commit", &base],
        &[],
    );
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("snapshot: commit"), "{stdout}");
}

#[test]
fn empty_scope_fails_with_a_typed_error() {
    let (_dir, repo, home) = rust_repo("code-empty");
    commit_all(&repo, "everything committed");

    let (ok, stdout, stderr) = review_code(&repo, &home, &["review", "code", "--working"], &[]);
    assert!(!ok);
    assert_eq!(stdout, "");
    assert!(
        stderr.contains("the review scope contains no changes to review"),
        "stderr: {stderr}"
    );
    // No review event was persisted for the refused scope.
    assert!(review_events(&repo).is_empty());
}

#[test]
fn conflicted_worktree_state_is_rejected_typed() {
    let (_dir, repo, home) = rust_repo("code-conflict");
    commit_all(&repo, "base");
    git(&repo, &["checkout", "--quiet", "-b", "side"]);
    std::fs::write(repo.join("src").join("queue.rs"), "side\n").expect("side");
    commit_all(&repo, "side");
    git(&repo, &["checkout", "--quiet", "main"]);
    std::fs::write(repo.join("src").join("queue.rs"), "main\n").expect("main");
    commit_all(&repo, "main");
    let _ = std::process::Command::new("git")
        .args(["merge", "--no-ff", "side"])
        .current_dir(&repo)
        .env("LC_ALL", "C")
        .status()
        .expect("spawn git");

    for mode in ["--working", "--staged"] {
        let (ok, stdout, stderr) = review_code(&repo, &home, &["review", "code", mode], &[]);
        assert!(!ok, "{mode} must fail on a conflicted index");
        assert_eq!(stdout, "");
        assert!(
            stderr.contains("failed to resolve the snapshot"),
            "stderr: {stderr}"
        );
    }
}

#[test]
fn json_output_is_strict_review_result_json() {
    let (_dir, repo, home) = rust_repo("code-json");
    commit_all(&repo, "base");
    std::fs::write(repo.join("src").join("queue.rs"), QUEUE_RS_V2).expect("modify");

    let (ok, stdout, stderr) = review_code(
        &repo,
        &home,
        &["review", "code", "--working", "--json"],
        &[],
    );
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert_eq!(stderr, "");

    let result: serde_json::Value = serde_json::from_str(&stdout).expect("strict JSON result");
    assert_eq!(result["kind"], "code");
    assert_eq!(result["snapshot"]["mode"], "working");
    let findings = result["findings"].as_array().expect("findings array");
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0]["severity"], "info");
    assert_eq!(findings[0]["cross_validated"], true);
    // Evidence cites the changed path of the diff.
    assert_eq!(
        findings[0]["evidence"][0]["path"],
        serde_json::json!("src/queue.rs")
    );
}

#[test]
fn failed_reviewer_after_retry_fails_the_review_explicitly() {
    let (_dir, repo, home) = rust_repo("code-fail");
    commit_all(&repo, "base");
    std::fs::write(repo.join("src").join("queue.rs"), QUEUE_RS_V2).expect("modify");

    let (ok, stdout, stderr) = review_code(
        &repo,
        &home,
        &["review", "code", "--working"],
        &[("NISSE_FAKE_REVIEWER_FAIL_FOR", "quality")],
    );
    assert!(!ok, "the review must fail explicitly");
    assert_eq!(stdout, "");
    assert!(
        stderr.contains("at least 2 are required"),
        "stderr: {stderr}"
    );
}

#[test]
fn review_code_outside_a_repository_fails_with_a_typed_error() {
    let dir = TempDir::new().expect("temp dir");
    let home = TempDir::new().expect("home");
    let out = run_nisse_in_with_env(
        dir.path(),
        &["review", "code", "--working"],
        home.path(),
        &[],
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
fn code_review_levels_are_clamped_to_the_reported_model_capabilities() {
    let (_dir, repo, home) = rust_repo("code-clamp");
    commit_all(&repo, "Add the queue");
    std::fs::write(repo.join("src").join("queue.rs"), QUEUE_RS_V2).expect("modify");

    write_user_config(
        home.path(),
        "[model_profiles.standard]
model = \"zai/glm-5.3\"
thinking_level = \"max\"

[model_profiles.strong]
model = \"zai/glm-5.3\"
",
    );
    // The fake host reports both profiles as supporting only `low`, so
    // the concurrency specialist's `high` default and the configured
    // `max` override clamp down with visible diagnostics.
    let (ok, _stdout, stderr) = review_code(
        &repo,
        &home,
        &["review", "code", "--working"],
        &[(
            "NISSE_PI_HOST_CAPABILITIES",
            r#"{"standard":["low"],"strong":["low"]}"#,
        )],
    );
    assert!(ok, "review failed: {stderr}");

    let event = review_event(&repo);
    let diagnostics: Vec<String> = event["diagnostics"]
        .as_array()
        .expect("diagnostics array")
        .iter()
        .map(|value| value.as_str().expect("diagnostic text").to_string())
        .collect();
    assert!(
        diagnostics.iter().any(|note| note.contains(
            "thinking level high adjusted to low for model `zai/glm-5.3` (profile `strong`)"
        )),
        "expected the concurrency specialist's high default to clamp; diagnostics: {diagnostics:?}"
    );
    assert!(
        diagnostics.iter().any(|note| note.contains(
            "thinking level max adjusted to low for model `zai/glm-5.3` (profile `standard`)"
        )),
        "expected the configured max override to clamp; diagnostics: {diagnostics:?}"
    );
    let specs = event["specs"].as_array().expect("specs array");
    for spec in specs {
        assert_eq!(
            spec["thinking_level"], "low",
            "every reviewer spec must carry the clamped level: {spec}"
        );
    }
}
