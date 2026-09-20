//! Retrieval evaluation (plan section 69, milestone 23).
//!
//! `nisse eval retrieval` runs the frozen retrieval fixtures (see
//! [`fixtures`](super::fixtures)) through the real retrieval pipeline
//! — lazy index refresh, snapshot membership, deterministic ranking —
//! and measures the frozen metrics:
//!
//! - **Recall\@K** — required sources found within the top-K ranked
//!   entries (K = [`RETRIEVAL_TOP_K`]);
//! - **MRR** — mean reciprocal rank of the required sources' first
//!   occurrences;
//! - **snapshot leakage count** — entries that do not belong to the
//!   target snapshot, verified *independently* of the retriever
//!   against the snapshot's own file set and content (cache presence
//!   never proves membership, plan section 26). Leakage always fails
//!   a fixture;
//! - **latency** — refresh plus lookup per fixture.
//!
//! The mandatory baseline variant (no embeddings) always runs. When an
//! optional embedding provider (plan section 34) is configured, the
//! same fixtures run again with cosine reranking enabled so the two
//! variants can be compared; provider failures degrade to the
//! baseline ranking inside the engine, so the variant never requires
//! the provider. The command's exit status follows the mandatory
//! baseline only; the embeddings variant is reported for comparison.
//!
//! Evaluation data is recorded in the machine-readable report (the
//! `machine-readable:` section of the rendered output). Nothing trains
//! models (plan section 69).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use thiserror::Error;

use super::fixtures::{FixtureRepository, RetrievalFixture, retrieval_fixtures};
use super::query::RetrievalQuery;
use crate::domain::context::{ContextBundle, ContextKind};
use crate::git::{GitRunner, RepoContext, SnapshotReader, SnapshotSpec, resolve_snapshot};
use crate::index::refresh_index;
use crate::providers::EmbeddingProvider;
use crate::retrieval::Retriever;
use crate::storage::StoragePaths;

/// K of the frozen Recall@K metric: the head of the ranked bundle that
/// must contain every required source.
pub const RETRIEVAL_TOP_K: usize = 10;

/// Label of the mandatory baseline variant.
pub const BASELINE_VARIANT: &str = "baseline";
/// Label of the optional embeddings comparison variant.
pub const EMBEDDINGS_VARIANT: &str = "embeddings";

/// Retrieval evaluation failures.
#[derive(Debug, Error)]
pub enum RetrievalEvalError {
    /// The frozen fixture set or its repository is invalid.
    #[error(transparent)]
    Fixture(#[from] super::fixtures::RetrievalFixtureError),
    /// A fixture execution failed (repository, index, or storage
    /// errors). Evaluation reports measurements; execution failures
    /// are errors, not results.
    #[error("evaluating fixture `{fixture}` failed: {message}")]
    Execution {
        /// Fixture that failed.
        fixture: String,
        /// Failure detail.
        message: String,
    },
}

/// One fixture's measured retrieval outcome.
#[derive(Debug, Clone)]
pub struct RetrievalFixtureOutcome {
    /// Fixture name.
    pub name: String,
    /// Target worktree label of the fixture.
    pub worktree: String,
    /// Required sources found within the top-K ranked entries.
    pub required_found: usize,
    /// Required sources expected by the fixture.
    pub required_total: usize,
    /// Recall@K.
    pub recall_at_k: f64,
    /// Mean reciprocal rank of the required sources.
    pub mean_reciprocal_rank: f64,
    /// Entries citing a forbidden source (precision signal; reported,
    /// not a correctness gate).
    pub forbidden_hits: usize,
    /// Entries that do not belong to the target snapshot (correctness
    /// gate: any leakage fails the fixture).
    pub snapshot_leakage: usize,
    /// Number of entries in the result bundle.
    pub entries: usize,
    /// Refresh-plus-lookup latency.
    pub latency: Duration,
    /// First fixture violation (`recall < 1` or leakage), `None` when
    /// the fixture passed.
    pub violation: Option<String>,
}

impl RetrievalFixtureOutcome {
    /// Whether the fixture satisfied its correctness gates.
    pub fn passed(&self) -> bool {
        self.violation.is_none()
    }
}

/// Aggregate report of one evaluation variant.
#[derive(Debug, Clone)]
pub struct RetrievalVariantReport {
    /// Variant label ([`BASELINE_VARIANT`] or [`EMBEDDINGS_VARIANT`]).
    pub label: &'static str,
    /// Provider name of the comparison variant; `None` for the
    /// mandatory baseline.
    pub provider: Option<String>,
    /// One outcome per fixture, frozen fixture order.
    pub outcomes: Vec<RetrievalFixtureOutcome>,
}

impl RetrievalVariantReport {
    /// Mean Recall@K across all fixtures.
    pub fn mean_recall_at_k(&self) -> f64 {
        mean(self.outcomes.iter().map(|o| o.recall_at_k))
    }

