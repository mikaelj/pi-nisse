//! Knowledge consolidation (plan sections 52–56; milestone 22).
//!
//! One conceptual operation — `consolidate(records)` — compacts the
//! *semantic* duplication of durable project knowledge without losing
//! provenance. It applies uniformly to one contributor's records and to
//! multiple contributors' records: authorship affects only the
//! provenance of the created records, never the consolidation
//! semantics (plan section 52). Observations are never compacted away:
//! they remain the authoritative raw learning data forever (plan
//! section 53; no observation archival exists in Nisse 1.0).
//!
//! # Fact consolidation (plan sections 54 and 55)
//!
//! 1. **Select active facts**: non-superseded supported facts.
//!    Superseded facts are provenance; unresolved-conflict facts are
//!    already preserved conflicts and not normal project truth.
//! 2. **Remove deterministic exact duplicates**: active facts sharing
//!    one normalized `(subject, text)` statement are consolidated
//!    *deterministically* — the statement is already agreed, so the
//!    merged candidate is the representative's verbatim statement and
//!    only the union of tags/evidence is computed. Permanent invariant
//!    10 still applies: the deterministic candidate is independently
//!    verified before anything becomes durable.
//! 3. **Create semantic candidate groups**: the remaining distinct
//!    statements are grouped by normalized subject (the deterministic
//!    lexical signal; optional Needle/embedding similarity remains an
//!    optional refinement that may only widen these groups). Groups of
//!    one are unrelated and untouched.
//! 4. **Classify the relation** deterministically from what the
//!    semantic jobs answer:
//!    - synthesis `MERGED` + verifier `supported` → **compatible**;
//!    - synthesis `CANNOT_MERGE`, or a contradicted merge candidate →
//!      **conflicting**;
//!    - verifier `insufficient` or a failed session → **uncertain**.
//! 5. **Synthesis** (compatible candidates): one fresh Pi session
//!    produces *the narrowest statement supported by the supplied
//!    facts and their evidence* (`MERGED <candidate>`) or refuses
//!    (`CANNOT_MERGE`). It is never instructed to invent a
//!    compromise.
//! 6. **Independent verification**: a separate fresh verifier session
//!    judges the merged candidate against the union of the group's
//!    evidence (permanent invariant 10).
//! 7. **Persist**: only a `supported` verified candidate becomes a
//!    durable supported fact under `.nisse/common/`.
//! 8. **Preserve originals**: every source record remains stored; the
//!    common record supersedes them so the *active* set is what
//!    compacts.
//! 9. **Provenance**: `source_record_ids` and `supersedes` name every
//!    source record.
//!
//! Conflicts (plan section 55) are preserved, never compromised away:
//! each conflicting claim is verified separately against evidence
//! rebuilt from the *current* target snapshot; only when the evidence
//! clearly supports exactly one claim — one `supported`, every other
//! `contradicted` — is the conflict resolved by a common fact carrying
//! the winning claim *verbatim*. Otherwise the conflict is retained:
//! the conflicting facts remain separate and active, and nothing is
//! written.
//!
//! # Lesson consolidation (plan section 56)
//!
//! Compatible lessons — active lessons whose source observations all
//! belong to one `(reviewer, category)` group — merge deterministically
//! with no LLM at all:
//!
//! 1. union and deduplicate `source_observation_ids`;
//! 2. reload the unique original observations;
//! 3. recompute the statistics from those unique observations (never
//!    `merged_support = support_A + support_B`: overlapping evidence
//!    is counted once — permanent invariant 7);
//! 4. recompute the deterministic routing hint and wording;
//! 5. create the common lesson superseding the originals, which remain
//!    stored together with every observation.
//!
//! Every failure is conservative: uncertainty retains the status quo,
//! never guessed truth (permanent invariant 21).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crate::domain::SCHEMA_VERSION;
use crate::domain::evidence::{EvidenceKind, EvidenceRef};
use crate::domain::facts::{FactVerificationResult, FactVerificationVerdict};
use crate::domain::ids::new_ulid;
use crate::domain::model::ThinkingLevel;
use crate::domain::records::{
    FactRecord, FactState, LessonRecord, MAX_EVIDENCE_REFS, MAX_ID_REFS, MAX_OBSERVATION_REFS,
    MAX_TAGS, ObservationRecord, ProjectRecord,
};
use crate::domain::time::now_utc_rfc3339;
use crate::git::{ObjectFormat, RepoContext, SnapshotReader};
use crate::jury::host::HostProcess;
use crate::jury::protocol::{
    FactSynthesisInput, FactSynthesisPayload, FactSynthesisResult, FactVerificationPayload,
    MAX_FACT_SYNTHESIS_FACTS,
};
use crate::learning::facts::{
    FACT_VERIFICATION_TIMEOUT_MS, MAX_FACT_EVIDENCE, fact_duplicate_key, normalize_key_text,
};
use crate::learning::lessons::{build_lesson_record, compute_group_stats};
use crate::learning::staleness::refresh_evidence;
use crate::storage::records::{RecordStore, StoredRecord, active_records};

/// Timeout of one fact-synthesis session.
pub const FACT_SYNTHESIS_TIMEOUT_MS: u64 = 120_000;

/// Slack added to a session timeout for driving one job to completion
/// (mirrors the other narrow-job exchanges).
const DRIVER_SLACK_MS: u64 = 10_000;

/// Maximum members of one consolidation group: bounded both by the
/// synthesis payload bound and by the record `supersedes` /
/// `source_record_ids` bound.
pub const MAX_GROUP_MEMBERS: usize = MAX_FACT_SYNTHESIS_FACTS;

/// Bounds and model selection of the consolidation jobs.
#[derive(Debug, Clone)]
pub struct ConsolidationOptions {
    /// Explicitly configured model profile for the synthesis and
    /// verifier sessions. Sessions never inherit a model.
    pub model_profile: String,
    /// Explicit thinking level for the sessions.
    pub thinking_level: ThinkingLevel,
    /// Per-job fact-synthesis session timeout (milliseconds).
    pub synthesis_timeout_ms: u64,
    /// Per-job verification session timeout (milliseconds).
    pub verification_timeout_ms: u64,
}

impl Default for ConsolidationOptions {
    fn default() -> Self {
        ConsolidationOptions {
            model_profile: "standard".to_string(),
            thinking_level: ThinkingLevel::Medium,
            synthesis_timeout_ms: FACT_SYNTHESIS_TIMEOUT_MS,
            verification_timeout_ms: FACT_VERIFICATION_TIMEOUT_MS,
        }
    }
}

/// The deterministic relation of one fact group (plan section 54
/// step 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactRelation {
    /// The group shares one normalized statement (deterministic
    /// duplicates).
    Duplicate,
    /// A merged statement was synthesized and independently verified.
    Compatible,
    /// The statements disagree; the conflict is preserved.
    Conflicting,
    /// The relationship could not be decided; nothing is consolidated.
    Uncertain,
}

impl FactRelation {
    /// Frozen label used in reports.
    pub fn as_str(self) -> &'static str {
        match self {
            FactRelation::Duplicate => "duplicate",
            FactRelation::Compatible => "compatible",
            FactRelation::Conflicting => "conflicting",
            FactRelation::Uncertain => "uncertain",
        }
    }
}

/// One consolidation candidate group of facts.
#[derive(Debug, Clone, PartialEq)]
pub struct FactConsolidationGroup {
    /// The representative subject as recorded by the earliest member.
    pub subject: String,
    /// The members, sorted by `(created_at, id)`; the first is the
    /// representative.
    pub members: Vec<FactRecord>,
}

