//! Deterministic lesson learning (plan sections 43 and 51; milestone 20).
//!
//! Lessons are what repeated review *outcomes* suggest is useful for
//! routing. Learning is primarily — in Nisse 1.0 entirely —
//! deterministic:
//!
//! 1. **Group observations** by stable descriptors: the finding's
//!    reviewer and category (the descriptors every observation carries
//!    by construction, plan section 41);
//! 2. **Deduplicate IDs**: statistics count unique original
//!    observations only (permanent invariant 7) — duplicate record IDs
//!    and semantically duplicate outcomes (the same review/finding
//!    outcome recorded again, e.g. after a branch merge) are counted
//!    once;
//! 3. **Compute statistics in Rust**: support (unique observations),
//!    positives (`fixed`/`accepted`), dismissals, and the routing
//!    conditions (the group's common path module and the tags shared
//!    by every observation) — never by an LLM (plan section 50);
//! 4. **Apply the frozen threshold** (plan section 51):
//!    `unique >= 5`, `positive >= 4`, `dismissed <= 1`;
//! 5. **Create the immutable lesson record** with a deterministic
//!    routing hint whose boost is capped at
//!    [`MAX_ROUTING_BOOST`](crate::domain::records::MAX_ROUTING_BOOST).
//!
//! Pi may generate only concise human-readable wording (plan section
//! 51); Nisse 1.0 derives the wording deterministically instead, so no
//! LLM participates in lesson learning at all. Lesson wording never
//! controls routing — the [`RulesClassifier`](crate::routing::rules::RulesClassifier)
//! consumes active lessons only through their routing hints, which may
//! raise scores but never suppress a reviewer.
//!
//! Maintenance is idempotent and conservative:
//!
//! - a group whose statistics are already represented by an active
//!   lesson with the same observation set creates nothing;
//! - new observations grow the group into a *new* immutable lesson
//!   that supersedes the previous one (the original remains for
//!   provenance);
//! - without a contributor identity, learning remains pending local
//!   state (plan section 44) — never durable records;
//! - storage failures surface as diagnostics and never fail the
//!   operation that triggered maintenance.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::domain::SCHEMA_VERSION;
use crate::domain::ids::new_ulid;
use crate::domain::records::{
    FeedbackOutcome, LessonRecord, MAX_OBSERVATION_REFS, MAX_ROUTING_BOOST, MAX_TAGS,
    ObservationRecord, ObservationType, ProjectRecord, RoutingHint, is_valid_contributor_id,
};
use crate::domain::time::now_utc_rfc3339;
use crate::git::RepoContext;
use crate::storage::StoragePaths;
use crate::storage::db::open_or_recover;
use crate::storage::records::{RecordStore, StoredRecord, active_records};
use crate::storage::schema::DbKind;

/// Initial routing-lesson threshold: unique observations required
/// (plan section 51).
pub const MIN_UNIQUE_OBSERVATIONS: usize = 5;

/// Initial routing-lesson threshold: positive outcomes required
/// (`fixed` or `accepted`, plan section 51).
pub const MIN_POSITIVE_OUTCOMES: usize = 4;

/// Initial routing-lesson threshold: dismissed outcomes allowed
/// (plan section 51).
pub const MAX_DISMISSED_OUTCOMES: usize = 1;

/// Decimal resolution of stored boost values (keeps serialized
/// lessons byte-stable for identical statistics).
const BOOST_RESOLUTION: f64 = 1000.0;

/// `pending_learning` row kind of deferred lesson maintenance.
pub const PENDING_KIND: &str = "lesson_maintenance";

/// Maximum groups described by one pending-maintenance row.
const MAX_PENDING_GROUPS: usize = 32;

/// Deterministic statistics of one observation group (plan section 51:
/// "Rust calculates support, positive, negative, routing conditions,
/// routing boost").
#[derive(Debug, Clone, PartialEq)]
pub struct GroupStats {
    /// Reviewer whose findings produced the observations.
    pub reviewer_id: String,
    /// Finding category of the group.
    pub category: String,
    /// Unique original observations in the group.
    pub support: usize,
    /// Positive outcomes (`fixed` or `accepted`).
    pub positives: usize,
    /// Dismissed outcomes.
    pub dismissed: usize,
    /// Sorted unique observation IDs of the group.
    pub observation_ids: Vec<String>,
    /// Routing path conditions: the common path module of the group
    /// (`<module>/**`), or empty when the group has no common module.
    pub path_globs: Vec<String>,
    /// Routing tag conditions: the tags shared by every observation of
    /// the group, or empty when there is no shared tag.
    pub required_tags_any: Vec<String>,
    /// Routing boost derived from the positive ratio, bounded by
    /// `MAX_ROUTING_BOOST`.
    pub boost: f64,
}

impl GroupStats {
    /// The frozen grouping key: (reviewer, finding category).
    pub fn key(&self) -> (&str, &str) {
        (&self.reviewer_id, &self.category)
    }

    /// Whether the group satisfies the frozen initial routing-lesson
    /// threshold (plan section 51).
    pub fn meets_threshold(&self) -> bool {
        self.support >= MIN_UNIQUE_OBSERVATIONS
            && self.positives >= MIN_POSITIVE_OUTCOMES
            && self.dismissed <= MAX_DISMISSED_OUTCOMES
    }
}

