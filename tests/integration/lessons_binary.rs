//! End-to-end tests for deterministic lesson learning (plan milestone
//! 20).
//!
//! The complete lesson path is exercised through the real `nisse`
//! binary with the deterministic fake review runtime:
//!
//! - repeated outcomes of one reviewer/category group cross the frozen
//!   threshold (`5` unique observations, `>= 4` positive, `<= 1`
//!   dismissed) and create an immutable lesson record;
//! - the feedback submission that completes the group reports the
//!   lesson; unrelated groups (other reviewers) create nothing;
//! - repeated submissions stay idempotent;
//! - reviews run deterministic learning maintenance themselves: without
//!   a contributor identity the learning stays pending local state
//!   (visible in `nisse records pending`), with one it is recorded.

use std::path::{Path, PathBuf};

use super::{run_nisse_in_with_env, write_user_config};
use tempfile::TempDir;

/// A rust source file whose working-tree change routes to the jury
/// [quality, concurrency] (same fixture family as the feedback tests).
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
/// result (`review_id`, `findings[0].id`).
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

/// Seed `count` durable review-outcome observations of one
/// (reviewer, category) group under `.nisse/people/alice/`.
///
/// Each seed uses a distinct review/finding pair, so every observation
/// is unique exactly like real feedback-created ones.
fn seed_observations(repo: &Path, count: usize, reviewer: &str, category: &str) {
    let dir = repo.join(".nisse").join("people").join("alice");
    std::fs::create_dir_all(&dir).expect("create record dir");
    for index in 0..count {
        let id = format!("01ARZ3NDEKTSV4RRFFQ69G5FA{index}");
        let review = format!("01ARZ3NDEKTSV4RRFFQ69G5FB{index}");
        let record = serde_json::json!({
            "kind": "observation",
            "schema_version": 1,
            "id": id,
            "author": "alice",
            "created_at": format!("2026-01-02T12:00:{index:02}Z"),
            "observation_type": "review_outcome",
            "review_id": review,
            "finding_id": format!("seed-{index}"),
            "reviewer_id": reviewer,
            "outcome": "fixed",
            "impact_class": "correctness",
            "category": category,
            "languages": ["rust"],
            "tags": ["correctness", category],
            "paths": ["src/queue.rs"],
            "snapshot": {
                "mode": "working",
                "base_commit": null,
                "base_tree": null,
                "head_commit": null,
                "head_tree": null,
            },
        });
        std::fs::write(
            dir.join(format!("{id}.json")),
            serde_json::to_string_pretty(&record).expect("record JSON"),
        )
        .expect("write record");
    }
}

/// The durable records of `repo` as parsed JSON values, keyed by kind.
fn records(repo: &Path, kind: &str) -> Vec<serde_json::Value> {
    let mut found = Vec::new();
    for group in ["people/alice", "common"] {
        let dir = repo.join(".nisse").join(group);
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
            if record["kind"] == kind {
                found.push(record);
            }
        }
    }
    found.sort_by_key(|record| record["id"].as_str().expect("id").to_string());
    found
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

