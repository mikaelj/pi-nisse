//! End-to-end tests for fact candidates and verification (plan
//! milestone 18).
//!
//! The complete fact-candidate pipeline is exercised through the real
//! `nisse` binary and the real Pi review host child process with the
//! deterministic fake reviewer runtime (scripted to propose candidate
//! facts) and the fake verifier runtime (scripted verdicts). The frozen
//! outcomes are proven at the binary level (plan section 47):
//!
//! - `supported` + a contributor identity → one durable supported fact
//!   under `.nisse/people/<id>/`;
//! - `contradicted` → rejected, nothing persisted;
//! - `insufficient` / verifier failure → pending local state;
//! - `supported` without identity → pending local state (plan section 44);
//! - low confidence / obvious secrets → deterministic rejection before
//!   any verification session;
//! - near-duplicates of an active supported fact → rejected.

use std::path::{Path, PathBuf};

use super::{run_nisse_in_with_env, write_user_config};
use tempfile::TempDir;

/// A plan whose routing selects the jury [quality, concurrency].
const QUEUE_PLAN: &str = "\
# Fix the race on the shared queue

## Approach

Move queue mutation behind a mutex and document the locking order.

## Tests

Add a regression test for concurrent enqueue.
";

const FACT_CLAIM: &str = "The sensor queue is bounded to 32 slots.";

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
    git(&repo, &["config", "user.email", "t@t"]);
    git(&repo, &["config", "core.autocrlf", "false"]);
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

/// Run one plan review whose fake reviewers propose one candidate fact
/// carrying `claim`, with the given extra environment.
fn review_with_candidate(
    repo: &Path,
    home: &TempDir,
    claim: &str,
    extra_env: &[(&str, &str)],
) -> (bool, String, String) {
    let mut env: Vec<(&str, &str)> = vec![("NISSE_FAKE_REVIEWER_FACT_CLAIM", claim)];
    env.extend_from_slice(extra_env);
    std::fs::write(repo.join("plan.md"), QUEUE_PLAN).expect("write plan");
    let out = run_nisse_in_with_env(
        repo,
        &["review", "plan", "--file", "plan.md"],
        home.path(),
        &env,
    );
    (
        out.status.success(),
        String::from_utf8(out.stdout).expect("stdout utf8"),
        String::from_utf8(out.stderr).expect("stderr utf8"),
    )
}

/// Configure the contributor identity `alice` in `home`.
fn configure_alice(home: &TempDir) {
    write_user_config(home.path(), "[contributor]\nid = \"alice\"\n");
}

/// The durable fact records of `repo` (kind fact, people location).
fn fact_records(repo: &Path) -> Vec<serde_json::Value> {
    let mut facts = Vec::new();
    let people = repo.join(".nisse/people");
    let Ok(contributors) = std::fs::read_dir(&people) else {
        return facts;
    };
    for contributor in contributors {
        let contributor_dir = contributor.expect("dir entry").path();
        if !contributor_dir.is_dir() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&contributor_dir) else {
            continue;
        };
        for entry in entries {
            let path = entry.expect("dir entry").path();
            if !path.to_string_lossy().ends_with(".json") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read record");
            let record: serde_json::Value = serde_json::from_str(&text).expect("record JSON");
            if record["kind"] == "fact" {
                facts.push(record);
            }
        }
    }
    facts
}

/// The fact-candidate rows of the shared cache's pending-learning state.
fn pending_rows(repo: &Path) -> Vec<serde_json::Value> {
    let db = repo.join(".git/nisse/cache.sqlite");
    if !db.exists() {
        return Vec::new();
    }
    let conn = rusqlite::Connection::open(&db).expect("open shared cache");
    let mut stmt = conn
        .prepare("SELECT payload FROM pending_learning WHERE kind = 'fact_candidate' ORDER BY id")
        .expect("prepare");
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query");
    rows.map(|row| serde_json::from_str(&row.expect("row")).expect("payload JSON"))
        .collect()
}