/// Group observations and compute their deterministic statistics.
///
/// Observations are grouped by `(reviewer_id, category)`; groups are
/// returned in deterministic `(reviewer, category)` order. Statistics
/// count unique original observations only:
///
/// - records with the same ID count once (permanent invariant 7);
/// - semantically duplicate outcomes — the same `(review_id,
///   finding_id)` pair, which can only arise from the same outcome
///   recorded twice, e.g. on branches later merged — count once
///   (the earliest by `(created_at, id)` wins);
/// - observations of other types than `review_outcome` are ignored
///   (the only 1.0 type, kept explicit for stability).
///
/// Routing conditions are derived conservatively:
///
/// - path globs exist only when every unique observation cites at
///   least one path and all cited paths share one common module; the
///   glob is that module plus `/**`;
/// - required tags are the tags shared by *every* unique observation
///   of the group.
pub fn compute_group_stats(observations: &[&ObservationRecord]) -> Vec<GroupStats> {
    let mut groups: BTreeMap<(String, String), Vec<&ObservationRecord>> = BTreeMap::new();
    for observation in observations {
        if observation.observation_type != ObservationType::ReviewOutcome {
            continue;
        }
        groups
            .entry((
                observation.reviewer_id.clone(),
                observation.category.clone(),
            ))
            .or_default()
            .push(observation);
    }

    groups
        .into_iter()
        .filter_map(|((reviewer_id, category), group)| {
            stats_for_group(&reviewer_id, &category, &group)
        })
        .collect()
}

/// Statistics of one already-grouped set of observations.
fn stats_for_group(
    reviewer_id: &str,
    category: &str,
    group: &[&ObservationRecord],
) -> Option<GroupStats> {
    // Unique original observations: deduplicate by ID, then by the
    // semantic outcome identity (review, finding) — the reviewer is
    // the group key. The earliest record wins, deterministically.
    let mut by_id: BTreeMap<&str, &ObservationRecord> = BTreeMap::new();
    for observation in group {
        by_id.entry(observation.id.as_str()).or_insert(observation);
    }
    let mut unique: Vec<&ObservationRecord> = by_id.into_values().collect();
    unique.sort_by_key(|observation| {
        (
            observation.created_at.as_str().to_string(),
            observation.id.as_str().to_string(),
        )
    });
    let mut seen_outcomes: BTreeSet<(&str, &str)> = BTreeSet::new();
    let unique: Vec<&ObservationRecord> = unique
        .into_iter()
        .filter(|observation| {
            seen_outcomes.insert((
                observation.review_id.as_str(),
                observation.finding_id.as_str(),
            ))
        })
        .collect();
    if unique.is_empty() {
        return None;
    }

    let support = unique.len();
    let positives = unique
        .iter()
        .filter(|observation| {
            matches!(
                observation.outcome,
                FeedbackOutcome::Fixed | FeedbackOutcome::Accepted
            )
        })
        .count();
    let dismissed = unique
        .iter()
        .filter(|observation| observation.outcome == FeedbackOutcome::Dismissed)
        .count();
    let observation_ids: Vec<String> = unique
        .iter()
        .map(|observation| observation.id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let path_globs = common_path_globs(&unique);
    let required_tags_any = common_tags(&unique);
    let boost = clamp_boost(MAX_ROUTING_BOOST * positives as f64 / support as f64);

    Some(GroupStats {
        reviewer_id: reviewer_id.to_string(),
        category: category.to_string(),
        support,
        positives,
        dismissed,
        observation_ids,
        path_globs,
        required_tags_any,
        boost,
    })
}

/// The common path module of the group as one `<module>/**` glob, or
/// empty when the observations do not share one.
///
/// The module is the longest component prefix shared by every cited
/// path, truncated to a *strict* directory (a path never matches its
/// own file glob `dir/**`). Observations without any path veto path
/// conditions entirely: the group is not known to be module-local.
fn common_path_globs(unique: &[&ObservationRecord]) -> Vec<String> {
    if unique
        .iter()
        .any(|observation| observation.paths.is_empty())
    {
        return Vec::new();
    }
    let mut paths: Vec<Vec<&str>> = unique
        .iter()
        .flat_map(|observation| observation.paths.iter())
        .map(|path| {
            path.split('/')
                .filter(|segment| !segment.is_empty())
                .collect()
        })
        .collect();
    if paths.is_empty() {
        return Vec::new();
    }
    paths.sort_by_key(|segments| segments.len());
    let shortest = paths[0].clone();
    // Longest component prefix shared by every path (0 when the paths
    // diverge at the root).
    let mut prefix_len = 0;
    'outer: for length in (1..=shortest.len()).rev() {
        for path in &paths {
            if path[..length] != shortest[..length] {
                continue 'outer;
            }
        }
        prefix_len = length;
        break;
    }
    // A strict directory: `dir/**` matches only paths strictly inside
    // `dir`, so a prefix that spans an entire path falls back to its
    // parent directory.
    if prefix_len == shortest.len() {
        prefix_len -= 1;
    }
    if prefix_len == 0 {
        return Vec::new();
    }
    let module = shortest[..prefix_len].join("/");
    vec![format!("{module}/**")]
}

/// The tags shared by every unique observation of the group, sorted
/// and bounded.
fn common_tags(unique: &[&ObservationRecord]) -> Vec<String> {
    let mut shared: Option<BTreeSet<&str>> = None;
    for observation in unique {
        let tags: BTreeSet<&str> = observation.tags.iter().map(String::as_str).collect();
        shared = Some(match shared {
            None => tags,
            Some(previous) => previous.intersection(&tags).copied().collect(),
        });
    }
    shared
        .unwrap_or_default()
        .into_iter()
        .take(MAX_TAGS)
        .map(str::to_string)
        .collect()
}

/// Round to the frozen boost resolution and clamp to the allowed
/// range; a non-finite value degrades to zero.
fn clamp_boost(boost: f64) -> f64 {
    if !boost.is_finite() || boost <= 0.0 {
        return 0.0;
    }
    ((boost * BOOST_RESOLUTION).round() / BOOST_RESOLUTION).clamp(0.0, MAX_ROUTING_BOOST)
}

