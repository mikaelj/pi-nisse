//! End-to-end tests for knowledge consolidation (plan milestone 22).
//!
//! The complete `nisse records compact` pipeline is exercised through
//! the real `nisse` binary and the real Pi review host child process
//! with the fake synthesis/verification runtimes (scripted results and
//! verdicts). The frozen outcomes are proven at the binary level (plan
//! sections 54–56):
//!
//! - compatible facts → synthesized `MERGED` candidate + independent
//!   `supported` verification → one durable common fact superseding
//!   the originals (which remain stored for provenance);
//! - exact duplicates (as after a Git merge of independently created
//!   records) → deterministic verbatim consolidation, no synthesis
//!   session;
//! - `CANNOT_MERGE` or a contradicted merge candidate → conflict
//!   handling: per-claim verification against current evidence,
//!   resolution only when exactly one claim is clearly supported,
//!   otherwise the conflict is retained and the originals stay
//!   separate;
//! - overlapping lessons → union of unique observations, recomputed
//!   statistics (never `support_A + support_B`), one common lesson;
//! - no contributor identity → explicit failure, nothing written;
//! - consolidation output survives ordinary Git merges and repeated
//!   runs are idempotent.

use std::path::{Path, PathBuf};

use super::{run_nisse_in_with_env, write_user_config};
use tempfile::TempDir;

/// Valid fixed ULIDs (canonical Crockford base32, verified against the
/// `ulid` crate).
const A: &str = "01ARZ3NDEKTSV4RRFFQ69G5FA0";
const B: &str = "01ARZ3NDEKTSV4RRFFQ69G5FA1";
const C: &str = "01ARZ3NDEKTSV4RRFFQ69G5FA2";
const D: &str = "01ARZ3NDEKTSV4RRFFQ69G5FA3";
const E: &str = "01ARZ3NDEKTSV4RRFFQ69G5FA4";
const F: &str = "01ARZ3NDEKTSV4RRFFQ69G5FA5";
const LESSON_A: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAB";
const LESSON_B: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAC";
const REVIEW: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

const QUEUE_SOURCE: &str = "pub struct Queue {\n    slots: [u8; 32],\n}\n\npub fn enqueue(queue: &mut Queue, item: u8) {\n    let guard = queue.lock.lock();\n    queue.slots[0] = item;\n}\n";

/// Create a repository with one committed source file and return its
/// root.
fn repo(label: &str) -> (TempDir, PathBuf, TempDir) {
    let dir = tempfile::Builder::new()
        .prefix(&format!("nisse-compact-{label}-"))
        .tempdir()
        .expect("temp dir");
    let root = dir.path().join("repo");
    std::fs::create_dir_all(root.join("src")).expect("create src");
    std::fs::write(root.join("src").join("queue.rs"), QUEUE_SOURCE).expect("write source");
    git(&root, &["init", "--quiet", "--initial-branch", "main"]);
    git(&root, &["add", "."]);
    git(
        &root,
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
    let home = TempDir::new().expect("home");
    (dir, root, home)
}

fn git(cwd: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("LC_ALL", "C")
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Configure the contributor identity `alice` in `home`.
fn configure_alice(home: &TempDir) {
    write_user_config(home.path(), "[contributor]\nid = \"alice\"\n");
}

fn evidence(excerpt: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": "source",
        "path": "src/queue.rs",
        "symbol": "enqueue",
        "line_start": 1,
        "line_end": 3,
        "content_key": null,
        "plan_section": null,
        "record_id": null,
        "excerpt": excerpt,
    })
}

/// A durable fact record.
fn fact_json(
    id: &str,
    author: &str,
    created_at: &str,
    subject: &str,
    text: &str,
    tags: &[&str],
) -> serde_json::Value {
    serde_json::json!({
        "schema_version": 1,
        "id": id,
        "kind": "fact",
        "author": author,
        "created_at": created_at,
        "subject": subject,
        "text": text,
        "tags": tags,
        "state": "supported",
        "evidence": [evidence("pub struct Queue {"), evidence("let guard = queue.lock.lock();")],
        "source_review_id": null,
        "source_record_ids": [],
        "supersedes": [],
    })
}

