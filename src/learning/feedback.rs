//! Review feedback and durable observations (plan sections 49 and 50).
//!
//! A known outcome for a review finding (`fixed`, `accepted`, or
//! `dismissed`) creates an immutable [`ObservationRecord`] — the
//! permanent raw learning data lesson learning is later derived from.
//! Observations are never inferred from later source changes: only the
//! explicit `nisse feedback` submission records an outcome, and Rust
//! constructs every authoritative observation field from
//!
//! - the **stored review** (the local review event of the shared
//!   cache, plan section 57);
//! - the **stored finding** (the validated final finding of that
//!   review's result);
//! - the **explicit outcome**;
//! - the review's **snapshot descriptors**.
//!
//! LLMs construct no observation identity or statistics (plan section
//! 50). One observation is created per reviewer that produced the
//! finding (a cross-validated finding yields one observation per
//! juror), each carrying the single `reviewer_id` the frozen
//! observation schema requires.
//!
//! Semantics:
//!
//! - **Unknown outcomes remain unknown**: nothing is ever recorded
//!   implicitly (plan section 49).
//! - **Idempotent submission** (plan section 66): a submission whose
//!   outcome is already durably recorded — by any contributor, in any
//!   branch or worktree — succeeds without creating a second record.
//! - **Immutable outcomes**: once an outcome is recorded for a
//!   (review, finding) pair, a *different* outcome is a typed conflict;
//!   durable records are never rewritten.
//! - **Missing contributor identity**: feedback is noninteractive; the
//!   outcome remains pending in the local `pending_learning` cache
//!   (plan section 44) and the submission fails explicitly rather than
//!   guessing an author.
//!
//! The `review_feedback` row of the shared cache is local bookkeeping
//! only (plan section 57); durability and idempotency come from the
//! `.nisse` observation records themselves, which are independent of
//! SQLite (permanent invariant 4).
//!
//! Recording new observations also triggers one deterministic
//! lesson-learning maintenance pass (plan section 51; milestone 20):
//! when the new outcomes push a group over the frozen threshold, an
//! immutable lesson record is created immediately. Maintenance is
//! best-effort — it can never fail an otherwise-successful submission.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::SCHEMA_VERSION;
use crate::domain::finding::FinalFinding;
use crate::domain::ids::new_ulid;
use crate::domain::records::{
    FeedbackOutcome, MAX_LANGUAGES, MAX_OBSERVATION_PATHS, MAX_TAGS, ObservationRecord,
    ObservationType, ProjectRecord, is_valid_contributor_id,
};
use crate::domain::review::ReviewResult;
use crate::domain::time::now_utc_rfc3339;
use crate::git::RepoContext;
use crate::index::language::Language;
use crate::storage::StoragePaths;
use crate::storage::db::open_or_recover;
use crate::storage::records::{RecordStore, RecordStoreError};
use crate::storage::schema::DbKind;

/// `pending_learning` row kind of retained review feedback.
pub const PENDING_KIND: &str = "review_feedback";

/// Typed feedback failures.
#[derive(Debug, Error)]
pub enum FeedbackError {
    /// No stored review event carries this ID in the local review
    /// history. Feedback requires its review to be known locally.
    #[error(
        "no stored review `{review_id}` exists in the local review history; run a review first"
    )]
    UnknownReview {
        /// The requested review identifier.
        review_id: String,
    },

    /// The stored review has no finding with this identifier.
    #[error("review `{review_id}` has no finding `{finding_id}`")]
    UnknownFinding {
        /// The review identifier.
        review_id: String,
        /// The requested finding identifier.
        finding_id: String,
    },

    /// A different outcome was already durably recorded for this
    /// finding; durable observations are immutable.
    #[error(
        "finding `{finding_id}` of review `{review_id}` was already recorded as `{recorded}`; \
         durable outcomes are immutable and cannot change to `{requested}`"
    )]
    OutcomeConflict {
        /// The review identifier.
        review_id: String,
        /// The finding identifier.
        finding_id: String,
        /// The outcome recorded durably earlier.
        recorded: FeedbackOutcome,
        /// The outcome requested now.
        requested: FeedbackOutcome,
    },

    /// The stored finding lost its reviewer provenance. An observation
    /// never fabricates a reviewer (plan section 50).
    #[error(
        "stored finding `{finding_id}` of review `{review_id}` carries no reviewer provenance; \
         refusing to fabricate an observation reviewer"
    )]
    MissingReviewerProvenance {
        /// The review identifier.
        review_id: String,
        /// The finding identifier.
        finding_id: String,
    },

    /// No contributor identity is available. The outcome was retained
    /// as pending local state (plan section 44).
    #[error(
        "no contributor identity available; the outcome remains pending locally — configure \
         `[contributor] id` in the user config and resubmit"
    )]
    NoContributorIdentity,

    /// A regenerable cache database could not be read or written.
    #[error("local cache failure: {0}")]
    Cache(String),

    /// The durable record store failed.
    #[error(transparent)]
    Records(#[from] RecordStoreError),
}

/// The outcome of a feedback submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedbackRecording {
    /// New durable observations were created: one `(reviewer_id,
    /// record_id)` pair per reviewer that produced the finding, plus
    /// any routing lessons the repeated outcomes deterministically
    /// formed (plan section 51; milestone 20).
    Recorded {
        /// The created observations, in finding reviewer order.
        observations: Vec<(String, String)>,
        /// Lessons created by the maintenance pass that followed, in
        /// deterministic group order (empty when no threshold was
        /// met).
        lessons: Vec<crate::learning::lessons::LessonCreated>,
    },
    /// The same outcome was already durably recorded; the submission
    /// is an idempotent no-op.
    AlreadyRecorded(FeedbackOutcome),
    /// The durable observation writes were deferred to the local
    /// `pending_learning` cache because record-directory creation was
    /// refused by the consent policy (the caches existed; a fresh
    /// repository fails closed before reaching this state).
    DeferredConsent {
        /// How many observations are waiting for consent.
        count: usize,
    },
}