/// Deterministic lesson wording (plan section 51: Pi may generate only
/// wording; Nisse 1.0 needs no LLM at all). The wording carries the
/// recomputable statistics; routing never reads it.
pub fn lesson_wording(stats: &GroupStats) -> String {
    let scope = if stats.path_globs.is_empty() {
        "any path".to_string()
    } else {
        stats.path_globs.join(", ")
    };
    let tags = if stats.required_tags_any.is_empty() {
        "no tag requirement".to_string()
    } else {
        stats.required_tags_any.join(", ")
    };
    format!(
        "Repeated outcomes of `{}` findings in category `{}`: {support} unique observations, \
         {positives} positive (fixed/accepted), {dismissed} dismissed. Routing boosts `{}` by \
         {boost} for changes under {scope} when tags include {tags}.",
        stats.reviewer_id,
        stats.category,
        stats.reviewer_id,
        support = stats.support,
        positives = stats.positives,
        dismissed = stats.dismissed,
        boost = stats.boost,
    )
}

/// Build the immutable lesson record of one threshold-met group.
/// Every routing-relevant field comes from `stats` alone.
pub fn build_lesson_record(
    stats: &GroupStats,
    author: &str,
    supersedes: Vec<String>,
    created_at: &str,
) -> LessonRecord {
    LessonRecord {
        schema_version: SCHEMA_VERSION,
        id: new_ulid(),
        author: author.to_string(),
        created_at: created_at.to_string(),
        subject: format!(
            "repeated `{}` findings of `{}`",
            stats.category, stats.reviewer_id
        ),
        text: lesson_wording(stats),
        tags: stats.required_tags_any.clone(),
        routing_hint: RoutingHint {
            reviewer_id: stats.reviewer_id.clone(),
            path_globs: stats.path_globs.clone(),
            required_tags_any: stats.required_tags_any.clone(),
            boost: stats.boost,
        },
        source_observation_ids: stats.observation_ids.clone(),
        supersedes,
    }
}

/// Threshold-met group statistics derived from the durable records.
///
/// Observations of every contributor participate (shared team
/// learning); the resulting lesson is authored by the contributor who
/// triggered maintenance. Groups are returned in deterministic
/// `(reviewer, category)` order.
pub fn derive_group_candidates(records: &[StoredRecord]) -> Vec<GroupStats> {
    let observations: Vec<&ObservationRecord> = records
        .iter()
        .filter_map(|stored| match &stored.record {
            ProjectRecord::Observation(observation) => Some(observation),
            _ => None,
        })
        .collect();
    compute_group_stats(&observations)
        .into_iter()
        .filter(GroupStats::meets_threshold)
        .collect()
}

/// One lesson created by a maintenance pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LessonCreated {
    /// The group's reviewer.
    pub reviewer_id: String,
    /// The group's finding category.
    pub category: String,
    /// The created lesson record ID.
    pub record_id: String,
    /// Lesson records superseded by the new one (kept for provenance).
    pub superseded: Vec<String>,
}

/// Human-readable `reviewer/category` label of one group.
pub fn group_label(reviewer_id: &str, category: &str) -> String {
    format!("{reviewer_id}/{category}")
}

/// Summary of one deterministic learning-maintenance pass.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LessonMaintenanceSummary {
    /// Lessons created, in deterministic group order.
    pub lessons_created: Vec<LessonCreated>,
    /// Human-readable diagnostics (deferred learning, skipped groups,
    /// storage failures). Maintenance never fails its caller.
    pub diagnostics: Vec<String>,
}

/// Deferred lesson maintenance retained locally (plan section 44:
/// learning remains pending when identity is unavailable).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingLessonMaintenance {
    /// Payload schema version; must be `1`.
    pub schema_version: u32,
    /// The threshold-met groups awaiting a contributor identity.
    pub groups: Vec<PendingLessonGroup>,
    /// Why the lessons were not recorded durably.
    pub reason: String,
    /// When the pass ran (UTC RFC3339).
    pub created_at: String,
}

/// One deferred group of a pending-maintenance row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingLessonGroup {
    /// The group's reviewer.
    pub reviewer_id: String,
    /// The group's finding category.
    pub category: String,
    /// The unique observation IDs the lesson would reference.
    pub observation_ids: Vec<String>,
}