/// A durable review-outcome observation.
fn observation_json(id: &str, author: &str, created_at: &str) -> serde_json::Value {
    serde_json::json!({
        "schema_version": 1,
        "id": id,
        "kind": "observation",
        "author": author,
        "created_at": created_at,
        "observation_type": "review_outcome",
        "review_id": REVIEW,
        "finding_id": format!("finding-{id}"),
        "reviewer_id": "concurrency",
        "outcome": "fixed",
        "impact_class": "correctness",
        "category": "race",
        "languages": ["rust"],
        "tags": ["concurrency", "race"],
        "paths": ["src/queue.rs"],
        "snapshot": {
            "mode": "working",
            "base_commit": null,
            "base_tree": null,
            "head_commit": null,
            "head_tree": null,
        },
    })
}

/// A durable lesson referencing `observations`.
fn lesson_json(
    id: &str,
    author: &str,
    created_at: &str,
    observations: &[&str],
) -> serde_json::Value {
    serde_json::json!({
        "schema_version": 1,
        "id": id,
        "kind": "lesson",
        "author": author,
        "created_at": created_at,
        "subject": "repeated `race` findings of `concurrency`",
        "text": "Repeated outcomes of `concurrency` findings in category `race`.",
        "tags": ["concurrency", "race"],
        "routing_hint": {
            "reviewer_id": "concurrency",
            "path_globs": ["src/**"],
            "required_tags_any": ["race"],
            "boost": 0.35,
        },
        "source_observation_ids": observations,
        "supersedes": [],
    })
}

/// Write a record into the worktree's `.nisse` tree; the file name is
/// the record's own ID (records are loaded strictly by identity).
fn write_record(root: &Path, relative_dir: &str, record: &serde_json::Value) {
    let id = record["id"].as_str().expect("record id");
    let path = root
        .join(".nisse")
        .join(relative_dir)
        .join(format!("{id}.json"));
    std::fs::create_dir_all(path.parent().unwrap()).expect("create record dir");
    let text = format!("{}\n", serde_json::to_string_pretty(record).unwrap());
    std::fs::write(&path, text).expect("write record");
}

/// Load every record of a location (`people` or `common`) as JSON.
fn records_at(root: &Path, location: &str) -> Vec<(PathBuf, serde_json::Value)> {
    let mut out = Vec::new();
    let base = root.join(".nisse").join(location);
    if location == "people" {
        let Ok(contributors) = std::fs::read_dir(&base) else {
            return out;
        };
        for contributor in contributors {
            let dir = contributor.expect("dir entry").path();
            if !dir.is_dir() {
                continue;
            }
            collect_json(&dir, &mut out);
        }
        return out;
    }
    if base.is_dir() {
        collect_json(&base, &mut out);
    }
    out
}

fn collect_json(dir: &Path, out: &mut Vec<(PathBuf, serde_json::Value)>) {
    for entry in std::fs::read_dir(dir).expect("read record dir") {
        let path = entry.expect("dir entry").path();
        if !path.to_string_lossy().ends_with(".json") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read record");
        out.push((path, serde_json::from_str(&text).expect("record JSON")));
    }
}

fn compact(root: &Path, home: &TempDir, env: &[(&str, &str)]) -> (bool, String, String) {
    let out = run_nisse_in_with_env(root, &["records", "compact"], home.path(), env);
    (
        out.status.success(),
        String::from_utf8(out.stdout).expect("stdout utf8"),
        String::from_utf8(out.stderr).expect("stderr utf8"),
    )
}