/// One compatible lesson group with its unique original observations
/// reloaded (plan section 56 steps 1–3).
#[derive(Debug, Clone, PartialEq)]
pub struct LessonConsolidationGroup {
    /// The reviewer of the group's findings.
    pub reviewer_id: String,
    /// The finding category of the group.
    pub category: String,
    /// The member lessons, sorted by `(created_at, id)`.
    pub members: Vec<LessonRecord>,
    /// The unique original observations of the unioned
    /// `source_observation_ids`, sorted by `(created_at, id)`.
    pub observations: Vec<ObservationRecord>,
}

/// The deterministic consolidation work derived from the durable
/// records. Pure: no sessions run during planning.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConsolidationPlan {
    /// Exact-duplicate fact groups (≥ 2 members sharing one normalized
    /// statement).
    pub exact_duplicates: Vec<FactConsolidationGroup>,
    /// Semantic fact groups (≥ 2 distinct statements sharing one
    /// normalized subject).
    pub semantic_groups: Vec<FactConsolidationGroup>,
    /// Compatible lesson groups (≥ 2 active lessons of one
    /// `(reviewer, category)` observation group).
    pub lesson_groups: Vec<LessonConsolidationGroup>,
    /// Human-readable planning diagnostics (skipped oversize groups,
    /// lessons with missing observations, …).
    pub diagnostics: Vec<String>,
}

impl ConsolidationPlan {
    /// Whether any fact work needs semantic sessions. Lesson
    /// consolidation is fully deterministic and needs no host.
    pub fn needs_host(&self) -> bool {
        !(self.exact_duplicates.is_empty() && self.semantic_groups.is_empty())
    }

    /// Whether the plan contains no work at all.
    pub fn is_empty(&self) -> bool {
        self.exact_duplicates.is_empty()
            && self.semantic_groups.is_empty()
            && self.lesson_groups.is_empty()
    }
}

/// Deterministic record ordering key: `(created_at, id)`.
trait RecordOrdering {
    fn ordering_key(&self) -> (&str, &str);
}

impl RecordOrdering for FactRecord {
    fn ordering_key(&self) -> (&str, &str) {
        (&self.created_at, &self.id)
    }
}

impl RecordOrdering for LessonRecord {
    fn ordering_key(&self) -> (&str, &str) {
        (&self.created_at, &self.id)
    }
}

impl RecordOrdering for ObservationRecord {
    fn ordering_key(&self) -> (&str, &str) {
        (&self.created_at, &self.id)
    }
}

/// Derive the deterministic consolidation plan of one record set (plan
/// sections 54 steps 1–3 and 56).
///
/// Grouping is deterministic: exact duplicates by normalized
/// `(subject, text)`, semantic candidates by normalized subject,
/// lessons by the `(reviewer, category)` key of their unique source
/// observations. Optional Needle/embedding similarity may only widen
/// the semantic groups later; the baseline never needs it.
pub fn plan_consolidation(records: &[StoredRecord]) -> ConsolidationPlan {
    let mut plan = ConsolidationPlan::default();

    // ---- Facts (plan section 54 steps 1–3) ---------------------------
    let mut active_facts: Vec<&FactRecord> = active_records(records)
        .into_iter()
        .filter_map(|stored| match &stored.record {
            ProjectRecord::Fact(fact) if fact.state == FactState::Supported => Some(fact),
            _ => None,
        })
        .collect();
    active_facts.sort_by(|a, b| a.ordering_key().cmp(&b.ordering_key()));

    // Step 2: deterministic exact duplicates.
    let mut by_statement: BTreeMap<String, Vec<&FactRecord>> = BTreeMap::new();
    for fact in &active_facts {
        by_statement
            .entry(fact_duplicate_key(&fact.subject, &fact.text))
            .or_default()
            .push(fact);
    }
    let mut consumed: BTreeSet<String> = BTreeSet::new();
    for (key, group) in &by_statement {
        if group.len() < 2 {
            continue;
        }
        if group.len() > MAX_GROUP_MEMBERS {
            plan.diagnostics.push(format!(
                "{} fact records share the statement `{key}`; groups are bounded by \
                 {MAX_GROUP_MEMBERS} members and were left untouched",
                group.len()
            ));
            for fact in group {
                consumed.insert(fact.id.clone());
            }
            continue;
        }
        plan.exact_duplicates.push(FactConsolidationGroup {
            subject: group[0].subject.clone(),
            members: group.iter().map(|fact| (*fact).clone()).collect(),
        });
        for fact in group {
            consumed.insert(fact.id.clone());
        }
    }

    // Step 3: semantic candidate groups of the remaining distinct
    // statements (normalized subject: the deterministic lexical
    // signal).
    let mut by_subject: BTreeMap<String, Vec<&FactRecord>> = BTreeMap::new();
    for fact in &active_facts {
        if consumed.contains(&fact.id) {
            continue;
        }
        by_subject
            .entry(normalize_key_text(&fact.subject))
            .or_default()
            .push(fact);
    }
    for (subject, group) in by_subject {
        if group.len() < 2 {
            continue;
        }
        if group.len() > MAX_GROUP_MEMBERS {
            plan.diagnostics.push(format!(
                "{} fact records share the subject `{subject}`; groups are bounded by \
                 {MAX_GROUP_MEMBERS} members and were left untouched",
                group.len()
            ));
            continue;
        }
        plan.semantic_groups.push(FactConsolidationGroup {
            subject: group[0].subject.clone(),
            members: group.iter().map(|fact| (*fact).clone()).collect(),
        });
    }

    // ---- Lessons (plan section 56) ------------------------------------
    plan_lesson_consolidation(records, &mut plan);
    plan
}

/// Lesson planning: group active lessons whose unique source
/// observations form one `(reviewer, category)` group.
fn plan_lesson_consolidation(records: &[StoredRecord], plan: &mut ConsolidationPlan) {
    let observations: BTreeMap<&str, &ObservationRecord> = records
        .iter()
        .filter_map(|stored| match &stored.record {
            ProjectRecord::Observation(observation) => Some((observation.id.as_str(), observation)),
            _ => None,
        })
        .collect();

    let mut active_lessons: Vec<&LessonRecord> = active_records(records)
        .into_iter()
        .filter_map(|stored| match &stored.record {
            ProjectRecord::Lesson(lesson) => Some(lesson),
            _ => None,
        })
        .collect();
    active_lessons.sort_by(|a, b| a.ordering_key().cmp(&b.ordering_key()));

    // Compatible lessons share the single (reviewer, category) key of
    // their complete observation set.
    let mut keyed: BTreeMap<(String, String), Vec<&LessonRecord>> = BTreeMap::new();
    for lesson in &active_lessons {
        let mut group_key: Option<(String, String)> = None;
        let mut usable = true;
        for id in &lesson.source_observation_ids {
            let Some(observation) = observations.get(id.as_str()) else {
                plan.diagnostics.push(format!(
                    "lesson {} references observation {id}, which is not stored; the lesson \
                     was left untouched",
                    lesson.id
                ));
                usable = false;
                break;
            };
            let key = (
                observation.reviewer_id.clone(),
                observation.category.clone(),
            );
            match &group_key {
                None => group_key = Some(key),
                Some(existing) if existing == &key => {}
                Some(existing) => {
                    plan.diagnostics.push(format!(
                        "lesson {} spans multiple finding groups ({}/{} and {}/{}); it was \
                         left untouched",
                        lesson.id, existing.0, existing.1, key.0, key.1
                    ));
                    usable = false;
                    break;
                }
            }
        }
        if let (true, Some(key)) = (usable, group_key) {
            keyed.entry(key).or_default().push(lesson);
        }
    }

    for ((reviewer_id, category), members) in keyed {
        if members.len() < 2 {
            continue;
        }
        if members.len() > MAX_ID_REFS {
            plan.diagnostics.push(format!(
                "{} lessons share the group {reviewer_id}/{category}; groups are bounded by \
                 {MAX_ID_REFS} members and were left untouched",
                members.len()
            ));
            continue;
        }
        // Union the observation IDs, deduplicate, and reload the
        // unique originals (plan section 56 steps 1–3).
        let mut union: BTreeMap<&str, &ObservationRecord> = BTreeMap::new();
        for lesson in &members {
            for id in &lesson.source_observation_ids {
                if let Some(observation) = observations.get(id.as_str()) {
                    union.insert(id.as_str(), observation);
                }
            }
        }
        let mut unique: Vec<&ObservationRecord> = union.into_values().collect();
        unique.sort_by(|a, b| a.ordering_key().cmp(&b.ordering_key()));
        if unique.len() > MAX_OBSERVATION_REFS {
            plan.diagnostics.push(format!(
                "the lessons of group {reviewer_id}/{category} reference {} unique \
                 observations; lessons are bounded by {MAX_OBSERVATION_REFS} and were left \
                 untouched",
                unique.len()
            ));
            continue;
        }
        plan.lesson_groups.push(LessonConsolidationGroup {
            reviewer_id,
            category,
            members: members.iter().map(|lesson| (*lesson).clone()).collect(),
            observations: unique.into_iter().cloned().collect(),
        });
    }
}