    /// Mean reciprocal rank across all fixtures.
    pub fn mean_reciprocal_rank(&self) -> f64 {
        mean(self.outcomes.iter().map(|o| o.mean_reciprocal_rank))
    }

    /// Total forbidden-source hits across all fixtures.
    pub fn forbidden_hits(&self) -> usize {
        self.outcomes.iter().map(|o| o.forbidden_hits).sum()
    }

    /// Total snapshot leakage across all fixtures (must be zero).
    pub fn snapshot_leakage(&self) -> usize {
        self.outcomes.iter().map(|o| o.snapshot_leakage).sum()
    }

    /// Total refresh-plus-lookup latency across all fixtures.
    pub fn latency_total(&self) -> Duration {
        self.outcomes.iter().map(|o| o.latency).sum()
    }

    /// Mean per-fixture latency.
    pub fn latency_mean(&self) -> Duration {
        if self.outcomes.is_empty() {
            return Duration::ZERO;
        }
        self.latency_total() / self.outcomes.len() as u32
    }

    /// Number of passed fixtures.
    pub fn fixtures_passed(&self) -> usize {
        self.outcomes.iter().filter(|o| o.passed()).count()
    }

    /// Whether every fixture passed.
    pub fn passed(&self) -> bool {
        self.outcomes.iter().all(|o| o.passed())
    }

    /// Whether this is the mandatory baseline whose failure fails the
    /// command.
    pub fn mandatory(&self) -> bool {
        self.provider.is_none()
    }
}

/// The file set and content of one target snapshot, used for the
/// leakage check independently of the retriever's membership tables.
#[derive(Debug, Clone)]
pub struct SnapshotContents {
    files: BTreeMap<String, String>,
}

impl SnapshotContents {
    /// Read every file of the target snapshot through Git (the
    /// semantic authority, plan section 26).
    pub fn from_reader(reader: &SnapshotReader) -> Result<Self, crate::git::GitError> {
        let mut files = BTreeMap::new();
        for path in reader.list_files()? {
            if let Some(file) = reader.read(&path)? {
                files.insert(path, String::from_utf8_lossy(&file.bytes).into_owned());
            }
        }
        Ok(SnapshotContents { files })
    }

    /// Whether the path is part of the snapshot.
    pub fn contains_path(&self, path: &str) -> bool {
        self.files.contains_key(path)
    }

    /// Whether `word` appears as a whole identifier token in the
    /// snapshot content of `path`.
    pub fn contains_word(&self, path: &str, word: &str) -> bool {
        self.files
            .get(path)
            .is_some_and(|content| contains_word(content, word))
    }

