//! Performance measurements and regression bounds (plan milestone 29).
//!
//! Milestone 29 measures the routine interactive operations and freezes
//! regression bounds for them:
//!
//! - **warm no-change refresh**: under 100 ms for an ordinary local
//!   repository when Git and the filesystem are responsive (frozen
//!   target; a 200-file committed tree stands in for "ordinary");
//! - **one-file refresh**: incremental — exactly one parse, still
//!   interactive;
//! - **SQLite growth**: repeated warm no-change refreshes must not grow
//!   the generated caches;
//! - **record loading**: scales linearly in stored records;
//! - **staleness assessment**: linear in facts (the ordinary review
//!   path assesses every record against the snapshot);
//! - **observation grouping** (lesson learning): no O(N²) blowup over
//!   observations — 4× the observations must stay far below 16× the
//!   work a quadratic pass would need;
//! - **retrieval**: `lookup` stays interactive on an indexed tree with
//!   records synced;
//! - **routing**: rules classification is immediate;
//! - **reviewer context**: initial context stays inside its frozen
//!   budget no matter how large the inputs are (plan section 23/31).
//!
//! The remaining milestone-29 measurements (Pi-host startup, end-to-end
//! reviewer concurrency obeying configuration) exercise the real child
//! process and live in `tests/integration/performance.rs`.
//!
//! The other VERIFY items — unchanged content not reparsed, reverted
//! blobs reusing artifacts, worktrees sharing valid artifacts — are
//! functional invariants with dedicated tests in
//! [`crate::index::refresh`] (`second_refresh_without_changes_reparses_nothing`,
//! `revert_reuses_the_original_cached_artifact`,
//! `linked_worktree_reuses_shared_artifacts_without_reparsing`) and are
//! re-asserted here through the refresh statistics of the timed runs.
//!
//! Timing bounds are frozen targets. CI machines under load may
//! legitimately exceed a wall-clock target without indicating a
//! regression, so every bound scales by the `NISSE_PERF_SCALE`
//! environment variable (a factor ≥ 1.0; the default 1.0 asserts the
//! frozen targets). Functional assertions never scale.
//!
//! All measurements print to stderr under `--nocapture` for
//! diagnostics.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::domain::SCHEMA_VERSION;
use crate::domain::context::{ContextBundle, ContextEntry, ContextKind};
use crate::domain::evidence::EvidenceRef;
use crate::domain::ids::new_ulid;
use crate::domain::records::FactState;
use crate::domain::records::{
    FactRecord, FeedbackOutcome, ObservationRecord, ObservationType, ProjectRecord,
};
use crate::domain::review::ImpactClass;
use crate::domain::snapshot::{SnapshotDescriptor, SnapshotMode};
use crate::git::testutil::{Fixture, commit_all, write_file};
use crate::git::{GitRunner, RepoContext, SnapshotSpec, resolve_snapshot};
use crate::index::refresh_index;
use crate::jury::catalog::Catalog;
use crate::learning::lessons::compute_group_stats;
use crate::learning::staleness::assess_records;
use crate::retrieval::Retriever;
use crate::retrieval::query::{MAX_QUERY_BYTES, RetrievalQuery, truncate_text};
use crate::routing::fixtures::routing_fixtures;
use crate::routing::rules::RulesClassifier;
use crate::storage::StoragePaths;
use crate::storage::records::{RecordStore, StoredRecord};

/// Frozen warm no-change refresh target (plan milestone 29).
const WARM_REFRESH_TARGET: Duration = Duration::from_millis(100);

/// Multiplier for every timing bound (see the module documentation).
fn scale() -> f64 {
    std::env::var("NISSE_PERF_SCALE")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|factor| factor.is_finite() && *factor >= 1.0)
        .unwrap_or(1.0)
}

/// Assert `measured` stays under `target` after applying the scale.
fn assert_under(label: &str, measured: Duration, target: Duration) {
    let bound = target.mul_f64(scale());
    assert!(
        measured <= bound,
        "{label}: {measured:?} exceeded the bound {bound:?} (frozen target {target:?}, scale {})",
        scale()
    );
}

/// The smallest of `runs` timings of `operation`.
fn min_of<R>(mut operation: impl FnMut() -> R, runs: usize, mut sink: impl FnMut(R)) -> Duration {
    let mut best = Duration::MAX;
    for _ in 0..runs {
        let started = Instant::now();
        let result = operation();
        let elapsed = started.elapsed();
        sink(result);
        best = best.min(elapsed);
    }
    best
}

