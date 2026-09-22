//! Fact staleness and targeted revalidation (plan section 48).
//!
//! Code-derived facts retain content provenance: their source evidence
//! entries carry the content key of the exact snapshot content they
//! were verified against (stamped deterministically by the Rust core
//! when a fact is persisted; see
//! [`stamp_content_provenance`]). When that supporting content is
//! absent from the current target snapshot — because the path was
//! deleted, the content key no longer matches, or (for evidence
//! without a recorded key) the cited excerpt is gone — the fact
//! becomes **potentially stale**.
//!
//! Potentially stale facts are never silently current truth (permanent
//! invariant 22: current source evidence outranks stale memory):
//!
//! - [`assess_records`] computes the stale set of a record store
//!   against one exact target snapshot;
//! - retrieval marks those facts `[potentially stale]` and downranks
//!   them below every unquestioned kind of knowledge (plan section
//!   31);
//! - they remain eligible for **targeted revalidation**
//!   ([`revalidate_fact`]): one fresh, independent Pi verification
//!   session judges the exact claim against the *current* content of
//!   the cited paths and answers `supported`, `contradicted`, or
//!   `insufficient` — the same frozen verdicts as fact-candidate
//!   verification (plan section 47 step 2).
//!
//! Revalidation updates knowledge immutably (plan section 48):
//!
//! - `supported` — a new supported fact record supersedes the old one
//!   with provenance refreshed to the current snapshot; the old record
//!   remains for provenance;
//! - `contradicted` — a new fact record in state
//!   [`UnresolvedConflict`](crate::domain::records::FactState::UnresolvedConflict)
//!   supersedes the old one: the claim is preserved verbatim but is no
//!   longer normal project truth — the conflict with current source is
//!   surfaced explicitly, never silently resolved (plan section 42);
//! - `insufficient` (including any verifier failure) — nothing is
//!   updated; the attempt is retained as pending local state in the
//!   regenerable cache.
//!
//! Without a contributor identity no durable record is created; the
//! revalidation stays pending locally (plan section 44).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::domain::SCHEMA_VERSION;
use crate::domain::evidence::{EvidenceKind, EvidenceRef};
use crate::domain::facts::{FactVerificationResult, FactVerificationVerdict};
use crate::domain::ids::new_ulid;
use crate::domain::model::ThinkingLevel;
use crate::domain::records::{FactRecord, FactState, MAX_EXCERPT_BYTES, ProjectRecord};
use crate::domain::time::now_utc_rfc3339;
use crate::git::{ObjectFormat, RepoContext, SnapshotReader};
use crate::index::content::git_blob_oid;
use crate::jury::findings::{ValidationContext, snapshot_content_keys};
use crate::jury::host::HostProcess;
use crate::jury::protocol::FactVerificationPayload;
use crate::storage::StoragePaths;
use crate::storage::records::{RecordStore, StoredRecord, active_records};

/// Timeout of one revalidation session.
pub const REVALIDATION_TIMEOUT_MS: u64 = 120_000;

/// Slack added to the session timeout for driving one job to
/// completion (mirrors the other narrow-job exchanges).
const REVALIDATION_DRIVER_SLACK_MS: u64 = 10_000;

/// `pending_learning` row kind of fact revalidations.
pub const PENDING_KIND: &str = "fact_revalidation";

/// Freshness of one fact against one exact target snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FactFreshness {
    /// Every source-evidence entry's supporting content is still part
    /// of the target snapshot.
    Current,
    /// At least one evidence entry's supporting content is absent from
    /// the target snapshot; the reasons are human-readable and
    /// deterministic.
    PotentiallyStale {
        /// Why the fact's support is no longer fully present.
        reasons: Vec<String>,
    },
}

impl FactFreshness {
    /// Whether this assessment marks the fact potentially stale.
    pub fn is_stale(&self) -> bool {
        matches!(self, FactFreshness::PotentiallyStale { .. })
    }
}

/// Canonical content provenance of a snapshot file: the Git blob OID
/// of its exact bytes. Every snapshot side answers to this key — tree
/// and index sides carry the OID Git recorded, and working-tree bytes
/// hash to it — which is exactly what lets a recorded key prove "this
/// is (or is not) the same content" across commits, branches, and
/// worktrees.
fn provenance_key(bytes: &[u8], format: ObjectFormat) -> String {
    format!("git:{}", git_blob_oid(bytes, format))
}