/// The outcome of one fact consolidation group.
#[derive(Debug, Clone, PartialEq)]
pub struct FactGroupOutcome {
    /// The representative subject.
    pub subject: String,
    /// The deterministic relation of the group.
    pub relation: FactRelation,
    /// The source record IDs, in member order.
    pub source_record_ids: Vec<String>,
    /// The created common record, when one was.
    pub common_record_id: Option<String>,
    /// Human-readable detail of what happened and why.
    pub detail: String,
}

/// The outcome of one lesson consolidation group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LessonGroupOutcome {
    /// The reviewer of the group.
    pub reviewer_id: String,
    /// The finding category of the group.
    pub category: String,
    /// The source lesson record IDs, in member order.
    pub source_record_ids: Vec<String>,
    /// The created common record, when one was.
    pub common_record_id: Option<String>,
    /// Number of unique observations the common lesson is derived from.
    pub unique_observations: usize,
    /// Human-readable detail.
    pub detail: String,
}

/// Summary of one consolidation run.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConsolidationSummary {
    /// One outcome per fact group, in deterministic plan order
    /// (exact duplicates first, then semantic groups).
    pub fact_groups: Vec<FactGroupOutcome>,
    /// One outcome per lesson group, in deterministic plan order.
    pub lesson_groups: Vec<LessonGroupOutcome>,
    /// Human-readable diagnostics.
    pub diagnostics: Vec<String>,
}

impl ConsolidationSummary {
    /// How many common facts were created.
    pub fn common_facts_created(&self) -> usize {
        self.fact_groups
            .iter()
            .filter(|group| group.common_record_id.is_some())
            .count()
    }

    /// How many conflicts remain unresolved (conflicting groups that
    /// created nothing).
    pub fn conflicts_retained(&self) -> usize {
        self.fact_groups
            .iter()
            .filter(|group| {
                group.relation == FactRelation::Conflicting && group.common_record_id.is_none()
            })
            .count()
    }

    /// How many common lessons were created.
    pub fn common_lessons_created(&self) -> usize {
        self.lesson_groups
            .iter()
            .filter(|group| group.common_record_id.is_some())
            .count()
    }
}

/// Execute one planned consolidation (plan sections 54–56).
///
/// `host` is required only for fact work
/// ([`ConsolidationPlan::needs_host`]); lesson consolidation is fully
/// deterministic. Every semantic failure is conservative: the group is
/// left untouched and a diagnostic is recorded — uncertainty produces
/// less automation, never guessed truth.
pub fn run_consolidation(
    host: Option<&mut HostProcess>,
    ctx: &RepoContext,
    reader: &SnapshotReader<'_>,
    format: ObjectFormat,
    plan: &ConsolidationPlan,
    author: &str,
    options: &ConsolidationOptions,
) -> ConsolidationSummary {
    let mut summary = ConsolidationSummary {
        diagnostics: plan.diagnostics.clone(),
        ..ConsolidationSummary::default()
    };
    let store = RecordStore::new(&ctx.worktree_root);
    let created_at = now_utc_rfc3339();
    let mut host = host;

    for group in &plan.exact_duplicates {
        consolidate_exact_duplicates(
            host.as_deref_mut(),
            &store,
            group,
            author,
            options,
            &created_at,
            &mut summary,
        );
    }
    for group in &plan.semantic_groups {
        consolidate_semantic_group(
            host.as_deref_mut(),
            &store,
            reader,
            format,
            group,
            author,
            options,
            &created_at,
            &mut summary,
        );
    }
    for group in &plan.lesson_groups {
        consolidate_lesson_group(&store, group, author, &created_at, &mut summary);
    }
    summary
}

/// Deterministic consolidation of one exact-duplicate group (plan
/// section 54 step 2): the statement is already agreed, so the merged
/// candidate is the representative's verbatim statement and only the
/// union of tags and evidence is computed. Independent verification
/// still gates durability (permanent invariant 10).
#[allow(clippy::too_many_arguments)]
fn consolidate_exact_duplicates(
    host: Option<&mut HostProcess>,
    store: &RecordStore,
    group: &FactConsolidationGroup,
    author: &str,
    options: &ConsolidationOptions,
    created_at: &str,
    summary: &mut ConsolidationSummary,
) {
    let representative = &group.members[0];
    let member_ids: Vec<String> = group.members.iter().map(|fact| fact.id.clone()).collect();
    let (evidence, truncated) = union_evidence(&group.members);
    if truncated {
        summary.diagnostics.push(format!(
            "the evidence union of the `{}` duplicate group was truncated to the \
             {MAX_EVIDENCE_REFS}-entry record bound",
            group.subject
        ));
    }
    let tags = union_tags(&group.members);

    let Some(host) = host else {
        summary.diagnostics.push(format!(
            "the duplicate group `{}` was left untouched: no semantic host was available",
            group.subject
        ));
        summary.fact_groups.push(FactGroupOutcome {
            subject: group.subject.clone(),
            relation: FactRelation::Uncertain,
            source_record_ids: member_ids,
            common_record_id: None,
            detail: "no host available".to_string(),
        });
        return;
    };

    // Independent verification of the agreed statement against the
    // union of the group's evidence (permanent invariant 10).
    let verification = verify_claim(host, &representative.text, &evidence, options);
    let detail = match verification.verdict {
        FactVerificationVerdict::Supported => {
            let record = common_fact_record(
                author,
                created_at,
                representative.subject.clone(),
                representative.text.clone(),
                tags,
                evidence,
                &member_ids,
            );
            match store.create_common(&ProjectRecord::Fact(record)) {
                Ok(stored) => {
                    summary.fact_groups.push(FactGroupOutcome {
                        subject: group.subject.clone(),
                        relation: FactRelation::Duplicate,
                        source_record_ids: member_ids,
                        common_record_id: Some(stored.id().to_string()),
                        detail: format!(
                            "consolidated {} identical statement(s) verbatim",
                            group.members.len()
                        ),
                    });
                    return;
                }
                Err(error) => format!("durable common fact creation failed: {error}"),
            }
        }
        FactVerificationVerdict::Contradicted => {
            "independent verification contradicted the shared statement; the originals were \
             retained"
                .to_string()
        }
        FactVerificationVerdict::Insufficient => verification
            .failure
            .map(|failure| format!("verification could not decide ({failure})"))
            .unwrap_or_else(|| "verification found the union of evidence insufficient".to_string()),
    };
    summary.diagnostics.push(format!(
        "the duplicate group `{}` was left untouched: {detail}",
        group.subject
    ));
    summary.fact_groups.push(FactGroupOutcome {
        subject: group.subject.clone(),
        relation: FactRelation::Duplicate,
        source_record_ids: member_ids,
        common_record_id: None,
        detail,
    });
}