/// An ordinary committed repository: `files` distinct small Rust
/// modules plus a few files of other indexed languages.
fn ordinary_repo(label: &str, files: usize) -> Fixture {
    let fixture = Fixture::repo(label);
    for index in 0..files {
        write_file(
            &fixture.root,
            &format!("src/mod{index:04}.rs"),
            &format!(
                "pub struct Item{index} {{\n    pub id: u32,\n}}\n\n\
                 pub fn process{index}(item: &Item{index}) -> u32 {{\n    item.id + {index}\n}}\n"
            ),
        );
    }
    write_file(
        &fixture.root,
        "web/app.ts",
        "export function boot(): number {\n    return 1;\n}\n",
    );
    write_file(
        &fixture.root,
        "src/main.c",
        "int main(void) { return 0; }\n",
    );
    commit_all(&fixture.root, "ordinary tree");
    fixture
}

fn discover(fixture: &Fixture) -> (RepoContext, ResolvedSnapshotGuard) {
    let ctx = RepoContext::discover(&GitRunner::new(), &fixture.root).unwrap();
    let resolved = resolve_snapshot(&ctx, &SnapshotSpec::Working).unwrap();
    (ctx, ResolvedSnapshotGuard(resolved))
}

/// Keeps a resolved snapshot alive next to its borrowed `RepoContext`.
struct ResolvedSnapshotGuard(crate::git::ResolvedSnapshot);

fn refresh(
    ctx: &RepoContext,
    resolved: &crate::git::ResolvedSnapshot,
) -> crate::index::RefreshStats {
    let paths = StoragePaths::for_repo(ctx);
    refresh_index(ctx, resolved, &paths).unwrap()
}

#[test]
fn warm_no_change_refresh_meets_the_interactive_target() {
    let fixture = ordinary_repo("perf-warm", 200);
    let (ctx, guard) = discover(&fixture);
    let resolved = &guard.0;

    let cold = refresh(&ctx, resolved);
    eprintln!(
        "perf: cold refresh of {} files: {:?}",
        cold.files_seen, cold.duration
    );

    let mut best = Duration::MAX;
    for _ in 0..3 {
        let stats = refresh(&ctx, resolved);
        assert_eq!(stats.files_parsed, 0, "warm refresh reparses nothing");
        assert_eq!(stats.files_reused, stats.files_seen, "every file is reused");
        assert_eq!(stats.files_removed, 0);
        eprintln!("perf: warm no-change refresh: {:?}", stats.duration);
        best = best.min(stats.duration);
    }
    assert_under(
        "warm no-change refresh (best of 3)",
        best,
        WARM_REFRESH_TARGET,
    );
}

#[test]
fn one_file_refresh_parses_exactly_one_file_and_stays_interactive() {
    let fixture = ordinary_repo("perf-one-file", 200);
    let (ctx, guard) = discover(&fixture);
    let resolved = &guard.0;
    refresh(&ctx, resolved);

    // Dirty exactly one tracked file.
    write_file(
        &fixture.root,
        "src/mod0007.rs",
        "pub struct Item7 {\n    pub id: u32,\n}\n\npub fn process7b(item: &Item7) -> u32 {\n    item.id\n}\n",
    );
    let stats = refresh(&ctx, resolved);
    assert_eq!(stats.files_parsed, 1, "exactly the modified file is parsed");
    assert_eq!(stats.files_reused, stats.files_seen - 1);
    eprintln!("perf: one-file refresh: {:?}", stats.duration);
    assert_under("one-file refresh", stats.duration, WARM_REFRESH_TARGET);
}