/// One retained feedback outcome awaiting a contributor identity
/// (never durable project knowledge; regenerable SQLite).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingReviewFeedback {
    /// Payload schema version; must be `1`.
    pub schema_version: u32,
    /// The review the outcome belongs to.
    pub review_id: String,
    /// The finding the outcome applies to.
    pub finding_id: String,
    /// The submitted outcome.
    pub outcome: FeedbackOutcome,
    /// Why the outcome was not recorded durably.
    pub reason: String,
    /// When the submission was retained (UTC RFC3339).
    pub created_at: String,
}

/// Load the stored result of one review from the local review history
/// (the shared cache's `review_event` table, plan section 57).
///
/// Returns `None` when no such review is known locally — an absent
/// cache database is simply an empty history.
pub fn load_review_result(
    paths: &StoragePaths,
    review_id: &str,
) -> Result<Option<ReviewResult>, FeedbackError> {
    if !paths.shared.is_file() {
        return Ok(None);
    }
    let cache = |message: String| FeedbackError::Cache(message);
    let db = open_or_recover(&paths.shared, DbKind::Shared)
        .map_err(|error| cache(format!("{}: {error}", error)))?;
    let payload: Option<String> = db
        .conn
        .query_row(
            "SELECT payload FROM review_event WHERE review_id = ?1",
            [review_id],
            |row| row.get(0),
        )
        .map(Some)
        .or_else(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })
        .map_err(|error| cache(error.to_string()))?;
    let Some(payload) = payload else {
        return Ok(None);
    };
    let value: serde_json::Value = serde_json::from_str(&payload).map_err(|error| {
        cache(format!(
            "review event `{review_id}` payload is not valid JSON: {error}"
        ))
    })?;
    let result: ReviewResult = serde_json::from_value(
        value.get("result").cloned().unwrap_or_default(),
    )
    .map_err(|error| {
        cache(format!(
            "stored review `{review_id}` does not match the review schema: {error}"
        ))
    })?;
    if result.review_id != review_id {
        return Err(cache(format!(
            "stored review event `{review_id}` carries result id `{}`",
            result.review_id
        )));
    }
    Ok(Some(result))
}

/// Distinct evidence paths of a finding, in evidence order, bounded to
/// the frozen observation path limit.
pub fn finding_paths(finding: &FinalFinding) -> Vec<String> {
    let mut seen = BTreeSet::new();
    finding
        .evidence
        .iter()
        .filter_map(|evidence| evidence.path.as_deref())
        .filter(|path| !path.is_empty())
        .filter(|path| seen.insert((*path).to_string()))
        .take(MAX_OBSERVATION_PATHS)
        .map(str::to_string)
        .collect()
}

/// Distinct index languages of the given paths, sorted, bounded to the
/// frozen observation language limit.
pub fn finding_languages(paths: &[String]) -> Vec<String> {
    let mut languages: BTreeSet<&'static str> = BTreeSet::new();
    for path in paths {
        if let Some(language) = Language::from_path(path) {
            languages.insert(language.label());
            if languages.len() >= MAX_LANGUAGES {
                break;
            }
        }
    }
    languages.into_iter().map(str::to_string).collect()
}

/// Deterministic topic tags of a finding: its impact class and
/// category, deduplicated, bounded.
pub fn finding_tags(finding: &FinalFinding) -> Vec<String> {
    let impact = serde_json::to_string(&finding.impact_class)
        .unwrap_or_else(|_| "\"other\"".to_string())
        .trim_matches('"')
        .to_string();
    let mut tags: Vec<String> = Vec::new();
    for tag in [impact, finding.category.clone()] {
        if !tag.is_empty() && !tags.contains(&tag) && tags.len() < MAX_TAGS {
            tags.push(tag);
        }
    }
    tags
}

/// Construct the immutable observation records of one known outcome
/// (plan section 50): every authoritative field comes from the stored
/// review/finding, the explicit outcome, and nothing else. One
/// observation per reviewer that produced the finding.
pub fn build_observations(
    result: &ReviewResult,
    finding: &FinalFinding,
    outcome: FeedbackOutcome,
    author: &str,
) -> Result<Vec<ObservationRecord>, FeedbackError> {
    if finding.reviewer_ids.is_empty() {
        return Err(FeedbackError::MissingReviewerProvenance {
            review_id: result.review_id.clone(),
            finding_id: finding.id.clone(),
        });
    }
    let paths = finding_paths(finding);
    let languages = finding_languages(&paths);
    let tags = finding_tags(finding);
    let created_at = now_utc_rfc3339();
    Ok(finding
        .reviewer_ids
        .iter()
        .map(|reviewer_id| ObservationRecord {
            schema_version: SCHEMA_VERSION,
            id: new_ulid(),
            author: author.to_string(),
            created_at: created_at.clone(),
            observation_type: ObservationType::ReviewOutcome,
            review_id: result.review_id.clone(),
            finding_id: finding.id.clone(),
            reviewer_id: reviewer_id.clone(),
            outcome,
            impact_class: finding.impact_class,
            category: finding.category.clone(),
            languages: languages.clone(),
            tags: tags.clone(),
            paths: paths.clone(),
            snapshot: result.snapshot.clone(),
        })
        .collect())
}

