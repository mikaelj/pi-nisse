//! Integration tests for fact staleness and targeted revalidation
//! (plan milestone 21).
//!
//! The staleness assessment and the revalidation driver are exercised
//! against real repositories and the real Pi review host child process
//! with the fake verifier runtime (scripted verdicts):
//!
//! - content-key staleness detection over the real snapshot reader;
//! - `supported` revalidation → new immutable record with fresh
//!   provenance, superseding (never replacing) the old one;
//! - `contradicted` revalidation → new record in state
//!   `unresolved_conflict`; the old record remains;
//! - `insufficient` / verifier failure / deleted support / missing
//!   contributor identity → nothing durable, pending local state.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use nisse::domain::evidence::{EvidenceKind, EvidenceRef};
use nisse::domain::records::{FactRecord, FactState, ProjectRecord};
use nisse::git::{
    GitRunner, ObjectFormat, RepoContext, ResolvedSnapshot, SnapshotReader, SnapshotSpec,
    resolve_snapshot,
};
use nisse::jury::findings::ValidationContext;
use nisse::jury::host::{HostProcess, PiHostLaunch};
use nisse::jury::protocol::{HostConfigPayload, HostModelProfile, HostRuntime};
use nisse::learning::facts::{PendingRow, load_pending};
use nisse::learning::staleness::{
    self, FactFreshness, RevalidationOptions, assess_fact, assess_records, revalidate_fact,
};
use nisse::storage::StoragePaths;
use nisse::storage::records::{RecordStore, StoredRecord};

// ---------------------------------------------------------------------------
// Repository fixtures
// ---------------------------------------------------------------------------

fn git(cwd: &Path, args: &[&str]) {
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
}

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn commit_all(root: &Path, message: &str) {
    git(root, &["add", "--all"]);
    git(root, &["commit", "--quiet", "-m", message]);
}

/// A disposable repository containing `src/queue.rs` with the queue
/// implementation the durable facts cite.
struct Repo {
    _dir: tempfile::TempDir,
    root: PathBuf,
    ctx: RepoContext,
    resolved: ResolvedSnapshot,
    storage: StoragePaths,
}

const QUEUE_SOURCE: &str = "pub struct Queue {\n    slots: [u8; 32],\n}\n\npub fn enqueue(queue: &mut Queue, item: u8) {\n    queue.slots[0] = item;\n}\n";

fn setup(label: &str) -> Repo {
    let dir = tempfile::Builder::new()
        .prefix(&format!("nisse-staleness-{label}-"))
        .tempdir()
        .expect("tempdir");
    git(dir.path(), &["init", "--quiet", "repo"]);
    let root = dir.path().join("repo").canonicalize().unwrap();
    for (key, value) in [
        ("user.email", "nisse@test.invalid"),
        ("user.name", "Nisse Test"),
        ("commit.gpgsign", "false"),
        ("tag.gpgsign", "false"),
    ] {
        git(&root, &["config", key, value]);
    }
    write(&root, "src/queue.rs", QUEUE_SOURCE);
    commit_all(&root, "queue");

    let ctx = RepoContext::discover(&GitRunner::new(), &root).expect("discovery");
    let resolved = resolve_snapshot(&ctx, &SnapshotSpec::Working).expect("snapshot resolution");
    let storage = StoragePaths::for_repo(&ctx);
    Repo {
        _dir: dir,
        root,
        ctx,
        resolved,
        storage,
    }
}

impl Repo {
    /// A validation context over a reader of the current working
    /// snapshot. The reader must outlive the returned context.
    fn validation<'a>(
        &self,
        reader: &'a SnapshotReader<'_>,
        known_record_ids: &'a BTreeSet<String>,
    ) -> ValidationContext<'a> {
        ValidationContext {
            reader,
            object_format: ObjectFormat::Sha1,
            changed_paths: None,
            known_record_ids,
        }
    }

    /// A reader over the current working snapshot.
    fn reader(&self) -> SnapshotReader<'_> {
        SnapshotReader::new(&self.ctx, &self.resolved).expect("reader")
    }
}