#[test]
fn repeated_warm_refreshes_do_not_grow_the_generated_caches() {
    let fixture = ordinary_repo("perf-growth", 120);
    let (ctx, guard) = discover(&fixture);
    let resolved = &guard.0;
    let paths = StoragePaths::for_repo(&ctx);
    refresh(&ctx, resolved);

    let counts = |db: &Path, table: &str| -> i64 {
        let conn = rusqlite::Connection::open(db).unwrap();
        conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
    };

    let baseline = (
        paths.shared.clone(),
        paths.worktree.clone(),
        counts(&paths.shared, "parsed_artifact"),
        counts(&paths.shared, "content_artifact"),
        counts(&paths.worktree, "manifest"),
        counts(&paths.worktree, "path_content"),
        std::fs::metadata(&paths.shared).unwrap().len(),
        std::fs::metadata(&paths.worktree).unwrap().len(),
    );

    for _ in 0..3 {
        refresh(&ctx, resolved);
    }

    let after = (
        paths.shared.clone(),
        paths.worktree.clone(),
        counts(&paths.shared, "parsed_artifact"),
        counts(&paths.shared, "content_artifact"),
        counts(&paths.worktree, "manifest"),
        counts(&paths.worktree, "path_content"),
        std::fs::metadata(&paths.shared).unwrap().len(),
        std::fs::metadata(&paths.worktree).unwrap().len(),
    );

    // Row counts are identical: warm refreshes write nothing new.
    assert_eq!(baseline.2, after.2, "parsed_artifact rows are stable");
    assert_eq!(baseline.3, after.3, "content_artifact rows are stable");
    assert_eq!(baseline.4, after.4, "manifest rows are stable");
    assert_eq!(baseline.5, after.5, "path_content rows are stable");
    eprintln!(
        "perf: sqlite size after cold vs 3 warm refreshes: shared {} -> {} bytes, worktree {} -> {} bytes",
        baseline.6, after.6, baseline.7, after.7
    );
    // File sizes stay within one small slack window (SQLite page/WAL
    // churn); growth would mean rewritten-unchanged rows.
    let slack = 256 * 1024;
    assert!(
        after.6.abs_diff(baseline.6) <= slack,
        "shared cache grew from {} to {} bytes across warm refreshes",
        baseline.6,
        after.6
    );
    assert!(
        after.7.abs_diff(baseline.7) <= slack,
        "worktree cache grew from {} to {} bytes across warm refreshes",
        baseline.7,
        after.7
    );
}

/// One durable supported fact citing `path` with `excerpt`.
fn fact_about(path: &str, excerpt: &str) -> ProjectRecord {
    ProjectRecord::Fact(FactRecord {
        schema_version: SCHEMA_VERSION,
        id: new_ulid(),
        author: "perf".to_string(),
        created_at: "2026-01-02T12:00:00Z".to_string(),
        subject: "performance fixture".to_string(),
        text: format!("The {path} module contains `{excerpt}`."),
        tags: vec!["perf".to_string()],
        state: FactState::Supported,
        evidence: vec![EvidenceRef {
            kind: crate::domain::evidence::EvidenceKind::Source,
            path: Some(path.to_string()),
            symbol: None,
            line_start: None,
            line_end: None,
            content_key: None,
            plan_section: None,
            record_id: None,
            input_part: None,
            excerpt: excerpt.to_string(),
        }],
        source_review_id: None,
        source_record_ids: Vec::new(),
        supersedes: Vec::new(),
    })
}

fn load_records(root: &Path) -> Vec<StoredRecord> {
    RecordStore::new(root).load().unwrap()
}

#[test]
fn record_loading_scales_linearly() {
    let fixture = ordinary_repo("perf-records", 10);
    let store = RecordStore::new(&fixture.root);

    // 300 durable facts (excerpts cite symbols that exist verbatim in
    // the committed modules, so none are stale).
    for index in 0..300 {
        store
            .create(&fact_about(
                &format!("src/mod{:04}.rs", index % 10),
                &format!("process{}", index % 10),
            ))
            .unwrap();
    }
    let first = min_of(
        || load_records(&fixture.root),
        3,
        |records: Vec<StoredRecord>| {
            assert_eq!(records.len(), 300);
        },
    );
    eprintln!("perf: load 300 records: {first:?}");

    // Grow to 1200 (4x) and compare: linear work scales ~4x, quadratic
    // ~16x; the bound between them catches superlinear regressions.
    for index in 300..1200 {
        store
            .create(&fact_about(
                &format!("src/mod{:04}.rs", index % 10),
                &format!("process{}", index % 10),
            ))
            .unwrap();
    }
    let second = min_of(
        || load_records(&fixture.root),
        3,
        |records: Vec<StoredRecord>| {
            assert_eq!(records.len(), 1200);
        },
    );
    eprintln!("perf: load 1200 records: {second:?}");

    assert_under(
        "record loading (1200 records)",
        second,
        Duration::from_millis(2000),
    );
    let ratio = second.as_secs_f64() / first.as_secs_f64().max(1e-6);
    assert!(
        ratio < 8.0,
        "loading 4x the records took {ratio:.1}x longer ({first:?} -> {second:?}); \
         superlinear record loading"
    );
}