#[test]
fn compatible_facts_merge_into_one_verified_common_fact() {
    let (_dir, root, home) = repo("compatible");
    configure_alice(&home);
    write_record(
        &root,
        "people/alice",
        &fact_json(
            A,
            "alice",
            "2026-05-01T10:00:00.000Z",
            "queue capacity",
            "The sensor queue is bounded to 32 slots.",
            &["queue"],
        ),
    );
    write_record(
        &root,
        "people/bob",
        &fact_json(
            B,
            "bob",
            "2026-05-02T10:00:00.000Z",
            "queue capacity",
            "The queue capacity is guarded by a mutex.",
            &["mutex"],
        ),
    );

    let merged = "The sensor queue is bounded to 32 slots and guarded by a mutex.";
    let (ok, stdout, stderr) = compact(
        &root,
        &home,
        &[
            (
                "NISSE_FAKE_FACT_SYNTHESIS_RESULTS",
                &format!("merge:{merged}"),
            ),
            ("NISSE_FAKE_FACT_VERIFICATION_VERDICTS", "supported"),
        ],
    );
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");

    // Exactly one common fact exists, provenanced to both sources.
    let common = records_at(&root, "common");
    assert_eq!(common.len(), 1, "common: {common:?}");
    let record = &common[0].1;
    assert_eq!(record["kind"], "fact");
    assert_eq!(record["author"], "alice");
    assert_eq!(record["state"], "supported");
    assert_eq!(record["text"], merged);
    assert_eq!(record["subject"], "queue capacity");
    let mut sources: Vec<&str> = record["source_record_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    sources.sort();
    assert_eq!(sources, vec![A, B]);
    assert_eq!(
        record["supersedes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect::<Vec<_>>(),
        sources
    );
    let tags: Vec<&str> = record["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    assert_eq!(tags, vec!["mutex", "queue"]);

    // The originals remain stored for provenance (immutable).
    let people = records_at(&root, "people");
    assert_eq!(people.len(), 2, "people: {people:?}");

    // The compacted active knowledge serves retrieval: the merged fact
    // is retrievable, the superseded originals are not (retrieval
    // indexes active records only), and the common record ranks with
    // the common boost (plan section 45).
    let out = run_nisse_in_with_env(&root, &["query", "queue capacity"], home.path(), &[]);
    let query_out = String::from_utf8(out.stdout).expect("stdout utf8");
    assert!(out.status.success(), "{query_out}");
    assert!(query_out.contains("project-fact"), "{query_out}");
    assert!(query_out.contains(merged), "{query_out}");

    // The report says what happened; the listing marks supersession.
    assert!(
        stdout.contains("[compatible] 2 source record(s)"),
        "{stdout}"
    );
    assert!(stdout.contains("1 common fact(s) created"), "{stdout}");
    assert!(stdout.contains("0 conflict(s) retained"), "{stdout}");
    let out = run_nisse_in_with_env(&root, &["records", "list"], home.path(), &[]);
    let listing = String::from_utf8(out.stdout).expect("stdout utf8");
    assert!(out.status.success(), "{listing}");
    assert!(listing.contains("[superseded]"), "{listing}");
    assert!(listing.contains("common"), "{listing}");

    // Idempotent: the second run finds a single active fact per subject.
    let (ok, stdout, stderr) = compact(&root, &home, &[]);
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stdout.contains("no consolidatable duplication found"),
        "{stdout}"
    );
    assert!(stdout.contains("0 common fact(s) created"), "{stdout}");
    assert_eq!(records_at(&root, "common").len(), 1);
    assert_eq!(records_at(&root, "people").len(), 2);
}

#[test]
fn exact_duplicates_consolidate_deterministically_without_synthesis() {
    let (_dir, root, home) = repo("duplicates");
    configure_alice(&home);
    // The same statement recorded independently by two contributors —
    // for example after merging two branches that each learned it.
    write_record(
        &root,
        "people/alice",
        &fact_json(
            A,
            "alice",
            "2026-05-01T10:00:00.000Z",
            "queue capacity",
            "The sensor queue is bounded to 32 slots.",
            &["queue"],
        ),
    );
    write_record(
        &root,
        "people/bob",
        &fact_json(
            B,
            "bob",
            "2026-05-02T10:00:00.000Z",
            "Queue  capacity",
            // Same statement modulo case and whitespace.
            "the sensor queue is  bounded to 32 slots.",
            &["queue"],
        ),
    );

    // Exactly one verification verdict: the deterministic candidate is
    // verified once. No synthesis result is scripted — were a synthesis
    // session consulted at all, it would answer `cannot_merge` and the
    // flow would diverge (leaving no common fact).
    let (ok, stdout, stderr) = compact(
        &root,
        &home,
        &[("NISSE_FAKE_FACT_VERIFICATION_VERDICTS", "supported")],
    );
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");

    let common = records_at(&root, "common");
    assert_eq!(common.len(), 1, "common: {common:?}");
    let record = &common[0].1;
    // The agreed statement is carried verbatim from the representative.
    assert_eq!(record["text"], "The sensor queue is bounded to 32 slots.");
    assert_eq!(record["subject"], "queue capacity");
    assert!(stdout.contains("[duplicate]"), "{stdout}");
    assert_eq!(records_at(&root, "people").len(), 2);
}

#[test]
fn unsafe_merge_refusal_retains_the_unresolved_conflict() {
    let (_dir, root, home) = repo("refusal");
    configure_alice(&home);
    write_record(
        &root,
        "people/alice",
        &fact_json(
            A,
            "alice",
            "2026-05-01T10:00:00.000Z",
            "queue capacity",
            "The sensor queue is bounded to 32 slots.",
            &["queue"],
        ),
    );
    write_record(
        &root,
        "people/bob",
        &fact_json(
            B,
            "bob",
            "2026-05-02T10:00:00.000Z",
            "queue capacity",
            "The sensor queue is unbounded.",
            &["queue"],
        ),
    );

    // Synthesis refuses (no compromise may be invented) and neither
    // claim is clearly supported by current evidence: the conflict is
    // retained, the originals stay separate, nothing is written.
    let (ok, stdout, stderr) = compact(
        &root,
        &home,
        &[
            ("NISSE_FAKE_FACT_SYNTHESIS_RESULTS", "cannot_merge"),
            (
                "NISSE_FAKE_FACT_VERIFICATION_VERDICTS",
                "insufficient,insufficient",
            ),
        ],
    );
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("[conflicting]"), "{stdout}");
    assert!(
        stdout.contains("1 conflict(s) retained unresolved"),
        "{stdout}"
    );
    assert!(stdout.contains("0 common fact(s) created"), "{stdout}");
    assert!(records_at(&root, "common").is_empty());
    assert_eq!(records_at(&root, "people").len(), 2);

    // Both originals remain active (not superseded).
    let out = run_nisse_in_with_env(&root, &["records", "list"], home.path(), &[]);
    let listing = String::from_utf8(out.stdout).expect("stdout utf8");
    assert!(out.status.success(), "{listing}");
    assert!(!listing.contains("[superseded]"), "{listing}");
}

#[test]
fn conflicting_claims_resolve_when_current_evidence_supports_one() {
    let (_dir, root, home) = repo("resolution");
    configure_alice(&home);
    write_record(
        &root,
        "people/alice",
        &fact_json(
            A,
            "alice",
            "2026-05-01T10:00:00.000Z",
            "queue capacity",
            "The sensor queue is bounded to 32 slots.",
            &["queue"],
        ),
    );
    write_record(
        &root,
        "people/bob",
        &fact_json(
            B,
            "bob",
            "2026-05-02T10:00:00.000Z",
            "queue capacity",
            "The sensor queue is unbounded.",
            &["queue"],
        ),
    );

    // Synthesis refuses; per-claim verification clearly supports the
    // first claim and contradicts the second: the conflict resolves in
    // favor of the winning formulation, carried verbatim.
    let (ok, stdout, stderr) = compact(
        &root,
        &home,
        &[
            ("NISSE_FAKE_FACT_SYNTHESIS_RESULTS", "cannot_merge"),
            (
                "NISSE_FAKE_FACT_VERIFICATION_VERDICTS",
                "supported,contradicted",
            ),
        ],
    );
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("resolved in favor"), "{stdout}");
    assert!(
        stdout.contains("0 conflict(s) retained unresolved"),
        "{stdout}"
    );

    let common = records_at(&root, "common");
    assert_eq!(common.len(), 1, "common: {common:?}");
    let record = &common[0].1;
    assert_eq!(record["text"], "The sensor queue is bounded to 32 slots.");
    assert_eq!(record["state"], "supported");
    // The resolution's evidence is anchored in current content
    // (refreshed provenance).
    assert!(
        record["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["content_key"]
                .as_str()
                .is_some_and(|key| key.starts_with("git:"))),
        "evidence: {}",
        record["evidence"]
    );
    assert_eq!(records_at(&root, "people").len(), 2);
}