/// Whether `haystack` contains `needle` as an exact byte substring.
fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack.len() >= needle.len()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// Assess one evidence entry against the target snapshot; every
/// returned string is one staleness reason.
fn assess_evidence(
    reader: &SnapshotReader<'_>,
    format: ObjectFormat,
    evidence: &EvidenceRef,
) -> Vec<String> {
    // Only source evidence is code-derived; plan and record evidence
    // are not snapshot members and never make a fact stale.
    if evidence.kind != EvidenceKind::Source {
        return Vec::new();
    }
    let Some(path) = &evidence.path else {
        return Vec::new();
    };
    match reader.read(path) {
        Err(error) => vec![format!(
            "evidence path `{path}` could not be read from the target snapshot: {error}"
        )],
        Ok(None) => vec![format!(
            "evidence path `{path}` is no longer part of the target snapshot"
        )],
        Ok(Some(file)) => {
            let keys = snapshot_content_keys(&file, format);
            if let Some(key) = &evidence.content_key {
                if !keys.contains(key) {
                    return vec![format!(
                        "the content of `{path}` changed; the recorded provenance \
                         `{key}` no longer matches the target snapshot"
                    )];
                }
                Vec::new()
            } else if contains_bytes(&file.bytes, evidence.excerpt.as_bytes()) {
                Vec::new()
            } else {
                vec![format!(
                    "the cited excerpt is no longer present in `{path}`"
                )]
            }
        }
    }
}

/// Assess one fact against the exact target snapshot (plan section
/// 48). Structural only: a `Current` fact is provenance-anchored to
/// present content, not proven semantically true.
pub fn assess_fact(
    fact: &FactRecord,
    reader: &SnapshotReader<'_>,
    format: ObjectFormat,
) -> FactFreshness {
    let mut reasons = Vec::new();
    for evidence in &fact.evidence {
        reasons.extend(assess_evidence(reader, format, evidence));
    }
    if reasons.is_empty() {
        FactFreshness::Current
    } else {
        FactFreshness::PotentiallyStale { reasons }
    }
}

/// The potentially stale facts of a record store against one exact
/// target snapshot: active (non-superseded) supported facts only —
/// superseded facts are provenance, and conflicting facts are already
/// not normal project truth.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StalenessReport {
    /// Potentially stale fact ID -> deterministic reasons.
    entries: BTreeMap<String, Vec<String>>,
}

impl StalenessReport {
    /// IDs of every potentially stale fact, sorted.
    pub fn ids(&self) -> BTreeSet<String> {
        self.entries.keys().cloned().collect()
    }

    /// Reasons of one fact, when it is potentially stale.
    pub fn reasons(&self, fact_id: &str) -> Option<&[String]> {
        self.entries.get(fact_id).map(Vec::as_slice)
    }