#[test]
fn staleness_assessment_scales_linearly_in_facts() {
    let fixture = ordinary_repo("perf-staleness", 10);
    let store = RecordStore::new(&fixture.root);
    for index in 0..200 {
        store
            .create(&fact_about(
                &format!("src/mod{:04}.rs", index % 10),
                &format!("process{}", index % 10),
            ))
            .unwrap();
    }
    let (ctx, guard) = discover(&fixture);
    let resolved = &guard.0;

    let assess = |records: &[StoredRecord]| {
        let reader = crate::git::SnapshotReader::new(&ctx, resolved).unwrap();
        let started = Instant::now();
        let report = assess_records(records, &reader, ctx.object_format().unwrap());
        (started.elapsed(), report)
    };

    let (first, _) = assess(&load_records(&fixture.root));
    eprintln!("perf: staleness over 200 facts: {first:?}");

    for index in 200..800 {
        store
            .create(&fact_about(
                &format!("src/mod{:04}.rs", index % 10),
                &format!("process{}", index % 10),
            ))
            .unwrap();
    }
    let records = load_records(&fixture.root);
    assert_eq!(records.len(), 800);
    let (second, report) = assess(&records);
    eprintln!("perf: staleness over 800 facts: {second:?}");

    assert_under(
        "staleness assessment (800 facts)",
        second,
        Duration::from_millis(2000),
    );
    let ratio = second.as_secs_f64() / first.as_secs_f64().max(1e-6);
    assert!(
        ratio < 8.0,
        "assessing 4x the facts took {ratio:.1}x longer ({first:?} -> {second:?}); \
         superlinear staleness assessment"
    );
    // The facts cite current committed content: none are stale (this
    // keeps the measured path on the real work, not an early exit).
    assert!(report.ids().is_empty(), "fixture facts must be current");
}

/// `count` observations of one group (`reviewer`, `category`).
fn observations(count: usize, reviewer: &str, category: &str) -> Vec<ObservationRecord> {
    (0..count)
        .map(|index| {
            let mut record = ObservationRecord {
                schema_version: SCHEMA_VERSION,
                id: new_ulid(),
                author: "perf".to_string(),
                created_at: "2026-01-02T12:00:00Z".to_string(),
                observation_type: ObservationType::ReviewOutcome,
                review_id: new_ulid(),
                finding_id: new_ulid(),
                reviewer_id: reviewer.to_string(),
                outcome: FeedbackOutcome::Fixed,
                impact_class: ImpactClass::Correctness,
                category: category.to_string(),
                languages: vec!["rust".to_string()],
                tags: vec![category.to_string()],
                paths: vec![format!("src/mod{:04}.rs", index % 8)],
                snapshot: SnapshotDescriptor {
                    mode: SnapshotMode::Working,
                    base_commit: None,
                    base_tree: None,
                    head_commit: None,
                    head_tree: None,
                },
            };
            record.created_at = format!("2026-01-02T12:00:{index:02}Z");
            record
        })
        .collect()
}

#[test]
fn observation_grouping_is_not_quadratic_in_observations() {
    // Lesson grouping is the deterministic heart of learning (plan
    // section 51) and runs on every review over every stored
    // observation; it must stay linear-ish in observation count.
    let build = |per_group: usize| -> Vec<ObservationRecord> {
        let mut all: Vec<ObservationRecord> = Vec::new();
        for reviewer in ["quality", "testing", "security", "concurrency"] {
            for category in ["race", "leak", "hang"] {
                all.extend(observations(per_group, reviewer, category));
            }
        }
        assert_eq!(all.len(), per_group * 12);
        all
    };

    // Best-of-3 timing (like the sibling scaling tests) filters
    // scheduler noise when the suite runs under parallel load.
    let small = build(50);
    let small_refs: Vec<&ObservationRecord> = small.iter().collect();
    let small_stats = compute_group_stats(&small_refs);
    assert_eq!(small_stats.len(), 12, "one group per reviewer/category");
    assert!(small_stats.iter().all(|group| group.support == 50));
    let small_duration = min_of(
        || compute_group_stats(&small_refs),
        3,
        |stats: Vec<_>| assert_eq!(stats.len(), 12),
    );
    eprintln!(
        "perf: grouping {} observations: {:?}",
        small_refs.len(),
        small_duration
    );

    let large = build(200);
    let large_refs: Vec<&ObservationRecord> = large.iter().collect();
    let large_stats = compute_group_stats(&large_refs);
    assert_eq!(large_stats.len(), 12);
    // Every group's statistics are exact (unique observations counted).
    assert!(large_stats.iter().all(|group| group.support == 200));
    let large_duration = min_of(
        || compute_group_stats(&large_refs),
        3,
        |stats: Vec<_>| assert_eq!(stats.len(), 12),
    );
    eprintln!(
        "perf: grouping {} observations: {:?}",
        large_refs.len(),
        large_duration
    );

    assert_under(
        "observation grouping (2400 observations)",
        large_duration,
        Duration::from_millis(1000),
    );
    let ratio = large_duration.as_secs_f64() / small_duration.as_secs_f64().max(1e-6);
    assert!(
        ratio < 8.0,
        "grouping 4x the observations took {ratio:.1}x longer ({small_duration:?} -> \
         {large_duration:?}); O(N^2) work over all observations"
    );
}