    /// Line count of the snapshot content of `path` (0 when absent).
    pub fn line_count(&self, path: &str) -> usize {
        self.files
            .get(path)
            .map_or(0, |content| content.lines().count())
    }
}

/// Whether `word` appears as a whole identifier token in `text`.
fn contains_word(text: &str, word: &str) -> bool {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .any(|token| token == word)
}

/// Whether an entry kind carries target-snapshot source content.
fn is_source_kind(kind: ContextKind) -> bool {
    matches!(
        kind,
        ContextKind::SymbolDefinition
            | ContextKind::SymbolReference
            | ContextKind::SourceSkeleton
            | ContextKind::Source
    )
}

/// Count entries that do not belong to the target snapshot: paths
/// absent from the snapshot, symbols absent from the snapshot content
/// of their path, and invalid line spans. Leakage always fails a
/// fixture (milestone 23).
pub fn count_snapshot_leakage(bundle: &ContextBundle, snapshot: &SnapshotContents) -> usize {
    let mut leakage = 0;
    for entry in &bundle.entries {
        if !is_source_kind(entry.kind) {
            continue;
        }
        let Some(path) = entry.path.as_deref() else {
            continue;
        };
        if !snapshot.contains_path(path) {
            leakage += 1;
            continue;
        }
        if let Some(symbol) = entry.symbol.as_deref()
            && !snapshot.contains_word(path, symbol)
        {
            leakage += 1;
            continue;
        }
        let lines = snapshot.line_count(path);
        let start_ok = entry
            .line_start
            .is_none_or(|start| start >= 1 && (start as usize) <= lines);
        let end_ok = entry.line_end.is_none_or(|end| (end as usize) <= lines);
        let order_ok = !matches!(
            (entry.line_start, entry.line_end),
            (Some(start), Some(end)) if end < start
        );
        if !(start_ok && end_ok && order_ok) {
            leakage += 1;
        }
    }
    leakage
}

/// Recall@K of the required sources: the fraction of `required` paths
/// present among the first `k` source entries of the ranked bundle.
/// Returns `(recall, found)`.
pub fn recall_at_k(bundle: &ContextBundle, required: &[&str], k: usize) -> (f64, usize) {
    if required.is_empty() {
        return (1.0, 0);
    }
    let top: BTreeSet<&str> = bundle
        .entries
        .iter()
        .filter(|entry| is_source_kind(entry.kind))
        .filter_map(|entry| entry.path.as_deref())
        .take(k)
        .collect();
    let found = required
        .iter()
        .filter(|required| top.contains(**required))
        .count();
    (found as f64 / required.len() as f64, found)
}

/// Reciprocal rank of one required source: `1 / rank` of its first
/// occurrence among the source entries, `0.0` when absent.
pub fn reciprocal_rank(bundle: &ContextBundle, required: &str) -> f64 {
    let rank = bundle
        .entries
        .iter()
        .filter(|entry| is_source_kind(entry.kind))
        .position(|entry| entry.path.as_deref() == Some(required));
    match rank {
        Some(index) => 1.0 / (index + 1) as f64,
        None => 0.0,
    }
}

/// Mean reciprocal rank of the required sources.
pub fn mean_reciprocal_rank(bundle: &ContextBundle, required: &[&str]) -> f64 {
    mean(
        required
            .iter()
            .map(|required| reciprocal_rank(bundle, required)),
    )
}

/// Entries citing a forbidden source.
pub fn count_forbidden_hits(bundle: &ContextBundle, forbidden: &[&str]) -> usize {
    bundle
        .entries
        .iter()
        .filter(|entry| is_source_kind(entry.kind))
        .filter(|entry| {
            entry
                .path
                .as_deref()
                .is_some_and(|path| forbidden.contains(&path))
        })
        .count()
}

/// The correctness-gate violation of one fixture outcome, if any:
/// required sources must be recalled and leakage is always failure.
pub fn fixture_violation(
    name: &str,
    recall: f64,
    found: usize,
    total: usize,
    leakage: usize,
) -> Option<String> {
    if recall < 1.0 {
        return Some(format!(
            "fixture `{name}` recalled {found}/{total} required sources within the top {RETRIEVAL_TOP_K}"
        ));
    }
    if leakage > 0 {
        return Some(format!(
            "fixture `{name}` leaked {leakage} entries from outside the target snapshot"
        ));
    }
    None
}

/// Run the frozen retrieval fixtures.
///
/// The mandatory baseline variant is always produced. When an
/// embedding provider is configured, a second variant runs the same
/// fixtures with optional reranking for comparison.
pub fn run_retrieval_eval(
    embedding_provider: Option<Arc<dyn EmbeddingProvider>>,
) -> Result<Vec<RetrievalVariantReport>, RetrievalEvalError> {
    let repo = FixtureRepository::build()?;
    let fixtures = retrieval_fixtures();
    super::fixtures::validate_fixture_set(&fixtures)
        .map_err(super::fixtures::RetrievalFixtureError::Invalid)?;
    repo.validate_against_fixtures(&fixtures)?;

    let baseline = run_variant(&repo, &fixtures, None)?;
    let mut variants = vec![baseline];
    if let Some(provider) = embedding_provider {
        variants.push(run_variant(&repo, &fixtures, Some(provider))?);
    }
    Ok(variants)
}

/// Run every fixture through one variant and aggregate the metrics.
fn run_variant(
    repo: &FixtureRepository,
    fixtures: &[RetrievalFixture],
    provider: Option<Arc<dyn EmbeddingProvider>>,
) -> Result<RetrievalVariantReport, RetrievalEvalError> {
    let mut outcomes = Vec::with_capacity(fixtures.len());
    for fixture in fixtures {
        outcomes.push(run_fixture(repo, fixture, provider.as_ref())?);
    }
    Ok(RetrievalVariantReport {
        label: if provider.is_some() {
            EMBEDDINGS_VARIANT
        } else {
            BASELINE_VARIANT
        },
        provider: provider.map(|provider| provider.name().to_string()),
        outcomes,
    })
}

/// Execute one fixture end to end and measure its outcome.
fn run_fixture(
    repo: &FixtureRepository,
    fixture: &RetrievalFixture,
    provider: Option<&Arc<dyn EmbeddingProvider>>,
) -> Result<RetrievalFixtureOutcome, RetrievalEvalError> {
    let execution = |message: String| RetrievalEvalError::Execution {
        fixture: fixture.name.to_string(),
        message,
    };

    let root = repo.root_for(fixture.worktree)?;
    let runner = GitRunner::new();
    let ctx = RepoContext::discover(&runner, root)
        .map_err(|err| execution(format!("repository discovery: {err}")))?;
    let resolved = resolve_snapshot(&ctx, &SnapshotSpec::Working)
        .map_err(|err| execution(format!("snapshot resolution: {err}")))?;
    let storage = StoragePaths::for_repo(&ctx);

    let start = Instant::now();
    refresh_index(&ctx, &resolved, &storage)
        .map_err(|err| execution(format!("index refresh: {err}")))?;
    let retriever = Retriever::open(&resolved, &storage)
        .map_err(|err| execution(format!("retriever open: {err}")))?;
    let retriever = match provider {
        Some(provider) => retriever.with_embedding_provider(provider.clone()),
        None => retriever,
    };
    let query = RetrievalQuery::from_text(fixture.query);
    let bundle = retriever
        .lookup(&query)
        .map_err(|err| execution(format!("lookup: {err}")))?;
    let latency = start.elapsed();

    // Independent snapshot verification for the leakage metric.
    let reader = SnapshotReader::new(&ctx, &resolved)
        .map_err(|err| execution(format!("snapshot reader: {err}")))?;
    let snapshot = SnapshotContents::from_reader(&reader)
        .map_err(|err| execution(format!("snapshot contents: {err}")))?;
    let leakage = count_snapshot_leakage(&bundle, &snapshot);

    let (recall, found) = recall_at_k(&bundle, &fixture.required_sources, RETRIEVAL_TOP_K);
    let violation = fixture_violation(
        fixture.name,
        recall,
        found,
        fixture.required_sources.len(),
        leakage,
    );

    Ok(RetrievalFixtureOutcome {
        name: fixture.name.to_string(),
        worktree: fixture.worktree.to_string(),
        required_found: found,
        required_total: fixture.required_sources.len(),
        recall_at_k: recall,
        mean_reciprocal_rank: mean_reciprocal_rank(&bundle, &fixture.required_sources),
        forbidden_hits: count_forbidden_hits(&bundle, &fixture.forbidden_sources),
        snapshot_leakage: leakage,
        entries: bundle.entries.len(),
        latency,
        violation,
    })
}

/// Render the complete retrieval evaluation report: a human-readable
/// summary followed by one machine-readable JSON line after the
/// `machine-readable:` marker.
pub fn render_retrieval_report(variants: &[RetrievalVariantReport]) -> String {
    let mut out = String::from("nisse eval retrieval\n");
    for variant in variants {
        out.push_str(&format!(
            "variant: {} ({})\n",
            variant.label,
            match &variant.provider {
                Some(provider) => format!("provider: {provider}"),
                None => "mandatory baseline".to_string(),
            }
        ));
        for outcome in &variant.outcomes {
            let verdict = if outcome.passed() { "pass" } else { "FAIL" };
            out.push_str(&format!(
                "  {verdict}  {:<20} recall@{}: {:.3}  mrr: {:.3}  forbidden: {}  leakage: {}\n",
                outcome.name,
                RETRIEVAL_TOP_K,
                outcome.recall_at_k,
                outcome.mean_reciprocal_rank,
                outcome.forbidden_hits,
                outcome.snapshot_leakage,
            ));
            if let Some(violation) = &outcome.violation {
                out.push_str(&format!("        {violation}\n"));
            }
        }
        out.push_str(&format!(
            "  mean recall@{}: {:.3}\n",
            RETRIEVAL_TOP_K,
            variant.mean_recall_at_k()
        ));
        out.push_str(&format!(
            "  mean reciprocal rank: {:.3}\n",
            variant.mean_reciprocal_rank()
        ));
        out.push_str(&format!("  forbidden hits: {}\n", variant.forbidden_hits()));
        out.push_str(&format!(
            "  snapshot leakage: {}\n",
            variant.snapshot_leakage()
        ));
        out.push_str(&format!(
            "  latency: mean {:.3} ms / total {:.3} ms\n",
            duration_ms(variant.latency_mean()),
            duration_ms(variant.latency_total())
        ));
        out.push_str(&format!(
            "  verdict: {}{}\n",
            if variant.passed() { "pass" } else { "fail" },
            if variant.mandatory() {
                String::new()
            } else {
                " (comparison only; exit status follows the mandatory baseline)".to_string()
            }
        ));
    }
    out.push_str("\nmachine-readable:\n");
    out.push_str(&render_retrieval_json(variants));
    out.push('\n');
    out
}

/// The machine-readable JSON form of the report (one compact line).
pub fn render_retrieval_json(variants: &[RetrievalVariantReport]) -> String {
    let report = JsonReport {
        evaluation: "retrieval",
        variants: variants
            .iter()
            .map(|variant| JsonVariant {
                label: variant.label,
                provider: variant.provider.as_deref(),
                mandatory: variant.mandatory(),
                mean_recall_at_k: round3(variant.mean_recall_at_k()),
                mean_reciprocal_rank: round3(variant.mean_reciprocal_rank()),
                forbidden_hits: variant.forbidden_hits(),
                snapshot_leakage: variant.snapshot_leakage(),
                fixtures_passed: variant.fixtures_passed(),
                fixtures_total: variant.outcomes.len(),
                latency_mean_ms: duration_ms(variant.latency_mean()),
                latency_total_ms: duration_ms(variant.latency_total()),
                passed: variant.passed(),
                fixtures: variant
                    .outcomes
                    .iter()
                    .map(|outcome| JsonFixture {
                        name: &outcome.name,
                        worktree: &outcome.worktree,
                        required_found: outcome.required_found,
                        required_total: outcome.required_total,
                        recall_at_k: round3(outcome.recall_at_k),
                        mean_reciprocal_rank: round3(outcome.mean_reciprocal_rank),
                        forbidden_hits: outcome.forbidden_hits,
                        snapshot_leakage: outcome.snapshot_leakage,
                        entries: outcome.entries,
                        latency_ms: duration_ms(outcome.latency),
                        passed: outcome.passed(),
                        violation: outcome.violation.as_deref(),
                    })
                    .collect(),
            })
            .collect(),
    };
    serde_json::to_string(&report).expect("evaluation reports always serialize")
}

/// Mean of a float iterator (0.0 when empty).
fn mean(values: impl Iterator<Item = f64>) -> f64 {
    let mut sum = 0.0;
    let mut count = 0;
    for value in values {
        sum += value;
        count += 1;
    }
    if count == 0 { 0.0 } else { sum / count as f64 }
}

/// Milliseconds with microsecond resolution.
fn duration_ms(duration: Duration) -> f64 {
    round3(duration.as_secs_f64() * 1000.0)
}

/// Round to three decimals so reports are stable to the eye.
fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

#[derive(Serialize)]
struct JsonReport<'a> {
    evaluation: &'static str,
    variants: Vec<JsonVariant<'a>>,
}