#[test]
fn supported_candidate_with_identity_becomes_a_durable_fact() {
    let (_dir, repo, home) = rust_repo("fact-supported");
    configure_alice(&home);
    let (ok, stdout, stderr) = review_with_candidate(
        &repo,
        &home,
        FACT_CLAIM,
        &[("NISSE_FAKE_FACT_VERIFICATION_VERDICTS", "supported")],
    );
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert_eq!(stderr, "");

    // Exactly one durable supported fact exists, authored by alice and
    // provenanced to the review that proposed it.
    let facts = fact_records(&repo);
    assert_eq!(facts.len(), 1, "facts: {facts:?}");
    let fact = &facts[0];
    assert_eq!(fact["kind"], "fact");
    assert_eq!(fact["author"], "alice");
    assert_eq!(fact["state"], "supported");
    assert_eq!(fact["text"], FACT_CLAIM);
    assert_eq!(fact["tags"], serde_json::json!(["fake"]));
    assert!(fact["evidence"].as_array().is_some_and(|e| !e.is_empty()));
    let review_id = fact["source_review_id"].as_str().expect("review id");
    assert!(
        stdout.contains(review_id),
        "review id {review_id} in stdout"
    );

    // The second juror's identical candidate is an in-review duplicate:
    // rejected, never verified, never persisted.
    let rows = pending_rows(&repo);
    assert_eq!(rows.len(), 1, "rows: {rows:?}");
    assert_eq!(rows[0]["disposition"], "rejected");
    assert!(
        rows[0]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("duplicate of an earlier candidate")),
        "reason: {}",
        rows[0]["reason"]
    );

    // The records commands surface both outcomes.
    let out = run_nisse_in_with_env(&repo, &["records", "list"], home.path(), &[]);
    let listing = String::from_utf8(out.stdout).expect("stdout utf8");
    assert!(out.status.success(), "{listing}");
    assert!(listing.contains("fact"), "{listing}");
    assert!(listing.contains("people/alice"), "{listing}");

    let out = run_nisse_in_with_env(&repo, &["records", "pending"], home.path(), &[]);
    let pending = String::from_utf8(out.stdout).expect("stdout utf8");
    assert!(out.status.success(), "{pending}");
    assert!(pending.contains("rejected fact candidate"), "{pending}");
}

#[test]
fn contradicted_candidate_is_never_persisted() {
    let (_dir, repo, home) = rust_repo("fact-contradicted");
    configure_alice(&home);
    let (ok, stdout, stderr) = review_with_candidate(
        &repo,
        &home,
        FACT_CLAIM,
        &[("NISSE_FAKE_FACT_VERIFICATION_VERDICTS", "contradicted")],
    );
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");

    assert!(fact_records(&repo).is_empty(), "nothing may be persisted");
    assert!(!repo.join(".nisse").exists(), "no durable records at all");

    // The verified candidate is rejected as contradicted.
    let rows = pending_rows(&repo);
    assert_eq!(rows.len(), 2, "rows: {rows:?}");
    assert_eq!(rows[0]["disposition"], "rejected");
    assert!(
        rows[0]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("contradicted")),
        "reason: {}",
        rows[0]["reason"]
    );
}

#[test]
fn insufficient_candidate_remains_pending() {
    let (_dir, repo, home) = rust_repo("fact-insufficient");
    configure_alice(&home);
    let (ok, stdout, stderr) = review_with_candidate(
        &repo,
        &home,
        FACT_CLAIM,
        &[("NISSE_FAKE_FACT_VERIFICATION_VERDICTS", "insufficient")],
    );
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(fact_records(&repo).is_empty());

    let rows = pending_rows(&repo);
    assert_eq!(rows.len(), 2, "rows: {rows:?}");
    assert_eq!(rows[0]["disposition"], "pending");
    assert!(
        rows[0]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("insufficient")),
        "reason: {}",
        rows[0]["reason"]
    );

    // `records pending` shows the pending candidate.
    let out = run_nisse_in_with_env(&repo, &["records", "pending"], home.path(), &[]);
    let pending = String::from_utf8(out.stdout).expect("stdout utf8");
    assert!(out.status.success(), "{pending}");
    assert!(pending.contains("pending fact candidate"), "{pending}");
}

#[test]
fn verifier_failure_keeps_the_candidate_pending() {
    let (_dir, repo, home) = rust_repo("fact-verifier-failure");
    configure_alice(&home);
    let (ok, stdout, stderr) = review_with_candidate(
        &repo,
        &home,
        FACT_CLAIM,
        &[("NISSE_FAKE_FACT_VERIFICATION_FAIL", "1")],
    );
    // A verifier outage never fails the review.
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(fact_records(&repo).is_empty());

    let rows = pending_rows(&repo);
    assert_eq!(rows.len(), 2, "rows: {rows:?}");
    assert_eq!(rows[0]["disposition"], "pending");
    assert!(
        rows[0]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("could not decide")),
        "reason: {}",
        rows[0]["reason"]
    );
}

