//! End-to-end tests for `nisse feedback` (plan milestone 19).
//!
//! The complete feedback path is exercised through the real `nisse`
//! binary: a real fake-runtime code review persists its review event,
//! then known outcomes (`--fixed`, `--accepted`, `--dismissed`) create
//! immutable durable observations constructed by Rust from the stored
//! review/finding/outcome/snapshot. The frozen semantics are proven at
//! the binary level (plan sections 49 and 50):
//!
//! - each outcome creates one observation per contributing reviewer;
//! - unknown review / unknown finding fail with typed errors;
//! - repeated submission is idempotent (no duplicate records);
//! - a different outcome for the same finding is an immutable conflict;
//! - missing contributor identity keeps the outcome pending locally;
//! - observations created on different branches merge cleanly in Git.

use std::path::{Path, PathBuf};

use super::{run_nisse_in_with_env, write_user_config};
use tempfile::TempDir;

/// A rust source file whose working-tree change routes to the jury
/// [quality, concurrency] (same fixture family as the code-review
/// tests).
const QUEUE_RS: &str = "pub struct Queue {\n    slots: [u8; 32],\n}\n\npub fn enqueue(queue: &mut Queue, item: u8) {\n    queue.slots[0] = item;\n}\n";

const QUEUE_RS_V2: &str = "pub struct Queue {\n    slots: [u8; 32],\n}\n\npub fn enqueue(queue: &mut Queue, item: u8) {\n    queue.slots[0] = item;\n}\n\npub fn drain(queue: &mut Queue) -> u8 {\n    queue.slots[0]\n}\n";

/// Create a repository with one committed rust source file and an
/// unstaged working-tree modification of it.
fn dirty_rust_repo(label: &str) -> (TempDir, PathBuf, TempDir) {
    let dir = tempfile::Builder::new()
        .prefix(label)
        .tempdir()
        .expect("temp dir");
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join("src")).expect("create src");
    std::fs::write(repo.join("src").join("queue.rs"), QUEUE_RS).expect("write source");
    git(&repo, &["init", "--quiet", "-b", "main"]);
    git(&repo, &["add", "--all"]);
    git(
        &repo,
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
    std::fs::write(repo.join("src").join("queue.rs"), QUEUE_RS_V2).expect("modify source");
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

/// Configure the contributor identity `alice` in `home`.
fn configure_alice(home: &TempDir) {
    write_user_config(home.path(), "[contributor]\nid = \"alice\"\n");
}

/// Run one fake-runtime working code review and return its strict JSON
/// result (`review_id`, `findings[0].id`, `snapshot`).
fn review_working(repo: &Path, home: &TempDir) -> serde_json::Value {
    let out = run_nisse_in_with_env(
        repo,
        &["review", "code", "--working", "--json"],
        home.path(),
        &[],
    );
    let stdout = String::from_utf8(out.stdout).expect("stdout utf8");
    let stderr = String::from_utf8(out.stderr).expect("stderr utf8");
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    serde_json::from_str(&stdout).expect("strict review JSON")
}

fn finding_id(result: &serde_json::Value) -> String {
    result["findings"][0]["id"]
        .as_str()
        .expect("finding id")
        .to_string()
}

/// Submit feedback and return (exit-success, stdout, stderr).
fn submit(
    repo: &Path,
    home: &TempDir,
    review_id: &str,
    finding: &str,
    flag: &str,
) -> (bool, String, String) {
    let out = run_nisse_in_with_env(
        repo,
        &["feedback", review_id, finding, flag],
        home.path(),
        &[],
    );
    (
        out.status.success(),
        String::from_utf8(out.stdout).expect("stdout utf8"),
        String::from_utf8(out.stderr).expect("stderr utf8"),
    )
}

/// The durable observation records of `repo` as parsed JSON values.
fn observation_records(repo: &Path) -> Vec<serde_json::Value> {
    let mut observations = Vec::new();
    let people = repo.join(".nisse/people");
    let Ok(contributors) = std::fs::read_dir(&people) else {
        return observations;
    };
    for contributor in contributors {
        let dir = contributor.expect("dir entry").path();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let path = entry.expect("dir entry").path();
            if !path.to_string_lossy().ends_with(".json") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read record");
            let record: serde_json::Value = serde_json::from_str(&text).expect("record JSON");
            if record["kind"] == "observation" {
                observations.push(record);
            }
        }
    }
    observations.sort_by_key(|record| {
        record["reviewer_id"]
            .as_str()
            .expect("reviewer id")
            .to_string()
    });
    observations
}

