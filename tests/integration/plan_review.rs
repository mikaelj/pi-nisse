//! Binary-level integration tests for `nisse review plan` (plan
//! milestone 15).
//!
//! The complete plan-review pipeline is exercised end to end through the
//! real `nisse` binary and the real Pi review host child process with the
//! deterministic fake reviewer runtime: plan input (`--file`/`--stdin`),
//! text/JSON rendering, deterministic routing (normal selection, dynamic
//! fallback, quality always, jury bounds), reviewer failure policy
//! (fewer than two valid reports fails explicitly; the one-retry policy
//! recovers transient failures), reviewer context evidence validated
//! against the working-tree snapshot, and the persisted local review
//! event.

use std::path::{Path, PathBuf};

use super::run_nisse_in_with_env;
use tempfile::TempDir;

/// A fake-runtime plan review in a rust repository whose plan mentions
/// the shared queue: expect jury [quality, concurrency].
const QUEUE_PLAN: &str = "\
# Fix the race on the shared queue

## Approach

Move queue mutation behind a mutex and document the locking order.

## Tests

Add a regression test for concurrent enqueue.
";

/// A plan with three strong specialist signals: expect a full jury of
/// quality plus three specialists (the frozen maximum).
const MULTI_PLAN: &str = "\
# Harden the auth flow with permission checks and authentication

## Migrate the users table schema with SQL

Move the users table to the new schema with an SQL migration.

## Fix the race on the shared queue

Guard the shared queue with a mutex.
";

/// A plan with no routing signal: expect the dynamic-domain-expert
/// fallback.
const NEUTRAL_PLAN: &str = "\
# Update the office layout

Move the desks and repaint the lounge.
";

/// Create a repository with one indexed Rust source file.
fn rust_repo(label: &str) -> (TempDir, PathBuf, TempDir) {
    let dir = tempfile::Builder::new()
        .prefix(label)
        .tempdir()
        .expect("temp dir");
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join("src")).expect("create src");
    std::fs::write(
        repo.join("src").join("queue.rs"),
        "pub struct Queue {\n    slots: [u8; 32],\n}\n\npub fn enqueue(queue: &mut Queue, item: u8) {\n    queue.slots[0] = item;\n}\n",
    )
    .expect("write source");
    git(&repo, &["init", "--quiet"]);
    let home = TempDir::new().expect("home");
    (dir, repo, home)
}