#[test]
fn supported_candidate_without_identity_remains_pending() {
    let (_dir, repo, home) = rust_repo("fact-no-identity");
    // Isolated home without any contributor identity: reviews are
    // noninteractive and never prompt (plan section 44).
    let (ok, stdout, stderr) = review_with_candidate(
        &repo,
        &home,
        FACT_CLAIM,
        &[("NISSE_FAKE_FACT_VERIFICATION_VERDICTS", "supported")],
    );
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(fact_records(&repo).is_empty());

    let rows = pending_rows(&repo);
    assert_eq!(rows.len(), 2, "rows: {rows:?}");
    assert_eq!(rows[0]["disposition"], "pending");
    assert!(
        rows[0]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("no contributor identity")),
        "reason: {}",
        rows[0]["reason"]
    );
}

#[test]
fn low_confidence_candidate_is_rejected_deterministically() {
    let (_dir, repo, home) = rust_repo("fact-low-confidence");
    configure_alice(&home);
    let (ok, stdout, stderr) = review_with_candidate(
        &repo,
        &home,
        FACT_CLAIM,
        &[
            ("NISSE_FAKE_REVIEWER_FACT_CONFIDENCE", "0.5"),
            // Even a scripted `supported` verdict must never be reached.
            ("NISSE_FAKE_FACT_VERIFICATION_VERDICTS", "supported"),
        ],
    );
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(fact_records(&repo).is_empty());

    let rows = pending_rows(&repo);
    assert_eq!(rows.len(), 2, "rows: {rows:?}");
    for row in &rows {
        assert_eq!(row["disposition"], "rejected");
        assert!(
            row["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("confidence")),
            "reason: {}",
            row["reason"]
        );
    }
}

#[test]
fn secret_candidate_is_rejected_deterministically() {
    let (_dir, repo, home) = rust_repo("fact-secret");
    configure_alice(&home);
    let claim = "The deploy account uses access key AKIAIOSFODNN7EXAMPLE.";
    let (ok, stdout, stderr) = review_with_candidate(&repo, &home, claim, &[]);
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(fact_records(&repo).is_empty());

    let rows = pending_rows(&repo);
    assert_eq!(rows.len(), 2, "rows: {rows:?}");
    for row in &rows {
        assert_eq!(row["disposition"], "rejected");
        assert!(
            row["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("obvious secret")),
            "reason: {}",
            row["reason"]
        );
    }
}

#[test]
fn duplicate_of_an_active_supported_fact_is_rejected() {
    let (_dir, repo, home) = rust_repo("fact-duplicate");
    configure_alice(&home);

    // First review: the candidate is verified and persisted.
    let (ok, stdout, stderr) = review_with_candidate(
        &repo,
        &home,
        FACT_CLAIM,
        &[("NISSE_FAKE_FACT_VERIFICATION_VERDICTS", "supported")],
    );
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert_eq!(fact_records(&repo).len(), 1);

    // Second review proposing the same knowledge: near-duplicate of the
    // active supported fact, rejected before any verification.
    let (ok, stdout, stderr) = review_with_candidate(
        &repo,
        &home,
        // Same claim modulo case and whitespace: still a duplicate.
        "the sensor queue is  bounded to  32 slots.",
        &[("NISSE_FAKE_FACT_VERIFICATION_VERDICTS", "supported")],
    );
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert_eq!(
        fact_records(&repo).len(),
        1,
        "the duplicate must not create a second fact"
    );

    let rows = pending_rows(&repo);
    assert!(
        rows.iter().all(|row| row["disposition"] == "rejected"),
        "{rows:?}"
    );
    assert!(
        rows.iter().any(|row| row["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("near-duplicate of an active supported fact"))),
        "rows: {rows:?}"
    );
}

#[test]
fn ungrounded_candidate_is_rejected_for_bad_evidence() {
    // A stdin plan in a repository with no path-bearing context: the
    // fake candidate carries path-less evidence, which cannot prove
    // snapshot grounding — the deterministic validation rejects it
    // before any verification session runs.
    let dir = tempfile::Builder::new()
        .prefix("fact-bad-evidence")
        .tempdir()
        .expect("temp dir");
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo");
    std::fs::write(repo.join("notes.txt"), "meeting notes\n").expect("write notes");
    git(&repo, &["init", "--quiet"]);
    git(&repo, &["config", "user.email", "t@t"]);
    git(&repo, &["config", "core.autocrlf", "false"]);
    git(&repo, &["config", "user.name", "T"]);
    let home = TempDir::new().expect("home");
    configure_alice(&home);

    let out = {
        use std::io::Write;
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_nisse"))
            .args(["review", "plan", "--stdin"])
            .current_dir(&repo)
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path().join("xdg"))
            .env("APPDATA", home.path().join("appdata"))
            .env("RUST_LOG", "off")
            .env("NISSE_PI_HOST_RUNTIME", "fake")
            .env("NISSE_FAKE_REVIEWER_FACT_CLAIM", FACT_CLAIM)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn nisse");
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(QUEUE_PLAN.as_bytes())
            .expect("write plan");
        child.wait_with_output().expect("wait")
    };
    assert!(
        out.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(fact_records(&repo).is_empty());

    let rows = pending_rows(&repo);
    assert!(!rows.is_empty(), "the rejected candidates are retained");
    for row in &rows {
        assert_eq!(row["disposition"], "rejected");
        assert!(
            row["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("evidence")),
            "reason: {}",
            row["reason"]
        );
    }
}

#[test]
fn records_pending_without_state_reports_empty() {
    let (_dir, repo, home) = rust_repo("fact-pending-empty");
    let out = run_nisse_in_with_env(&repo, &["records", "pending"], home.path(), &[]);
    assert!(out.status.success());
    let pending = String::from_utf8(out.stdout).expect("stdout utf8");
    assert!(pending.starts_with("nisse records pending\n"), "{pending}");
    assert!(pending.contains("no pending learning state"), "{pending}");
}

#[test]
fn persisted_facts_carry_provenance_and_stale_ones_are_marked() {
    let (_dir, repo, home) = rust_repo("fact-staleness");
    configure_alice(&home);
    let (ok, stdout, stderr) = review_with_candidate(
        &repo,
        &home,
        FACT_CLAIM,
        &[("NISSE_FAKE_FACT_VERIFICATION_VERDICTS", "supported")],
    );
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");

    // The persisted fact carries Rust-stamped content provenance
    // (plan section 48): every source evidence entry names the exact
    // snapshot content it was verified against.
    let facts = fact_records(&repo);
    assert_eq!(facts.len(), 1, "facts: {facts:?}");
    let evidence = facts[0]["evidence"].as_array().expect("evidence");
    assert!(
        evidence.iter().all(|entry| entry["kind"] == "source"
            && entry["content_key"]
                .as_str()
                .is_some_and(|key| key.starts_with("git:"))),
        "stamped provenance expected: {evidence:?}"
    );
    let evidence_path = evidence[0]["path"]
        .as_str()
        .expect("evidence path")
        .to_owned();

    // While the support is unchanged, the fact is presented unmarked.
    let out = run_nisse_in_with_env(&repo, &["query", "sensor queue bounded"], home.path(), &[]);
    let stdout = String::from_utf8(out.stdout).expect("stdout utf8");
    assert!(out.status.success(), "{stdout}");
    assert!(stdout.contains("project-fact"), "{stdout}");
    assert!(!stdout.contains("[potentially stale]"), "{stdout}");

    // The supporting source changes: the recorded provenance no longer
    // matches the target snapshot, so the fact is never again
    // presented as unquestioned current truth (plan section 48).
    let source = std::fs::read_to_string(repo.join(&evidence_path)).expect("read source");
    std::fs::write(
        repo.join(&evidence_path),
        format!("{source}\n// the queue discipline changed\n"),
    )
    .expect("write source");

    let out = run_nisse_in_with_env(&repo, &["query", "sensor queue bounded"], home.path(), &[]);
    let stdout = String::from_utf8(out.stdout).expect("stdout utf8");
    assert!(out.status.success(), "{stdout}");
    assert!(stdout.contains("project-fact"), "{stdout}");
    assert!(stdout.contains("[potentially stale]"), "{stdout}");

    // Machine-readable output carries the same marking.
    let out = run_nisse_in_with_env(
        &repo,
        &["query", "sensor queue bounded", "--json"],
        home.path(),
        &[],
    );
    let stdout = String::from_utf8(out.stdout).expect("stdout utf8");
    assert!(out.status.success(), "{stdout}");
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("one JSON document");
    assert!(
        report["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .any(|entry| entry["kind"] == "project_fact"
                && entry["title"]
                    .as_str()
                    .is_some_and(|title| title.contains("[potentially stale]"))),
        "{stdout}"
    );
}