#[test]
fn verifier_rejection_of_merged_candidate_falls_back_to_conflict_handling() {
    let (_dir, root, home) = repo("verifier-rejection");
    configure_alice(&home);
    write_record(
        &root,
        "people/alice",
        &fact_json(
            A,
            "alice",
            "2026-05-01T10:00:00.000Z",
            "queue capacity",
            "The sensor queue is bounded to 32 slots.",
            &["queue"],
        ),
    );
    write_record(
        &root,
        "people/bob",
        &fact_json(
            B,
            "bob",
            "2026-05-02T10:00:00.000Z",
            "queue capacity",
            "The queue capacity is guarded by a mutex.",
            &["mutex"],
        ),
    );

    // The synthesized merge is contradicted; per-claim verification
    // then clearly supports only the first claim: the merge candidate
    // is discarded and the conflict resolves with the winner verbatim.
    let (ok, stdout, stderr) = compact(
        &root,
        &home,
        &[
            (
                "NISSE_FAKE_FACT_SYNTHESIS_RESULTS",
                "merge:The queue is unbounded and unguarded.",
            ),
            (
                "NISSE_FAKE_FACT_VERIFICATION_VERDICTS",
                "contradicted,supported,contradicted",
            ),
        ],
    );
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");

    let common = records_at(&root, "common");
    assert_eq!(common.len(), 1, "common: {common:?}");
    let record = &common[0].1;
    // The invented merge candidate never becomes durable knowledge.
    assert_eq!(record["text"], "The sensor queue is bounded to 32 slots.");
    assert!(
        stdout.contains("contradicted the merged statement"),
        "{stdout}"
    );
}