/// Create a repository with no indexed languages at all.
fn plain_repo(label: &str) -> (TempDir, PathBuf, TempDir) {
    let dir = tempfile::Builder::new()
        .prefix(label)
        .tempdir()
        .expect("temp dir");
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo");
    std::fs::write(repo.join("notes.txt"), "meeting notes\n").expect("write notes");
    git(&repo, &["init", "--quiet"]);
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

/// Run a plan review of `plan` (written to `plan.md` inside the repo)
/// and return (exit-success, stdout, stderr).
fn review_plan_file(
    repo: &Path,
    home: &TempDir,
    plan: &str,
    extra_env: &[(&str, &str)],
) -> (bool, String, String) {
    std::fs::write(repo.join("plan.md"), plan).expect("write plan");
    let out = run_nisse_in_with_env(
        repo,
        &["review", "plan", "--file", "plan.md"],
        home.path(),
        extra_env,
    );
    (
        out.status.success(),
        String::from_utf8(out.stdout).expect("stdout utf8"),
        String::from_utf8(out.stderr).expect("stderr utf8"),
    )
}

/// The single persisted review event payload of `repo`.
fn review_event_payload(repo: &Path) -> serde_json::Value {
    let db = repo.join(".git/nisse/cache.sqlite");
    let conn = rusqlite::Connection::open(&db).expect("open shared cache");
    let payload: String = conn
        .query_row(
            "SELECT payload FROM review_event ORDER BY created_at DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .expect("read review event");
    serde_json::from_str(&payload).expect("event payload is JSON")
}

fn jury_ids(event: &serde_json::Value) -> Vec<String> {
    event["jury"]["seats"]
        .as_array()
        .expect("jury seats")
        .iter()
        .map(|seat| seat["reviewer_id"].as_str().expect("seat id").to_string())
        .collect()
}

#[test]
fn plan_review_from_file_renders_text_and_succeeds() {
    let (_dir, repo, home) = rust_repo("plan-file");
    let (ok, stdout, stderr) = review_plan_file(&repo, &home, QUEUE_PLAN, &[]);
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert_eq!(stderr, "", "no logs on stderr");
    assert!(stdout.contains("nisse plan review "), "{stdout}");
    assert!(stdout.contains("snapshot: working"), "{stdout}");
    assert!(stdout.contains("summary: "), "{stdout}");
    // The deterministic fake reviewers each report one informational
    // finding about the same plan evidence: one cross-validated group.
    assert!(stdout.contains("findings (1)"), "{stdout}");
    assert!(stdout.contains("[info]"), "{stdout}");
    assert!(
        stdout.contains("cross-validated by concurrency, quality"),
        "{stdout}"
    );
    // Reviewer context evidence is validated against the working-tree
    // snapshot: the in-repo plan file is citable source.
    assert!(stdout.contains("plan.md"), "{stdout}");
}

#[test]
fn plan_review_from_stdin_emits_strict_json() {
    let (_dir, repo, home) = rust_repo("plan-stdin");
    std::fs::write(repo.join("plan2.md"), QUEUE_PLAN).expect("write plan");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_nisse"))
        .args(["review", "plan", "--stdin", "--json"])
        .current_dir(&repo)
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join("xdg"))
        .env("APPDATA", home.path().join("appdata"))
        .env("RUST_LOG", "off")
        .env("NISSE_PI_HOST_RUNTIME", "fake")
        .stdin(std::fs::File::open(repo.join("plan2.md")).expect("open plan"))
        .output()
        .expect("spawn nisse");
    let stdout = String::from_utf8(out.stdout).expect("stdout utf8");
    let stderr = String::from_utf8(out.stderr).expect("stderr utf8");
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert_eq!(stderr, "");

    let result: serde_json::Value = serde_json::from_str(&stdout).expect("strict JSON result");
    assert_eq!(result["kind"], "plan");
    assert_eq!(result["snapshot"]["mode"], "working");
    assert!(
        result["review_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );
    let findings = result["findings"].as_array().expect("findings array");
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0]["severity"], "info");
    assert_eq!(findings[0]["cross_validated"], true);
    assert_eq!(
        findings[0]["reviewer_ids"],
        serde_json::json!(["concurrency", "quality"])
    );
    // Stdin plans carry no path, so the fake reviewers' path-less source
    // evidence is dropped by validation; the informational finding
    // survives without evidence (only high/blocker require it).
    assert_eq!(findings[0]["evidence"], serde_json::json!([]));
}

#[test]
fn normal_routing_selects_specialists_and_quality_always_leads() {
    let (_dir, repo, home) = rust_repo("plan-routing");
    let (ok, stdout, stderr) = review_plan_file(&repo, &home, QUEUE_PLAN, &[]);
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");

    let event = review_event_payload(&repo);
    assert_eq!(jury_ids(&event), vec!["quality", "concurrency"]);
    // The routing decision and the request descriptors are recorded.
    assert_eq!(
        event["routing_input"]["descriptors"][0],
        "Fix the race on the shared queue"
    );
    assert_eq!(
        event["routing_input"]["languages"],
        serde_json::json!(["rust"])
    );
    assert_eq!(event["decision"]["dynamic_fallback"], false);
    // Every juror completed with a report and the synthetic reviewer
    // contexts were recorded.
    for outcome in event["outcomes"].as_array().expect("outcomes") {
        assert_eq!(outcome["outcome"], "completed");
        assert!(outcome["summary"].as_str().is_some_and(|s| !s.is_empty()));
    }
    let specs = event["specs"].as_array().expect("specs");
    assert_eq!(specs.len(), 2);
    for spec in specs {
        assert!(spec["context_entries"].as_u64().is_some_and(|n| n >= 1));
        assert!(spec["checklist_items"].as_u64().is_some_and(|n| n >= 1));
        assert_eq!(spec["timeout_ms"], 300_000);
    }
    // The result and validated fingerprints are persisted too.
    assert_eq!(event["result"]["kind"], "plan");
    assert_eq!(
        event["validated"].as_array().expect("validated").len(),
        2,
        "one validated finding per fake reviewer"
    );
}

#[test]
fn multi_signal_plan_fills_the_jury_to_the_frozen_maximum() {
    let (_dir, repo, home) = rust_repo("plan-multi");
    let (ok, stdout, stderr) = review_plan_file(&repo, &home, MULTI_PLAN, &[]);
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");

    let event = review_event_payload(&repo);
    let jury = jury_ids(&event);
    assert_eq!(jury.first().map(String::as_str), Some("quality"));
    assert_eq!(jury.len(), 4, "jury: {jury:?}");
    assert!(jury.contains(&"security".to_string()), "jury: {jury:?}");
    assert!(jury.contains(&"database".to_string()), "jury: {jury:?}");
    assert!(jury.contains(&"concurrency".to_string()), "jury: {jury:?}");
    // Every scenario keeps the frozen jury bounds 2..=4.
    assert!((2..=4).contains(&jury.len()));
}

#[test]
fn dynamic_fallback_uses_the_constrained_domain_description() {
    let (_dir, repo, home) = plain_repo("plan-fallback");
    let (ok, stdout, stderr) = review_plan_file(&repo, &home, NEUTRAL_PLAN, &[]);
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");

    let event = review_event_payload(&repo);
    assert_eq!(jury_ids(&event), vec!["quality", "dynamic-domain-expert"]);
    assert_eq!(event["decision"]["dynamic_fallback"], true);
    // The fake runtime's deterministic description was used: the expert
    // seat carries the domain label and its risks extend the checklist.
    let description = &event["domain_description"];
    assert_eq!(description["domain_label"], "office layout");
    let expert_spec = event["specs"]
        .as_array()
        .expect("specs")
        .iter()
        .find(|spec| spec["reviewer_id"] == "dynamic-domain-expert")
        .expect("expert spec");
    assert_eq!(
        expert_spec["specialization"],
        "domain expert: office layout"
    );
    let static_checklist = static_expert_checklist_len();
    assert!(
        expert_spec["checklist_items"].as_u64().expect("checklist") > static_checklist as u64,
        "domain risks must extend the static checklist"
    );
    // The expert's relevant practice tag brings its practices into the
    // reviewer context (plan section 23).
    assert!(
        expert_spec["context_entries"]
            .as_u64()
            .is_some_and(|n| n >= 1)
    );
}

#[test]
fn many_heading_plan_still_routes_and_describes() {
    // Regression: the plan carries more headings than the
    // domain-description request lists may hold individually; the
    // bounded descriptor set must still cross the boundary.
    let (_dir, repo, home) = plain_repo("plan-many-headings");
    let plan: String = (0..40)
        .map(|index| format!("# Section heading {index}\n\nbody text\n"))
        .collect();
    let (ok, stdout, stderr) = review_plan_file(&repo, &home, &plan, &[]);
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("nisse plan review "), "{stdout}");

    let event = review_event_payload(&repo);
    assert_eq!(jury_ids(&event), vec!["quality", "dynamic-domain-expert"]);
    assert!(
        event["domain_description"]["domain_label"]
            .as_str()
            .is_some_and(|label| !label.is_empty())
    );
}