/// Create one durable supported fact citing `src/queue.rs` with
/// Rust-stamped content provenance (exactly what the milestone-18
/// pipeline persists after this milestone).
fn create_fact(repo: &Repo, text: &str) -> FactRecord {
    let evidence = vec![EvidenceRef {
        kind: EvidenceKind::Source,
        path: Some("src/queue.rs".to_string()),
        symbol: Some("enqueue".to_string()),
        line_start: Some(5),
        line_end: Some(7),
        content_key: None,
        plan_section: None,
        record_id: None,
        excerpt: "queue.slots[0] = item;".to_string(),
    }];
    let stamped = {
        let reader = repo.reader();
        let known = BTreeSet::new();
        staleness::stamp_content_provenance(&repo.validation(&reader, &known), &evidence)
    };
    assert!(
        stamped[0].content_key.is_some(),
        "provenance must be stamped"
    );
    let record = FactRecord {
        schema_version: 1,
        id: nisse::domain::ids::new_ulid(),
        author: "alice".to_string(),
        created_at: nisse::domain::time::now_utc_rfc3339(),
        subject: "queue capacity".to_string(),
        text: text.to_string(),
        tags: vec!["queue".to_string()],
        state: FactState::Supported,
        evidence: stamped,
        source_review_id: None,
        source_record_ids: Vec::new(),
        supersedes: Vec::new(),
    };
    RecordStore::new(&repo.root)
        .create(&ProjectRecord::Fact(record.clone()))
        .expect("fact creation");
    record
}

/// The single stored fact of the repository.
fn the_fact(repo: &Repo) -> FactRecord {
    let records = RecordStore::new(&repo.root).load().expect("records");
    let facts: Vec<&StoredRecord> = records
        .iter()
        .filter(|stored| matches!(stored.record, ProjectRecord::Fact(_)))
        .collect();
    assert_eq!(facts.len(), 1, "exactly one fact expected");
    match &facts[0].record {
        ProjectRecord::Fact(fact) => fact.clone(),
        other => panic!("unexpected record {other:?}"),
    }
}

/// Spawn the fake-runtime host with scripted fact-verification verdicts.
fn verdict_host(verdicts: &str) -> HostProcess {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("runtime/pi-host/src/main.ts");
    let launch = PiHostLaunch::new("node", script, HostRuntime::Fake)
        .env("NISSE_FAKE_FACT_VERIFICATION_VERDICTS", verdicts);
    let mut host = HostProcess::spawn(&launch).expect("spawn failed");
    host.wait_ready(Duration::from_secs(20))
        .expect("host_ready");
    let config = HostConfigPayload {
        models: BTreeMap::from([(
            "standard".to_string(),
            HostModelProfile {
                model: "acme/local-small".to_string(),
                endpoint: None,
            },
        )]),
    };
    host.send_host_config(&config).expect("host_config");
    host
}

fn fast_options() -> RevalidationOptions {
    RevalidationOptions {
        timeout_ms: 20_000,
        ..RevalidationOptions::default()
    }
}

/// The fact-revalidation pending rows of the shared cache.
fn revalidation_rows(repo: &Repo) -> Vec<serde_json::Value> {
    load_pending(&repo.storage)
        .expect("pending rows")
        .into_iter()
        .filter(|row: &PendingRow| row.kind == staleness::PENDING_KIND)
        .map(|row| row.payload)
        .collect()
}

fn assess(repo: &Repo, fact: &FactRecord) -> FactFreshness {
    let reader = SnapshotReader::new(&repo.ctx, &repo.resolved).expect("reader");
    assess_fact(fact, &reader, ObjectFormat::Sha1)
}

// ---------------------------------------------------------------------------
// Staleness detection
// ---------------------------------------------------------------------------