/// Run one deterministic learning-maintenance pass (plan section 58
/// step 21).
///
/// `records` must be the freshly loaded durable records of `ctx`'s
/// worktree. The pass is idempotent: groups whose statistics are
/// already current (an active lesson referencing exactly the same
/// observation set) create nothing; grown groups create a new lesson
/// superseding the previous one. Failures — including a missing
/// contributor identity (plan section 44) — surface as diagnostics or
/// pending local state, never as errors.
pub fn run_learning_maintenance(
    ctx: &RepoContext,
    paths: Option<&StoragePaths>,
    records: &[StoredRecord],
    contributor_id: Option<&str>,
) -> LessonMaintenanceSummary {
    let mut summary = LessonMaintenanceSummary::default();
    let candidates = derive_group_candidates(records);
    if candidates.is_empty() {
        return summary;
    }

    // Without a contributor identity no durable record may be created:
    // the deferred learning stays pending local state (plan section 44).
    let Some(author) = contributor_id.filter(|id| is_valid_contributor_id(id)) else {
        let groups: Vec<PendingLessonGroup> = candidates
            .iter()
            .take(MAX_PENDING_GROUPS)
            .map(|stats| PendingLessonGroup {
                reviewer_id: stats.reviewer_id.clone(),
                category: stats.category.clone(),
                observation_ids: stats.observation_ids.clone(),
            })
            .collect();
        if candidates.len() > MAX_PENDING_GROUPS {
            summary.diagnostics.push(format!(
                "{} threshold-met group(s) exist but only the first {MAX_PENDING_GROUPS} are \
                 described by the pending state",
                candidates.len()
            ));
        }
        if let Some(paths) = paths {
            let entry = PendingLessonMaintenance {
                schema_version: SCHEMA_VERSION,
                groups,
                reason: "no contributor identity available (plan section 44)".to_string(),
                created_at: now_utc_rfc3339(),
            };
            match store_pending_maintenance(paths, &entry) {
                Ok(()) => summary.diagnostics.push(format!(
                    "lesson learning for {} group(s) remains pending locally: no contributor \
                     identity available",
                    entry.groups.len()
                )),
                Err(error) => summary
                    .diagnostics
                    .push(format!("failed to retain pending lesson learning: {error}")),
            }
        } else {
            summary.diagnostics.push(format!(
                "lesson learning for {} group(s) skipped: no contributor identity and no local \
                 cache to retain pending state",
                candidates.len()
            ));
        }
        return summary;
    };

    // Groups partition observations by (reviewer, category), so an
    // active lesson sharing an observation with a candidate must be an
    // earlier lesson of exactly that group.
    let active_lessons: Vec<&LessonRecord> = active_records(records)
        .into_iter()
        .filter_map(|stored| match &stored.record {
            ProjectRecord::Lesson(lesson) => Some(lesson),
            _ => None,
        })
        .collect();
    let store = RecordStore::new(&ctx.worktree_root);
    let created_at = now_utc_rfc3339();
    let mut satisfied: BTreeSet<(String, String)> = BTreeSet::new();

    for stats in &candidates {
        if stats.observation_ids.len() > MAX_OBSERVATION_REFS {
            summary.diagnostics.push(format!(
                "group {} exceeds the frozen lesson observation-reference bound ({} > {}) and \
                 was skipped",
                group_label(&stats.reviewer_id, &stats.category),
                stats.observation_ids.len(),
                MAX_OBSERVATION_REFS
            ));
            continue;
        }
        let candidate_set: BTreeSet<&str> =
            stats.observation_ids.iter().map(String::as_str).collect();
        let mut superseded: Vec<String> = Vec::new();
        let mut already_current = false;
        for lesson in &active_lessons {
            if lesson.routing_hint.reviewer_id != stats.reviewer_id {
                continue;
            }
            let shared = lesson
                .source_observation_ids
                .iter()
                .any(|id| candidate_set.contains(id.as_str()));
            if !shared {
                continue;
            }
            let existing_set: BTreeSet<&str> = lesson
                .source_observation_ids
                .iter()
                .map(String::as_str)
                .collect();
            if existing_set == candidate_set {
                already_current = true;
                break;
            }
            superseded.push(lesson.id.clone());
        }
        if already_current {
            satisfied.insert((stats.reviewer_id.clone(), stats.category.clone()));
            continue;
        }
        superseded.sort();
        superseded.dedup();

        let record = build_lesson_record(stats, author, superseded.clone(), &created_at);
        match store.create(&ProjectRecord::Lesson(record)) {
            Ok(stored) => {
                satisfied.insert((stats.reviewer_id.clone(), stats.category.clone()));
                summary.lessons_created.push(LessonCreated {
                    reviewer_id: stats.reviewer_id.clone(),
                    category: stats.category.clone(),
                    record_id: stored.id().to_string(),
                    superseded,
                })
            }
            Err(error) => summary.diagnostics.push(format!(
                "failed to create the lesson for group {}: {error}",
                group_label(&stats.reviewer_id, &stats.category)
            )),
        }
    }

    // Deferred-maintenance rows whose groups are now represented by
    // active lessons are satisfied: prune them from the regenerable
    // local state so `records pending` never reports learning that has
    // since happened.
    if let Some(paths) = paths
        && !satisfied.is_empty()
        && let Err(error) = prune_satisfied_pending(paths, &satisfied)
    {
        summary.diagnostics.push(format!(
            "failed to prune satisfied pending lesson learning: {error}"
        ));
    }
    summary
}