#[test]
fn each_outcome_creates_durable_observations_from_the_stored_review() {
    let (_dir, repo, home) = dirty_rust_repo("feedback-outcomes");
    configure_alice(&home);

    for (flag, outcome) in [
        ("--fixed", "fixed"),
        ("--accepted", "accepted"),
        ("--dismissed", "dismissed"),
    ] {
        let result = review_working(&repo, &home);
        let review_id = result["review_id"].as_str().expect("review id").to_string();
        let finding = finding_id(&result);

        let (ok, stdout, stderr) = submit(&repo, &home, &review_id, &finding, flag);
        assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
        assert!(stdout.contains("nisse feedback"), "{stdout}");
        assert!(stdout.contains(&format!("outcome: {outcome}")), "{stdout}");
        assert!(stdout.contains("recorded observations: 2"), "{stdout}");
        assert!(stdout.contains("reviewer=quality"), "{stdout}");
        assert!(stdout.contains("reviewer=concurrency"), "{stdout}");

        // The two observations of this review carry exactly the
        // metadata Rust derived from the stored review and finding.
        let all = observation_records(&repo);
        let mine: Vec<&serde_json::Value> = all
            .iter()
            .filter(|record| record["review_id"] == serde_json::json!(review_id))
            .collect();
        assert_eq!(mine.len(), 2, "one per contributing reviewer");
        for record in &mine {
            assert_eq!(record["kind"], "observation");
            assert_eq!(record["author"], "alice");
            assert_eq!(record["observation_type"], "review_outcome");
            assert_eq!(record["outcome"], serde_json::json!(outcome));
            assert_eq!(record["finding_id"], serde_json::json!(finding));
            assert_eq!(record["impact_class"], "correctness");
            assert_eq!(record["category"], "fake");
            assert_eq!(record["languages"], serde_json::json!(["rust"]));
            assert_eq!(record["tags"], serde_json::json!(["correctness", "fake"]));
            assert_eq!(record["paths"], serde_json::json!(["src/queue.rs"]));
            assert_eq!(record["snapshot"], result["snapshot"]);
            assert!(
                record["reviewer_id"] == serde_json::json!("quality")
                    || record["reviewer_id"] == serde_json::json!("concurrency"),
                "reviewer: {}",
                record["reviewer_id"]
            );
            assert!(
                record["id"].as_str().is_some_and(
                    |id| id.len() == 26 && id.chars().all(|c| c.is_ascii_alphanumeric())
                ),
                "canonical ULID id"
            );
        }
    }

    // Three reviews x two reviewers = six immutable observations.
    assert_eq!(observation_records(&repo).len(), 6);
}

#[test]
fn unknown_review_and_unknown_finding_fail_with_typed_errors() {
    let (_dir, repo, home) = dirty_rust_repo("feedback-unknown");
    configure_alice(&home);
    let result = review_working(&repo, &home);
    let review_id = result["review_id"].as_str().expect("review id");
    let finding = finding_id(&result);

    let (ok, stdout, stderr) = submit(
        &repo,
        &home,
        "01AAAAAAAAAAAAAAAAAAAAAAAAAAAAA0",
        &finding,
        "--fixed",
    );
    assert!(!ok, "unknown review must fail");
    assert_eq!(stdout, "");
    assert!(stderr.contains("no stored review"), "stderr: {stderr}");
    assert!(
        stderr.contains("01AAAAAAAAAAAAAAAAAAAAAAAAAAAAA0"),
        "stderr: {stderr}"
    );

    let (ok, stdout, stderr) = submit(&repo, &home, review_id, "no-such-finding", "--fixed");
    assert!(!ok, "unknown finding must fail");
    assert_eq!(stdout, "");
    assert!(
        stderr.contains("has no finding `no-such-finding`"),
        "stderr: {stderr}"
    );

    assert!(observation_records(&repo).is_empty(), "nothing recorded");
}