#[test]
fn unchanged_evidence_keeps_the_fact_current() {
    let repo = setup("unchanged");
    let fact = create_fact(&repo, "The sensor queue is bounded to 32 slots.");

    assert_eq!(assess(&repo, &fact), FactFreshness::Current);

    // The whole-record view agrees: nothing is stale.
    let records = RecordStore::new(&repo.root).load().unwrap();
    let reader = SnapshotReader::new(&repo.ctx, &repo.resolved).unwrap();
    let report = assess_records(&records, &reader, ObjectFormat::Sha1);
    assert!(report.is_empty());
}

#[test]
fn modified_and_deleted_content_mark_the_fact_stale() {
    let repo = setup("modified");
    let fact = create_fact(&repo, "The sensor queue is bounded to 32 slots.");

    // Modified content: the stamped provenance no longer matches.
    // The queue grows but keeps content at the cited location, so
    // revalidation has current evidence to judge against.
    write(
        &repo.root,
        "src/queue.rs",
        "pub struct Queue {\n    slots: [u8; 64],\n}\n\npub fn enqueue(queue: &mut Queue, item: u8) {\n    queue.slots[0] = item;\n}\n",
    );
    assert!(assess(&repo, &fact).is_stale());

    // Deleted content: the cited path is gone entirely.
    std::fs::remove_file(repo.root.join("src/queue.rs")).unwrap();
    match assess(&repo, &fact) {
        FactFreshness::PotentiallyStale { reasons } => {
            assert!(
                reasons
                    .iter()
                    .any(|reason| reason.contains("no longer part of the target snapshot")),
                "{reasons:?}"
            );
        }
        other => panic!("expected staleness, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Targeted revalidation
// ---------------------------------------------------------------------------

#[test]
fn supported_revalidation_supersedes_with_fresh_provenance() {
    let repo = setup("revalidate-supported");
    let fact = create_fact(&repo, "The sensor queue is bounded to 32 slots.");
    let fact_id = fact.id.clone();

    // The support moved on (the queue grew).
    write(
        &repo.root,
        "src/queue.rs",
        "pub struct Queue {\n    slots: [u8; 64],\n}\n\npub fn enqueue(queue: &mut Queue, item: u8) {\n    queue.slots[0] = item;\n}\n",
    );
    assert!(assess(&repo, &fact).is_stale());

    let known: BTreeSet<String> = RecordStore::new(&repo.root)
        .load()
        .unwrap()
        .iter()
        .map(|stored| stored.id().to_string())
        .collect();
    let reader = repo.reader();
    let validation = repo.validation(&reader, &known);
    let mut host = verdict_host("supported");
    let outcome = revalidate_fact(
        &mut host,
        &repo.ctx,
        Some(&repo.storage),
        &validation,
        &fact,
        Some("alice"),
        &fast_options(),
    );
    host.shutdown(Duration::from_secs(5)).ok();

    assert_eq!(outcome.verdict.as_str(), "supported");
    assert_eq!(outcome.fact_id, fact_id);
    let new_id = outcome.new_record_id.expect("durable update expected");
    assert!(revalidation_rows(&repo).is_empty(), "nothing pending");

    // The old record remains on disk; the new one supersedes it with
    // current-snapshot provenance.
    let store = RecordStore::new(&repo.root);
    let records = store.load().unwrap();
    let old = records
        .iter()
        .find(|stored| stored.id() == fact_id)
        .expect("the old fact remains for provenance");
    let old_path = old.path.clone();
    assert!(old_path.exists());
    let updated = match &records
        .iter()
        .find(|stored| stored.id() == new_id)
        .expect("the updated fact exists")
        .record
    {
        ProjectRecord::Fact(fact) => fact.clone(),
        other => panic!("unexpected record {other:?}"),
    };
    assert_eq!(updated.state, FactState::Supported);
    assert_eq!(updated.supersedes, vec![fact_id.clone()]);
    assert_eq!(updated.source_record_ids, vec![fact_id]);
    assert_eq!(updated.text, "The sensor queue is bounded to 32 slots.");
    assert_eq!(updated.evidence.len(), 1);
    // The refreshed provenance matches the *current* content, which is
    // exactly why the updated fact is no longer stale.
    assert_ne!(
        updated.evidence[0].content_key, fact.evidence[0].content_key,
        "provenance must have moved to the current content"
    );
    let reader = repo.reader();
    assert_eq!(
        assess_fact(&updated, &reader, ObjectFormat::Sha1),
        FactFreshness::Current
    );
}

#[test]
fn contradicted_revalidation_records_an_unresolved_conflict() {
    let repo = setup("revalidate-contradicted");
    let fact = create_fact(&repo, "The sensor queue is bounded to 32 slots.");
    let fact_id = fact.id.clone();

    // The queue grows but keeps content at the cited location, so
    // revalidation has current evidence to judge against.
    write(
        &repo.root,
        "src/queue.rs",
        "pub struct Queue {\n    slots: [u8; 64],\n}\n\npub fn enqueue(queue: &mut Queue, item: u8) {\n    queue.slots[0] = item;\n}\n",
    );

    let known: BTreeSet<String> = RecordStore::new(&repo.root)
        .load()
        .unwrap()
        .iter()
        .map(|stored| stored.id().to_string())
        .collect();
    let reader = repo.reader();
    let validation = repo.validation(&reader, &known);
    let mut host = verdict_host("contradicted");
    let outcome = revalidate_fact(
        &mut host,
        &repo.ctx,
        Some(&repo.storage),
        &validation,
        &fact,
        Some("alice"),
        &fast_options(),
    );
    host.shutdown(Duration::from_secs(5)).ok();

    assert_eq!(outcome.verdict.as_str(), "contradicted");
    let new_id = outcome.new_record_id.expect("durable update expected");
    let records = RecordStore::new(&repo.root).load().unwrap();
    assert!(
        records.iter().any(|stored| stored.id() == fact_id),
        "the old fact remains"
    );
    let updated = match &records
        .iter()
        .find(|stored| stored.id() == new_id)
        .expect("the updated fact exists")
        .record
    {
        ProjectRecord::Fact(fact) => fact.clone(),
        other => panic!("unexpected record {other:?}"),
    };
    // The claim is preserved verbatim but is no longer normal truth.
    assert_eq!(updated.state, FactState::UnresolvedConflict);
    assert_eq!(updated.text, fact.text);
    assert_eq!(updated.supersedes, vec![fact_id]);
    assert!(revalidation_rows(&repo).is_empty());
}

#[test]
fn insufficient_revalidation_changes_nothing_and_stays_pending() {
    let repo = setup("revalidate-insufficient");
    let fact = create_fact(&repo, "The sensor queue is bounded to 32 slots.");
    // The queue grows but keeps content at the cited location, so
    // revalidation has current evidence to judge against.
    write(
        &repo.root,
        "src/queue.rs",
        "pub struct Queue {\n    slots: [u8; 64],\n}\n\npub fn enqueue(queue: &mut Queue, item: u8) {\n    queue.slots[0] = item;\n}\n",
    );

    let reader = repo.reader();
    let known = BTreeSet::new();
    let validation = repo.validation(&reader, &known);
    let mut host = verdict_host("insufficient");
    let outcome = revalidate_fact(
        &mut host,
        &repo.ctx,
        Some(&repo.storage),
        &validation,
        &fact,
        Some("alice"),
        &fast_options(),
    );
    host.shutdown(Duration::from_secs(5)).ok();

    assert_eq!(outcome.verdict.as_str(), "insufficient");
    assert!(outcome.new_record_id.is_none());
    assert!(
        outcome
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("insufficient")),
        "{outcome:?}"
    );

    // Exactly the original fact exists; the attempt is pending.
    let fact_after = the_fact(&repo);
    assert_eq!(fact_after.id, fact.id);
    let rows = revalidation_rows(&repo);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["verdict"], "insufficient");
    assert_eq!(rows[0]["fact_id"], fact.id.as_str());
}

#[test]
fn verifier_failure_is_forced_insufficient() {
    let repo = setup("revalidate-failure");
    let fact = create_fact(&repo, "The sensor queue is bounded to 32 slots.");
    // The queue grows but keeps content at the cited location, so
    // revalidation has current evidence to judge against.
    write(
        &repo.root,
        "src/queue.rs",
        "pub struct Queue {\n    slots: [u8; 64],\n}\n\npub fn enqueue(queue: &mut Queue, item: u8) {\n    queue.slots[0] = item;\n}\n",
    );

    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("runtime/pi-host/src/main.ts");
    let launch = PiHostLaunch::new("node", script, HostRuntime::Fake)
        .env("NISSE_FAKE_FACT_VERIFICATION_FAIL", "1");
    let mut host = HostProcess::spawn(&launch).expect("spawn failed");
    host.wait_ready(Duration::from_secs(20))
        .expect("host_ready");

    let reader = repo.reader();
    let known = BTreeSet::new();
    let validation = repo.validation(&reader, &known);
    let outcome = revalidate_fact(
        &mut host,
        &repo.ctx,
        Some(&repo.storage),
        &validation,
        &fact,
        Some("alice"),
        &fast_options(),
    );
    host.shutdown(Duration::from_secs(5)).ok();

    assert_eq!(outcome.verdict.as_str(), "insufficient");
    assert!(outcome.failure.is_some(), "{outcome:?}");
    assert!(outcome.new_record_id.is_none());
    // The original fact is untouched and the failure is pending.
    assert_eq!(the_fact(&repo).id, fact.id);
    let rows = revalidation_rows(&repo);
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("could not decide")),
        "{}",
        rows[0]["reason"]
    );
}