/// Checklist length of the bundled dynamic-domain-expert profile.
fn static_expert_checklist_len() -> usize {
    nisse::jury::catalog::Catalog::bundled()
        .expect("catalog")
        .dynamic_domain_expert()
        .checklist
        .len()
}

#[test]
fn dynamic_description_failure_falls_back_to_the_static_template() {
    let (_dir, repo, home) = plain_repo("plan-fallback-fail");
    let (ok, stdout, stderr) = review_plan_file(
        &repo,
        &home,
        NEUTRAL_PLAN,
        &[("NISSE_FAKE_DOMAIN_DESCRIPTION_FAIL", "1")],
    );
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");

    let event = review_event_payload(&repo);
    assert_eq!(jury_ids(&event), vec!["quality", "dynamic-domain-expert"]);
    // No description was used; the review recorded the fallback
    // diagnostic and proceeded with the static template.
    assert_eq!(event["domain_description"], serde_json::Value::Null);
    let diagnostics = event["diagnostics"].as_array().expect("diagnostics");
    assert!(
        diagnostics.iter().any(
            |diagnostic| diagnostic.as_str().is_some_and(|message| message
                .contains("dynamic domain description fell back to the static template"))
        ),
        "diagnostics: {diagnostics:?}"
    );
    let expert_spec = event["specs"]
        .as_array()
        .expect("specs")
        .iter()
        .find(|spec| spec["reviewer_id"] == "dynamic-domain-expert")
        .expect("expert spec");
    assert_eq!(
        expert_spec["checklist_items"].as_u64().unwrap(),
        static_expert_checklist_len() as u64
    );
}