#[test]
fn repeated_submission_is_idempotent() {
    let (_dir, repo, home) = dirty_rust_repo("feedback-idempotent");
    configure_alice(&home);
    let result = review_working(&repo, &home);
    let review_id = result["review_id"].as_str().expect("review id").to_string();
    let finding = finding_id(&result);

    let (ok, stdout, stderr) = submit(&repo, &home, &review_id, &finding, "--fixed");
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("recorded observations: 2"), "{stdout}");
    assert_eq!(observation_records(&repo).len(), 2);

    // The identical submission succeeds without creating records.
    let (ok, stdout, stderr) = submit(&repo, &home, &review_id, &finding, "--fixed");
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("already recorded: fixed"), "{stdout}");
    assert_eq!(observation_records(&repo).len(), 2, "no duplicates");

    // The local bookkeeping row exists exactly once.
    let db = repo.join(".git/nisse/cache.sqlite");
    let conn = rusqlite::Connection::open(&db).expect("open shared cache");
    let count: i64 = conn
        .query_row(
            "SELECT count(*) FROM review_feedback WHERE review_id = ?1 AND finding_id = ?2",
            [&review_id, &finding],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);

    // A different outcome for the same finding is an immutable
    // conflict, recorded nowhere.
    let (ok, stdout, stderr) = submit(&repo, &home, &review_id, &finding, "--dismissed");
    assert!(!ok, "conflicting outcome must fail");
    assert_eq!(stdout, "");
    assert!(
        stderr.contains("already recorded as `fixed`"),
        "stderr: {stderr}"
    );
    assert_eq!(observation_records(&repo).len(), 2);
    for record in observation_records(&repo) {
        assert_eq!(record["outcome"], "fixed");
    }
}

#[test]
fn missing_identity_keeps_the_outcome_pending_then_records_on_resubmission() {
    let (_dir, repo, home) = dirty_rust_repo("feedback-no-identity");
    // No contributor identity configured; feedback is noninteractive
    // and never prompts (plan section 44).
    let result = review_working(&repo, &home);
    let review_id = result["review_id"].as_str().expect("review id").to_string();
    let finding = finding_id(&result);

    let (ok, stdout, stderr) = submit(&repo, &home, &review_id, &finding, "--fixed");
    assert!(!ok, "missing identity must fail explicitly");
    assert_eq!(stdout, "");
    assert!(
        stderr.contains("no contributor identity"),
        "stderr: {stderr}"
    );
    assert!(observation_records(&repo).is_empty(), "nothing durable");

    // The outcome was retained as pending local state.
    let out = run_nisse_in_with_env(&repo, &["records", "pending"], home.path(), &[]);
    let pending = String::from_utf8(out.stdout).expect("stdout utf8");
    assert!(out.status.success(), "{pending}");
    assert!(
        pending.contains("pending review feedback (fixed)"),
        "{pending}"
    );
    assert!(pending.contains(&review_id), "{pending}");

    // Once an identity exists, the resubmission records durably.
    configure_alice(&home);
    let (ok, stdout, stderr) = submit(&repo, &home, &review_id, &finding, "--fixed");
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert_eq!(observation_records(&repo).len(), 2);
}

#[test]
fn observations_merge_across_branches() {
    let (_dir, repo, home) = dirty_rust_repo("feedback-merge");
    configure_alice(&home);

    // The review is known from the shared Git-dir cache and survives
    // branch switches.
    let result = review_working(&repo, &home);
    let review_id = result["review_id"].as_str().expect("review id").to_string();
    let finding = finding_id(&result);

    // Branch b2 now (before any observation exists).
    git(&repo, &["branch", "b2"]);

    // b1 records the outcome and commits it.
    git(&repo, &["checkout", "--quiet", "-b", "b1"]);
    let (ok, stdout, stderr) = submit(&repo, &home, &review_id, &finding, "--fixed");
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    let first = observation_records(&repo);
    assert_eq!(first.len(), 2);
    let b1_ids: Vec<&str> = first
        .iter()
        .map(|record| record["id"].as_str().expect("id"))
        .collect();
    git(&repo, &["add", ".nisse"]);
    git(
        &repo,
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=T",
            "commit",
            "--quiet",
            "-m",
            "nisse: feedback on b1",
        ],
    );

    // b2 records the same outcome independently: different immutable
    // records (fresh ULIDs) that never collide with b1's.
    git(&repo, &["checkout", "--quiet", "b2"]);
    assert!(observation_records(&repo).is_empty(), "b2 starts clean");
    let (ok, stdout, stderr) = submit(&repo, &home, &review_id, &finding, "--fixed");
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    let second = observation_records(&repo);
    assert_eq!(second.len(), 2);
    for record in &second {
        let id = record["id"].as_str().expect("id");
        assert!(!b1_ids.contains(&id), "distinct record ids");
    }
    git(&repo, &["add", ".nisse"]);
    git(
        &repo,
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=T",
            "commit",
            "--quiet",
            "-m",
            "nisse: feedback on b2",
        ],
    );

    // Both branches merge cleanly; every observation survives.
    git(&repo, &["checkout", "--quiet", "main"]);
    git(&repo, &["merge", "--quiet", "--no-edit", "b1"]);
    git(&repo, &["merge", "--quiet", "--no-edit", "b2"]);
    let merged = observation_records(&repo);
    assert_eq!(merged.len(), 4, "both branches' observations survive");

    // The merged durable records keep the submission idempotent.
    let (ok, stdout, stderr) = submit(&repo, &home, &review_id, &finding, "--fixed");
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("already recorded: fixed"), "{stdout}");
    assert_eq!(observation_records(&repo).len(), 4);

    // `records list` loads the merged tree strictly.
    let out = run_nisse_in_with_env(&repo, &["records", "list"], home.path(), &[]);
    let listing = String::from_utf8(out.stdout).expect("stdout utf8");
    assert!(out.status.success(), "{listing}");
    assert!(listing.contains("observations (4)"), "{listing}");
    assert!(listing.contains("submitted by alice"), "{listing}");
}