#[test]
fn synthesis_failure_leaves_the_group_untouched() {
    let (_dir, root, home) = repo("synthesis-failure");
    configure_alice(&home);
    write_record(
        &root,
        "people/alice",
        &fact_json(
            A,
            "alice",
            "2026-05-01T10:00:00.000Z",
            "queue capacity",
            "The sensor queue is bounded to 32 slots.",
            &["queue"],
        ),
    );
    write_record(
        &root,
        "people/bob",
        &fact_json(
            B,
            "bob",
            "2026-05-02T10:00:00.000Z",
            "queue capacity",
            "The queue capacity is guarded by a mutex.",
            &["mutex"],
        ),
    );

    // A failed synthesis session is uncertainty, never a merge: the
    // group stays untouched and the command still succeeds.
    let (ok, stdout, stderr) = compact(&root, &home, &[("NISSE_FAKE_FACT_SYNTHESIS_FAIL", "1")]);
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("[uncertain]"), "{stdout}");
    assert!(stdout.contains("0 common fact(s) created"), "{stdout}");
    assert!(records_at(&root, "common").is_empty());
    assert_eq!(records_at(&root, "people").len(), 2);
}

#[test]
fn overlapping_lessons_merge_without_double_counting() {
    let (_dir, root, home) = repo("lessons");
    configure_alice(&home);
    // Six fixed race observations; two lessons referencing overlapping
    // subsets (5 + 4 = 9 references, 6 unique observations).
    let observations = [A, B, C, D, E, F];
    for (index, id) in observations.iter().enumerate() {
        let author = if index % 2 == 0 { "alice" } else { "bob" };
        write_record(
            &root,
            &format!("people/{author}"),
            &observation_json(
                id,
                author,
                &format!("2026-05-{:02}T10:00:00.000Z", index + 1),
            ),
        );
    }
    write_record(
        &root,
        "people/alice",
        &lesson_json(
            LESSON_A,
            "alice",
            "2026-05-11T10:00:00.000Z",
            &[A, B, C, D, E],
        ),
    );
    write_record(
        &root,
        "people/bob",
        &lesson_json(LESSON_B, "bob", "2026-05-12T10:00:00.000Z", &[C, D, E, F]),
    );

    // Lesson consolidation is deterministic: no host session runs, so
    // the command works without any scripted semantic environment.
    let (ok, stdout, stderr) = compact(&root, &home, &[]);
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stdout.contains("lesson group concurrency/race 2 source record(s)"),
        "{stdout}"
    );
    assert!(stdout.contains("1 common lesson(s) created"), "{stdout}");

    let common = records_at(&root, "common");
    assert_eq!(common.len(), 1, "common: {common:?}");
    let record = &common[0].1;
    assert_eq!(record["kind"], "lesson");
    assert_eq!(record["author"], "alice");
    assert_eq!(record["routing_hint"]["reviewer_id"], "concurrency");
    // The union counts 6 unique observations — never support_A +
    // support_B = 9 (permanent invariant 7).
    let mut unique: Vec<&str> = record["source_observation_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    unique.sort_unstable();
    assert_eq!(unique, vec![A, B, C, D, E, F]);
    // Supersedes names both original lessons (provenance).
    let mut superseded: Vec<&str> = record["supersedes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    superseded.sort_unstable();
    assert_eq!(superseded, vec![LESSON_A, LESSON_B]);
    // Recomputed boost: 6/6 positives at the frozen cap.
    assert_eq!(record["routing_hint"]["boost"], 0.35);

    // Originals and observations remain stored.
    assert_eq!(records_at(&root, "people").len(), 8);

    // Idempotent: one active lesson remains for the group.
    let (ok, stdout, stderr) = compact(&root, &home, &[]);
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("0 common lesson(s) created"), "{stdout}");
    assert_eq!(records_at(&root, "common").len(), 1);
}