/// Consolidation of one semantic group (plan sections 54 steps 4–7 and
/// 55).
#[allow(clippy::too_many_arguments)]
fn consolidate_semantic_group(
    host: Option<&mut HostProcess>,
    store: &RecordStore,
    reader: &SnapshotReader<'_>,
    format: ObjectFormat,
    group: &FactConsolidationGroup,
    author: &str,
    options: &ConsolidationOptions,
    created_at: &str,
    summary: &mut ConsolidationSummary,
) {
    let member_ids: Vec<String> = group.members.iter().map(|fact| fact.id.clone()).collect();
    let Some(host) = host else {
        summary.diagnostics.push(format!(
            "the fact group `{}` was left untouched: no semantic host was available",
            group.subject
        ));
        summary.fact_groups.push(FactGroupOutcome {
            subject: group.subject.clone(),
            relation: FactRelation::Uncertain,
            source_record_ids: member_ids,
            common_record_id: None,
            detail: "no host available".to_string(),
        });
        return;
    };

    // Step 5: one fresh synthesis session over the group's claims and
    // evidence.
    let payload = FactSynthesisPayload {
        synthesis_id: new_ulid(),
        timeout_ms: options.synthesis_timeout_ms,
        model_profile: options.model_profile.clone(),
        thinking_level: options.thinking_level,
        subject: group.subject.clone(),
        facts: group
            .members
            .iter()
            .map(|fact| FactSynthesisInput {
                text: fact.text.clone(),
                evidence: truncate(fact.evidence.clone(), MAX_FACT_EVIDENCE),
            })
            .collect(),
    };
    if let Err(error) = payload.validate() {
        summary.diagnostics.push(format!(
            "the fact group `{}` was left untouched: the synthesis payload was invalid ({error})",
            group.subject
        ));
        summary.fact_groups.push(FactGroupOutcome {
            subject: group.subject.clone(),
            relation: FactRelation::Uncertain,
            source_record_ids: member_ids,
            common_record_id: None,
            detail: format!("invalid synthesis payload: {error}"),
        });
        return;
    }
    let synthesis = host.start_fact_synthesis(&payload).and_then(|()| {
        host.complete_fact_synthesis(
            &payload.synthesis_id,
            Duration::from_millis(options.synthesis_timeout_ms + DRIVER_SLACK_MS),
        )
    });
    let synthesis = match synthesis {
        Ok(outcome) => outcome,
        Err(error) => {
            let detail = format!("the synthesis session failed: {error}");
            summary.diagnostics.push(format!(
                "the fact group `{}` was left untouched: {detail}",
                group.subject
            ));
            summary
                .fact_groups
                .push(uncertain(group, member_ids, detail));
            return;
        }
    };
    let candidate = match synthesis.result {
        Some(FactSynthesisResult::Merged { candidate }) => candidate,
        Some(FactSynthesisResult::CannotMerge) => {
            // The statements disagree: conflict handling (plan
            // section 55).
            handle_conflict(
                host,
                store,
                reader,
                format,
                group,
                member_ids,
                author,
                options,
                created_at,
                "synthesis refused to merge the statements",
                summary,
            );
            return;
        }
        None => {
            let reason = synthesis
                .error
                .map(|error| format!("{}: {}", error.code, error.message))
                .unwrap_or_else(|| "unknown failure".to_string());
            let detail = format!("the synthesis session failed: {reason}");
            summary.diagnostics.push(format!(
                "the fact group `{}` was left untouched: {detail}",
                group.subject
            ));
            summary
                .fact_groups
                .push(uncertain(group, member_ids, detail));
            return;
        }
    };

    // Step 6: independent verification of the merged candidate against
    // the union of the group's evidence.
    let (union, truncated) = union_evidence(&group.members);
    if truncated {
        summary.diagnostics.push(format!(
            "the evidence union of the `{}` fact group was truncated to the \
             {MAX_EVIDENCE_REFS}-entry record bound",
            group.subject
        ));
    }
    let verification = verify_claim(host, &candidate, &union, options);
    match verification.verdict {
        FactVerificationVerdict::Supported => {
            // Step 7: only a supported verified candidate becomes a
            // durable common fact.
            let record = common_fact_record(
                author,
                created_at,
                group.subject.clone(),
                candidate.clone(),
                union_tags(&group.members),
                union,
                &member_ids,
            );
            let detail = format!(
                "merged {} statements into one verified common fact",
                group.members.len()
            );
            match store.create_common(&ProjectRecord::Fact(record)) {
                Ok(stored) => summary.fact_groups.push(FactGroupOutcome {
                    subject: group.subject.clone(),
                    relation: FactRelation::Compatible,
                    source_record_ids: member_ids,
                    common_record_id: Some(stored.id().to_string()),
                    detail,
                }),
                Err(error) => {
                    let detail = format!("durable common fact creation failed: {error}");
                    summary.diagnostics.push(format!(
                        "the fact group `{}` was left untouched: {detail}",
                        group.subject
                    ));
                    summary.fact_groups.push(FactGroupOutcome {
                        subject: group.subject.clone(),
                        relation: FactRelation::Compatible,
                        source_record_ids: member_ids,
                        common_record_id: None,
                        detail,
                    });
                }
            }
        }
        FactVerificationVerdict::Contradicted => {
            // The merged statement is contradicted: the claims are
            // conflicting, not compatible — conflict handling.
            handle_conflict(
                host,
                store,
                reader,
                format,
                group,
                member_ids,
                author,
                options,
                created_at,
                "independent verification contradicted the merged statement",
                summary,
            );
        }
        FactVerificationVerdict::Insufficient => {
            let detail = verification
                .failure
                .map(|failure| format!("verification could not decide ({failure})"))
                .unwrap_or_else(|| {
                    "verification found the union of evidence insufficient".to_string()
                });
            summary.diagnostics.push(format!(
                "the fact group `{}` was left untouched: {detail}",
                group.subject
            ));
            summary
                .fact_groups
                .push(uncertain(group, member_ids, detail));
        }
    }
}