    /// Whether no fact is potentially stale.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Number of potentially stale facts.
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Assess every active supported fact of `records` against the exact
/// target snapshot read by `reader` (plan section 48).
pub fn assess_records(
    records: &[StoredRecord],
    reader: &SnapshotReader<'_>,
    format: ObjectFormat,
) -> StalenessReport {
    let mut report = StalenessReport::default();
    for stored in active_records(records) {
        if let ProjectRecord::Fact(fact) = &stored.record
            && fact.state == FactState::Supported
            && let FactFreshness::PotentiallyStale { reasons } = assess_fact(fact, reader, format)
        {
            report.entries.insert(fact.id.clone(), reasons);
        }
    }
    report
}

/// Stamp content provenance onto the evidence of a fact that is about
/// to be persisted (plan section 48: "code-derived facts retain
/// content provenance"). Each `source` entry whose path still reads
/// back from the target snapshot gets the canonical content key of
/// that content; everything else is left unchanged. Rust computes the
/// key — a reviewer never supplies its own provenance.
pub fn stamp_content_provenance(
    validation: &ValidationContext<'_>,
    evidence: &[EvidenceRef],
) -> Vec<EvidenceRef> {
    evidence
        .iter()
        .map(|entry| {
            if entry.kind != EvidenceKind::Source {
                return entry.clone();
            }
            let Some(path) = &entry.path else {
                return entry.clone();
            };
            match validation.reader.read(path) {
                Ok(Some(file)) => EvidenceRef {
                    content_key: Some(provenance_key(&file.bytes, validation.object_format)),
                    ..entry.clone()
                },
                // Validation already proved snapshot membership; a
                // transient failure keeps the entry unstamped rather
                // than failing the fact.
                _ => entry.clone(),
            }
        })
        .collect()
}

/// The result of rebuilding one fact's evidence against the current
/// target snapshot. Shared by targeted revalidation (plan section 48)
/// and the conflict handling of knowledge consolidation (plan section
/// 55: "retrieve current evidence where applicable").
#[derive(Debug, Clone, PartialEq)]
pub struct RefreshedEvidence {
    /// Evidence entries anchored in the current snapshot: refreshed
    /// `source` entries (current excerpts, clamped ranges, stamped
    /// provenance) plus non-source entries carried over unchanged.
    pub evidence: Vec<EvidenceRef>,
    /// Why a source entry could not be refreshed (deleted paths,
    /// unreadable content, empty cited locations).
    pub unavailable: Vec<String>,
}

/// Truncate to a byte bound on a char boundary.
fn truncate_bytes(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Lines `start..=end` (1-based, inclusive) of `content` as one
/// excerpt, clamped to the actual line count.
fn excerpt_of_lines(content: &str, start: u32, end: u32) -> String {
    let start = (start.max(1) as usize).max(1);
    let end = (end.max(start as u32) as usize).max(start);
    let lines: Vec<&str> = content.lines().collect();
    let last = end.min(lines.len());
    if start > lines.len() || start > last {
        return String::new();
    }
    lines[start - 1..last].join("\n")
}

/// Rebuild one fact's evidence against the current target snapshot.
///
/// For every `source` entry whose path is still present: the excerpt
/// becomes the current content at the cited location (the original
/// excerpt when it is still contained verbatim), the line range is
/// clamped to the current line count, and the content key is stamped
/// from the current bytes. Entries whose support vanished are dropped
/// with a retained note; non-source entries are carried over.
pub fn refresh_evidence(
    reader: &SnapshotReader<'_>,
    format: ObjectFormat,
    fact: &FactRecord,
) -> RefreshedEvidence {
    let mut refreshed = RefreshedEvidence {
        evidence: Vec::new(),
        unavailable: Vec::new(),
    };
    for entry in &fact.evidence {
        if entry.kind != EvidenceKind::Source {
            refreshed.evidence.push(entry.clone());
            continue;
        }
        let Some(path) = &entry.path else {
            refreshed
                .unavailable
                .push("a source evidence entry carries no path".to_string());
            continue;
        };
        let file = match reader.read(path) {
            Ok(Some(file)) => file,
            Ok(None) => {
                refreshed
                    .unavailable
                    .push(format!("`{path}` is no longer part of the target snapshot"));
                continue;
            }
            Err(error) => {
                refreshed
                    .unavailable
                    .push(format!("`{path}` could not be read: {error}"));
                continue;
            }
        };
        let content = String::from_utf8_lossy(&file.bytes).into_owned();
        let excerpt = if contains_bytes(&file.bytes, entry.excerpt.as_bytes()) {
            entry.excerpt.clone()
        } else {
            let start = entry.line_start.unwrap_or(1);
            let end = entry.line_end.unwrap_or(start);
            excerpt_of_lines(&content, start, end)
        };
        let excerpt = truncate_bytes(&excerpt, MAX_EXCERPT_BYTES).to_string();
        if excerpt.is_empty() {
            refreshed.unavailable.push(format!(
                "`{path}` no longer carries content at the cited location"
            ));
            continue;
        }
        let lines = content.lines().count().max(1) as u32;
        refreshed.evidence.push(EvidenceRef {
            kind: EvidenceKind::Source,
            path: Some(path.clone()),
            symbol: entry.symbol.clone(),
            line_start: entry.line_start.map(|line| line.min(lines)),
            line_end: entry.line_end.map(|line| line.min(lines)),
            content_key: Some(provenance_key(&file.bytes, format)),
            plan_section: None,
            record_id: None,
            input_part: None,
            excerpt,
        });
    }
    refreshed
}

/// Bounds and model selection of one revalidation session.
#[derive(Debug, Clone)]
pub struct RevalidationOptions {
    /// Explicitly configured model profile for the verifier session.
    /// Verifier sessions never inherit a model.
    pub model_profile: String,
    /// Explicit thinking level for the verifier session.
    pub thinking_level: ThinkingLevel,
    /// Session timeout (milliseconds).
    pub timeout_ms: u64,
}

impl Default for RevalidationOptions {
    fn default() -> Self {
        RevalidationOptions {
            model_profile: "standard".to_string(),
            thinking_level: ThinkingLevel::Medium,
            timeout_ms: REVALIDATION_TIMEOUT_MS,
        }
    }
}

/// The outcome of one targeted revalidation.
#[derive(Debug, Clone, PartialEq)]
pub struct RevalidationOutcome {
    /// The revalidated fact.
    pub fact_id: String,
    /// The verdict of the (possibly skipped) verification session.
    pub verdict: FactVerificationVerdict,
    /// The updated record created for `supported`/`contradicted`
    /// verdicts, when a contributor identity was available.
    pub new_record_id: Option<String>,
    /// Why the verifier session could not decide, when it failed. A
    /// failed session always yields `insufficient`.
    pub failure: Option<String>,
    /// Why no durable record was created (insufficient current
    /// evidence, missing contributor identity, storage failure), when
    /// applicable.
    pub reason: Option<String>,
}

/// Run one targeted revalidation of a potentially stale fact (plan
/// section 48) against the exact target snapshot of `validation`.
///
/// The verification session is fresh and independent — it is never a
/// reviewer of any review — and receives the exact claim plus evidence
/// rebuilt from the *current* snapshot content. When no current source
/// support can be rebuilt at all, revalidation is deterministically
/// `insufficient` without starting a session: there is nothing for a
/// verifier to check the claim against.
///
/// Durable updates require a contributor identity (plan section 44);
/// without one the attempt stays pending local state.
pub fn revalidate_fact(
    host: &mut HostProcess,
    ctx: &RepoContext,
    paths: Option<&StoragePaths>,
    validation: &ValidationContext<'_>,
    fact: &FactRecord,
    contributor: Option<&str>,
    options: &RevalidationOptions,
) -> RevalidationOutcome {
    let refreshed = refresh_evidence(validation.reader, validation.object_format, fact);
    let has_current_source = refreshed
        .evidence
        .iter()
        .any(|entry| entry.kind == EvidenceKind::Source);
    if !has_current_source {
        let reason = format!(
            "the supporting content is absent from the current target snapshot ({})",
            if refreshed.unavailable.is_empty() {
                "no source evidence entries".to_string()
            } else {
                refreshed.unavailable.join("; ")
            }
        );
        return finish(
            paths,
            fact,
            FactVerificationVerdict::Insufficient,
            None,
            None,
            Some(reason),
        );
    }

    // One fresh independent verification session over the exact claim
    // and the current-source evidence (plan section 47 step 2
    // semantics). A session failure is forced `insufficient` — the
    // same fail-safe as candidate verification — and never fails the
    // caller.
    let payload = FactVerificationPayload {
        verification_id: new_ulid(),
        timeout_ms: options.timeout_ms,
        model_profile: options.model_profile.clone(),
        thinking_level: options.thinking_level,
        claim: fact.text.clone(),
        evidence: refreshed.evidence.clone(),
    };
    let verification = match host.start_fact_verification(&payload).and_then(|()| {
        host.complete_fact_verification(
            &payload.verification_id,
            Duration::from_millis(options.timeout_ms + REVALIDATION_DRIVER_SLACK_MS),
        )
    }) {
        Ok(result) => result,
        Err(error) => FactVerificationResult {
            verification_id: payload.verification_id.clone(),
            verdict: FactVerificationVerdict::Insufficient,
            failure: Some(error.to_string()),
        },
    };

    match verification.verdict {
        FactVerificationVerdict::Supported => update_knowledge(
            paths,
            ctx,
            fact,
            FactState::Supported,
            &refreshed.evidence,
            contributor,
        ),
        FactVerificationVerdict::Contradicted => update_knowledge(
            paths,
            ctx,
            fact,
            FactState::UnresolvedConflict,
            &refreshed.evidence,
            contributor,
        ),
        FactVerificationVerdict::Insufficient => {
            let reason = match &verification.failure {
                Some(failure) => {
                    format!("revalidation could not decide ({failure}); the fact stays stale")
                }
                None => "revalidation found the current evidence insufficient; the fact stays \
                         stale"
                    .to_string(),
            };
            finish(
                paths,
                fact,
                FactVerificationVerdict::Insufficient,
                None,
                verification.failure,
                Some(reason),
            )
        }
    }
}

/// Create the updated immutable record for a decided revalidation, or
/// keep the attempt pending when no contributor identity exists (plan
/// section 44) or storage fails.
fn update_knowledge(
    paths: Option<&StoragePaths>,
    ctx: &RepoContext,
    fact: &FactRecord,
    state: FactState,
    evidence: &[EvidenceRef],
    contributor: Option<&str>,
) -> RevalidationOutcome {
    let verdict = if state == FactState::Supported {
        FactVerificationVerdict::Supported
    } else {
        FactVerificationVerdict::Contradicted
    };
    let Some(author) = contributor else {
        return finish(
            paths,
            fact,
            verdict,
            None,
            None,
            Some(
                "no contributor identity available; the updated fact remains pending \
                 (plan section 44)"
                    .to_string(),
            ),
        );
    };
    let record = FactRecord {
        schema_version: SCHEMA_VERSION,
        id: new_ulid(),
        author: author.to_string(),
        created_at: now_utc_rfc3339(),
        subject: fact.subject.clone(),
        text: fact.text.clone(),
        tags: fact.tags.clone(),
        state,
        evidence: evidence.to_vec(),
        source_review_id: fact.source_review_id.clone(),
        source_record_ids: vec![fact.id.clone()],
        supersedes: vec![fact.id.clone()],
    };
    match RecordStore::new(&ctx.worktree_root).create(&ProjectRecord::Fact(record)) {
        Ok(stored) => finish(
            paths,
            fact,
            verdict,
            Some(stored.id().to_string()),
            None,
            None,
        ),
        Err(error) => {
            let reason = format!("durable fact update failed: {error}");
            // The session decided, but nothing could be persisted: the
            // real verdict is preserved for diagnostics while the
            // attempt stays pending.
            finish(paths, fact, verdict, None, None, Some(reason))
        }
    }
}

/// Assemble the outcome and retain a pending-learning row for every
/// revalidation that produced no durable record.
fn finish(
    paths: Option<&StoragePaths>,
    fact: &FactRecord,
    verdict: FactVerificationVerdict,
    new_record_id: Option<String>,
    failure: Option<String>,
    reason: Option<String>,
) -> RevalidationOutcome {
    let outcome = RevalidationOutcome {
        fact_id: fact.id.clone(),
        verdict,
        new_record_id,
        failure,
        reason: reason.clone(),
    };
    if outcome.new_record_id.is_none()
        && let Some(paths) = paths
    {
        let entry = PendingFactRevalidation {
            schema_version: SCHEMA_VERSION,
            fact_id: fact.id.clone(),
            verdict,
            reason: reason.unwrap_or_else(|| "revalidation created no durable record".to_string()),
            created_at: now_utc_rfc3339(),
        };
        if let Ok(payload) = serde_json::to_string(&entry)
            && let Err(error) = crate::learning::facts::store_pending_row(
                paths,
                PENDING_KIND,
                &payload,
                &entry.created_at,
            )
        {
            tracing::warn!(%error, "failed to retain the fact revalidation in pending state");
        }
    }
    outcome
}

/// One fact-revalidation attempt retained in local pending-learning
/// state (never durable project knowledge; regenerable SQLite, plan
/// sections 44 and 57).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingFactRevalidation {
    /// Payload schema version; must be `1`.
    pub schema_version: u32,
    /// The fact that was revalidated.
    pub fact_id: String,
    /// The verdict of the attempt.
    pub verdict: FactVerificationVerdict,
    /// Why the attempt produced no durable record.
    pub reason: String,
    /// When the attempt was recorded (UTC RFC3339).
    pub created_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::evidence::EvidenceKind;
    use crate::git::GitRunner;
    use crate::git::ResolvedSnapshot;
    use crate::git::SnapshotSpec;
    use crate::git::resolve_snapshot;
    use crate::git::testutil::{Fixture, commit_all, write_file};
    use crate::storage::records::RecordLocation;

    fn source_evidence(path: &str, excerpt: &str, key: Option<String>) -> EvidenceRef {
        EvidenceRef {
            kind: EvidenceKind::Source,
            path: Some(path.to_string()),
            symbol: None,
            line_start: Some(1),
            line_end: Some(1),
            content_key: key,
            plan_section: None,
            record_id: None,
            input_part: None,
            excerpt: excerpt.to_string(),
        }
    }

    /// A fixture repository whose working snapshot contains `src/a.rs`
    /// with three lines; the context, snapshot, and reader are leaked
    /// so tests can borrow them for the lifetime of the process.
    struct Repo {
        fixture: Fixture,
        reader: SnapshotReader<'static>,
    }

    fn setup(name: &str) -> Repo {
        let fixture = Fixture::repo(name);
        write_file(&fixture.root, "src/a.rs", "fn alpha() {\n    marker\n}\n");
        commit_all(&fixture.root, "one");
        let ctx: &'static RepoContext = Box::leak(Box::new(
            RepoContext::discover(&GitRunner::new(), &fixture.root).unwrap(),
        ));
        let resolved: &'static ResolvedSnapshot = Box::leak(Box::new(
            resolve_snapshot(ctx, &SnapshotSpec::Working).unwrap(),
        ));
        let reader = SnapshotReader::new(ctx, resolved).unwrap();
        Repo { fixture, reader }
    }

    fn fact(evidence: Vec<EvidenceRef>) -> FactRecord {
        FactRecord {
            schema_version: 1,
            id: new_ulid(),
            author: "alice".to_string(),
            created_at: now_utc_rfc3339(),
            subject: "alpha marker".to_string(),
            text: "The alpha function contains a marker.".to_string(),
            tags: vec!["alpha".to_string()],
            state: FactState::Supported,
            evidence,
            source_review_id: None,
            source_record_ids: Vec::new(),
            supersedes: Vec::new(),
        }
    }

    fn current_key(repo: &Repo, path: &str) -> String {
        let file = repo.reader.read(path).unwrap().unwrap();
        provenance_key(&file.bytes, ObjectFormat::Sha1)
    }

    #[test]
    fn unchanged_evidence_is_current() {
        let repo = setup("stale-unchanged");
        let key = current_key(&repo, "src/a.rs");

        // Recorded provenance that matches the snapshot content.
        let with_key = fact(vec![source_evidence("src/a.rs", "marker", Some(key))]);
        assert_eq!(
            assess_fact(&with_key, &repo.reader, ObjectFormat::Sha1),
            FactFreshness::Current
        );

        // Evidence without a key stays current while its excerpt is
        // still verbatim present.
        let keyless = fact(vec![source_evidence("src/a.rs", "marker", None)]);
        assert_eq!(
            assess_fact(&keyless, &repo.reader, ObjectFormat::Sha1),
            FactFreshness::Current
        );

        // Multiple fresh entries stay current.
        write_file(&repo.fixture.root, "src/b.rs", "const B: u8 = 1;\n");
        let file = repo.reader.read("src/b.rs").unwrap().unwrap();
        let two = fact(vec![
            source_evidence("src/a.rs", "marker", Some(current_key(&repo, "src/a.rs"))),
            source_evidence(
                "src/b.rs",
                "const B",
                Some(provenance_key(&file.bytes, ObjectFormat::Sha1)),
            ),
        ]);
        assert_eq!(
            assess_fact(&two, &repo.reader, ObjectFormat::Sha1),
            FactFreshness::Current
        );
    }

    #[test]
    fn modified_content_is_potentially_stale() {
        let repo = setup("stale-modified");
        let key = current_key(&repo, "src/a.rs");
        let record = fact(vec![source_evidence("src/a.rs", "marker", Some(key))]);

        // The content changes: the recorded key no longer matches.
        write_file(
            &repo.fixture.root,
            "src/a.rs",
            "fn alpha() {\n    renamed\n}\n",
        );
        match assess_fact(&record, &repo.reader, ObjectFormat::Sha1) {
            FactFreshness::PotentiallyStale { reasons } => {
                assert!(
                    reasons.iter().any(|reason| reason.contains("changed")),
                    "{reasons:?}"
                );
            }
            other => panic!("expected staleness, got {other:?}"),
        }

        // Keyless evidence detects the same change through the excerpt.
        let keyless = fact(vec![source_evidence("src/a.rs", "marker", None)]);
        assert!(assess_fact(&keyless, &repo.reader, ObjectFormat::Sha1).is_stale());
    }

    #[test]
    fn deleted_content_is_potentially_stale() {
        let repo = setup("stale-deleted");
        let key = current_key(&repo, "src/a.rs");
        let record = fact(vec![source_evidence("src/a.rs", "marker", Some(key))]);

        std::fs::remove_file(repo.fixture.root.join("src/a.rs")).unwrap();
        match assess_fact(&record, &repo.reader, ObjectFormat::Sha1) {
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

    #[test]
    fn one_stale_entry_suffices() {
        let repo = setup("stale-any");
        let key = current_key(&repo, "src/a.rs");
        let record = fact(vec![
            source_evidence("src/a.rs", "marker", Some(key)),
            source_evidence("src/gone.rs", "nothing", None),
        ]);
        assert!(assess_fact(&record, &repo.reader, ObjectFormat::Sha1).is_stale());
    }

    #[test]
    fn non_source_evidence_never_makes_a_fact_stale() {
        let repo = setup("stale-nonsource");
        let mut entry = source_evidence("src/gone.rs", "nothing", None);
        entry.kind = EvidenceKind::Plan;
        entry.path = None;
        entry.plan_section = Some("Approach".to_string());
        let record = fact(vec![entry]);
        assert_eq!(
            assess_fact(&record, &repo.reader, ObjectFormat::Sha1),
            FactFreshness::Current
        );
    }

    #[test]
    fn assess_records_covers_active_supported_facts_only() {
        let repo = setup("stale-report");
        let store = RecordStore::new(&repo.fixture.root);
        let evidence = || {
            vec![source_evidence(
                "src/a.rs",
                "marker",
                Some(current_key(&repo, "src/a.rs")),
            )]
        };

        // A superseded fact and its active successor.
        let older = store
            .create(&ProjectRecord::Fact(fact(evidence())))
            .unwrap();
        let mut newer_record = fact(evidence());
        newer_record.supersedes = vec![older.id().to_string()];
        let newer = store.create(&ProjectRecord::Fact(newer_record)).unwrap();

        // A conflicting fact (already not normal truth) and a lesson.
        let mut conflicting = fact(vec![source_evidence("src/missing.rs", "x", None)]);
        conflicting.state = FactState::UnresolvedConflict;
        store.create(&ProjectRecord::Fact(conflicting)).unwrap();
        store
            .create(&ProjectRecord::Lesson(
                crate::domain::records::testutil::lesson("alice"),
            ))
            .unwrap();

        // Unchanged content: nothing is stale.
        let records = store.load().unwrap();
        assert!(assess_records(&records, &repo.reader, ObjectFormat::Sha1).is_empty());

        // Modified content: only the active supported fact is stale.
        write_file(
            &repo.fixture.root,
            "src/a.rs",
            "fn alpha() {\n    other\n}\n",
        );
        let report = assess_records(&store.load().unwrap(), &repo.reader, ObjectFormat::Sha1);
        assert_eq!(report.ids(), BTreeSet::from([newer.id().to_string()]));
        assert!(report.reasons(newer.id()).is_some());
        assert_eq!(report.len(), 1);
    }

    #[test]
    fn stamping_records_deterministic_provenance() {
        let repo = setup("stale-stamp");
        let known: &'static BTreeSet<String> = Box::leak(Box::new(BTreeSet::new()));
        let validation = ValidationContext {
            reader: &repo.reader,
            object_format: ObjectFormat::Sha1,
            changed_paths: None,
            known_record_ids: known,
        };

        let keyless = source_evidence("src/a.rs", "marker", None);
        let stamped = stamp_content_provenance(&validation, std::slice::from_ref(&keyless));
        assert_eq!(
            stamped[0].content_key.as_deref(),
            Some(current_key(&repo, "src/a.rs").as_str())
        );
        // Stamping twice is stable.
        assert_eq!(stamp_content_provenance(&validation, &stamped), stamped);

        // A path outside the snapshot is left unchanged.
        let missing = source_evidence("src/gone.rs", "nothing", None);
        assert_eq!(
            stamp_content_provenance(&validation, std::slice::from_ref(&missing)),
            vec![missing]
        );

        // Non-source entries are carried over.
        let mut plan_entry = source_evidence("src/a.rs", "marker", None);
        plan_entry.kind = EvidenceKind::Plan;
        plan_entry.path = None;
        plan_entry.plan_section = Some("Approach".to_string());
        assert_eq!(
            stamp_content_provenance(&validation, &[plan_entry.clone()]),
            vec![plan_entry]
        );
    }

    #[test]
    fn refresh_rebuilds_evidence_against_current_content() {
        let repo = setup("stale-refresh");
        let old_key = current_key(&repo, "src/a.rs");
        let record = fact(vec![source_evidence("src/a.rs", "marker", Some(old_key))]);

        // Content moves on: the excerpt vanished and the file grew.
        write_file(
            &repo.fixture.root,
            "src/a.rs",
            "fn alpha() {\n    one\n    two\n}\n",
        );
        let refreshed = refresh_evidence(&repo.reader, ObjectFormat::Sha1, &record);
        assert!(
            refreshed.unavailable.is_empty(),
            "{:?}",
            refreshed.unavailable
        );
        assert_eq!(refreshed.evidence.len(), 1);
        let entry = &refreshed.evidence[0];
        assert_eq!(entry.path.as_deref(), Some("src/a.rs"));
        assert_eq!(
            entry.content_key.as_deref(),
            Some(current_key(&repo, "src/a.rs").as_str())
        );
        assert_eq!(entry.excerpt, "fn alpha() {");
        assert_eq!(entry.line_start, Some(1));
        assert_eq!(entry.line_end, Some(1));

        // A deleted path is reported, not guessed.
        std::fs::remove_file(repo.fixture.root.join("src/a.rs")).unwrap();
        let refreshed = refresh_evidence(&repo.reader, ObjectFormat::Sha1, &record);
        assert!(refreshed.evidence.is_empty());
        assert!(
            refreshed
                .unavailable
                .iter()
                .any(|note| note.contains("no longer part of the target snapshot")),
            "{:?}",
            refreshed.unavailable
        );
    }

    #[test]
    fn pending_revalidation_rows_roundtrip() {
        let entry = PendingFactRevalidation {
            schema_version: SCHEMA_VERSION,
            fact_id: new_ulid(),
            verdict: FactVerificationVerdict::Insufficient,
            reason: "revalidation could not decide (session_timeout: timed out)".to_string(),
            created_at: "2026-04-01T12:00:00.000Z".to_string(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"verdict\":\"insufficient\""));
        assert_eq!(
            serde_json::from_str::<PendingFactRevalidation>(&json).unwrap(),
            entry
        );
    }

    #[test]
    fn excerpt_helpers_clamp_and_bound() {
        assert_eq!(excerpt_of_lines("a\nb\nc", 1, 2), "a\nb");
        assert_eq!(excerpt_of_lines("a\nb\nc", 2, 99), "b\nc");
        assert_eq!(excerpt_of_lines("a\nb\nc", 9, 10), "");
        assert_eq!(excerpt_of_lines("a\nb\nc", 0, 1), "a");
        let long = "ä".repeat(100);
        assert_eq!(truncate_bytes(&long, 9).len(), 8, "char boundary");
        assert_eq!(truncate_bytes(&long, 10).len(), 10);
        assert_eq!(truncate_bytes("short", 10), "short");
    }

    /// Storage placement check used by the revalidation paths (the
    /// host-driven outcomes are covered by the integration tests).
    #[test]
    fn updated_records_use_the_people_location() {
        let dir = tempfile::tempdir().unwrap();
        let store = RecordStore::new(dir.path());
        let evidence = vec![source_evidence("src/a.rs", "marker", None)];
        let original = store
            .create(&ProjectRecord::Fact(fact(evidence.clone())))
            .unwrap();
        let mut updated = fact(evidence);
        updated.supersedes = vec![original.id().to_string()];
        updated.source_record_ids = vec![original.id().to_string()];
        let created = store.create(&ProjectRecord::Fact(updated)).unwrap();
        assert_eq!(
            created.location,
            RecordLocation::People {
                contributor: "alice".to_string()
            }
        );
        assert!(original.path.exists(), "the old record remains");
    }
}