/// Record a known outcome for one finding of a stored review (plan
/// sections 49–50).
///
/// The durable `.nisse` observation records are the authority for
/// idempotency and immutability; the local `review_feedback` row is
/// best-effort bookkeeping that never fails an otherwise-successful
/// submission.
pub fn record_feedback(
    ctx: &RepoContext,
    paths: &StoragePaths,
    review_id: &str,
    finding_id: &str,
    outcome: FeedbackOutcome,
    contributor_id: Option<&str>,
) -> Result<FeedbackRecording, FeedbackError> {
    // The stored review and stored finding are the only sources of
    // observation metadata (plan section 50).
    let Some(result) = load_review_result(paths, review_id)? else {
        return Err(FeedbackError::UnknownReview {
            review_id: review_id.to_string(),
        });
    };
    let Some(finding) = result.findings.iter().find(|f| f.id == finding_id) else {
        return Err(FeedbackError::UnknownFinding {
            review_id: review_id.to_string(),
            finding_id: finding_id.to_string(),
        });
    };

    // Durable idempotency/immutability: any existing observation of
    // this (review, finding) pair decides the submission, regardless
    // of which contributor, branch, or worktree recorded it.
    let store = RecordStore::new(&ctx.worktree_root);
    // Replay any observations an earlier unconsented run deferred (the
    // caches exist at this point; failures are warnings only). Replaying
    // before the scan below makes resubmission idempotent.
    let _flushed = flush_deferred_observations(&store, paths);
    let records = store.load()?;
    for stored in &records {
        let ProjectRecord::Observation(observation) = &stored.record else {
            continue;
        };
        if observation.review_id == review_id && observation.finding_id == finding_id {
            if observation.outcome == outcome {
                record_feedback_row(paths, review_id, finding_id, outcome);
                return Ok(FeedbackRecording::AlreadyRecorded(outcome));
            }
            return Err(FeedbackError::OutcomeConflict {
                review_id: review_id.to_string(),
                finding_id: finding_id.to_string(),
                recorded: observation.outcome,
                requested: outcome,
            });
        }
    }

    // Feedback is noninteractive: without a contributor identity the
    // outcome remains pending locally (plan section 44) and the
    // submission fails explicitly rather than guessing an author.
    let Some(author) = contributor_id.filter(|id| is_valid_contributor_id(id)) else {
        let entry = PendingReviewFeedback {
            schema_version: SCHEMA_VERSION,
            review_id: review_id.to_string(),
            finding_id: finding_id.to_string(),
            outcome,
            reason: "no contributor identity available (plan section 44)".to_string(),
            created_at: now_utc_rfc3339(),
        };
        store_pending_feedback(paths, &entry)?;
        return Err(FeedbackError::NoContributorIdentity);
    };

    // Immutable create of one observation per contributing reviewer.
    let observations = build_observations(&result, finding, outcome, author)?;
    let mut created = Vec::new();
    let mut deferred = 0usize;
    for observation in observations {
        match store.create(&ProjectRecord::Observation(observation.clone())) {
            Ok(stored) => {
                created.push((observation.reviewer_id.clone(), stored.id().to_string()));
            }
            // CONSENT GATE deferral: the local caches already exist (the
            // stored review was read above), so a refused record-tree
            // directory defers durably instead of failing the command;
            // the next run with consent flushes it.
            Err(RecordStoreError::DirectoryConsentRequired { path }) => {
                let reason = crate::config::consent::consent_error_text(&path);
                store_deferred_observation(paths, &observation, &reason)?;
                deferred += 1;
            }
            Err(other) => return Err(other.into()),
        }
    }
    if deferred > 0 {
        if created.is_empty() {
            return Ok(FeedbackRecording::DeferredConsent { count: deferred });
        }
        // Mixed outcome: some observations were recorded, others could
        // not be. Never report a clean recording while part of the
        // outcome waits on consent.
        eprintln!(
            "nisse: {deferred} observation(s) deferred pending directory consent; \
             see `nisse records pending`"
        );
    }
    record_feedback_row(paths, review_id, finding_id, outcome);

    // Repeated outcomes deterministically form routing lessons (plan
    // section 51). The pass reloads the records (including the new
    // observations) and is idempotent; its failures are warnings only.
    let lessons = crate::learning::lessons::maintain_after_feedback(ctx, paths, author);
    Ok(FeedbackRecording::Recorded {
        observations: created,
        lessons,
    })
}

/// Best-effort local bookkeeping row (plan section 57). Failures only
/// warn: the durable observations already decide the outcome.
fn record_feedback_row(
    paths: &StoragePaths,
    review_id: &str,
    finding_id: &str,
    outcome: FeedbackOutcome,
) {
    let result = (|| -> Result<(), String> {
        let db = open_or_recover(&paths.shared, DbKind::Shared)
            .map_err(|error| format!("{}: {error}", error))?;
        db.conn
            .execute(
                "INSERT OR IGNORE INTO review_feedback (review_id, finding_id, outcome, created_at) \
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    review_id,
                    finding_id,
                    outcome.as_str(),
                    now_utc_rfc3339()
                ],
            )
            .map(|_| ())
            .map_err(|error| error.to_string())
    })();
    if let Err(error) = result {
        tracing::warn!(
            review_id,
            finding_id,
            "failed to record the local review_feedback row: {error}"
        );
    }
}

/// `pending_learning`-style row kind of a consent-deferred observation
/// (rendered by `records pending`).
pub const DEFERRED_KIND: &str = "deferred_observation";

/// The consent-deferral sidecar: an append-only JSONL file NEXT TO the
/// shared cache database, deliberately outside it — the cache is
/// wipe-on-corruption ("regenerable"), but while consent is refused the
/// deferral is the only copy of the outcome and must survive cache
/// recovery.
fn deferred_observations_path(paths: &StoragePaths) -> std::path::PathBuf {
    paths
        .shared
        .parent()
        .map(|dir| dir.join("deferred-observations.jsonl"))
        .unwrap_or_else(|| paths.shared.clone())
}