/// Conflict handling of one fact group (plan section 55): preserve the
/// conflict, verify each claim separately against evidence rebuilt
/// from the *current* target snapshot, resolve only when the evidence
/// clearly supports exactly one claim, and otherwise retain the
/// unresolved conflict — the originals stay separate and active, and
/// nothing is written.
#[allow(clippy::too_many_arguments)]
fn handle_conflict(
    host: &mut HostProcess,
    store: &RecordStore,
    reader: &SnapshotReader<'_>,
    format: ObjectFormat,
    group: &FactConsolidationGroup,
    member_ids: Vec<String>,
    author: &str,
    options: &ConsolidationOptions,
    created_at: &str,
    reason: &str,
    summary: &mut ConsolidationSummary,
) {
    // Steps 2–3: retrieve current evidence where applicable and verify
    // each claim separately.
    let mut supported: Vec<(usize, Vec<EvidenceRef>)> = Vec::new();
    let mut supported_count = 0usize;
    let mut contradicted_count = 0usize;
    let mut verdict_labels: Vec<&'static str> = Vec::new();
    for (index, fact) in group.members.iter().enumerate() {
        let refreshed = refresh_evidence(reader, format, fact);
        let has_current_source = refreshed
            .evidence
            .iter()
            .any(|entry| entry.kind == EvidenceKind::Source);
        let verification = if has_current_source {
            verify_claim(host, &fact.text, &refreshed.evidence, options)
        } else {
            FactVerificationResult {
                verification_id: String::new(),
                verdict: FactVerificationVerdict::Insufficient,
                failure: Some(
                    "the supporting content is absent from the current target snapshot".to_string(),
                ),
            }
        };
        match verification.verdict {
            FactVerificationVerdict::Supported => {
                supported_count += 1;
                supported.push((index, refreshed.evidence));
                verdict_labels.push("supported");
            }
            FactVerificationVerdict::Contradicted => {
                contradicted_count += 1;
                verdict_labels.push("contradicted");
            }
            FactVerificationVerdict::Insufficient => verdict_labels.push("insufficient"),
        }
    }

    // Step 4: resolve only when the evidence clearly supports exactly
    // one claim — one supported, every other contradicted.
    if supported_count == 1 && contradicted_count == group.members.len() - 1 {
        let (winner_index, evidence) = supported.remove(0);
        let winner = &group.members[winner_index];
        // The winning formulation is carried verbatim — never a
        // compromise invented to reduce record count.
        let record = common_fact_record(
            author,
            created_at,
            winner.subject.clone(),
            winner.text.clone(),
            winner.tags.clone(),
            evidence,
            &member_ids,
        );
        match store.create_common(&ProjectRecord::Fact(record)) {
            Ok(stored) => {
                summary.fact_groups.push(FactGroupOutcome {
                    subject: group.subject.clone(),
                    relation: FactRelation::Conflicting,
                    source_record_ids: member_ids,
                    common_record_id: Some(stored.id().to_string()),
                    detail: format!(
                        "{reason}; the conflict was resolved in favor of record {} whose \
                         claim the current evidence clearly supports",
                        winner.id
                    ),
                });
                return;
            }
            Err(error) => {
                let detail = format!(
                    "{reason}; resolution failed to persist: {error}; the originals were \
                     retained"
                );
                summary.diagnostics.push(format!(
                    "the conflict group `{}` was left untouched: {detail}",
                    group.subject
                ));
                summary.fact_groups.push(FactGroupOutcome {
                    subject: group.subject.clone(),
                    relation: FactRelation::Conflicting,
                    source_record_ids: member_ids,
                    common_record_id: None,
                    detail,
                });
                return;
            }
        }
    }

    // Step 5: retain the unresolved conflict.
    let detail = format!(
        "{reason}; per-claim verification answered [{}]; the conflict is retained unresolved \
         and the originals stay separate",
        verdict_labels.join(", ")
    );
    summary.fact_groups.push(FactGroupOutcome {
        subject: group.subject.clone(),
        relation: FactRelation::Conflicting,
        source_record_ids: member_ids,
        common_record_id: None,
        detail,
    });
}

/// Deterministic consolidation of one compatible lesson group (plan
/// section 56): statistics and routing conditions are recomputed from
/// the unique original observations — never summed per lesson.
fn consolidate_lesson_group(
    store: &RecordStore,
    group: &LessonConsolidationGroup,
    author: &str,
    created_at: &str,
    summary: &mut ConsolidationSummary,
) {
    let member_ids: Vec<String> = group
        .members
        .iter()
        .map(|lesson| lesson.id.clone())
        .collect();
    let skipped = |detail: String, summary: &mut ConsolidationSummary| {
        summary.diagnostics.push(format!(
            "lesson group {}/{} was left untouched: {detail}",
            group.reviewer_id, group.category
        ));
        summary.lesson_groups.push(LessonGroupOutcome {
            reviewer_id: group.reviewer_id.clone(),
            category: group.category.clone(),
            source_record_ids: member_ids.clone(),
            common_record_id: None,
            unique_observations: group.observations.len(),
            detail,
        });
    };

    // Steps 3–5: recompute the statistics deterministically from the
    // reloaded unique observations.
    let observation_refs: Vec<&ObservationRecord> = group.observations.iter().collect();
    let stats = compute_group_stats(&observation_refs)
        .into_iter()
        .find(|stats| stats.reviewer_id == group.reviewer_id && stats.category == group.category);
    let Some(stats) = stats else {
        skipped(
            "the unioned observations no longer form one finding group".to_string(),
            summary,
        );
        return;
    };
    if !stats.meets_threshold() {
        skipped(
            format!(
                "the recomputed statistics ({} unique observations, {} positive, {} \
                 dismissed) no longer meet the frozen lesson threshold",
                stats.support, stats.positives, stats.dismissed
            ),
            summary,
        );
        return;
    }

    // Steps 6–7: create the common lesson superseding the originals
    // (which remain stored together with every observation).
    let record = build_lesson_record(&stats, author, member_ids.clone(), created_at);
    match store.create_common(&ProjectRecord::Lesson(record)) {
        Ok(stored) => summary.lesson_groups.push(LessonGroupOutcome {
            reviewer_id: group.reviewer_id.clone(),
            category: group.category.clone(),
            source_record_ids: member_ids,
            common_record_id: Some(stored.id().to_string()),
            unique_observations: stats.observation_ids.len(),
            detail: format!(
                "merged {} lessons over {} unique observations with recomputed statistics",
                group.members.len(),
                stats.observation_ids.len()
            ),
        }),
        Err(error) => {
            skipped(
                format!("durable common lesson creation failed: {error}"),
                summary,
            );
        }
    }
}

/// One uncertain fact-group outcome.
fn uncertain(
    group: &FactConsolidationGroup,
    member_ids: Vec<String>,
    detail: String,
) -> FactGroupOutcome {
    FactGroupOutcome {
        subject: group.subject.clone(),
        relation: FactRelation::Uncertain,
        source_record_ids: member_ids,
        common_record_id: None,
        detail,
    }
}

/// Run one independent verification session of `claim` against
/// `evidence`. A session failure is forced `insufficient` (the same
/// fail-safe as candidate verification).
fn verify_claim(
    host: &mut HostProcess,
    claim: &str,
    evidence: &[EvidenceRef],
    options: &ConsolidationOptions,
) -> FactVerificationResult {
    let evidence = truncate(evidence.to_vec(), MAX_FACT_EVIDENCE);
    if evidence.is_empty() {
        return FactVerificationResult {
            verification_id: String::new(),
            verdict: FactVerificationVerdict::Insufficient,
            failure: Some("no evidence to verify against".to_string()),
        };
    }
    let payload = FactVerificationPayload {
        verification_id: new_ulid(),
        timeout_ms: options.verification_timeout_ms,
        model_profile: options.model_profile.clone(),
        thinking_level: options.thinking_level,
        claim: claim.to_string(),
        evidence,
    };
    match host.start_fact_verification(&payload).and_then(|()| {
        host.complete_fact_verification(
            &payload.verification_id,
            Duration::from_millis(options.verification_timeout_ms + DRIVER_SLACK_MS),
        )
    }) {
        Ok(result) => result,
        Err(error) => FactVerificationResult {
            verification_id: payload.verification_id,
            verdict: FactVerificationVerdict::Insufficient,
            failure: Some(error.to_string()),
        },
    }
}

/// Build the durable common fact of a consolidation (plan section 54
/// steps 7–9): provenance names every source record, and the new
/// record supersedes them (the originals remain stored).
#[allow(clippy::too_many_arguments)]
fn common_fact_record(
    author: &str,
    created_at: &str,
    subject: String,
    text: String,
    tags: Vec<String>,
    evidence: Vec<EvidenceRef>,
    member_ids: &[String],
) -> FactRecord {
    FactRecord {
        schema_version: SCHEMA_VERSION,
        id: new_ulid(),
        author: author.to_string(),
        created_at: created_at.to_string(),
        subject,
        text,
        tags,
        state: FactState::Supported,
        evidence,
        source_review_id: None,
        source_record_ids: member_ids.to_vec(),
        supersedes: member_ids.to_vec(),
    }
}