#[test]
fn deleted_support_is_deterministically_insufficient_without_a_session() {
    let repo = setup("revalidate-deleted");
    let fact = create_fact(&repo, "The sensor queue is bounded to 32 slots.");

    // The cited file is deleted: even a scripted `supported` verdict
    // must never be consulted — there is no current evidence to check
    // the claim against.
    std::fs::remove_file(repo.root.join("src/queue.rs")).unwrap();

    let reader = repo.reader();
    let known = BTreeSet::new();
    let validation = repo.validation(&reader, &known);
    let mut host = verdict_host("supported");
    let outcome = revalidate_fact(
        &mut host,
        &repo.ctx,
        Some(&repo.storage),
        &validation,
        &fact,
        Some("alice"),
        &fast_options(),
    );
    host.shutdown(Duration::from_secs(5)).ok();

    assert_eq!(outcome.verdict.as_str(), "insufficient");
    assert!(outcome.new_record_id.is_none());
    assert!(
        outcome
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("absent from the current target snapshot")),
        "{outcome:?}"
    );
    assert_eq!(the_fact(&repo).id, fact.id);
    assert_eq!(revalidation_rows(&repo).len(), 1);
}

#[test]
fn revalidation_without_identity_stays_pending() {
    let repo = setup("revalidate-no-identity");
    let fact = create_fact(&repo, "The sensor queue is bounded to 32 slots.");
    // The queue grows but keeps content at the cited location, so
    // revalidation has current evidence to judge against.
    write(
        &repo.root,
        "src/queue.rs",
        "pub struct Queue {\n    slots: [u8; 64],\n}\n\npub fn enqueue(queue: &mut Queue, item: u8) {\n    queue.slots[0] = item;\n}\n",
    );

    let reader = repo.reader();
    let known = BTreeSet::new();
    let validation = repo.validation(&reader, &known);
    let mut host = verdict_host("supported");
    let outcome = revalidate_fact(
        &mut host,
        &repo.ctx,
        Some(&repo.storage),
        &validation,
        &fact,
        None,
        &fast_options(),
    );
    host.shutdown(Duration::from_secs(5)).ok();

    // The verdict was supported, but without a contributor identity no
    // durable record may be created (plan section 44).
    assert_eq!(outcome.verdict.as_str(), "supported");
    assert!(outcome.new_record_id.is_none());
    assert!(
        outcome
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("no contributor identity")),
        "{outcome:?}"
    );
    assert_eq!(the_fact(&repo).id, fact.id);
    let rows = revalidation_rows(&repo);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["verdict"], "supported");
}