#[test]
fn consolidated_records_survive_an_ordinary_git_merge() {
    let (_dir, root, home) = repo("git-merge");
    configure_alice(&home);

    // Two branches independently learn compatible facts (as after
    // parallel reviews).
    git(&root, &["checkout", "--quiet", "-b", "knowledge"]);
    write_record(
        &root,
        "people/alice",
        &fact_json(
            A,
            "alice",
            "2026-05-01T10:00:00.000Z",
            "queue capacity",
            "The sensor queue is bounded to 32 slots.",
            &["queue"],
        ),
    );
    write_record(
        &root,
        "people/bob",
        &fact_json(
            B,
            "bob",
            "2026-05-02T10:00:00.000Z",
            "queue capacity",
            "The queue capacity is guarded by a mutex.",
            &["mutex"],
        ),
    );
    git(&root, &["add", ".nisse"]);
    git(
        &root,
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=T",
            "commit",
            "--quiet",
            "-m",
            "records",
        ],
    );

    let merged = "The sensor queue is bounded to 32 slots and guarded by a mutex.";
    let (ok, stdout, stderr) = compact(
        &root,
        &home,
        &[
            (
                "NISSE_FAKE_FACT_SYNTHESIS_RESULTS",
                &format!("merge:{merged}"),
            ),
            ("NISSE_FAKE_FACT_VERIFICATION_VERDICTS", "supported"),
        ],
    );
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    git(&root, &["add", ".nisse"]);
    git(
        &root,
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=T",
            "commit",
            "--quiet",
            "-m",
            "consolidated",
        ],
    );

    // The consolidated knowledge travels through an ordinary merge.
    git(&root, &["checkout", "--quiet", "main"]);
    assert!(records_at(&root, "common").is_empty());
    git(
        &root,
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=T",
            "merge",
            "--quiet",
            "--no-ff",
            "-m",
            "merge knowledge",
            "knowledge",
        ],
    );

    let out = run_nisse_in_with_env(&root, &["records", "list"], home.path(), &[]);
    let listing = String::from_utf8(out.stdout).expect("stdout utf8");
    assert!(out.status.success(), "{listing}");
    assert_eq!(records_at(&root, "common").len(), 1);
    assert_eq!(records_at(&root, "people").len(), 2);

    // After the merge the active set is already compact: rerunning is
    // a no-op even though the branch that ran the consolidation is gone.
    let (ok, stdout, stderr) = compact(&root, &home, &[]);
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("0 common fact(s) created"), "{stdout}");
    assert_eq!(records_at(&root, "common").len(), 1);
}