/// Union the evidence of a group's members, deduplicated by full
/// identity and bounded by the record evidence bound. Returns whether
/// truncation dropped entries.
fn union_evidence(members: &[FactRecord]) -> (Vec<EvidenceRef>, bool) {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut union: Vec<EvidenceRef> = Vec::new();
    for fact in members {
        for entry in &fact.evidence {
            let key = serde_json::to_string(entry).unwrap_or_default();
            if seen.insert(key) {
                union.push(entry.clone());
            }
        }
    }
    let truncated = union.len() > MAX_EVIDENCE_REFS;
    union.truncate(MAX_EVIDENCE_REFS);
    (union, truncated)
}

/// Union the tags of a group's members: sorted, deduplicated, bounded.
fn union_tags(members: &[FactRecord]) -> Vec<String> {
    let tags: BTreeSet<String> = members
        .iter()
        .flat_map(|fact| fact.tags.iter().cloned())
        .collect();
    tags.into_iter().take(MAX_TAGS).collect()
}

/// Truncate a vector to `bound`.
fn truncate<T>(mut values: Vec<T>, bound: usize) -> Vec<T> {
    values.truncate(bound);
    values
}

/// Render the human-readable `nisse records compact` report.
pub fn render_compact_report(summary: &ConsolidationSummary) -> String {
    let mut out = String::from("nisse records compact\n");
    if summary.fact_groups.is_empty() && summary.lesson_groups.is_empty() {
        out.push_str("no consolidatable duplication found\n");
    }
    for group in &summary.fact_groups {
        out.push_str(&format!(
            "fact group `{}` [{}] {} source record(s)\n",
            group.subject,
            group.relation.as_str(),
            group.source_record_ids.len()
        ));
        if let Some(id) = &group.common_record_id {
            out.push_str(&format!("  common fact {id} created\n"));
        }
        out.push_str(&format!("  {}\n", group.detail));
    }
    for group in &summary.lesson_groups {
        out.push_str(&format!(
            "lesson group {}/{} {} source record(s)\n",
            group.reviewer_id,
            group.category,
            group.source_record_ids.len()
        ));
        if let Some(id) = &group.common_record_id {
            out.push_str(&format!("  common lesson {id} created\n"));
        }
        out.push_str(&format!("  {}\n", group.detail));
    }
    for diagnostic in &summary.diagnostics {
        out.push_str(&format!("note: {diagnostic}\n"));
    }
    out.push_str(&format!(
        "{} common fact(s) created, {} conflict(s) retained unresolved, {} common lesson(s) \
         created\n",
        summary.common_facts_created(),
        summary.conflicts_retained(),
        summary.common_lessons_created()
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::new_ulid;
    use crate::domain::records::FeedbackOutcome;
    use crate::domain::records::testutil::{fact as build_fact, lesson as build_lesson};
    use crate::git::testutil::{Fixture, commit_all, write_file};
    use crate::git::{GitRunner, SnapshotSpec, resolve_snapshot};

    /// A fact with explicit identity and statement. `id` must be a
    /// freshly generated canonical ULID.
    fn fact(id: &str, created_at: &str, author: &str, subject: &str, text: &str) -> FactRecord {
        let mut record = build_fact(author);
        record.id = id.to_string();
        record.created_at = created_at.to_string();
        record.subject = subject.to_string();
        record.text = text.to_string();
        record
    }

    /// An observation with explicit identity and outcome.
    fn observation(
        id: &str,
        created_at: &str,
        author: &str,
        reviewer: &str,
        category: &str,
        outcome: FeedbackOutcome,
    ) -> ObservationRecord {
        let mut record = crate::domain::records::testutil::observation(author);
        record.id = id.to_string();
        record.created_at = created_at.to_string();
        record.reviewer_id = reviewer.to_string();
        record.category = category.to_string();
        record.outcome = outcome;
        record
    }

    /// A lesson referencing explicit observations.
    fn lesson(id: &str, created_at: &str, author: &str, observations: &[&str]) -> LessonRecord {
        let mut record = build_lesson(author);
        record.id = id.to_string();
        record.created_at = created_at.to_string();
        record.source_observation_ids = observations.iter().map(|id| id.to_string()).collect();
        record.routing_hint.reviewer_id = "concurrency".to_string();
        record
    }

    fn load(store: &RecordStore) -> Vec<StoredRecord> {
        store.load().unwrap()
    }

    /// Create `count` fixed race observations across `alice`/`bob` in
    /// `store` and return their generated IDs (ordered by creation).
    fn seed_race_observations(store: &RecordStore, count: usize) -> Vec<String> {
        let mut ids = Vec::new();
        for index in 0..count {
            let id = new_ulid();
            store
                .create(&ProjectRecord::Observation(observation(
                    &id,
                    &format!("2026-05-{:02}T10:00:00Z", index + 1),
                    if index % 2 == 0 { "alice" } else { "bob" },
                    "concurrency",
                    "race",
                    FeedbackOutcome::Fixed,
                )))
                .unwrap();
            ids.push(id);
        }
        ids
    }

    #[test]
    fn empty_record_sets_plan_nothing() {
        let plan = plan_consolidation(&[]);
        assert!(plan.is_empty());
        assert!(!plan.needs_host());
        assert!(
            render_compact_report(&ConsolidationSummary::default())
                .contains("no consolidatable duplication found")
        );
    }

    #[test]
    fn exact_duplicate_detection_normalizes_statements() {
        let dir = tempfile::tempdir().unwrap();
        let store = RecordStore::new(dir.path());
        let first = new_ulid();
        let second = new_ulid();
        // Two records with the same statement modulo case/whitespace,
        // from different contributors; `first` is the earliest.
        store
            .create(&ProjectRecord::Fact(fact(
                &first,
                "2026-05-01T10:00:00Z",
                "alice",
                "queue capacity",
                "The sensor queue is bounded to 32 slots.",
            )))
            .unwrap();
        store
            .create(&ProjectRecord::Fact(fact(
                &second,
                "2026-05-02T10:00:00Z",
                "bob",
                "Queue  capacity",
                "the sensor queue is  bounded to 32 slots.",
            )))
            .unwrap();

        let plan = plan_consolidation(&load(&store));
        assert_eq!(plan.exact_duplicates.len(), 1);
        let group = &plan.exact_duplicates[0];
        assert_eq!(group.members.len(), 2);
        // The earliest record by (created_at, id) is the representative.
        assert_eq!(group.members[0].id, first);
        assert_eq!(group.subject, "queue capacity");
        assert!(
            plan.semantic_groups.is_empty(),
            "{:?}",
            plan.semantic_groups
        );
        assert!(plan.needs_host());
    }

    #[test]
    fn semantic_groups_share_a_normalized_subject() {
        let dir = tempfile::tempdir().unwrap();
        let store = RecordStore::new(dir.path());
        let bounded = new_ulid();
        let mutexed = new_ulid();
        let audio = new_ulid();
        let superseded = new_ulid();
        let conflicting = new_ulid();
        let fresh = new_ulid();
        store
            .create(&ProjectRecord::Fact(fact(
                &bounded,
                "2026-05-01T10:00:00Z",
                "alice",
                "queue capacity",
                "The sensor queue is bounded to 32 slots.",
            )))
            .unwrap();
        store
            .create(&ProjectRecord::Fact(fact(
                &mutexed,
                "2026-05-02T10:00:00Z",
                "bob",
                "Queue Capacity",
                "The queue capacity is guarded by a mutex.",
            )))
            .unwrap();
        // Different subject: unrelated.
        store
            .create(&ProjectRecord::Fact(fact(
                &audio,
                "2026-05-03T10:00:00Z",
                "carol",
                "audio pipeline",
                "The audio thread never allocates.",
            )))
            .unwrap();
        // Superseded and conflicting facts never participate.
        store
            .create(&ProjectRecord::Fact(fact(
                &superseded,
                "2026-04-01T10:00:00Z",
                "alice",
                "queue capacity",
                "Old knowledge superseded.",
            )))
            .unwrap();
        let mut conflict_record = fact(
            &conflicting,
            "2026-04-02T10:00:00Z",
            "alice",
            "queue capacity",
            "A preserved conflict.",
        );
        conflict_record.state = FactState::UnresolvedConflict;
        store.create(&ProjectRecord::Fact(conflict_record)).unwrap();
        let mut fresh_record = fact(
            &fresh,
            "2026-05-04T10:00:00Z",
            "alice",
            "queue capacity",
            "Current knowledge.",
        );
        fresh_record.supersedes = vec![superseded.clone()];
        store.create(&ProjectRecord::Fact(fresh_record)).unwrap();

        let plan = plan_consolidation(&load(&store));
        assert!(plan.exact_duplicates.is_empty());
        assert_eq!(plan.semantic_groups.len(), 1);
        // The three active supported facts of the shared subject group;
        // the superseded and conflicting ones do not.
        assert_eq!(plan.semantic_groups[0].members.len(), 3);
        let ids: Vec<&str> = plan.semantic_groups[0]
            .members
            .iter()
            .map(|fact| fact.id.as_str())
            .collect();
        assert_eq!(
            ids,
            vec![bounded.as_str(), mutexed.as_str(), fresh.as_str()]
        );
    }

    #[test]
    fn lesson_groups_share_one_finding_group() {
        let dir = tempfile::tempdir().unwrap();
        let store = RecordStore::new(dir.path());
        let observations = seed_race_observations(&store, 6);
        // An observation of a different finding group.
        let migration = new_ulid();
        store
            .create(&ProjectRecord::Observation(observation(
                &migration,
                "2026-05-10T10:00:00Z",
                "alice",
                "database",
                "migration",
                FeedbackOutcome::Fixed,
            )))
            .unwrap();
        let lesson_a = new_ulid();
        let lesson_b = new_ulid();
        let lesson_other = new_ulid();
        let refs: Vec<String> = observations[..5].to_vec();
        let refs: Vec<&str> = refs.iter().map(String::as_str).collect();
        store
            .create(&ProjectRecord::Lesson(lesson(
                &lesson_a,
                "2026-05-11T10:00:00Z",
                "alice",
                &refs,
            )))
            .unwrap();
        let refs: Vec<String> = observations[2..].to_vec();
        let refs: Vec<&str> = refs.iter().map(String::as_str).collect();
        store
            .create(&ProjectRecord::Lesson(lesson(
                &lesson_b,
                "2026-05-12T10:00:00Z",
                "bob",
                &refs,
            )))
            .unwrap();
        // A lesson of a different finding group: never compatible with
        // the race lessons.
        let migration_ref = migration.clone();
        let mut other = lesson(
            &lesson_other,
            "2026-05-13T10:00:00Z",
            "alice",
            &[migration_ref.as_str()],
        );
        other.routing_hint.reviewer_id = "database".to_string();
        store.create(&ProjectRecord::Lesson(other)).unwrap();

        let plan = plan_consolidation(&load(&store));
        assert_eq!(plan.lesson_groups.len(), 1, "{:?}", plan.diagnostics);
        let group = &plan.lesson_groups[0];
        assert_eq!(group.reviewer_id, "concurrency");
        assert_eq!(group.category, "race");
        assert_eq!(group.members.len(), 2);
        // The union references 6 unique observations, not the summed 9.
        assert_eq!(group.observations.len(), 6);
        assert!(!plan.needs_host());
    }

    #[test]
    fn lessons_with_missing_observations_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let store = RecordStore::new(dir.path());
        // Only one of the referenced observations exists; the missing
        // one is a valid ULID that is simply not stored.
        let present = new_ulid();
        let missing = new_ulid();
        store
            .create(&ProjectRecord::Observation(observation(
                &present,
                "2026-05-01T10:00:00Z",
                "alice",
                "concurrency",
                "race",
                FeedbackOutcome::Fixed,
            )))
            .unwrap();
        let lesson_a = new_ulid();
        let lesson_b = new_ulid();
        store
            .create(&ProjectRecord::Lesson(lesson(
                &lesson_a,
                "2026-05-02T10:00:00Z",
                "alice",
                &[present.as_str(), missing.as_str()],
            )))
            .unwrap();
        store
            .create(&ProjectRecord::Lesson(lesson(
                &lesson_b,
                "2026-05-03T10:00:00Z",
                "bob",
                &[present.as_str()],
            )))
            .unwrap();

        let plan = plan_consolidation(&load(&store));
        assert!(plan.lesson_groups.is_empty());
        assert!(
            plan.diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains(&missing))
        );
    }

    #[test]
    fn union_evidence_and_tags_dedupe_and_bound() {
        let base = build_fact("alice");
        let a_id = new_ulid();
        let b_id = new_ulid();
        let mut a = fact(&a_id, "2026-05-01T10:00:00Z", "alice", "s", "t a");
        a.evidence = vec![base.evidence[0].clone()];
        a.tags = vec!["queue".to_string(), "shared".to_string()];
        let mut b = fact(&b_id, "2026-05-02T10:00:00Z", "bob", "s", "t b");
        // Identical evidence entry: deduplicated.
        b.evidence = a.evidence.clone();
        b.tags = vec!["mutex".to_string()];

        let (evidence, truncated) = union_evidence(&[a.clone(), b.clone()]);
        assert_eq!(evidence.len(), 1);
        assert!(!truncated);
        assert_eq!(
            union_tags(&[a, b]),
            vec![
                "mutex".to_string(),
                "queue".to_string(),
                "shared".to_string()
            ]
        );

        // Unbounded unions truncate at the record bound.
        let big_id = new_ulid();
        let mut big = fact(&big_id, "2026-05-03T10:00:00Z", "carol", "s", "t");
        big.evidence = (0..MAX_EVIDENCE_REFS + 5)
            .map(|index| {
                let mut entry = base.evidence[0].clone();
                entry.excerpt = format!("excerpt {index}");
                entry
            })
            .collect();
        let (evidence, truncated) = union_evidence(&[big]);
        assert_eq!(evidence.len(), MAX_EVIDENCE_REFS);
        assert!(truncated);
    }

    /// A repository fixture whose working snapshot is readable; the
    /// context and resolved snapshot are leaked so the reader can
    /// outlive the setup call.
    struct Repo {
        fixture: Fixture,
        ctx: &'static RepoContext,
        reader: SnapshotReader<'static>,
    }

    fn repo_with_reader(label: &str) -> Repo {
        let fixture = Fixture::repo(label);
        write_file(&fixture.root, "src/a.rs", "fn alpha() {\n    marker\n}\n");
        commit_all(&fixture.root, "one");
        let ctx: &'static RepoContext = Box::leak(Box::new(
            RepoContext::discover(&GitRunner::new(), &fixture.root).unwrap(),
        ));
        let resolved: &'static crate::git::ResolvedSnapshot = Box::leak(Box::new(
            resolve_snapshot(ctx, &SnapshotSpec::Working).unwrap(),
        ));
        let reader = SnapshotReader::new(ctx, resolved).unwrap();
        Repo {
            fixture,
            ctx,
            reader,
        }
    }

    #[test]
    fn lesson_consolidation_runs_without_a_host_and_never_double_counts() {
        let repo = repo_with_reader("consolidate-lessons");
        let store = RecordStore::new(&repo.fixture.root);
        let observations = seed_race_observations(&store, 6);
        let lesson_a = new_ulid();
        let lesson_b = new_ulid();
        let refs: Vec<String> = observations[..5].to_vec();
        let refs: Vec<&str> = refs.iter().map(String::as_str).collect();
        store
            .create(&ProjectRecord::Lesson(lesson(
                &lesson_a,
                "2026-05-11T10:00:00Z",
                "alice",
                &refs,
            )))
            .unwrap();
        let refs: Vec<String> = observations[2..].to_vec();
        let refs: Vec<&str> = refs.iter().map(String::as_str).collect();
        store
            .create(&ProjectRecord::Lesson(lesson(
                &lesson_b,
                "2026-05-12T10:00:00Z",
                "bob",
                &refs,
            )))
            .unwrap();

        let records = load(&store);
        let plan = plan_consolidation(&records);
        assert!(!plan.needs_host());
        let format = repo.ctx.object_format().unwrap();
        let summary = run_consolidation(
            None,
            repo.ctx,
            &repo.reader,
            format,
            &plan,
            "alice",
            &ConsolidationOptions::default(),
        );
        assert_eq!(summary.common_lessons_created(), 1, "{summary:?}");
        assert_eq!(summary.conflicts_retained(), 0);

        // The common lesson: 6 unique observations (never 5 + 4 = 9),
        // and provenance supersedes both originals.
        let lessons: Vec<LessonRecord> = load(&store)
            .into_iter()
            .filter_map(|stored| match stored.record {
                ProjectRecord::Lesson(lesson) => Some(lesson),
                _ => None,
            })
            .collect();
        assert_eq!(lessons.len(), 3, "the originals remain stored");
        let common = lessons
            .iter()
            .find(|lesson| lesson.supersedes.len() == 2)
            .expect("the common lesson supersedes both originals");
        assert_eq!(common.author, "alice");
        let expected: Vec<String> = observations
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        assert_eq!(common.source_observation_ids, expected);
        assert_eq!(
            common.routing_hint.boost,
            crate::domain::records::MAX_ROUTING_BOOST
        );

        // Idempotent: the second run finds one active lesson per group.
        let second_plan = plan_consolidation(&load(&store));
        assert!(second_plan.lesson_groups.is_empty());
        let second = run_consolidation(
            None,
            repo.ctx,
            &repo.reader,
            format,
            &second_plan,
            "alice",
            &ConsolidationOptions::default(),
        );
        assert_eq!(second.common_lessons_created(), 0);
        assert_eq!(load(&store).len(), 9);
    }

    #[test]
    fn lesson_union_below_threshold_creates_nothing() {
        let repo = repo_with_reader("consolidate-below-threshold");
        let store = RecordStore::new(&repo.fixture.root);
        // Five observations, three fixed and two dismissed: the union
        // (5 unique, 3 positive, 2 dismissed) misses the frozen
        // threshold (dismissed <= 1).
        let mut ids = Vec::new();
        for index in 0..5 {
            let id = new_ulid();
            let outcome = if index < 3 {
                FeedbackOutcome::Fixed
            } else {
                FeedbackOutcome::Dismissed
            };
            store
                .create(&ProjectRecord::Observation(observation(
                    &id,
                    &format!("2026-05-{:02}T10:00:00Z", index + 1),
                    "alice",
                    "concurrency",
                    "race",
                    outcome,
                )))
                .unwrap();
            ids.push(id);
        }
        let lesson_a = new_ulid();
        let lesson_b = new_ulid();
        for (lesson_id, created_at, observations) in [
            (&lesson_a, "2026-05-11T10:00:00Z", &ids[..4]),
            (&lesson_b, "2026-05-12T10:00:00Z", &ids[1..]),
        ] {
            let refs: Vec<&str> = observations.iter().map(String::as_str).collect();
            store
                .create(&ProjectRecord::Lesson(lesson(
                    lesson_id, created_at, "alice", &refs,
                )))
                .unwrap();
        }

        let records = load(&store);
        let plan = plan_consolidation(&records);
        assert_eq!(plan.lesson_groups.len(), 1);
        let format = repo.ctx.object_format().unwrap();
        let summary = run_consolidation(
            None,
            repo.ctx,
            &repo.reader,
            format,
            &plan,
            "alice",
            &ConsolidationOptions::default(),
        );
        assert_eq!(summary.common_lessons_created(), 0);
        assert!(
            summary
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("threshold"))
        );
        // Nothing was written.
        assert_eq!(load(&store).len(), 7);
    }

    #[test]
    fn fact_groups_without_a_host_stay_untouched() {
        let repo = repo_with_reader("consolidate-no-host");
        let store = RecordStore::new(&repo.fixture.root);
        let a = new_ulid();
        let b = new_ulid();
        store
            .create(&ProjectRecord::Fact(fact(
                &a,
                "2026-05-01T10:00:00Z",
                "alice",
                "queue capacity",
                "The sensor queue is bounded to 32 slots.",
            )))
            .unwrap();
        store
            .create(&ProjectRecord::Fact(fact(
                &b,
                "2026-05-02T10:00:00Z",
                "bob",
                "queue capacity",
                "The queue capacity is guarded by a mutex.",
            )))
            .unwrap();
        let records = load(&store);
        let plan = plan_consolidation(&records);
        assert!(plan.needs_host());
        let format = repo.ctx.object_format().unwrap();
        let summary = run_consolidation(
            None,
            repo.ctx,
            &repo.reader,
            format,
            &plan,
            "alice",
            &ConsolidationOptions::default(),
        );
        // Every fact group degraded to uncertain; nothing was written.
        assert_eq!(summary.common_facts_created(), 0);
        assert!(
            summary
                .fact_groups
                .iter()
                .all(|group| group.relation == FactRelation::Uncertain
                    && group.common_record_id.is_none())
        );
        assert_eq!(load(&store).len(), 2);
    }

    #[test]
    fn group_bounds_stay_within_record_reference_bounds() {
        const _: () = assert!(MAX_GROUP_MEMBERS <= MAX_ID_REFS);
        const _: () = assert!(MAX_GROUP_MEMBERS <= MAX_FACT_SYNTHESIS_FACTS);
    }

    #[test]
    fn report_renders_groups_and_summary() {
        let common_id = new_ulid();
        let summary = ConsolidationSummary {
            fact_groups: vec![FactGroupOutcome {
                subject: "queue capacity".to_string(),
                relation: FactRelation::Compatible,
                source_record_ids: vec!["a".to_string(), "b".to_string()],
                common_record_id: Some(common_id.clone()),
                detail: "merged 2 statements into one verified common fact".to_string(),
            }],
            lesson_groups: Vec::new(),
            diagnostics: vec!["a note".to_string()],
        };
        let text = render_compact_report(&summary);
        assert!(text.starts_with("nisse records compact\n"));
        assert!(text.contains("[compatible] 2 source record(s)"), "{text}");
        assert!(
            text.contains(&format!("common fact {common_id} created")),
            "{text}"
        );
        assert!(text.contains("note: a note"), "{text}");
        assert!(
            text.ends_with(
                "1 common fact(s) created, 0 conflict(s) retained unresolved, 0 common \
                 lesson(s) created\n"
            ),
            "{text}"
        );

        let empty = render_compact_report(&ConsolidationSummary::default());
        assert!(
            empty.contains("no consolidatable duplication found"),
            "{empty}"
        );
    }
}