/// Delete deferred-maintenance rows whose every group is now
/// represented by an active lesson. Best-effort hygiene of the
/// regenerable local cache, never durable knowledge.
fn prune_satisfied_pending(
    paths: &StoragePaths,
    satisfied: &BTreeSet<(String, String)>,
) -> Result<(), String> {
    let db = open_or_recover(&paths.shared, DbKind::Shared)
        .map_err(|error| format!("{}: {error}", error))?;
    let mut stmt = db
        .conn
        .prepare("SELECT id, payload FROM pending_learning WHERE kind = ?1")
        .map_err(|error| error.to_string())?;
    let rows = stmt
        .query_map([PENDING_KIND], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    for (id, payload) in rows {
        let Ok(entry) = serde_json::from_str::<PendingLessonMaintenance>(&payload) else {
            continue;
        };
        if entry
            .groups
            .iter()
            .all(|group| satisfied.contains(&(group.reviewer_id.clone(), group.category.clone())))
        {
            db.conn
                .execute("DELETE FROM pending_learning WHERE id = ?1", [id])
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

/// Reload the durable records of `ctx` and run one maintenance pass
/// for a known author (used by the feedback path, where the
/// contributor identity is already established). Diagnostics are
/// warnings only: the operation that triggered maintenance never
/// fails because of it.
pub fn maintain_after_feedback(
    ctx: &RepoContext,
    paths: &StoragePaths,
    author: &str,
) -> Vec<LessonCreated> {
    let summary = match RecordStore::new(&ctx.worktree_root).load() {
        Ok(records) => run_learning_maintenance(ctx, Some(paths), &records, Some(author)),
        Err(error) => LessonMaintenanceSummary {
            lessons_created: Vec::new(),
            diagnostics: vec![format!("lesson maintenance skipped: {error}")],
        },
    };
    for diagnostic in &summary.diagnostics {
        tracing::warn!("{diagnostic}");
    }
    summary.lessons_created
}

/// Retain one deferred-maintenance row, deduplicated by its group
/// content (the row's timestamp may differ) so repeated passes without
/// an identity do not grow the local state (the regenerable cache,
/// never durable knowledge).
pub fn store_pending_maintenance(
    paths: &StoragePaths,
    entry: &PendingLessonMaintenance,
) -> Result<(), String> {
    let db = open_or_recover(&paths.shared, DbKind::Shared)
        .map_err(|error| format!("{}: {error}", error))?;

    // A row with the same groups already marks the deferred learning.
    let mut stmt = db
        .conn
        .prepare("SELECT payload FROM pending_learning WHERE kind = ?1")
        .map_err(|error| error.to_string())?;
    let payloads = stmt
        .query_map([PENDING_KIND], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<String>, _>>()
        .map_err(|error| error.to_string())?;
    for payload in payloads {
        let stored: PendingLessonMaintenance =
            serde_json::from_str(&payload).map_err(|error| error.to_string())?;
        if stored.groups == entry.groups {
            return Ok(());
        }
    }

    let payload = serde_json::to_string(entry).map_err(|error| error.to_string())?;
    db.conn
        .execute(
            "INSERT INTO pending_learning (kind, payload, created_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![PENDING_KIND, payload, entry.created_at],
        )
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::review::ImpactClass;
    use crate::domain::routing::RoutingInput;
    use crate::domain::snapshot::{SnapshotDescriptor, SnapshotMode};
    use crate::git::testutil::{Fixture, commit_all, git_in, write_file};
    use crate::git::{GitRunner, RepoContext};
    use crate::jury::catalog::Catalog;
    use crate::routing::rules::RulesClassifier;

    /// An observation with explicit stable descriptors. IDs are fresh
    /// canonical ULIDs (the frozen schema validates them); callers keep
    /// the returned records for identity assertions.
    fn observation(
        reviewer_id: &str,
        category: &str,
        outcome: FeedbackOutcome,
        paths: &[&str],
    ) -> ObservationRecord {
        ObservationRecord {
            schema_version: SCHEMA_VERSION,
            id: new_ulid(),
            author: "alice".to_string(),
            created_at: "2026-01-02T12:00:00Z".to_string(),
            observation_type: ObservationType::ReviewOutcome,
            review_id: new_ulid(),
            finding_id: format!("f-{}", crate::domain::ids::new_ulid()),
            reviewer_id: reviewer_id.to_string(),
            outcome,
            impact_class: ImpactClass::Correctness,
            category: category.to_string(),
            languages: vec!["rust".to_string()],
            tags: vec!["correctness".to_string(), category.to_string()],
            paths: paths.iter().map(|p| p.to_string()).collect(),
            snapshot: SnapshotDescriptor {
                mode: SnapshotMode::Working,
                base_commit: None,
                base_tree: None,
                head_commit: None,
                head_tree: None,
            },
        }
    }

    fn refs(records: &[ObservationRecord]) -> Vec<&ObservationRecord> {
        records.iter().collect()
    }

    fn race_group(count: usize, dismissed: usize) -> Vec<ObservationRecord> {
        (0..count)
            .map(|index| {
                let outcome = if index < dismissed {
                    FeedbackOutcome::Dismissed
                } else {
                    FeedbackOutcome::Fixed
                };
                observation("concurrency", "race", outcome, &["src/queue/mod.rs"])
            })
            .collect()
    }

    #[test]
    fn group_stats_count_support_positives_and_dismissals() {
        let records = race_group(5, 1);
        let stats = compute_group_stats(&refs(&records));
        assert_eq!(stats.len(), 1);
        let group = &stats[0];
        assert_eq!(group.key(), ("concurrency", "race"));
        assert_eq!(group.support, 5);
        assert_eq!(group.positives, 4);
        assert_eq!(group.dismissed, 1);
        // Observation IDs are unique and sorted.
        let mut expected: Vec<String> = records.iter().map(|r| r.id.clone()).collect();
        expected.sort();
        assert_eq!(group.observation_ids, expected);
        // Accepted outcomes are positive too.
        let mut records = race_group(5, 0);
        records[0].outcome = FeedbackOutcome::Accepted;
        let group = &compute_group_stats(&refs(&records))[0];
        assert_eq!(group.positives, 5);
    }

    #[test]
    fn duplicate_ids_and_duplicate_outcomes_count_once() {
        // The same record listed twice (defense in depth: the store
        // already rejects duplicate IDs) and the same outcome recorded
        // again with a fresh ID — as after a branch merge — count as
        // one unique observation.
        let mut records = race_group(4, 0);
        let duplicated = records[0].clone();
        records.push(duplicated);
        let mut semantic_copy = records[1].clone();
        semantic_copy.id = new_ulid();
        semantic_copy.created_at = "2026-06-01T00:00:00Z".to_string();
        records.push(semantic_copy);

        let group = &compute_group_stats(&refs(&records))[0];
        assert_eq!(group.support, 4, "duplicates must not inflate support");
        // The earliest record of the duplicated outcome wins.
        assert!(group.observation_ids.contains(&records[1].id));
        assert_eq!(group.observation_ids.len(), 4);
        // Five raw records but four unique observations reach the
        // threshold only with a genuine fifth outcome.
        assert!(!group.meets_threshold());
        records.push(observation(
            "concurrency",
            "race",
            FeedbackOutcome::Fixed,
            &["src/queue/mod.rs"],
        ));
        let group = &compute_group_stats(&refs(&records))[0];
        assert_eq!(group.support, 5);
        assert!(group.meets_threshold());
    }

    #[test]
    fn threshold_gates_groups() {
        // Four unique observations: below the threshold.
        assert!(!compute_group_stats(&refs(&race_group(4, 0)))[0].meets_threshold());
        // Five unique with four positive and one dismissed: at it.
        assert!(compute_group_stats(&refs(&race_group(5, 1)))[0].meets_threshold());
        // Six unique, four positive, two dismissed: too many dismissals.
        assert!(!compute_group_stats(&refs(&race_group(6, 2)))[0].meets_threshold());
        // Five unique with only three positive: not enough positives.
        let records = race_group(5, 2);
        assert_eq!(compute_group_stats(&refs(&records))[0].positives, 3);
        assert!(!compute_group_stats(&refs(&records))[0].meets_threshold());
    }

    #[test]
    fn boost_derives_from_the_positive_ratio_and_is_capped() {
        let all_positive = &compute_group_stats(&refs(&race_group(5, 0)))[0];
        assert_eq!(all_positive.boost, MAX_ROUTING_BOOST);
        // Even a huge group never exceeds the frozen maximum.
        let big = &compute_group_stats(&refs(&race_group(500, 0)))[0];
        assert_eq!(big.boost, MAX_ROUTING_BOOST);
        let one_dismissed = &compute_group_stats(&refs(&race_group(5, 1)))[0];
        assert!(
            (one_dismissed.boost - 0.28).abs() < 1e-9,
            "boost {}",
            one_dismissed.boost
        );
        // The record schema accepts the derived values, and invalid
        // ratios degrade to zero.
        assert_eq!(clamp_boost(f64::NAN), 0.0);
        assert_eq!(clamp_boost(-1.0), 0.0);
        assert_eq!(clamp_boost(9.0), MAX_ROUTING_BOOST);
    }

    #[test]
    fn path_and_tag_conditions_are_conservative() {
        // All observations cite one shared module.
        let group = &compute_group_stats(&refs(&race_group(5, 0)))[0];
        assert_eq!(group.path_globs, vec!["src/queue/**".to_string()]);
        assert_eq!(
            group.required_tags_any,
            vec!["correctness".to_string(), "race".to_string()]
        );

        // A module shared by only some paths widens to the common
        // parent; observations without paths veto path conditions.
        let mut records = race_group(5, 0);
        records[0].paths = vec!["src/isr/handler.rs".to_string()];
        let group = &compute_group_stats(&refs(&records))[0];
        assert_eq!(group.path_globs, vec!["src/**".to_string()]);

        let mut records = race_group(5, 0);
        records[0].paths = Vec::new();
        let group = &compute_group_stats(&refs(&records))[0];
        assert!(group.path_globs.is_empty());

        // Root-level paths have no common module.
        let records: Vec<ObservationRecord> = (0..5)
            .map(|index| {
                observation(
                    "concurrency",
                    "race",
                    FeedbackOutcome::Fixed,
                    &[&format!("file{index}.rs")],
                )
            })
            .collect();
        assert!(
            compute_group_stats(&refs(&records))[0]
                .path_globs
                .is_empty()
        );

        // Tags are the intersection across observations.
        let mut records = race_group(5, 0);
        records[0].tags = vec!["race".to_string()];
        let group = &compute_group_stats(&refs(&records))[0];
        assert_eq!(group.required_tags_any, vec!["race".to_string()]);
    }

    #[test]
    fn groups_are_partitioned_and_ordered() {
        let mut records = race_group(5, 0);
        records.extend((0..5).map(|_| {
            observation(
                "database",
                "migration",
                FeedbackOutcome::Fixed,
                &["db/migrations/001.sql"],
            )
        }));
        records.extend((0..5).map(|_| {
            observation(
                "quality",
                "race",
                FeedbackOutcome::Fixed,
                &["src/queue/mod.rs"],
            )
        }));
        let stats = compute_group_stats(&refs(&records));
        let keys: Vec<_> = stats.iter().map(GroupStats::key).collect();
        assert_eq!(
            keys,
            vec![
                ("concurrency", "race"),
                ("database", "migration"),
                ("quality", "race"),
            ]
        );
    }

    #[test]
    fn wording_is_deterministic_and_bounded() {
        let group = &compute_group_stats(&refs(&race_group(5, 1)))[0];
        let first = lesson_wording(group);
        assert_eq!(first, lesson_wording(group));
        assert!(first.contains("concurrency"));
        assert!(first.contains("5 unique observations"));
        assert!(first.contains("src/queue/**"));
        assert!(first.len() < crate::domain::records::MAX_TEXT_BYTES);

        let lesson = build_lesson_record(group, "alice", Vec::new(), "2026-01-19T12:00:00Z");
        ProjectRecord::Lesson(lesson.clone()).validate().unwrap();
        assert_eq!(lesson.routing_hint.reviewer_id, "concurrency");
        assert_eq!(lesson.routing_hint.boost, group.boost);
        assert_eq!(lesson.source_observation_ids, group.observation_ids);
        assert_eq!(lesson.supersedes, Vec::<String>::new());
    }

    /// A real repository with `count` durable race observations.
    fn repo_with_observations(
        label: &str,
        count: usize,
        dismissed: usize,
    ) -> (Fixture, RepoContext) {
        let fixture = Fixture::repo(label);
        write_file(&fixture.root, "src/queue/mod.rs", "pub fn enqueue() {}\n");
        commit_all(&fixture.root, "one");
        let store = RecordStore::new(&fixture.root);
        for index in 0..count {
            let outcome = if index < dismissed {
                FeedbackOutcome::Dismissed
            } else {
                FeedbackOutcome::Fixed
            };
            let record = observation("concurrency", "race", outcome, &["src/queue/mod.rs"]);
            store
                .create(&ProjectRecord::Observation(record))
                .expect("seed observation");
        }
        let ctx = RepoContext::discover(&GitRunner::new(), &fixture.root).unwrap();
        (fixture, ctx)
    }

    fn storage_paths(ctx: &RepoContext) -> StoragePaths {
        StoragePaths::for_repo(ctx)
    }

    fn lessons(fixture: &Fixture) -> Vec<LessonRecord> {
        RecordStore::new(&fixture.root)
            .load()
            .unwrap()
            .into_iter()
            .filter_map(|stored| match stored.record {
                ProjectRecord::Lesson(lesson) => Some(lesson),
                _ => None,
            })
            .collect()
    }

    fn records_of(fixture: &Fixture) -> Vec<StoredRecord> {
        RecordStore::new(&fixture.root).load().unwrap()
    }

    #[test]
    fn maintenance_creates_idempotent_and_superseding_lessons() {
        let (fixture, ctx) = repo_with_observations("lessons-idempotent", 5, 0);
        let paths = storage_paths(&ctx);
        let records = records_of(&fixture);

        let first = run_learning_maintenance(&ctx, Some(&paths), &records, Some("alice"));
        assert!(first.diagnostics.is_empty(), "{:?}", first.diagnostics);
        assert_eq!(first.lessons_created.len(), 1);
        let created = &first.lessons_created[0];
        assert_eq!(created.reviewer_id, "concurrency");
        assert_eq!(created.category, "race");
        assert!(created.superseded.is_empty());

        // The lesson lives under the author's contributor directory.
        let stored = lessons(&fixture);
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].id, created.record_id);
        assert_eq!(stored[0].author, "alice");
        assert!(records_of(&fixture).iter().any(|stored| {
            stored.id() == created.record_id
                && stored
                    .path
                    .to_string_lossy()
                    .contains(".nisse/people/alice/")
        }));

        // A second pass with unchanged observations creates nothing.
        let records = records_of(&fixture);
        let second = run_learning_maintenance(&ctx, Some(&paths), &records, Some("bob"));
        assert!(second.lessons_created.is_empty());
        assert_eq!(lessons(&fixture).len(), 1);

        // A new observation grows the group: a new lesson supersedes
        // the old one, which remains stored for provenance.
        let store = RecordStore::new(&fixture.root);
        store
            .create(&ProjectRecord::Observation(observation(
                "concurrency",
                "race",
                FeedbackOutcome::Fixed,
                &["src/queue/mod.rs"],
            )))
            .unwrap();
        let records = records_of(&fixture);
        let third = run_learning_maintenance(&ctx, Some(&paths), &records, Some("bob"));
        assert_eq!(third.lessons_created.len(), 1);
        let grown = &third.lessons_created[0];
        assert_eq!(grown.superseded, vec![created.record_id.clone()]);

        let stored = lessons(&fixture);
        assert_eq!(stored.len(), 2, "the original lesson remains");
        let new_lesson = stored
            .iter()
            .find(|lesson| lesson.id == grown.record_id)
            .unwrap();
        assert_eq!(new_lesson.source_observation_ids.len(), 6);
        assert_eq!(new_lesson.supersedes, vec![created.record_id.clone()]);
        // Only the newest lesson is active.
        let records = records_of(&fixture);
        let active = active_records(&records);
        let active_lessons: Vec<String> = active
            .iter()
            .filter(|stored| matches!(stored.record, ProjectRecord::Lesson(_)))
            .map(|stored| stored.id().to_string())
            .collect();
        assert_eq!(active_lessons, vec![grown.record_id.clone()]);
    }

    #[test]
    fn maintenance_without_identity_stays_pending_then_records() {
        let (fixture, ctx) = repo_with_observations("lessons-pending", 5, 0);
        let paths = storage_paths(&ctx);
        let records = records_of(&fixture);

        let first = run_learning_maintenance(&ctx, Some(&paths), &records, None);
        assert!(first.lessons_created.is_empty());
        assert_eq!(first.diagnostics.len(), 1);
        assert!(
            first.diagnostics[0].contains("pending locally"),
            "{:?}",
            first.diagnostics
        );
        assert!(lessons(&fixture).is_empty(), "nothing durable");

        // The pending row is deduplicated by payload.
        let second = run_learning_maintenance(&ctx, Some(&paths), &records, None);
        assert!(second.lessons_created.is_empty());
        let db = open_or_recover(&paths.shared, DbKind::Shared).unwrap();
        let count: i64 = db
            .conn
            .query_row(
                "SELECT count(*) FROM pending_learning WHERE kind = ?1",
                [PENDING_KIND],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        // With an identity the same observations become a lesson.
        let third = run_learning_maintenance(&ctx, Some(&paths), &records, Some("alice"));
        assert_eq!(third.lessons_created.len(), 1);
        assert_eq!(lessons(&fixture).len(), 1);

        // The satisfied pending row is pruned: `records pending` never
        // reports learning that has since happened.
        let db = open_or_recover(&paths.shared, DbKind::Shared).unwrap();
        let count: i64 = db
            .conn
            .query_row(
                "SELECT count(*) FROM pending_learning WHERE kind = ?1",
                [PENDING_KIND],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn sub_threshold_observations_create_nothing() {
        let (fixture, ctx) = repo_with_observations("lessons-below-threshold", 4, 0);
        let paths = storage_paths(&ctx);
        let records = records_of(&fixture);
        let summary = run_learning_maintenance(&ctx, Some(&paths), &records, Some("alice"));
        assert!(summary.lessons_created.is_empty());
        assert!(summary.diagnostics.is_empty());
        assert!(lessons(&fixture).is_empty());
        assert!(!paths.shared.is_file(), "no pending row for an empty pass");
    }

    #[test]
    fn lessons_boost_only_their_own_reviewer() {
        // Derived lessons change exactly one reviewer's score.
        let (fixture, ctx) = repo_with_observations("lessons-unrelated", 5, 0);
        let paths = storage_paths(&ctx);
        let records = records_of(&fixture);
        run_learning_maintenance(&ctx, Some(&paths), &records, Some("alice"));

        let catalog = Catalog::bundled().unwrap();
        let classifier = RulesClassifier::new(catalog.clone());
        let input = crate::domain::routing::RoutingInput {
            descriptors: vec!["Tune the shared queue".to_string()],
            changed_paths: vec!["src/queue/mod.rs".to_string()],
            deterministic_tags: vec!["correctness".to_string(), "race".to_string()],
            ..RoutingInput::default()
        };
        let without = classifier.classify(&input);
        let with = classifier.classify(&crate::domain::routing::RoutingInput {
            lessons: crate::jury::review::active_lesson_hints(&active_records(&records_of(
                &fixture,
            ))),
            ..input.clone()
        });
        for score in &without.scores {
            let after = with
                .scores
                .iter()
                .find(|s| s.reviewer_id == score.reviewer_id)
                .unwrap();
            if score.reviewer_id == "concurrency" {
                assert_eq!(after.lesson_boost, 0.35);
                assert!(after.score > score.score);
            } else {
                assert_eq!(after, score, "unrelated reviewer changed");
            }
        }
    }

    #[test]
    fn git_shared_lessons_route_later_reviews() {
        // Records created and consolidated on one branch travel through
        // Git and improve routing after a normal merge (milestone 20:
        // "Git sharing").
        let (fixture, ctx) = repo_with_observations("lessons-git-sharing", 5, 0);
        let paths = storage_paths(&ctx);
        let records = records_of(&fixture);
        let summary = run_learning_maintenance(&ctx, Some(&paths), &records, Some("alice"));
        assert_eq!(summary.lessons_created.len(), 1);
        let lesson_id = summary.lessons_created[0].record_id.clone();

        // Share the durable records through ordinary Git.
        git_in(&fixture.root, &["checkout", "--quiet", "-b", "learning"]);
        git_in(&fixture.root, &["add", ".nisse"]);
        git_in(&fixture.root, &["commit", "--quiet", "-m", "nisse records"]);
        git_in(
            &fixture.root,
            &["checkout", "--quiet", &fixture.branch_name],
        );
        assert!(lessons(&fixture).is_empty(), "main has no records yet");
        git_in(
            &fixture.root,
            &[
                "merge",
                "--quiet",
                "--no-ff",
                "-m",
                "merge learning",
                "learning",
            ],
        );

        // The merged lesson is active and routes later reviews.
        let merged = records_of(&fixture);
        let active = active_records(&merged);
        let hints = crate::jury::review::active_lesson_hints(&active);
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].reviewer_id, "concurrency");
        assert!(merged.iter().any(|stored| stored.id() == lesson_id));

        let catalog = Catalog::bundled().unwrap();
        let classifier = RulesClassifier::new(catalog.clone());
        let input = crate::domain::routing::RoutingInput {
            descriptors: vec!["Tune the shared queue".to_string()],
            changed_paths: vec!["src/queue/mod.rs".to_string()],
            deterministic_tags: vec!["correctness".to_string(), "race".to_string()],
            lessons: hints,
            ..RoutingInput::default()
        };
        let decision = classifier.classify(&input);
        assert!(
            decision
                .selected_specialists
                .contains(&"concurrency".to_string())
        );
        assert!(!decision.dynamic_fallback);

        // Without the shared lesson the same input falls back.
        let bare = classifier.classify(&crate::domain::routing::RoutingInput {
            descriptors: vec!["Tune the shared queue".to_string()],
            changed_paths: vec!["src/queue/mod.rs".to_string()],
            deterministic_tags: vec!["correctness".to_string(), "race".to_string()],
            ..RoutingInput::default()
        });
        assert!(bare.dynamic_fallback);
    }

    #[test]
    fn cross_contributor_observations_form_one_group() {
        // Observations from different contributors feed the same group;
        // the lesson author is whoever triggered maintenance.
        let (fixture, ctx) = repo_with_observations("lessons-cross-contributor", 3, 0);
        let store = RecordStore::new(&fixture.root);
        for _ in 0..2 {
            let mut record = observation(
                "concurrency",
                "race",
                FeedbackOutcome::Fixed,
                &["src/queue/mod.rs"],
            );
            record.author = "bob".to_string();
            store.create(&ProjectRecord::Observation(record)).unwrap();
        }
        let paths = storage_paths(&ctx);
        let records = records_of(&fixture);
        let summary = run_learning_maintenance(&ctx, Some(&paths), &records, Some("bob"));
        assert_eq!(summary.lessons_created.len(), 1);
        let lesson = &lessons(&fixture)[0];
        assert_eq!(lesson.author, "bob");
        assert_eq!(lesson.source_observation_ids.len(), 5);
    }
}