#[test]
fn compact_without_identity_fails_explicitly_and_writes_nothing() {
    let (_dir, root, home) = repo("no-identity");
    // Isolated home without any contributor identity: compact is
    // noninteractive here (stdin is not a terminal) and must never
    // guess an author (plan section 44).
    write_record(
        &root,
        "people/alice",
        &fact_json(
            A,
            "alice",
            "2026-05-01T10:00:00.000Z",
            "queue capacity",
            "The sensor queue is bounded to 32 slots.",
            &["queue"],
        ),
    );
    write_record(
        &root,
        "people/bob",
        &fact_json(
            B,
            "bob",
            "2026-05-02T10:00:00.000Z",
            "queue capacity",
            "The queue capacity is guarded by a mutex.",
            &["mutex"],
        ),
    );

    let out = run_nisse_in_with_env(&root, &["records", "compact"], home.path(), &[]);
    assert!(!out.status.success(), "compact must fail without identity");
    let stdout = String::from_utf8(out.stdout).expect("stdout utf8");
    let stderr = String::from_utf8(out.stderr).expect("stderr utf8");
    assert!(
        stdout.contains("contributor identity") || stderr.contains("contributor identity"),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(records_at(&root, "common").is_empty());
    assert_eq!(records_at(&root, "people").len(), 2, "nothing was modified");
}

#[test]
fn empty_repository_reports_cleanly() {
    let (_dir, root, home) = repo("empty");
    configure_alice(&home);
    let (ok, stdout, stderr) = compact(&root, &home, &[]);
    assert!(ok, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.starts_with("nisse records compact\n"), "{stdout}");
    assert!(
        stdout.contains("no consolidatable duplication found"),
        "{stdout}"
    );
    assert!(
        stdout.ends_with("0 common fact(s) created, 0 conflict(s) retained unresolved, 0 common lesson(s) created\n"),
        "{stdout}"
    );
    assert!(!root.join(".nisse").exists(), "no records were created");
}

#[test]
fn compact_outside_a_repository_fails() {
    let dir = tempfile::Builder::new()
        .prefix("nisse-compact-norepo-")
        .tempdir()
        .expect("temp dir");
    let home = TempDir::new().expect("home");
    let out = run_nisse_in_with_env(dir.path(), &["records", "compact"], home.path(), &[]);
    assert!(!out.status.success());
    let stderr = String::from_utf8(out.stderr).expect("stderr utf8");
    assert!(stderr.contains("Git repository"), "{stderr}");
}