/// The idempotency key of one deferred observation: the same
/// (review, finding, reviewer, outcome) may be deferred repeatedly and
/// must yield exactly one durable observation.
fn deferred_key(observation: &crate::domain::records::ObservationRecord) -> String {
    format!(
        "{}|{}|{}|{}",
        observation.review_id, observation.finding_id, observation.reviewer_id, observation.outcome
    )
}

/// Defer one observation durably (see [`deferred_observations_path`]).
/// Idempotent per [`deferred_key`]: a repeated unconsented submission
/// never appends a second copy.
/// The sidecar payload format version.
const DEFERRED_FORMAT_VERSION: u32 = 1;

fn store_deferred_observation(
    paths: &StoragePaths,
    observation: &crate::domain::records::ObservationRecord,
    reason: &str,
) -> Result<(), FeedbackError> {
    let sidecar = deferred_observations_path(paths);
    let key = deferred_key(observation);
    let lock = crate::config::consent::AdvisoryLock::acquire(&sidecar.with_extension("jsonl.lock"))
        .map_err(|error| FeedbackError::Cache(error.to_string()))?;
    let result = (|| {
        let existing = std::fs::read_to_string(&sidecar).unwrap_or_default();
        for line in existing.lines().filter(|line| !line.trim().is_empty()) {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(line)
                && parsed
                    .get("observation")
                    .cloned()
                    .and_then(|value| {
                        serde_json::from_value::<crate::domain::records::ObservationRecord>(value)
                            .ok()
                    })
                    .is_some_and(|obs| deferred_key(&obs) == key)
            {
                return Ok(()); // already deferred
            }
        }
        let payload = serde_json::json!({
            "v": DEFERRED_FORMAT_VERSION,
            "observation": observation,
            "reason": reason,
        });
        let line = serde_json::to_string(&payload)
            .map_err(|error| FeedbackError::Cache(error.to_string()))?;
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&sidecar)
            .map_err(|error| FeedbackError::Cache(format!("{}: {error}", sidecar.display())))?;
        writeln!(file, "{line}").map_err(|error| FeedbackError::Cache(error.to_string()))?;
        // The deferral is the only copy of the outcome while consent is
        // refused: fsync before claiming it is retained (file data plus
        // the parent directory entry, which sync_data alone does not
        // cover for a newly created file).
        file.sync_data()
            .map_err(|error| FeedbackError::Cache(error.to_string()))?;
        drop(file);
        if let Some(parent) = sidecar.parent() {
            crate::storage::fsutil::best_effort_parent_sync(parent);
        }
        Ok(())
    })();
    lock.release();
    result
}