#[test]
fn finding_ids_are_visible_in_the_text_review_output() {
    let (_dir, repo, home) = dirty_rust_repo("feedback-text-id");
    configure_alice(&home);
    let out = run_nisse_in_with_env(&repo, &["review", "code", "--working"], home.path(), &[]);
    let stdout = String::from_utf8(out.stdout).expect("stdout utf8");
    let stderr = String::from_utf8(out.stderr).expect("stderr utf8");
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stdout.contains("id: "),
        "text output must expose finding ids for feedback: {stdout}"
    );
}

#[test]
fn feedback_accepts_unambiguous_ulid_prefixes_and_rejects_ambiguous_ones() {
    let (repo_dir, repo, _home_guard) = dirty_rust_repo("feedback-prefix");
    let _ = repo_dir;
    let home = TempDir::new().unwrap();
    configure_alice(&home);

    let result = review_working(&repo, &home);
    let review_id = result["review_id"].as_str().unwrap().to_string();
    let finding = finding_id(&result);

    // A unique 12-character review-id prefix resolves.
    let prefix = &review_id[..12];
    let (ok, stdout, stderr) = submit(&repo, &home, prefix, &finding, "--fixed");
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("recorded observations: 2"), "{stdout}");

    // A finding-id prefix resolves the same way.
    let finding_prefix = &finding[..12];
    let (ok, stdout, stderr) = submit(&repo, &home, &review_id, finding_prefix, "--fixed");
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("already recorded: fixed"), "{stdout}");

    // A different outcome for the same finding stays a typed conflict.
    let (ok, _stdout, stderr) = submit(&repo, &home, prefix, finding_prefix, "--dismissed");
    assert!(!ok, "outcomes are immutable");
    assert!(stderr.contains("immutable"), "{stderr}");

    // Unknown and non-matching prefixes fail loudly without writing.
    let (ok, _stdout, stderr) = submit(
        &repo,
        &home,
        "01AAAAAAAAAAAAAAAAAAAAAAAA",
        &finding,
        "--fixed",
    );
    assert!(!ok);
    assert!(stderr.contains("no stored review"), "{stderr}");
    assert_eq!(observation_records(&repo).len(), 2);

    // A second stored review makes a shared prefix ambiguous: the typed
    // error lists the candidates on stderr, nothing records.
    let second = review_working(&repo, &home);
    let second_id = second["review_id"].as_str().unwrap().to_string();
    let common = review_id
        .chars()
        .zip(second_id.chars())
        .take_while(|(a, b)| a == b)
        .count();
    let ambiguous_prefix = &review_id[..common];
    let (ok, _stdout, stderr) = submit(&repo, &home, ambiguous_prefix, &finding, "--fixed");
    assert!(!ok, "ambiguous prefixes fail closed");
    assert!(stderr.contains("ambiguous review id"), "{stderr}");
    assert!(stderr.contains(&review_id), "{stderr}");
    assert!(stderr.contains(&second_id), "{stderr}");
    assert_eq!(observation_records(&repo).len(), 2);
}