#[derive(Serialize)]
struct JsonVariant<'a> {
    label: &'a str,
    provider: Option<&'a str>,
    mandatory: bool,
    mean_recall_at_k: f64,
    mean_reciprocal_rank: f64,
    forbidden_hits: usize,
    snapshot_leakage: usize,
    fixtures_passed: usize,
    fixtures_total: usize,
    latency_mean_ms: f64,
    latency_total_ms: f64,
    passed: bool,
    fixtures: Vec<JsonFixture<'a>>,
}

#[derive(Serialize)]
struct JsonFixture<'a> {
    name: &'a str,
    worktree: &'a str,
    required_found: usize,
    required_total: usize,
    recall_at_k: f64,
    mean_reciprocal_rank: f64,
    forbidden_hits: usize,
    snapshot_leakage: usize,
    entries: usize,
    latency_ms: f64,
    passed: bool,
    violation: Option<&'a str>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::context::ContextEntry;
    use crate::retrieval::fixtures::{FEATURE_WORKTREE, MAIN_WORKTREE};

    fn source_entry(path: &str, symbol: Option<&str>, line: Option<(u32, u32)>) -> ContextEntry {
        ContextEntry {
            kind: ContextKind::SymbolDefinition,
            title: String::new(),
            path: Some(path.to_string()),
            symbol: symbol.map(str::to_string),
            line_start: line.map(|(start, _)| start),
            line_end: line.map(|(_, end)| end),
            record_id: None,
            practice_id: None,
            text: String::new(),
            score: 1.0,
        }
    }

    fn snapshot() -> SnapshotContents {
        SnapshotContents {
            files: BTreeMap::from([(
                "src/mod.rs".to_string(),
                "pub fn beta_fn() -> u8 {\n    2\n}\n".to_string(),
            )]),
        }
    }

    #[test]
    fn recall_at_k_counts_only_the_head_of_source_entries() {
        let mut bundle = ContextBundle::empty();
        bundle.entries.push(source_entry("other/a.rs", None, None));
        bundle.entries.push(source_entry("src/mod.rs", None, None));

        let (recall, found) = recall_at_k(&bundle, &["src/mod.rs"], 1);
        assert!((recall - 0.0).abs() < 1e-12);
        assert_eq!(found, 0);

        let (recall, found) = recall_at_k(&bundle, &["src/mod.rs"], 2);
        assert!((recall - 1.0).abs() < 1e-12);
        assert_eq!(found, 1);

        // Empty required sets are trivially complete.
        assert_eq!(recall_at_k(&bundle, &[], 3), (1.0, 0));
    }

    #[test]
    fn reciprocal_rank_follows_entry_order() {
        let mut bundle = ContextBundle::empty();
        bundle.entries.push(source_entry("a.rs", None, None));
        bundle.entries.push(source_entry("b.rs", None, None));
        bundle.entries.push(source_entry("c.rs", None, None));

        assert!((reciprocal_rank(&bundle, "a.rs") - 1.0).abs() < 1e-12);
        assert!((reciprocal_rank(&bundle, "b.rs") - 0.5).abs() < 1e-12);
        assert!((reciprocal_rank(&bundle, "missing.rs") - 0.0).abs() < 1e-12);
        assert!((mean_reciprocal_rank(&bundle, &["a.rs", "missing.rs"]) - 0.5).abs() < 1e-12);
    }

    #[test]
    fn forbidden_hits_count_citing_entries() {
        let mut bundle = ContextBundle::empty();
        bundle.entries.push(source_entry("a.rs", None, None));
        bundle.entries.push(source_entry("b.rs", None, None));
        assert_eq!(count_forbidden_hits(&bundle, &["b.rs"]), 1);
        assert_eq!(count_forbidden_hits(&bundle, &["c.rs"]), 0);
    }

    #[test]
    fn leakage_counter_detects_every_violation_kind() {
        let snapshot = snapshot();

        let mut bundle = ContextBundle::empty();
        // 1. A member entry with a present symbol and valid span.
        bundle
            .entries
            .push(source_entry("src/mod.rs", Some("beta_fn"), Some((1, 3))));
        // 2. A path absent from the target snapshot.
        bundle.entries.push(source_entry(
            "src/legacy/old_module.rs",
            Some("legacy_hook"),
            None,
        ));
        // 3. A member path whose symbol is not in the snapshot content.
        bundle
            .entries
            .push(source_entry("src/mod.rs", Some("alpha_fn"), None));
        // 4. A line span beyond the file.
        bundle
            .entries
            .push(source_entry("src/mod.rs", Some("beta_fn"), Some((1, 99))));
        // 5. An inverted span.
        bundle
            .entries
            .push(source_entry("src/mod.rs", Some("beta_fn"), Some((3, 1))));
        // 6. A start before line 1.
        bundle
            .entries
            .push(source_entry("src/mod.rs", Some("beta_fn"), Some((0, 2))));
        // 7. Non-source entries never count.
        bundle.entries.push(ContextEntry {
            kind: ContextKind::Practice,
            title: String::new(),
            path: Some(".nisse/common/x.json".to_string()),
            symbol: None,
            line_start: None,
            line_end: None,
            record_id: None,
            practice_id: Some("p".to_string()),
            text: String::new(),
            score: 0.5,
        });

        assert_eq!(count_snapshot_leakage(&bundle, &snapshot), 5);
    }

    #[test]
    fn fixture_violation_covers_recall_and_leakage() {
        assert_eq!(fixture_violation("f", 1.0, 2, 2, 0), None);
        let violation = fixture_violation("f", 0.5, 1, 2, 0).unwrap();
        assert!(violation.contains("recalled 1/2"), "{violation}");
        let violation = fixture_violation("f", 1.0, 2, 2, 3).unwrap();
        assert!(violation.contains("leaked 3"), "{violation}");
    }

    #[test]
    fn metric_aggregates_are_computed_correctly() {
        let report = RetrievalVariantReport {
            label: BASELINE_VARIANT,
            provider: None,
            outcomes: vec![
                RetrievalFixtureOutcome {
                    name: "a".to_string(),
                    worktree: MAIN_WORKTREE.to_string(),
                    required_found: 2,
                    required_total: 2,
                    recall_at_k: 1.0,
                    mean_reciprocal_rank: 1.0,
                    forbidden_hits: 0,
                    snapshot_leakage: 0,
                    entries: 5,
                    latency: Duration::from_millis(10),
                    violation: None,
                },
                RetrievalFixtureOutcome {
                    name: "b".to_string(),
                    worktree: FEATURE_WORKTREE.to_string(),
                    required_found: 1,
                    required_total: 2,
                    recall_at_k: 0.5,
                    mean_reciprocal_rank: 0.25,
                    forbidden_hits: 2,
                    snapshot_leakage: 1,
                    entries: 4,
                    latency: Duration::from_millis(30),
                    violation: Some("leaked".to_string()),
                },
            ],
        };
        assert!((report.mean_recall_at_k() - 0.75).abs() < 1e-12);
        assert!((report.mean_reciprocal_rank() - 0.625).abs() < 1e-12);
        assert_eq!(report.forbidden_hits(), 2);
        assert_eq!(report.snapshot_leakage(), 1);
        assert_eq!(report.fixtures_passed(), 1);
        assert!(!report.passed());
        assert!(report.mandatory());
        assert_eq!(report.latency_total(), Duration::from_millis(40));
        assert_eq!(report.latency_mean(), Duration::from_millis(20));

        let empty = RetrievalVariantReport {
            label: BASELINE_VARIANT,
            provider: None,
            outcomes: Vec::new(),
        };
        assert!(empty.passed());
        assert_eq!(empty.latency_mean(), Duration::ZERO);
    }

    /// A deterministic embedding provider for the comparison variant:
    /// never fails, vectors derived from the text length.
    struct ConstantEmbedder;

    impl EmbeddingProvider for ConstantEmbedder {
        fn name(&self) -> &'static str {
            "test-constant"
        }

        fn embed(
            &self,
            texts: &[&str],
        ) -> Result<crate::providers::ProviderEmbeddings, crate::providers::ProviderError> {
            Ok(crate::providers::ProviderEmbeddings {
                model: "constant-1".to_string(),
                vectors: texts
                    .iter()
                    .map(|text| vec![(text.len() % 7) as f32 + 1.0, 1.0])
                    .collect(),
            })
        }
    }

    /// The identity of one outcome, ignoring latency (which varies).
    fn signature(outcome: &RetrievalFixtureOutcome) -> String {
        format!(
            "{}|{}/{}|{:.3}|{:.3}|{}|{}|{:?}",
            outcome.name,
            outcome.required_found,
            outcome.required_total,
            outcome.recall_at_k,
            outcome.mean_reciprocal_rank,
            outcome.forbidden_hits,
            outcome.snapshot_leakage,
            outcome.violation,
        )
    }

    #[test]
    fn deterministic_baseline_passes_all_fixtures_with_zero_leakage() {
        let first = run_retrieval_eval(None).unwrap();
        let second = run_retrieval_eval(None).unwrap();

        assert_eq!(
            first.len(),
            1,
            "no provider means only the baseline variant"
        );
        let baseline = &first[0];
        assert_eq!(baseline.label, BASELINE_VARIANT);
        assert_eq!(
            baseline.outcomes.len(),
            super::super::fixtures::retrieval_fixtures().len()
        );

        // Every fixture recalls all required sources within the top K
        // with zero snapshot leakage.
        for outcome in &baseline.outcomes {
            assert!(
                outcome.passed(),
                "{}",
                outcome.violation.clone().unwrap_or_default()
            );
            assert_eq!(outcome.snapshot_leakage, 0, "{}", outcome.name);
            assert_eq!(outcome.recall_at_k, 1.0, "{}", outcome.name);
        }
        assert!(baseline.passed());
        assert_eq!(baseline.snapshot_leakage(), 0);

        // The isolation fixture explicitly proves zero branch leakage:
        // its query names main-only content while running against the
        // feature snapshot.
        let isolation = baseline
            .outcomes
            .iter()
            .find(|outcome| outcome.name == "snapshot-isolation")
            .unwrap();
        assert_eq!(isolation.worktree, FEATURE_WORKTREE);
        assert_eq!(isolation.snapshot_leakage, 0);

        // Determinism: identical outcomes on a second run (latency
        // aside).
        let signatures: Vec<String> = baseline.outcomes.iter().map(signature).collect();
        let second_signatures: Vec<String> = second[0].outcomes.iter().map(signature).collect();
        assert_eq!(signatures, second_signatures);
    }

    #[test]
    fn embeddings_variant_is_reported_alongside_the_baseline() {
        let provider: Arc<dyn EmbeddingProvider> = Arc::new(ConstantEmbedder);
        let variants = run_retrieval_eval(Some(provider)).unwrap();
        assert_eq!(variants.len(), 2);
        assert_eq!(variants[0].label, BASELINE_VARIANT);
        assert!(variants[0].mandatory());
        assert_eq!(variants[1].label, EMBEDDINGS_VARIANT);
        assert_eq!(variants[1].provider.as_deref(), Some("test-constant"));
        assert!(!variants[1].mandatory());
        // Reranking is optional refinement: both variants must satisfy
        // the correctness gates.
        assert!(variants[1].passed(), "{:?}", variants[1].outcomes);
        assert_eq!(variants[1].snapshot_leakage(), 0);
    }

    #[test]
    fn report_renders_text_and_machine_readable_json() {
        let variants = run_retrieval_eval(None).unwrap();
        let report = render_retrieval_report(&variants);
        assert!(report.starts_with("nisse eval retrieval\n"));
        assert!(report.contains("variant: baseline (mandatory baseline)"));
        assert!(report.contains("snapshot-isolation"));
        assert!(report.contains("snapshot leakage: 0"));
        assert!(report.contains("verdict: pass"));

        let json = report
            .split("machine-readable:\n")
            .nth(1)
            .expect("machine-readable marker")
            .lines()
            .next()
            .expect("one JSON line");
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(value["evaluation"], "retrieval");
        let variant = &value["variants"][0];
        assert_eq!(variant["label"], "baseline");
        assert_eq!(variant["mandatory"], true);
        assert_eq!(variant["passed"], true);
        assert_eq!(variant["snapshot_leakage"], 0);
        assert_eq!(
            variant["fixtures_total"].as_u64().unwrap() as usize,
            variants[0].outcomes.len()
        );
        assert!(variant["mean_recall_at_k"].as_f64().unwrap() >= 0.999);
        assert_eq!(variant["fixtures"][0]["recall_at_k"].as_f64().unwrap(), 1.0);
        assert!(variant["fixtures"][0]["latency_ms"].as_f64().unwrap() >= 0.0);
    }
}