#[test]
fn repeated_outcomes_form_a_deterministic_lesson() {
    let (_dir, repo, home) = dirty_rust_repo("lessons-feedback");
    configure_alice(&home);

    // Four seeded outcomes of the (quality, fake) group; the fake
    // runtime's finding completes the group with its fifth.
    seed_observations(&repo, 4, "quality", "fake");

    let result = review_working(&repo, &home);
    let review_id = result["review_id"].as_str().expect("review id").to_string();
    let finding = result["findings"][0]["id"]
        .as_str()
        .expect("finding id")
        .to_string();

    let (ok, stdout, stderr) = submit(&repo, &home, &review_id, &finding, "--fixed");
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("recorded observations: 2"), "{stdout}");
    assert!(stdout.contains("lessons created: 1"), "{stdout}");
    assert!(stdout.contains("quality/fake"), "{stdout}");

    // Exactly one lesson exists, for the completed group only.
    let lessons = records(&repo, "lesson");
    assert_eq!(lessons.len(), 1, "no lesson for the 1-observation group");
    let lesson = &lessons[0];
    assert_eq!(lesson["author"], "alice");
    assert_eq!(lesson["subject"], "repeated `fake` findings of `quality`");
    assert_eq!(lesson["routing_hint"]["reviewer_id"], "quality");
    assert_eq!(
        lesson["routing_hint"]["path_globs"],
        serde_json::json!(["src/**"])
    );
    assert_eq!(
        lesson["routing_hint"]["required_tags_any"],
        serde_json::json!(["correctness", "fake"])
    );
    assert_eq!(lesson["routing_hint"]["boost"], serde_json::json!(0.35));
    assert_eq!(
        lesson["source_observation_ids"].as_array().map(Vec::len),
        Some(5)
    );
    assert_eq!(lesson["supersedes"], serde_json::json!([]));
    // All five unique observations are referenced, seeds included.
    for index in 0..4 {
        assert!(
            lesson["source_observation_ids"]
                .as_array()
                .expect("ids")
                .contains(&serde_json::json!(format!(
                    "01ARZ3NDEKTSV4RRFFQ69G5FA{index}"
                ))),
            "seed {index} referenced"
        );
    }

    // Repeated submission stays idempotent and creates no new lesson.
    let (ok, stdout, stderr) = submit(&repo, &home, &review_id, &finding, "--fixed");
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("already recorded: fixed"), "{stdout}");
    assert_eq!(records(&repo, "lesson").len(), 1);

    // The lesson is ordinary durable record state.
    let out = run_nisse_in_with_env(&repo, &["records", "list"], home.path(), &[]);
    let stdout = String::from_utf8(out.stdout).expect("stdout utf8");
    assert!(stdout.contains("lesson"), "{stdout}");
    assert!(
        stdout.contains("repeated `fake` findings of `quality`"),
        "{stdout}"
    );
    assert!(stdout.contains("lessons (1)"), "{stdout}");
    assert!(stdout.contains("observations (6)"), "{stdout}");
}

#[test]
fn reviews_run_learning_maintenance_pending_then_recorded() {
    let (_dir, repo, home) = dirty_rust_repo("lessons-review-maintenance");

    // Five seeded outcomes of the (concurrency, race) group.
    seed_observations(&repo, 5, "concurrency", "race");

    // Without a contributor identity, the review's deterministic
    // learning maintenance (plan section 58 step 21) defers the lesson
    // to pending local state; nothing durable is created.
    let out = review_raw(&repo, &home);
    assert!(out.status.success(), "review must succeed");
    assert!(records(&repo, "lesson").is_empty(), "nothing durable");

    let out = run_nisse_in_with_env(&repo, &["records", "pending"], home.path(), &[]);
    let stdout = String::from_utf8(out.stdout).expect("stdout utf8");
    assert!(
        stdout.contains("pending lesson maintenance (1 group(s): concurrency/race)"),
        "{stdout}"
    );
    assert!(stdout.contains("no contributor identity"), "{stdout}");

    // With an identity, the next review's maintenance records the
    // lesson from the same observations.
    configure_alice(&home);
    let out = review_raw(&repo, &home);
    assert!(out.status.success(), "review must succeed");
    let lessons = records(&repo, "lesson");
    assert_eq!(lessons.len(), 1);
    assert_eq!(lessons[0]["routing_hint"]["reviewer_id"], "concurrency");
    assert_eq!(lessons[0]["routing_hint"]["boost"], serde_json::json!(0.35));
    assert_eq!(lessons[0]["author"], "alice");

    // A further review changes nothing (the group is already current).
    let out = review_raw(&repo, &home);
    assert!(out.status.success(), "review must succeed");
    assert_eq!(records(&repo, "lesson").len(), 1);

    // The satisfied pending row is gone: `records pending` never
    // reports learning that has since happened.
    let out = run_nisse_in_with_env(&repo, &["records", "pending"], home.path(), &[]);
    let stdout = String::from_utf8(out.stdout).expect("stdout utf8");
    assert!(stdout.contains("no pending learning state"), "{stdout}");
}

/// Run one fake-runtime working code review, returning the raw output.
fn review_raw(repo: &Path, home: &TempDir) -> std::process::Output {
    run_nisse_in_with_env(repo, &["review", "code", "--working"], home.path(), &[])
}