#[test]
fn failed_reviewer_after_retry_fails_the_review_explicitly() {
    let (_dir, repo, home) = rust_repo("plan-fail");
    let (ok, stdout, stderr) = review_plan_file(
        &repo,
        &home,
        QUEUE_PLAN,
        &[("NISSE_FAKE_REVIEWER_FAIL_FOR", "quality")],
    );
    assert!(!ok, "the review must fail explicitly");
    assert_eq!(stdout, "", "no result is printed for a failed review");
    assert!(
        stderr.contains("at least 2 are required"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("reviewer `quality` failed after its retry"),
        "stderr: {stderr}"
    );
}

#[test]
fn transient_reviewer_failure_is_recovered_by_the_single_retry() {
    let (_dir, repo, home) = rust_repo("plan-retry");
    let (ok, stdout, stderr) = review_plan_file(
        &repo,
        &home,
        QUEUE_PLAN,
        &[("NISSE_FAKE_REVIEWER_FAIL_ONCE_FOR", "quality")],
    );
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("findings (1)"), "{stdout}");

    // The review event records both completed reviewers.
    let event = review_event_payload(&repo);
    for outcome in event["outcomes"].as_array().expect("outcomes") {
        assert_eq!(outcome["outcome"], "completed");
    }
}

#[test]
fn empty_and_oversized_plans_fail_before_any_review_work() {
    let (_dir, repo, home) = rust_repo("plan-empty");

    let (ok, stdout, stderr) = review_plan_file(&repo, &home, "   \n", &[]);
    assert!(!ok);
    assert_eq!(stdout, "");
    assert!(stderr.contains("the plan is empty"), "stderr: {stderr}");

    let (ok, stdout, stderr) = review_plan_file(&repo, &home, &"x".repeat(256 * 1024 + 1), &[]);
    assert!(!ok);
    assert_eq!(stdout, "");
    assert!(stderr.contains("exceeds"), "stderr: {stderr}");

    // A missing plan file fails with a typed read error.
    let out = run_nisse_in_with_env(
        &repo,
        &["review", "plan", "--file", "missing.md"],
        home.path(),
        &[],
    );
    assert!(!out.status.success());
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "");
    assert!(
        String::from_utf8(out.stderr)
            .unwrap()
            .contains("failed to read plan"),
        "stderr must name the unreadable plan file"
    );
}

#[test]
fn plan_review_outside_a_repository_fails_with_a_typed_error() {
    let dir = TempDir::new().expect("temp dir");
    let home = TempDir::new().expect("home");
    let out = run_nisse_in_with_env(dir.path(), &["review", "plan", "--stdin"], home.path(), &[]);
    assert!(!out.status.success());
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "");
    assert!(
        String::from_utf8(out.stderr)
            .unwrap()
            .contains("not inside a Git repository"),
        "stderr must require repository context"
    );
}