#[test]
fn retrieval_lookup_stays_interactive() {
    let fixture = ordinary_repo("perf-retrieval", 120);
    // Durable records participate in retrieval.
    let store = RecordStore::new(&fixture.root);
    for index in 0..20 {
        store
            .create(&fact_about(
                &format!("src/mod{:04}.rs", index),
                &format!("process{index}"),
            ))
            .unwrap();
    }
    let (ctx, guard) = discover(&fixture);
    let resolved = &guard.0;
    let paths = StoragePaths::for_repo(&ctx);
    refresh(&ctx, resolved);

    let records = load_records(&fixture.root);
    let retriever = Retriever::open(resolved, &paths).unwrap();
    retriever.sync_records(&records, &BTreeSet::new()).unwrap();
    retriever
        .sync_practices(&Catalog::bundled().unwrap())
        .unwrap();

    let query = RetrievalQuery::from_text(truncate_text(
        &format!("{} process7 {}", "queue".repeat(10), "item".repeat(10)),
        MAX_QUERY_BYTES,
    ));
    let lookup = || retriever.lookup(&query).unwrap();
    let best = min_of(lookup, 3, |bundle: ContextBundle| {
        assert!(
            !bundle.entries.is_empty(),
            "the fixture must retrieve entries"
        );
    });
    eprintln!("perf: retrieval lookup (best of 3): {best:?}");
    assert_under("retrieval lookup", best, Duration::from_millis(250));

    // Symbol lookups stay interactive as well.
    let best_symbols = min_of(
        || retriever.symbol_lookup("process7").unwrap(),
        3,
        |entries: Vec<ContextEntry>| {
            assert!(!entries.is_empty(), "the symbol must resolve");
        },
    );
    eprintln!("perf: symbol lookup (best of 3): {best_symbols:?}");
    assert_under("symbol lookup", best_symbols, Duration::from_millis(250));
}

#[test]
fn routing_classification_is_immediate() {
    let catalog = Catalog::bundled().unwrap();
    let classifier = RulesClassifier::new(catalog.clone());
    let started = Instant::now();
    let fixtures = routing_fixtures();
    let decisions: Vec<_> = fixtures
        .iter()
        .map(|f| classifier.classify(&f.input))
        .collect();
    let elapsed = started.elapsed();
    assert_eq!(decisions.len(), fixtures.len());
    eprintln!(
        "perf: rules classification of {} fixtures: {:?}",
        fixtures.len(),
        elapsed
    );
    assert_under(
        "routing classification (all fixtures)",
        elapsed,
        Duration::from_millis(250),
    );
}

#[test]
fn reviewer_initial_context_stays_within_its_frozen_budget() {
    use crate::jury::review::INITIAL_CONTEXT_BUDGET;

    // Far more candidate entries, and far larger ones, than the budget
    // allows: the assembled bundle must still respect every bound.
    let mut candidates: Vec<ContextEntry> = Vec::new();
    for index in 0..200 {
        candidates.push(ContextEntry {
            kind: ContextKind::Source,
            title: format!("source {index}"),
            path: Some(format!("src/mod{index:04}.rs")),
            symbol: Some(format!("process{index}")),
            line_start: Some(1),
            line_end: Some(3),
            record_id: None,
            practice_id: None,
            text: "x".repeat(16 * 1024),
            score: 1.0 - (index as f64) / 1000.0,
        });
    }
    let bundle = crate::retrieval::query::assemble_bundle(candidates, &INITIAL_CONTEXT_BUDGET);
    assert!(bundle.entries.len() <= INITIAL_CONTEXT_BUDGET.max_entries);
    let total: usize = bundle.entries.iter().map(|entry| entry.text.len()).sum();
    assert!(total <= INITIAL_CONTEXT_BUDGET.max_total_text_bytes);
    assert!(
        bundle
            .entries
            .iter()
            .all(|entry| entry.text.len() <= INITIAL_CONTEXT_BUDGET.max_entry_text_bytes)
    );
    eprintln!(
        "perf: bounded initial context: {} entries, {} total text bytes (budget: {} entries, \
         {} bytes)",
        bundle.entries.len(),
        total,
        INITIAL_CONTEXT_BUDGET.max_entries,
        INITIAL_CONTEXT_BUDGET.max_total_text_bytes
    );
}