/// Flush consent-deferred observations into the durable record store.
///
/// Called before recording new feedback: rows deferred by an earlier
/// unconsented run are replayed (and removed from the sidecar) once
/// creation is allowed. Rows whose key is already durably recorded are
/// dropped; unparseable lines are dropped with a warning. Failures of
/// the durable create leave the row in place for a later run.
pub fn flush_deferred_observations(store: &RecordStore, paths: &StoragePaths) -> usize {
    let sidecar = deferred_observations_path(paths);
    let Ok(_guard) =
        crate::config::consent::AdvisoryLock::acquire(&sidecar.with_extension("jsonl.lock"))
    else {
        return 0;
    };
    let Ok(text) = std::fs::read_to_string(&sidecar) else {
        return 0;
    };
    let recorded: std::collections::BTreeSet<String> = store
        .load()
        .map(|records| {
            records
                .iter()
                .filter_map(|stored| match &stored.record {
                    ProjectRecord::Observation(observation) => Some(deferred_key(observation)),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    let mut flushed = 0;
    let mut remaining = String::new();
    let mut recorded = recorded; // extended after each successful create
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let parsed = serde_json::from_str::<serde_json::Value>(line).ok();
        // A parseable line of a FOREIGN version stays in the sidecar
        // untouched: a later compatible binary may still flush it.
        let foreign_version = parsed.as_ref().is_some_and(|parsed| {
            parsed.get("v").and_then(serde_json::Value::as_u64)
                != Some(DEFERRED_FORMAT_VERSION as u64)
        });
        if foreign_version {
            remaining.push_str(line);
            remaining.push('\n');
            continue;
        }
        let observation = parsed
            .and_then(|parsed| parsed.get("observation").cloned())
            .and_then(|value| {
                serde_json::from_value::<crate::domain::records::ObservationRecord>(value).ok()
            });
        let Some(observation) = observation else {
            // Structurally unparseable lines are quarantined (kept for
            // inspection), never silently destroyed.
            quarantine_deferred_line(&sidecar, line);
            continue;
        };
        if recorded.contains(&deferred_key(&observation)) {
            continue; // already durable; drop the line
        }
        let key = deferred_key(&observation);
        if store
            .create(&ProjectRecord::Observation(observation))
            .is_ok()
        {
            flushed += 1;
            // A second sidecar line with the same key must not produce a
            // second durable observation.
            recorded.insert(key);
        } else {
            remaining.push_str(line);
            remaining.push('\n');
        }
    }
    // Publish atomically, and only when something actually changed: the
    // sidecar is the only copy of the remaining outcomes, so it must
    // never appear partially written (and an unchanged rewrite under a
    // still-refused consent would churn the file on every submission).
    if remaining != text {
        let _ = crate::storage::fsutil::atomic_write_replace(&sidecar, remaining.as_bytes());
    }
    flushed
}

/// Append one unusable sidecar line to the quarantine file next to the
/// sidecar instead of destroying it.
fn quarantine_deferred_line(sidecar: &std::path::Path, line: &str) {
    tracing::warn!("quarantining unparseable deferred-observation line");
    let quarantine = sidecar.with_extension("corrupt.jsonl");
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(quarantine)
    {
        let _ = writeln!(file, "{line}");
    }
}

/// Retain one feedback outcome in the local `pending_learning` cache.
pub fn store_pending_feedback(
    paths: &StoragePaths,
    entry: &PendingReviewFeedback,
) -> Result<(), FeedbackError> {
    let db = open_or_recover(&paths.shared, DbKind::Shared)
        .map_err(|error| FeedbackError::Cache(format!("{}: {error}", error)))?;
    let payload =
        serde_json::to_string(entry).map_err(|error| FeedbackError::Cache(error.to_string()))?;
    db.conn
        .execute(
            "INSERT INTO pending_learning (kind, payload, created_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![PENDING_KIND, payload, entry.created_at],
        )
        .map(|_| ())
        .map_err(|error| FeedbackError::Cache(error.to_string()))
}

/// Render the human-readable feedback confirmation.
pub fn render_feedback(
    review_id: &str,
    finding_id: &str,
    outcome: FeedbackOutcome,
    recording: &FeedbackRecording,
) -> String {
    let mut out = String::from("nisse feedback\n");
    out.push_str(&format!("review: {review_id}\n"));
    out.push_str(&format!("finding: {finding_id}\n"));
    out.push_str(&format!("outcome: {outcome}\n"));
    match recording {
        FeedbackRecording::Recorded {
            observations,
            lessons,
        } => {
            out.push_str(&format!("recorded observations: {}\n", observations.len()));
            for (reviewer_id, record_id) in observations {
                out.push_str(&format!("  {record_id} reviewer={reviewer_id}\n"));
            }
            out.push_str(&format!("lessons created: {}\n", lessons.len()));
            for lesson in lessons {
                out.push_str(&format!(
                    "  {} {}",
                    lesson.record_id,
                    crate::learning::lessons::group_label(&lesson.reviewer_id, &lesson.category)
                ));
                if !lesson.superseded.is_empty() {
                    out.push_str(&format!(" (supersedes {})", lesson.superseded.len()));
                }
                out.push('\n');
            }
        }
        FeedbackRecording::DeferredConsent { count } => {
            out.push_str(&format!(
                "observations pending directory consent: {count}\n"
            ));
            out.push_str(
                "(the outcomes were retained locally and will be recorded once \
             directory creation is allowed; see `nisse records pending`)\n",
            );
        }
        FeedbackRecording::AlreadyRecorded(recorded) => {
            out.push_str(&format!("already recorded: {recorded}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sidecar helpers need no repository: StoragePaths over a tempdir.
    fn sidecar_paths(dir: &std::path::Path) -> StoragePaths {
        let nisse = dir.join("nisse");
        std::fs::create_dir_all(&nisse).unwrap();
        StoragePaths {
            shared: nisse.join("cache.sqlite"),
            worktree: nisse.join("worktree.sqlite"),
        }
    }

    fn sample_observation(_id_seed: &str) -> crate::domain::records::ObservationRecord {
        crate::domain::records::ObservationRecord {
            schema_version: SCHEMA_VERSION,
            id: new_ulid(),
            author: "alice".to_string(),
            created_at: now_utc_rfc3339(),
            observation_type: crate::domain::records::ObservationType::ReviewOutcome,
            review_id: new_ulid(),
            finding_id: new_ulid(),
            reviewer_id: "quality".to_string(),
            outcome: FeedbackOutcome::Fixed,
            impact_class: crate::domain::review::ImpactClass::Correctness,
            category: "test".to_string(),
            languages: vec![],
            tags: vec![],
            paths: vec![],
            snapshot: crate::domain::snapshot::SnapshotDescriptor {
                mode: crate::domain::snapshot::SnapshotMode::Working,
                base_commit: None,
                base_tree: None,
                head_commit: None,
                head_tree: None,
            },
        }
    }

    #[test]
    fn deferred_sidecar_is_idempotent_quarantines_and_retains() {
        // Explicit consent (independent of ambient cargo [env]).
        let previous = std::env::var("NISSE_ALLOW_DIR_CREATION").ok();
        unsafe { std::env::set_var("NISSE_ALLOW_DIR_CREATION", "1") };
        let dir = tempfile::tempdir().unwrap();
        let paths = sidecar_paths(dir.path());

        // Repeated deferrals of the same key append exactly one line.
        let observation = sample_observation("1");
        store_deferred_observation(&paths, &observation, "reason a").unwrap();
        store_deferred_observation(&paths, &observation, "reason b").unwrap();
        let sidecar = deferred_observations_path(&paths);
        let text = std::fs::read_to_string(&sidecar).unwrap();
        assert_eq!(text.lines().count(), 1, "{text}");

        // A corrupt line plus a valid one: flush quarantines the corrupt
        // line, records the valid one, and keeps nothing stale.
        let valid = sample_observation("2");
        store_deferred_observation(&paths, &valid, "reason c").unwrap();
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&sidecar)
                .unwrap();
            writeln!(file, "{{not json").unwrap();
        }
        let store = RecordStore::new(dir.path());
        let flushed = flush_deferred_observations(&store, &paths);
        assert!(flushed >= 1);
        let early_keys: Vec<String> = store
            .load()
            .unwrap()
            .iter()
            .filter_map(|stored| match &stored.record {
                ProjectRecord::Observation(observation) => Some(deferred_key(observation)),
                _ => None,
            })
            .collect();
        assert!(
            early_keys.contains(&deferred_key(&observation)),
            "{early_keys:?}"
        );
        assert!(early_keys.contains(&deferred_key(&valid)), "{early_keys:?}");
        let remaining = std::fs::read_to_string(&sidecar).unwrap();
        assert!(!remaining.contains("find-1"), "{remaining}");
        assert!(!remaining.contains("find-2"), "{remaining}");
        let quarantine = dir
            .path()
            .join("nisse")
            .join("deferred-observations.corrupt.jsonl");
        assert!(
            std::fs::read_to_string(&quarantine)
                .unwrap()
                .contains("{not json"),
            "corrupt line must be quarantined, not destroyed"
        );

        // Retention on failed create: with the record root blocked by a
        // regular file where `.nisse/people/<author>` belongs, the
        // deferral survives the flush and flushes on a later run.
        let blocked = sample_observation("3");
        store_deferred_observation(&paths, &blocked, "blocked").unwrap();
        let people = dir.path().join(".nisse").join("people");
        std::fs::remove_dir_all(&people).unwrap();
        std::fs::create_dir_all(people.parent().unwrap()).unwrap();
        std::fs::write(&people, "not a directory").unwrap();
        let flushed_blocked = flush_deferred_observations(&store, &paths);
        assert_eq!(flushed_blocked, 0, "create must fail while blocked");
        let retained = std::fs::read_to_string(&sidecar).unwrap();
        assert!(retained.contains(&blocked.finding_id), "{retained}");
        std::fs::remove_file(&people).unwrap();
        let flushed_retry = flush_deferred_observations(&store, &paths);
        assert!(flushed_retry >= 1);
        assert!(
            !std::fs::read_to_string(&sidecar)
                .unwrap()
                .contains(&blocked.finding_id),
            "the blocked deferral must flush once unblocked"
        );

        // A parseable line of a foreign version stays in the sidecar
        // (a later compatible binary may flush it); it is not
        // quarantined.
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&sidecar)
                .unwrap();
            writeln!(
                file,
                r#"{{"v": 99, "observation": {{}}, "reason": "future"}}"#
            )
            .unwrap();
        }
        flush_deferred_observations(&store, &paths);
        assert!(
            std::fs::read_to_string(&sidecar)
                .unwrap()
                .contains(r#""v": 99"#),
            "foreign-version lines must remain in place"
        );

        // The blocked deferral is durable too.
        let late_keys: Vec<String> = store
            .load()
            .unwrap()
            .iter()
            .filter_map(|stored| match &stored.record {
                ProjectRecord::Observation(observation) => Some(deferred_key(observation)),
                _ => None,
            })
            .collect();
        assert!(late_keys.contains(&deferred_key(&blocked)), "{late_keys:?}");
        match previous {
            Some(value) => unsafe { std::env::set_var("NISSE_ALLOW_DIR_CREATION", value) },
            None => unsafe { std::env::remove_var("NISSE_ALLOW_DIR_CREATION") },
        }
    }

    use crate::domain::evidence::{EvidenceKind, EvidenceRef};
    use crate::domain::ids::is_valid_ulid;
    use crate::domain::review::ImpactClass;
    use crate::git::testutil::{Fixture, commit_all, write_file};
    use crate::git::{GitRunner, RepoContext, SnapshotSpec, resolve_snapshot};
    use crate::storage::db::open_or_recover as open_db;

    /// A fixture repository whose shared cache holds one review event
    /// with one two-reviewer finding that cites two indexed paths.
    struct Setup {
        _fixture: Fixture,
        ctx: RepoContext,
        paths: StoragePaths,
        result: ReviewResult,
    }

    fn setup(label: &str) -> Setup {
        let fixture = Fixture::repo(label);
        write_file(&fixture.root, "src/a.rs", "fn a() {}\n");
        write_file(&fixture.root, "src/b.py", "def b():\n    pass\n");
        commit_all(&fixture.root, "one");
        let ctx = RepoContext::discover(&GitRunner::new(), &fixture.root).unwrap();
        let paths = StoragePaths::for_repo(&ctx);

        let resolved = resolve_snapshot(&ctx, &SnapshotSpec::Working).unwrap();
        let finding = FinalFinding {
            id: new_ulid(),
            impact_class: ImpactClass::Correctness,
            severity: crate::domain::review::Severity::High,
            category: "race".to_string(),
            title: "Queue race".to_string(),
            claim: "Two contexts mutate the queue.".to_string(),
            explanation: "ISR writes during reads.".to_string(),
            recommendation: "Guard access.".to_string(),
            evidence: vec![
                source_evidence("src/a.rs", 1, 2),
                source_evidence("src/b.py", 3, 4),
                source_evidence("src/a.rs", 1, 2),
                source_evidence("notes.txt", 5, 6),
            ],
            source_finding_ids: vec!["quality/fake-1".to_string()],
            reviewer_ids: vec!["quality".to_string(), "concurrency".to_string()],
            cross_validated: true,
        };
        let result = ReviewResult {
            review_id: new_ulid(),
            kind: crate::domain::review::ReviewKind::Code,
            created_at: now_utc_rfc3339(),
            snapshot: resolved.descriptor.clone(),
            summary: "One finding.".to_string(),
            findings: vec![finding],
        };

        let db = open_db(&paths.shared, DbKind::Shared).unwrap();
        db.conn
            .execute(
                "INSERT INTO review_event (review_id, kind, snapshot, payload, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    result.review_id,
                    "code",
                    serde_json::to_string(&result.snapshot).unwrap(),
                    serde_json::to_string(&serde_json::json!({ "result": &result })).unwrap(),
                    result.created_at,
                ],
            )
            .unwrap();

        Setup {
            _fixture: fixture,
            ctx,
            paths,
            result,
        }
    }

    fn source_evidence(path: &str, start: u32, end: u32) -> EvidenceRef {
        EvidenceRef {
            kind: EvidenceKind::Source,
            path: Some(path.to_string()),
            symbol: None,
            line_start: Some(start),
            line_end: Some(end),
            content_key: None,
            plan_section: None,
            record_id: None,
            input_part: None,
            excerpt: "content".to_string(),
        }
    }

    fn submit(
        setup: &Setup,
        outcome: FeedbackOutcome,
        contributor: Option<&str>,
    ) -> Result<FeedbackRecording, FeedbackError> {
        record_feedback(
            &setup.ctx,
            &setup.paths,
            &setup.result.review_id,
            &setup.result.findings[0].id,
            outcome,
            contributor,
        )
    }

    fn observation_records(setup: &Setup) -> Vec<ObservationRecord> {
        RecordStore::new(&setup.ctx.worktree_root)
            .load()
            .unwrap()
            .into_iter()
            .filter_map(|stored| match stored.record {
                ProjectRecord::Observation(observation) => Some(observation),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn load_review_result_finds_absent_and_known_reviews() {
        let setup = setup("feedback-load");
        let loaded = load_review_result(&setup.paths, &setup.result.review_id).unwrap();
        assert_eq!(loaded.as_ref(), Some(&setup.result));

        assert_eq!(
            load_review_result(&setup.paths, "01AAAAAAAAAAAAAAAAAAAAAAAAAAAAA0").unwrap(),
            None
        );

        // A missing cache database is an empty history.
        let empty = Fixture::repo("feedback-load-empty");
        write_file(&empty.root, "a.txt", "a\n");
        commit_all(&empty.root, "one");
        let empty_ctx = RepoContext::discover(&GitRunner::new(), &empty.root).unwrap();
        let empty_paths = StoragePaths::for_repo(&empty_ctx);
        assert_eq!(
            load_review_result(&empty_paths, "01AAAAAAAAAAAAAAAAAAAAAAAAAAAAA0").unwrap(),
            None
        );
    }

    #[test]
    fn load_review_result_rejects_malformed_events() {
        let setup = setup("feedback-load-malformed");
        let db = open_db(&setup.paths.shared, DbKind::Shared).unwrap();
        db.conn
            .execute(
                "INSERT INTO review_event (review_id, kind, snapshot, payload, created_at) \
                 VALUES ('01BBBBBBBBBBBBBBBBBBBBBBBBBBBBB0', 'code', '{}', \
                 '{\"result\": {\"broken\": true}}', '2026-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
        let error =
            load_review_result(&setup.paths, "01BBBBBBBBBBBBBBBBBBBBBBBBBBBBB0").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not match the review schema"),
            "{error}"
        );
    }

    #[test]
    fn observation_metadata_is_derived_deterministically() {
        let setup = setup("feedback-derive");
        let finding = &setup.result.findings[0];

        // Paths: distinct evidence paths in order, bounded; languages:
        // the index languages of those paths, sorted.
        assert_eq!(
            finding_paths(finding),
            vec![
                "src/a.rs".to_string(),
                "src/b.py".to_string(),
                "notes.txt".to_string(),
            ]
        );
        assert_eq!(
            finding_languages(&finding_paths(finding)),
            vec!["python".to_string(), "rust".to_string()]
        );
        assert_eq!(
            finding_tags(finding),
            vec!["correctness".to_string(), "race".to_string()]
        );

        let observations =
            build_observations(&setup.result, finding, FeedbackOutcome::Fixed, "alice").unwrap();
        assert_eq!(observations.len(), 2, "one per contributing reviewer");
        for (observation, reviewer) in observations.iter().zip(["quality", "concurrency"]) {
            assert_eq!(observation.reviewer_id, reviewer);
            assert_eq!(observation.observation_type.as_str(), "review_outcome");
            assert_eq!(observation.outcome, FeedbackOutcome::Fixed);
            assert_eq!(observation.review_id, setup.result.review_id);
            assert_eq!(observation.finding_id, finding.id);
            assert_eq!(observation.impact_class, ImpactClass::Correctness);
            assert_eq!(observation.category, "race");
            assert_eq!(observation.languages, vec!["python", "rust"]);
            assert_eq!(observation.tags, vec!["correctness", "race"]);
            assert_eq!(observation.paths, finding_paths(finding));
            assert_eq!(observation.snapshot, setup.result.snapshot);
            assert_eq!(observation.author, "alice");
            assert!(is_valid_ulid(&observation.id));
            // Every derived record satisfies the frozen schema.
            ProjectRecord::Observation(observation.clone())
                .validate()
                .unwrap();
        }
        // Distinct records per reviewer.
        assert_ne!(observations[0].id, observations[1].id);
    }

    #[test]
    fn finding_without_reviewer_provenance_is_rejected() {
        let setup = setup("feedback-no-provenance");
        let mut finding = setup.result.findings[0].clone();
        finding.reviewer_ids = Vec::new();
        let error = build_observations(&setup.result, &finding, FeedbackOutcome::Fixed, "alice")
            .unwrap_err();
        assert!(
            error.to_string().contains("no reviewer provenance"),
            "{error}"
        );
    }

    #[test]
    fn record_feedback_creates_one_observation_per_reviewer() {
        let setup = setup("feedback-record");
        let recording = submit(&setup, FeedbackOutcome::Fixed, Some("alice")).unwrap();
        let FeedbackRecording::Recorded { observations, .. } = recording else {
            panic!("expected new observations");
        };
        // One observation per contributing reviewer, in reviewer order.
        let reviewers: Vec<&str> = observations
            .iter()
            .map(|(reviewer, _)| reviewer.as_str())
            .collect();
        assert_eq!(reviewers, vec!["quality", "concurrency"]);

        let stored = observation_records(&setup);
        assert_eq!(stored.len(), 2);
        assert!(
            stored
                .iter()
                .all(|observation| observation.outcome == FeedbackOutcome::Fixed)
        );
        // Every returned record id is the stored observation of its
        // reviewer (loading sorts by file name, so compare as a set).
        let stored_by_reviewer: std::collections::BTreeSet<(String, String)> = stored
            .iter()
            .map(|observation| (observation.reviewer_id.clone(), observation.id.clone()))
            .collect();
        for (reviewer, id) in &observations {
            assert!(stored_by_reviewer.contains(&(reviewer.clone(), id.clone())));
        }

        // The local bookkeeping row exists exactly once.
        let db = open_db(&setup.paths.shared, DbKind::Shared).unwrap();
        let count: i64 = db
            .conn
            .query_row(
                "SELECT count(*) FROM review_feedback WHERE review_id = ?1 AND finding_id = ?2",
                rusqlite::params![setup.result.review_id, setup.result.findings[0].id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn unknown_review_and_unknown_finding_fail_with_typed_errors() {
        let setup = setup("feedback-unknown");

        let error = record_feedback(
            &setup.ctx,
            &setup.paths,
            "01CCCCCCCCCCCCCCCCCCCCCCCCCCCCC0",
            &setup.result.findings[0].id,
            FeedbackOutcome::Fixed,
            Some("alice"),
        )
        .unwrap_err();
        assert!(
            matches!(error, FeedbackError::UnknownReview { .. }),
            "{error}"
        );

        let error = record_feedback(
            &setup.ctx,
            &setup.paths,
            &setup.result.review_id,
            "no-such-finding",
            FeedbackOutcome::Fixed,
            Some("alice"),
        )
        .unwrap_err();
        assert!(
            matches!(error, FeedbackError::UnknownFinding { .. }),
            "{error}"
        );
        assert!(observation_records(&setup).is_empty());
    }

    #[test]
    fn repeated_submission_is_idempotent_and_outcomes_are_immutable() {
        let setup = setup("feedback-idempotent");

        submit(&setup, FeedbackOutcome::Accepted, Some("alice")).unwrap();
        assert_eq!(observation_records(&setup).len(), 2);

        // The same submission again — by another contributor even — is
        // an idempotent no-op that creates nothing.
        let recording = submit(&setup, FeedbackOutcome::Accepted, Some("bob")).unwrap();
        assert_eq!(
            recording,
            FeedbackRecording::AlreadyRecorded(FeedbackOutcome::Accepted)
        );
        assert_eq!(observation_records(&setup).len(), 2);

        // A different outcome for the same finding is a typed conflict.
        let error = submit(&setup, FeedbackOutcome::Dismissed, Some("alice")).unwrap_err();
        assert!(
            matches!(
                &error,
                FeedbackError::OutcomeConflict {
                    recorded: FeedbackOutcome::Accepted,
                    requested: FeedbackOutcome::Dismissed,
                    ..
                }
            ),
            "{error}"
        );
        assert_eq!(observation_records(&setup).len(), 2, "nothing changed");

        // The other finding outcome flows only through explicit ids:
        // feedback for a different finding of the same review would be
        // independent (covered by binary tests).
    }

    #[test]
    fn missing_identity_retains_pending_state_and_fails_explicitly() {
        let setup = setup("feedback-no-identity");
        let error = submit(&setup, FeedbackOutcome::Fixed, None).unwrap_err();
        assert!(
            matches!(error, FeedbackError::NoContributorIdentity),
            "{error}"
        );
        // An invalid configured id is treated as missing.
        let error = submit(&setup, FeedbackOutcome::Fixed, Some("../evil")).unwrap_err();
        assert!(
            matches!(error, FeedbackError::NoContributorIdentity),
            "{error}"
        );
        assert!(observation_records(&setup).is_empty(), "nothing durable");

        // Two pending rows were retained locally, retrievable as rows.
        let db = open_db(&setup.paths.shared, DbKind::Shared).unwrap();
        let count: i64 = db
            .conn
            .query_row(
                "SELECT count(*) FROM pending_learning WHERE kind = ?1",
                [PENDING_KIND],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);

        // After an identity becomes available the resubmission records.
        submit(&setup, FeedbackOutcome::Fixed, Some("alice")).unwrap();
        assert_eq!(observation_records(&setup).len(), 2);
    }

    #[test]
    fn rendering_covers_both_outcomes() {
        let recorded = FeedbackRecording::Recorded {
            observations: vec![(
                "quality".to_string(),
                "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string(),
            )],
            lessons: vec![crate::learning::lessons::LessonCreated {
                reviewer_id: "concurrency".to_string(),
                category: "race".to_string(),
                record_id: "01ARZ3NDEKTSV4RRFFQ69G5FAW".to_string(),
                superseded: Vec::new(),
            }],
        };
        let text = render_feedback("r1", "f1", FeedbackOutcome::Fixed, &recorded);
        assert!(text.starts_with("nisse feedback\n"));
        assert!(text.contains("outcome: fixed"));
        assert!(text.contains("recorded observations: 1"));
        assert!(text.contains("reviewer=quality"));
        assert!(text.contains("lessons created: 1"));
        assert!(text.contains("concurrency/race"));

        let text = render_feedback(
            "r1",
            "f1",
            FeedbackOutcome::Dismissed,
            &FeedbackRecording::AlreadyRecorded(FeedbackOutcome::Dismissed),
        );
        assert!(text.contains("already recorded: dismissed"));
        assert!(!text.contains("lessons created"));
    }
}
