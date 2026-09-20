//! End-to-end review orchestration (plan sections 58–60).
//!
//! [`run_plan_review`] executes the frozen review pipeline for a plan:
//!
//! 1. discover repository/worktree (owned by the caller);
//! 2. resolve the target snapshot — plan review always reviews the
//!    current working tree (plan section 25);
//! 3. lazily refresh the source index for exactly that snapshot;
//! 4. derive deterministic request descriptors from the plan;
//! 5. load durable records and retrieve preliminary project
//!    facts/lessons;
//! 6. build the [`RoutingInput`];
//! 7. route specialists with the mandatory rules classifier;
//! 8. enforce the jury rules deterministically (`quality` always, at
//!    most three specialists, 2..=4 total, dynamic fallback);
//! 9. build one [`ReviewerExecutionSpec`] per juror;
//! 10. construct strong bounded initial reviewer contexts (plan first,
//!     then retrieved source, facts, lessons, and the reviewer's
//!     practices; plan section 23);
//! 11. start the Pi review host and forward the resolved model
//!     profiles;
//! 12. when the dynamic fallback fired, obtain a constrained
//!     [`DynamicDomainDescription`] through the frozen protocol exchange
//!     and combine it with the static expert template (plan section 19)
//!     — a failed description only loses the specialization, never the
//!     review;
//! 13. execute the reviewers independently with the frozen retry
//!     policy;
//! 14. require at least two valid independent reports (plan section 36)
//!     or fail the review explicitly;
//! 15. validate reports/findings/evidence against the snapshot;
//! 16. consolidate through the synthesis pipeline;
//! 17. construct the [`ReviewResult`];
//! 18. persist the local review event in the shared cache (plan
//!     section 57).
//!
//! [`run_code_review`] executes the same frozen pipeline for a code
//! review (plan section 60) over any of the five snapshot modes: the
//! deterministic diff of the resolved snapshot replaces the plan as the
//! primary review input and routing signal (`working`, `staged`,
//! `commit`, `range`, `branch`; plan section 25), reviewer initial
//! context starts from the real unified diff plus related source
//! retrieved for the changed symbols and paths (plan section 23), and
//! finding validation proves every diff-evidence path against the exact
//! review scope.
//!
//! This order is stable. No step consults an LLM for orchestration:
//! every routing, jury, budget, and persistence decision is
//! deterministic Rust, and LLM output enters only as validated
//! reviewer reports and the constrained domain description.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde_json::json;
use thiserror::Error;

use crate::domain::context::{ContextBundle, ContextEntry, ContextKind};
use crate::domain::ids::new_ulid;
use crate::domain::records::{ProjectRecord, RoutingHint};
use crate::domain::review::{ReviewKind, ReviewResult, Severity};
use crate::domain::reviewer::{DynamicDomainDescription, ReviewerExecutionSpec, ReviewerProfile};
use crate::domain::snapshot::SnapshotMode;
use crate::git::{
    DiffStatus, RepoContext, ResolvedSnapshot, SnapshotPatch, SnapshotReader, snapshot_diff,
    snapshot_patches,
};
use crate::index::artifact::PARSER_VERSION;
use crate::index::refresh_index;
use crate::retrieval::Retriever;
use crate::retrieval::query::{
    ContextBudget, MAX_QUERY_BYTES, RetrievalQuery, assemble_bundle, truncate_text,
};
use crate::routing::rules::RulesClassifier;
use crate::storage::StoragePaths;
use crate::storage::db::open_or_recover;
use crate::storage::records::{RecordStore, StoredRecord};
use crate::storage::schema::DbKind;

use super::catalog::{Catalog, DYNAMIC_DOMAIN_EXPERT_ID};
use super::findings::{RejectedFinding, ValidatedFinding, ValidationContext, validate_report};
use super::host::{HostError, HostProcess, PiHostLaunch, ReviewerOutcome};
use super::plan::build_jury_plan;
use super::protocol::{
    DomainDescriptionPayload, HostConfigPayload, HostRuntime, StartReviewPayload,
    ToolRequestPayload, ToolResponsePayload,
};
use super::synthesis::{ConsolidationOptions, consolidate};
use super::tools::ReviewerToolServer;

/// Maximum accepted plan input size.
pub const MAX_PLAN_BYTES: usize = 256 * 1024;

/// Maximum bytes of the plan carried in one initial-context entry.
/// Larger plans are truncated front-first (deterministically).
pub const PLAN_CONTEXT_MAX_BYTES: usize = 32 * 1024;

/// Maximum number of request descriptors derived from a plan and the
/// maximum byte length of one descriptor.
pub const MAX_PLAN_DESCRIPTORS: usize = 32;
pub const MAX_DESCRIPTOR_BYTES: usize = 160;

/// Maximum number of languages reported to routing.
pub const MAX_ROUTING_LANGUAGES: usize = 16;

/// Maximum number of deterministic tags reported to routing.
pub const MAX_ROUTING_TAGS: usize = 16;

/// Maximum number of active lesson routing hints fed to routing.
pub const MAX_ROUTING_LESSONS: usize = 64;

/// Lookup budget granted to every reviewer session.
pub const REVIEWER_MAX_LOOKUP_CALLS: u32 = 8;

/// Returned-context budget granted to every reviewer session.
pub const REVIEWER_MAX_CONTEXT_BYTES: u64 = 65_536;

/// Session timeout granted to every reviewer session.
pub const REVIEWER_TIMEOUT_MS: u64 = 300_000;

/// Budget of the initial reviewer context (plan section 23: strong
/// initial context within budget).
pub const INITIAL_CONTEXT_BUDGET: ContextBudget = ContextBudget {
    max_entries: 64,
    max_entry_text_bytes: PLAN_CONTEXT_MAX_BYTES,
    max_total_text_bytes: 96 * 1024,
};

/// Maximum number of practice statements bundled per reviewer from the
/// reviewer's own practice tags.
pub const MAX_PRACTICES_PER_REVIEWER: usize = 8;

/// Byte bound of one bundled practice statement.
pub const PRACTICE_TEXT_MAX_BYTES: usize = 1024;

/// Timeout of the constrained dynamic-domain description session.
pub const DOMAIN_DESCRIPTION_TIMEOUT_MS: u64 = 60_000;

/// Timeout waiting for the host's `host_ready`.
pub const HOST_READY_TIMEOUT: Duration = Duration::from_secs(20);

/// Timeout of one review round (the attempt and the retry round are
/// bounded separately; sessions additionally enforce their own spec
/// timeout).
pub const REVIEW_ROUND_TIMEOUT: Duration = Duration::from_secs(310);

/// Byte bound of the normalized domain label.
pub const DOMAIN_LABEL_MAX_BYTES: usize = 64;

/// Byte bound of one normalized focus term / risk / practice tag of a
/// dynamic domain description.
pub const DOMAIN_TERM_MAX_BYTES: usize = 128;

/// Maximum entries kept per normalized description list.
pub const DOMAIN_LIST_MAX_ENTRIES: usize = 8;

/// Plan review failures.
#[derive(Debug, Error)]
pub enum PlanReviewError {
    /// The plan input is empty, oversized, or otherwise unusable.
    #[error("invalid plan input: {0}")]
    InvalidPlan(String),

    /// A Git operation failed.
    #[error(transparent)]
    Git(#[from] crate::git::GitError),

    /// The source index could not be refreshed.
    #[error(transparent)]
    Index(#[from] crate::index::IndexError),

    /// Retrieval failed.
    #[error(transparent)]
    Retrieval(#[from] crate::retrieval::RetrievalError),

    /// The bundled catalog failed to load.
    #[error(transparent)]
    Catalog(#[from] super::catalog::CatalogError),

    /// Durable records failed to load.
    #[error(transparent)]
    Records(#[from] crate::storage::records::RecordStoreError),

    /// Jury construction failed (routing produced an inconsistent
    /// decision; treated as an internal invariant violation).
    #[error(transparent)]
    Jury(#[from] super::plan::JuryPlanError),

    /// The Pi review host boundary failed.
    #[error(transparent)]
    Host(#[from] HostError),

    /// Fewer than two valid independent reviewer reports exist (plan
    /// section 36). The review fails explicitly instead of guessing.
    #[error(
        "review `{review_id}` cannot continue: only {valid} valid independent reviewer report(s) exist; at least 2 are required; {}",
        failures.join("; ")
    )]
    TooFewValidReports {
        /// The failed review.
        review_id: String,
        /// Number of valid reports.
        valid: usize,
        /// Why the other reviewers failed (after their retry).
        failures: Vec<String>,
    },

    /// A regenerable cache database could not be read or written.
    #[error("local cache failure: {0}")]
    Cache(String),
}

/// The plan under review.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanInput {
    /// Full plan text (already bounded by the caller).
    pub text: String,
    /// Repository-relative path of the plan file, when the plan was
    /// read from inside the worktree.
    pub path: Option<String>,
}

/// Everything the plan-review pipeline needs besides the repository.
#[derive(Debug, Clone)]
pub struct PlanReviewOptions {
    /// How to launch the Pi review host child process.
    pub launch: PiHostLaunch,
    /// Bounds of the consolidation pipeline.
    pub consolidation: ConsolidationOptions,
    /// Resolved model profiles forwarded to the host through
    /// `host_config` (from trusted local configuration; reviewer
    /// sessions never inherit a model).
    pub model_profiles: BTreeMap<String, crate::domain::model::ModelProfile>,
}

impl PlanReviewOptions {
    /// Options for `launch` with default consolidation bounds and no
    /// configured model profiles.
    pub fn new(launch: PiHostLaunch) -> Self {
        PlanReviewOptions {
            launch,
            consolidation: ConsolidationOptions::default(),
            model_profiles: BTreeMap::new(),
        }
    }
}

/// The completed plan review: the public result plus the deterministic
/// bookkeeping recorded with it.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanReviewOutcome {
    /// The review result returned to callers.
    pub result: ReviewResult,
    /// The routing decision that selected the jury.
    pub decision: crate::domain::routing::RoutingDecision,
    /// The jury that reviewed the plan.
    pub jury: crate::domain::jury::JuryPlan,
    /// The dynamic domain description used by the expert seat, when the
    /// fallback fired and the session succeeded.
    pub domain_description: Option<DynamicDomainDescription>,
    /// Diagnostics of the run (domain-description fallbacks, synthesis
    /// fallbacks; rejected findings are recorded in the review event).
    pub diagnostics: Vec<String>,
}

/// Run the complete plan-review pipeline (see the module documentation
/// for the frozen step order).
///
/// `resolved` must be the working-tree snapshot of `ctx` (plan section
/// 25); the index is refreshed lazily for exactly that snapshot.
pub fn run_plan_review(
    ctx: &RepoContext,
    resolved: &ResolvedSnapshot,
    paths: &StoragePaths,
    plan: &PlanInput,
    options: &PlanReviewOptions,
) -> Result<PlanReviewOutcome, PlanReviewError> {
    let plan_text = plan.text.trim();
    if plan_text.is_empty() {
        return Err(PlanReviewError::InvalidPlan(
            "the plan is empty".to_string(),
        ));
    }
    if plan.text.len() > MAX_PLAN_BYTES {
        return Err(PlanReviewError::InvalidPlan(format!(
            "the plan exceeds {MAX_PLAN_BYTES} bytes"
        )));
    }

    // 3. Refresh the source index for exactly the target snapshot.
    refresh_index(ctx, resolved, paths)?;

    // 4. Deterministic request descriptors.
    let descriptors = plan_descriptors(plan_text);

    // 5. Durable records + preliminary facts/lessons retrieval.
    let catalog = Catalog::bundled()?;
    let records = RecordStore::new(&ctx.worktree_root).load()?;
    let retriever = Retriever::open(resolved, paths)?;
    retriever.sync_records(&records)?;
    retriever.sync_practices(&catalog)?;
    let query = RetrievalQuery::from_text(truncate_text(plan_text, MAX_QUERY_BYTES));
    let shared_context = retriever.lookup(&query)?;
    let active = crate::storage::records::active_records(&records);

    // 6. Routing input.
    let routing_input = crate::domain::routing::RoutingInput {
        descriptors: descriptors.clone(),
        changed_paths: Vec::new(),
        languages: snapshot_languages(paths)?,
        changed_symbols: Vec::new(),
        deterministic_tags: record_tags(&shared_context, &active),
        lessons: active_lesson_hints(&active),
    };

    // 7. Deterministic routing; 8. jury rules.
    let decision = RulesClassifier::new(catalog.clone()).classify(&routing_input);
    let jury = build_jury_plan(&decision, &catalog)?;

    // 11. Start the Pi host early so every semantic job shares it.
    let mut host = HostProcess::spawn(&options.launch)?;
    host.wait_ready(HOST_READY_TIMEOUT)?;
    let host_config = HostConfigPayload::from_config_profiles(&options.model_profiles);
    host.send_host_config(&host_config)?;

    // 12. Constrained dynamic-domain description for the fallback seat.
    let mut diagnostics: Vec<String> = Vec::new();
    let mut domain_description: Option<DynamicDomainDescription> = None;
    if jury.uses_dynamic_fallback() {
        let expert = catalog.dynamic_domain_expert();
        let payload = DomainDescriptionPayload {
            description_id: new_ulid(),
            timeout_ms: DOMAIN_DESCRIPTION_TIMEOUT_MS,
            model_profile: expert.default_model_profile.clone(),
            thinking_level: expert.default_thinking_level,
            descriptors: descriptors.clone(),
            changed_paths: Vec::new(),
            languages: routing_input.languages.clone(),
        };
        host.start_domain_description(&payload)?;
        let budget = Duration::from_millis(DOMAIN_DESCRIPTION_TIMEOUT_MS + 10_000);
        let outcome = host.complete_domain_description(&payload.description_id, budget)?;
        match normalize_domain_description(outcome.description, &catalog) {
            Some(normalized) => domain_description = Some(normalized),
            None => {
                let reason = outcome
                    .error
                    .as_ref()
                    .map(|error| format!("{}: {}", error.code, error.message))
                    .unwrap_or_else(|| "the description was not usable".to_string());
                diagnostics.push(format!(
                    "dynamic domain description fell back to the static template: {reason}"
                ));
            }
        }
    }

    // 9./10. Execution specs with strong initial contexts.
    let known_record_ids: BTreeSet<String> = records
        .iter()
        .map(StoredRecord::id)
        .map(str::to_string)
        .collect();
    let plan_entry = plan_input_entry(plan);
    let specs: Vec<ReviewerExecutionSpec> = jury
        .seats
        .iter()
        .map(|seat| {
            let profile = catalog
                .profile(&seat.reviewer_id)
                .expect("jury construction validated every seat against the catalog");
            let description = if seat.reviewer_id == DYNAMIC_DOMAIN_EXPERT_ID {
                domain_description.as_ref()
            } else {
                None
            };
            let seat = seat_plan(
                profile,
                plan_review_instructions(&profile.base_instructions),
                description,
            );
            let context = initial_context(
                std::slice::from_ref(&plan_entry),
                &shared_context,
                &catalog,
                &seat.practice_tags,
                &INITIAL_CONTEXT_BUDGET,
            );
            execution_spec(profile, seat, context)
        })
        .collect();

    // 13. Execute reviewers independently (frozen one-retry policy).
    let review_id = new_ulid();
    let payload = StartReviewPayload {
        review_id: review_id.clone(),
        kind: ReviewKind::Plan,
        reviewers: specs.clone(),
    };
    let tool_reader = SnapshotReader::new(ctx, resolved)?;
    let mut tool_server =
        ReviewerToolServer::new(review_id.clone(), &specs, retriever, tool_reader);
    let mut serve = |request: &ToolRequestPayload| -> ToolResponsePayload {
        // `complete_review` stamps the authoritative request id.
        tool_server.handle("", request)
    };
    let outcomes =
        super::execution::execute_review(&mut host, &payload, &mut serve, REVIEW_ROUND_TIMEOUT)?;

    // 14. At least two valid independent reports (plan section 36).
    let mut reports = Vec::new();
    let mut failures = Vec::new();
    for (reviewer_id, outcome) in &outcomes {
        match outcome {
            ReviewerOutcome::Completed(report) => {
                reports.push((reviewer_id.clone(), report.clone()));
            }
            ReviewerOutcome::Failed(error) => failures.push(format!(
                "reviewer `{reviewer_id}` failed after its retry ({}: {})",
                error.code, error.message
            )),
        }
    }
    if reports.len() < 2 {
        return Err(PlanReviewError::TooFewValidReports {
            review_id,
            valid: reports.len(),
            failures,
        });
    }

    // 15. Validate every report against the exact snapshot. A plan
    // review has no diff scope: an empty changed-path set rejects any
    // diff evidence a reviewer might hallucinate (plan section 37).
    let validation_reader = SnapshotReader::new(ctx, resolved)?;
    let empty_changed_paths: BTreeSet<String> = BTreeSet::new();
    let validation_context = ValidationContext {
        reader: &validation_reader,
        object_format: ctx.object_format()?,
        changed_paths: Some(&empty_changed_paths),
        known_record_ids: &known_record_ids,
    };
    let mut validated: Vec<ValidatedFinding> = Vec::new();
    let mut rejected: Vec<RejectedFinding> = Vec::new();
    for (reviewer_id, report) in &reports {
        let validation = validate_report(&validation_context, reviewer_id, report);
        validated.extend(validation.accepted);
        rejected.extend(validation.rejected);
    }

    // 16. Consolidation (equivalence checks + synthesis, bounded).
    let synthesis = consolidate(
        &mut host,
        ReviewKind::Plan,
        &validated,
        &options.consolidation,
    )?;
    diagnostics.extend(synthesis.diagnostics.iter().map(|d| d.message.clone()));

    // 17. ReviewResult.
    let result = ReviewResult {
        review_id: review_id.clone(),
        kind: ReviewKind::Plan,
        created_at: crate::domain::time::now_utc_rfc3339(),
        snapshot: resolved.descriptor.clone(),
        summary: synthesis.summary.clone(),
        findings: synthesis.findings.clone(),
    };

    // 18. Persist the local review event (regenerable shared cache).
    let source = json!({
        "plan_source": match plan.path.as_deref() {
            Some(path) => json!({ "path": path }),
            None => json!({ "stdin": true }),
        },
    });
    let event = ReviewEvent {
        result: &result,
        routing_input: &routing_input,
        decision: &decision,
        jury: &jury,
        domain_description: domain_description.as_ref(),
        specs: &specs,
        outcomes: &outcomes,
        validated: &validated,
        rejected: &rejected,
        synthesis_diagnostics: &synthesis.diagnostics,
        diagnostics: &diagnostics,
        source: &source,
    };
    persist_review_event(paths, &event).map_err(PlanReviewError::Cache)?;

    let _ = host.shutdown(Duration::from_secs(5));

    Ok(PlanReviewOutcome {
        result,
        decision,
        jury,
        domain_description,
        diagnostics,
    })
}

/// The plan as the leading initial-context entry (plan section 23: the
/// plan is always the primary review input).
fn plan_input_entry(plan: &PlanInput) -> ContextEntry {
    let truncated = truncate_text(&plan.text, PLAN_CONTEXT_MAX_BYTES);
    let text = if truncated.len() < plan.text.len() {
        format!("{truncated}\n… [plan truncated after {PLAN_CONTEXT_MAX_BYTES} bytes]")
    } else {
        truncated.to_string()
    };
    ContextEntry {
        kind: ContextKind::Plan,
        title: "review input: implementation plan".to_string(),
        path: plan.path.clone(),
        symbol: None,
        line_start: None,
        line_end: None,
        record_id: None,
        practice_id: None,
        text,
        score: 1.0,
    }
}

/// Derive bounded deterministic request descriptors from the plan:
/// Markdown heading lines; when the plan has none, its first non-empty
/// line. Descriptors feed routing only.
pub fn plan_descriptors(plan_text: &str) -> Vec<String> {
    let mut descriptors: Vec<String> = Vec::new();
    let mut fallback: Option<String> = None;
    for line in plan_text.lines() {
        let heading = line.trim_start().strip_prefix('#');
        let heading = heading.map(|rest| rest.trim_start_matches('#').trim());
        if let Some(heading) = heading {
            if !heading.is_empty() && descriptors.len() < MAX_PLAN_DESCRIPTORS {
                descriptors.push(bounded(heading, MAX_DESCRIPTOR_BYTES));
            }
        } else if fallback.is_none() && !line.trim().is_empty() {
            fallback = Some(bounded(line.trim(), MAX_DESCRIPTOR_BYTES));
        }
    }
    if descriptors.is_empty()
        && let Some(first) = fallback
    {
        descriptors.push(first);
    }
    descriptors
}

/// Truncate to a byte bound on a char boundary.
fn bounded(text: &str, max_bytes: usize) -> String {
    truncate_text(text, max_bytes).to_string()
}

/// Distinct index languages of the freshly refreshed target snapshot,
/// read from the worktree manifest (the same refresh that bound the
/// snapshot; `Retriever::open` has already verified the binding).
fn snapshot_languages(paths: &StoragePaths) -> Result<Vec<String>, PlanReviewError> {
    let cache = |err: rusqlite::Error| PlanReviewError::Cache(err.to_string());
    let db = open_or_recover(&paths.worktree, DbKind::Worktree)
        .map_err(|err| PlanReviewError::Cache(format!("{}: {err}", err)))?;
    let mut stmt = db
        .conn
        .prepare(
            "SELECT DISTINCT pc.language FROM path_content pc \
             JOIN manifest m ON m.path = pc.path AND m.content_key = pc.content_key \
             WHERE pc.language IS NOT NULL ORDER BY pc.language",
        )
        .map_err(cache)?;
    let languages = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(cache)?
        .collect::<Result<Vec<String>, _>>()
        .map_err(cache)?;
    Ok(languages.into_iter().take(MAX_ROUTING_LANGUAGES).collect())
}

/// Deterministic routing tags from the retrieved records: the tags of
/// matched facts and lessons (plan section 33 classifier input).
fn record_tags(context: &ContextBundle, active: &[&StoredRecord]) -> Vec<String> {
    let wanted: BTreeSet<&str> = context
        .entries
        .iter()
        .filter_map(|entry| entry.record_id.as_deref())
        .collect();
    let mut tags: Vec<String> = Vec::new();
    let mut seen = BTreeSet::new();
    for stored in active {
        if !wanted.contains(stored.id()) {
            continue;
        }
        let record_tags: &[String] = match &stored.record {
            ProjectRecord::Fact(fact) => &fact.tags,
            ProjectRecord::Lesson(lesson) => &lesson.tags,
            ProjectRecord::Observation(_) => continue,
        };
        for tag in record_tags {
            let tag = tag.to_lowercase();
            if seen.insert(tag.clone()) && tags.len() < MAX_ROUTING_TAGS {
                tags.push(tag);
            }
        }
    }
    tags
}

/// Routing hints of active lessons (bounded). Lesson wording never
/// influences routing; only these deterministic hints do.
fn active_lesson_hints(active: &[&StoredRecord]) -> Vec<RoutingHint> {
    active
        .iter()
        .filter_map(|stored| match &stored.record {
            ProjectRecord::Lesson(lesson) => Some(lesson.routing_hint.clone()),
            _ => None,
        })
        .take(MAX_ROUTING_LESSONS)
        .collect()
}

/// One reviewer seat's adjusted methodology for a review: the static
/// profile combined with review-kind instructions and, for the dynamic
/// expert, the constrained domain description.
struct SeatPlan {
    /// Seat specialization label.
    specialization: String,
    /// Full reviewer instructions.
    instructions: String,
    /// Checklist items (static first, domain risks appended).
    checklist: Vec<String>,
    /// Practice tags whose packs may enter the reviewer's context.
    practice_tags: Vec<String>,
}

/// Combine a reviewer profile with review-kind instructions and an
/// optional constrained domain description (plan section 19: the
/// description can never add tools, providers, or capabilities — those
/// come only from the static profile; the static checklist items always
/// come first).
fn seat_plan(
    profile: &ReviewerProfile,
    kind_instructions: String,
    description: Option<&DynamicDomainDescription>,
) -> SeatPlan {
    let mut specialization = profile.description.clone();
    let mut instructions = kind_instructions;
    let mut checklist = profile.checklist.clone();
    let mut practice_tags: Vec<String> = profile.practice_tags.clone();

    if let Some(description) = description {
        specialization = format!(
            "domain expert: {}",
            bounded(&description.domain_label, MAX_DESCRIPTOR_BYTES)
        );
        instructions.push_str("\n\n## Domain description (focus your review)\n");
        instructions.push_str(&format!(
            "- domain: {}\n",
            bounded(&description.domain_label, DOMAIN_LABEL_MAX_BYTES)
        ));
        if !description.focus_terms.is_empty() {
            instructions.push_str(&format!(
                "- focus terms: {}\n",
                description.focus_terms.join(", ")
            ));
        }
        if !description.relevant_practice_tags.is_empty() {
            instructions.push_str(&format!(
                "- relevant practice tags: {}\n",
                description.relevant_practice_tags.join(", ")
            ));
        }
        for risk in &description.risks_to_check {
            // Domain risks extend the static checklist; the static items
            // always come first.
            if !checklist.contains(risk) {
                checklist.push(risk.clone());
            }
        }
        practice_tags.extend(description.relevant_practice_tags.iter().cloned());
    }

    SeatPlan {
        specialization: bounded(&specialization, MAX_DESCRIPTOR_BYTES * 2),
        instructions,
        checklist,
        practice_tags,
    }
}

/// Assemble one juror's final execution spec from its seat plan and
/// assembled initial context (plan section 18).
fn execution_spec(
    profile: &ReviewerProfile,
    seat: SeatPlan,
    initial_context: ContextBundle,
) -> ReviewerExecutionSpec {
    ReviewerExecutionSpec {
        reviewer_id: profile.id.clone(),
        specialization: seat.specialization,
        instructions: seat.instructions,
        checklist: seat.checklist,
        model_profile: profile.default_model_profile.clone(),
        thinking_level: profile.default_thinking_level,
        initial_context: initial_context.entries,
        allowed_capabilities: profile.allowed_capabilities.clone(),
        max_lookup_calls: REVIEWER_MAX_LOOKUP_CALLS,
        max_returned_context_bytes: REVIEWER_MAX_CONTEXT_BYTES,
        timeout_ms: REVIEWER_TIMEOUT_MS,
    }
}

/// Reviewer instructions for a plan review: the static base
/// instructions plus the frozen plan-review framing.
fn plan_review_instructions(base: &str) -> String {
    format!(
        "{base}\n\n## Review kind: plan\n\nThis is a plan review. The implementation plan in \
the initial context is the primary review input. The plan describes future work; judge whether \
the plan is sound, complete, and consistent with the current project source also present in \
your context. Cite plan sections or project source as evidence."
    )
}

/// Reviewer instructions for a code review: the static base
/// instructions plus the frozen code-review framing.
fn code_review_instructions(base: &str) -> String {
    format!(
        "{base}\n\n## Review kind: code\n\nThis is a code review of concrete changes. The \nunified diff entries in the initial context are the primary review input; they describe exactly \nwhat changed between the review's base and target snapshots. Review the change itself and its \ninteraction with the surrounding project source also present in your context. Cite diff paths \n(with line ranges taken from the diff hunks) or project source as evidence."
    )
}

/// Assemble one reviewer's bounded initial context (plan section 23):
/// the review input first, then the shared retrieved context, then the
/// practices of the reviewer's (possibly domain-extended) tags.
fn initial_context(
    input_entries: &[ContextEntry],
    shared: &ContextBundle,
    catalog: &Catalog,
    practice_tags: &[String],
    budget: &ContextBudget,
) -> ContextBundle {
    let mut candidates = input_entries.to_vec();
    candidates.extend(shared.entries.iter().cloned());
    let mut practices = 0usize;
    for tag in practice_tags {
        let Some(pack) = catalog.practice_pack(tag) else {
            continue;
        };
        for practice in &pack.practices {
            if practices >= MAX_PRACTICES_PER_REVIEWER || candidates.len() >= budget.max_entries {
                break;
            }
            practices += 1;
            candidates.push(ContextEntry {
                kind: ContextKind::Practice,
                title: format!("practice: {}", practice.title),
                path: None,
                symbol: None,
                line_start: None,
                line_end: None,
                record_id: None,
                practice_id: Some(practice.id.clone()),
                text: bounded(&practice.statement, PRACTICE_TEXT_MAX_BYTES),
                // Practices of the reviewer's own tags rank just above
                // generic FTS matches of other content.
                score: 0.55,
            });
        }
    }
    assemble_bundle(candidates, budget)
}

// ---------------------------------------------------------------------
// Code review (plan section 60)
// ---------------------------------------------------------------------

/// Maximum changed paths reported to routing and the domain description.
pub const MAX_ROUTING_CHANGED_PATHS: usize = 64;

/// Maximum changed paths queried for their defined symbols.
pub const MAX_SYMBOL_PATHS: usize = 64;

/// Maximum changed symbols reported to routing and retrieval.
pub const MAX_CHANGED_SYMBOLS: usize = 32;

/// Maximum commit subjects used as request descriptors.
pub const MAX_COMMIT_SUBJECTS: usize = 8;

/// Maximum request descriptors derived from one code change.
pub const MAX_CODE_DESCRIPTORS: usize = 32;

/// Maximum changed paths whose unified diff enters reviewer context.
pub const MAX_DIFF_FILES_IN_CONTEXT: usize = 32;

/// Per-file byte budget of one unified diff in reviewer context.
pub const DIFF_ENTRY_MAX_BYTES: usize = 16 * 1024;

/// Aggregate byte budget of the diff portion of reviewer context.
pub const DIFF_TOTAL_MAX_BYTES: usize = 64 * 1024;

/// Code review failures.
#[derive(Debug, Error)]
pub enum CodeReviewError {
    /// The review scope is unusable (empty diff, malformed revision).
    #[error("invalid review scope: {0}")]
    InvalidScope(String),

    /// A Git operation failed.
    #[error(transparent)]
    Git(#[from] crate::git::GitError),

    /// The source index could not be refreshed.
    #[error(transparent)]
    Index(#[from] crate::index::IndexError),

    /// Retrieval failed.
    #[error(transparent)]
    Retrieval(#[from] crate::retrieval::RetrievalError),

    /// The bundled catalog failed to load.
    #[error(transparent)]
    Catalog(#[from] super::catalog::CatalogError),

    /// Durable records failed to load.
    #[error(transparent)]
    Records(#[from] crate::storage::records::RecordStoreError),

    /// Jury construction failed (routing produced an inconsistent
    /// decision; treated as an internal invariant violation).
    #[error(transparent)]
    Jury(#[from] super::plan::JuryPlanError),

    /// The Pi review host boundary failed.
    #[error(transparent)]
    Host(#[from] HostError),

    /// Fewer than two valid independent reviewer reports exist (plan
    /// section 36). The review fails explicitly instead of guessing.
    #[error(
        "review `{review_id}` cannot continue: only {valid} valid independent reviewer report(s) exist; at least 2 are required; {}",
        failures.join("; ")
    )]
    TooFewValidReports {
        /// The failed review.
        review_id: String,
        /// Number of valid reports.
        valid: usize,
        /// Why the other reviewers failed (after their retry).
        failures: Vec<String>,
    },

    /// A regenerable cache database could not be read or written.
    #[error("local cache failure: {0}")]
    Cache(String),
}

/// Everything the code-review pipeline needs besides the repository.
#[derive(Debug, Clone)]
pub struct CodeReviewOptions {
    /// How to launch the Pi review host child process.
    pub launch: PiHostLaunch,
    /// Bounds of the consolidation pipeline.
    pub consolidation: ConsolidationOptions,
    /// Resolved model profiles forwarded to the host through
    /// `host_config` (from trusted local configuration; reviewer
    /// sessions never inherit a model).
    pub model_profiles: BTreeMap<String, crate::domain::model::ModelProfile>,
}

impl CodeReviewOptions {
    /// Options for `launch` with default consolidation bounds and no
    /// configured model profiles.
    pub fn new(launch: PiHostLaunch) -> Self {
        CodeReviewOptions {
            launch,
            consolidation: ConsolidationOptions::default(),
            model_profiles: BTreeMap::new(),
        }
    }
}

/// The completed code review: the public result plus the deterministic
/// bookkeeping recorded with it.
#[derive(Debug, Clone, PartialEq)]
pub struct CodeReviewOutcome {
    /// The review result returned to callers.
    pub result: ReviewResult,
    /// The routing decision that selected the jury.
    pub decision: crate::domain::routing::RoutingDecision,
    /// The jury that reviewed the change.
    pub jury: crate::domain::jury::JuryPlan,
    /// The dynamic domain description used by the expert seat, when the
    /// fallback fired and the session succeeded.
    pub domain_description: Option<DynamicDomainDescription>,
    /// All changed paths of the review scope (the authority for diff
    /// evidence validation), in path order.
    pub changed_paths: Vec<String>,
    /// Diagnostics of the run (domain-description fallbacks, synthesis
    /// fallbacks; rejected findings are recorded in the review event).
    pub diagnostics: Vec<String>,
}

/// Run the complete code-review pipeline (plan section 60) over the
/// already-resolved snapshot of any of the five modes.
///
/// The pipeline is the frozen order of plan section 58 with the diff of
/// the resolved snapshot in place of the plan: the real unified diff is
/// the primary reviewer input and routing signal, retrieval starts from
/// the changed symbols and paths, and every diff-evidence path is
/// validated against the exact review scope.
pub fn run_code_review(
    ctx: &RepoContext,
    resolved: &ResolvedSnapshot,
    paths: &StoragePaths,
    options: &CodeReviewOptions,
) -> Result<CodeReviewOutcome, CodeReviewError> {
    // 3. Refresh the source index for exactly the target snapshot.
    refresh_index(ctx, resolved, paths)?;

    // 4. Deterministic request/diff descriptors.
    let diff = snapshot_diff(ctx, resolved)?;
    if diff.is_empty() {
        return Err(CodeReviewError::InvalidScope(
            "the review scope contains no changes to review".to_string(),
        ));
    }
    let changed_paths: Vec<String> = diff.iter().map(|entry| entry.path.clone()).collect();
    let routing_paths: Vec<String> = changed_paths
        .iter()
        .take(MAX_ROUTING_CHANGED_PATHS)
        .cloned()
        .collect();
    let subjects = commit_subjects(ctx, resolved)?;
    let descriptors = code_descriptors(subjects, &routing_paths);
    let languages = changed_languages(paths, &routing_paths)?;
    let symbols = changed_symbols(paths, &routing_paths)?;

    // 5. Durable records + preliminary facts/lessons retrieval.
    let catalog = Catalog::bundled()?;
    let records = RecordStore::new(&ctx.worktree_root).load()?;
    let retriever = Retriever::open(resolved, paths)?;
    retriever.sync_records(&records)?;
    retriever.sync_practices(&catalog)?;
    let query = RetrievalQuery {
        text: retrieval_text(&descriptors, &symbols),
        changed_symbols: symbols.clone(),
        changed_paths: routing_paths.clone(),
    };
    let shared_context = retriever.lookup(&query)?;
    let active = crate::storage::records::active_records(&records);

    // 6. Routing input.
    let routing_input = crate::domain::routing::RoutingInput {
        descriptors: descriptors.clone(),
        changed_paths: routing_paths.clone(),
        languages: languages.clone(),
        changed_symbols: symbols.clone(),
        deterministic_tags: record_tags(&shared_context, &active),
        lessons: active_lesson_hints(&active),
    };

    // 7. Deterministic routing; 8. jury rules.
    let decision = RulesClassifier::new(catalog.clone()).classify(&routing_input);
    let jury = build_jury_plan(&decision, &catalog)?;

    // 11. Start the Pi host early so every semantic job shares it.
    let mut host = HostProcess::spawn(&options.launch)?;
    host.wait_ready(HOST_READY_TIMEOUT)?;
    let host_config = HostConfigPayload::from_config_profiles(&options.model_profiles);
    host.send_host_config(&host_config)?;

    // 12. Constrained dynamic-domain description for the fallback seat.
    let mut diagnostics: Vec<String> = Vec::new();
    let mut domain_description: Option<DynamicDomainDescription> = None;
    if jury.uses_dynamic_fallback() {
        let expert = catalog.dynamic_domain_expert();
        let payload = DomainDescriptionPayload {
            description_id: new_ulid(),
            timeout_ms: DOMAIN_DESCRIPTION_TIMEOUT_MS,
            model_profile: expert.default_model_profile.clone(),
            thinking_level: expert.default_thinking_level,
            descriptors: descriptors.clone(),
            changed_paths: routing_paths.clone(),
            languages: languages.clone(),
        };
        host.start_domain_description(&payload)?;
        let budget = Duration::from_millis(DOMAIN_DESCRIPTION_TIMEOUT_MS + 10_000);
        let outcome = host.complete_domain_description(&payload.description_id, budget)?;
        match normalize_domain_description(outcome.description, &catalog) {
            Some(normalized) => domain_description = Some(normalized),
            None => {
                let reason = outcome
                    .error
                    .as_ref()
                    .map(|error| format!("{}: {}", error.code, error.message))
                    .unwrap_or_else(|| "the description was not usable".to_string());
                diagnostics.push(format!(
                    "dynamic domain description fell back to the static template: {reason}"
                ));
            }
        }
    }

    // 9./10. Execution specs: the real diff leads the initial context,
    // followed by related source retrieved for the change.
    let known_record_ids: BTreeSet<String> = records
        .iter()
        .map(StoredRecord::id)
        .map(str::to_string)
        .collect();
    let patches = snapshot_patches(
        ctx,
        resolved,
        &diff,
        MAX_DIFF_FILES_IN_CONTEXT,
        DIFF_ENTRY_MAX_BYTES,
    )?;
    let diff_context = diff_context_entries(&patches);
    let specs: Vec<ReviewerExecutionSpec> = jury
        .seats
        .iter()
        .map(|seat| {
            let profile = catalog
                .profile(&seat.reviewer_id)
                .expect("jury construction validated every seat against the catalog");
            let description = if seat.reviewer_id == DYNAMIC_DOMAIN_EXPERT_ID {
                domain_description.as_ref()
            } else {
                None
            };
            let seat = seat_plan(
                profile,
                code_review_instructions(&profile.base_instructions),
                description,
            );
            let context = code_initial_context(
                &diff_context,
                &shared_context,
                &catalog,
                &seat.practice_tags,
            );
            execution_spec(profile, seat, context)
        })
        .collect();

    // 13. Execute reviewers independently (frozen one-retry policy).
    let review_id = new_ulid();
    let payload = StartReviewPayload {
        review_id: review_id.clone(),
        kind: ReviewKind::Code,
        reviewers: specs.clone(),
    };
    let tool_reader = SnapshotReader::new(ctx, resolved)?;
    let mut tool_server =
        ReviewerToolServer::new(review_id.clone(), &specs, retriever, tool_reader);
    let mut serve = |request: &ToolRequestPayload| -> ToolResponsePayload {
        // `complete_review` stamps the authoritative request id.
        tool_server.handle("", request)
    };
    let outcomes =
        super::execution::execute_review(&mut host, &payload, &mut serve, REVIEW_ROUND_TIMEOUT)?;

    // 14. At least two valid independent reports (plan section 36).
    let mut reports = Vec::new();
    let mut failures = Vec::new();
    for (reviewer_id, outcome) in &outcomes {
        match outcome {
            ReviewerOutcome::Completed(report) => {
                reports.push((reviewer_id.clone(), report.clone()));
            }
            ReviewerOutcome::Failed(error) => failures.push(format!(
                "reviewer `{reviewer_id}` failed after its retry ({}: {})",
                error.code, error.message
            )),
        }
    }
    if reports.len() < 2 {
        return Err(CodeReviewError::TooFewValidReports {
            review_id,
            valid: reports.len(),
            failures,
        });
    }

    // 15. Validate every report against the exact snapshot; diff
    // evidence must cite a path of the review scope (plan section 37).
    let validation_reader = SnapshotReader::new(ctx, resolved)?;
    let changed_set: BTreeSet<String> = changed_paths.iter().cloned().collect();
    let validation_context = ValidationContext {
        reader: &validation_reader,
        object_format: ctx.object_format()?,
        changed_paths: Some(&changed_set),
        known_record_ids: &known_record_ids,
    };
    let mut validated: Vec<ValidatedFinding> = Vec::new();
    let mut rejected: Vec<RejectedFinding> = Vec::new();
    for (reviewer_id, report) in &reports {
        let validation = validate_report(&validation_context, reviewer_id, report);
        validated.extend(validation.accepted);
        rejected.extend(validation.rejected);
    }

    // 16. Consolidation (equivalence checks + synthesis, bounded).
    let synthesis = consolidate(
        &mut host,
        ReviewKind::Code,
        &validated,
        &options.consolidation,
    )?;
    diagnostics.extend(synthesis.diagnostics.iter().map(|d| d.message.clone()));

    // 17. ReviewResult.
    let result = ReviewResult {
        review_id: review_id.clone(),
        kind: ReviewKind::Code,
        created_at: crate::domain::time::now_utc_rfc3339(),
        snapshot: resolved.descriptor.clone(),
        summary: synthesis.summary.clone(),
        findings: synthesis.findings.clone(),
    };

    // 18. Persist the local review event (regenerable shared cache).
    let source = json!({
        "code_source": {
            "mode": resolved.descriptor.mode_label(),
            "descriptors": descriptors,
            "changed_path_count": diff.len(),
            "changed_paths": changed_paths,
        },
    });
    let event = ReviewEvent {
        result: &result,
        routing_input: &routing_input,
        decision: &decision,
        jury: &jury,
        domain_description: domain_description.as_ref(),
        specs: &specs,
        outcomes: &outcomes,
        validated: &validated,
        rejected: &rejected,
        synthesis_diagnostics: &synthesis.diagnostics,
        diagnostics: &diagnostics,
        source: &source,
    };
    persist_review_event(paths, &event).map_err(CodeReviewError::Cache)?;

    let _ = host.shutdown(Duration::from_secs(5));

    Ok(CodeReviewOutcome {
        result,
        decision,
        jury,
        domain_description,
        changed_paths,
        diagnostics,
    })
}

/// Commit subjects of the review scope, when the mode knows them: the
/// reviewed commit's subject for `commit`, and the bounded subjects of
/// the included commits (newest first) for `range` and `branch`.
/// `working`/`staged` changes carry no commit message.
fn commit_subjects(
    ctx: &RepoContext,
    resolved: &ResolvedSnapshot,
) -> Result<Vec<String>, CodeReviewError> {
    let descriptor = &resolved.descriptor;
    match descriptor.mode {
        SnapshotMode::Commit => {
            let Some(head) = &descriptor.head_commit else {
                return Ok(Vec::new());
            };
            Ok(vec![ctx.commit_subject(head)?])
        }
        SnapshotMode::Range | SnapshotMode::Branch => {
            let (Some(base), Some(head)) = (&descriptor.base_commit, &descriptor.head_commit)
            else {
                return Ok(Vec::new());
            };
            Ok(ctx.commit_subjects_between(base, head, MAX_COMMIT_SUBJECTS)?)
        }
        SnapshotMode::Working | SnapshotMode::Staged => Ok(Vec::new()),
    }
}

/// Deterministic request descriptors of a code change: the commit
/// subjects of the scope (when known) followed by the space-separated
/// path tokens of every changed path, all bounded.
fn code_descriptors(subjects: Vec<String>, changed_paths: &[String]) -> Vec<String> {
    let mut descriptors: Vec<String> = Vec::new();
    for subject in subjects {
        let subject = subject.trim();
        if subject.is_empty() {
            continue;
        }
        if descriptors.len() >= MAX_CODE_DESCRIPTORS {
            break;
        }
        descriptors.push(bounded(subject, MAX_DESCRIPTOR_BYTES));
    }
    for path in changed_paths {
        if descriptors.len() >= MAX_CODE_DESCRIPTORS {
            break;
        }
        // Directory components, file stem, and extension become
        // matchable words (for example `db/migrations/0010_users.sql`
        // contributes `migrations` and `sql`).
        descriptors.push(bounded(&path.replace('/', " "), MAX_DESCRIPTOR_BYTES));
    }
    descriptors
}

/// Free-text of the retrieval query: descriptors and changed symbol
/// names, bounded to the query budget.
fn retrieval_text(descriptors: &[String], symbols: &[String]) -> String {
    let mut text = String::new();
    for part in descriptors.iter().chain(symbols) {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(part);
    }
    truncate_text(&text, MAX_QUERY_BYTES).to_owned()
}

/// Distinct index languages of the changed paths, read from the
/// worktree manifest of the freshly refreshed target snapshot.
fn changed_languages(
    paths: &StoragePaths,
    changed: &[String],
) -> Result<Vec<String>, CodeReviewError> {
    let cache = |err: rusqlite::Error| CodeReviewError::Cache(err.to_string());
    let db = open_or_recover(&paths.worktree, DbKind::Worktree)
        .map_err(|err| CodeReviewError::Cache(format!("{}: {err}", err)))?;
    let mut languages: BTreeSet<String> = BTreeSet::new();
    let mut stmt = db
        .conn
        .prepare(
            "SELECT pc.language FROM path_content pc \n             JOIN manifest m ON m.path = pc.path AND m.content_key = pc.content_key \n             WHERE pc.path = ?1 AND pc.language IS NOT NULL",
        )
        .map_err(cache)?;
    for path in changed {
        let rows = stmt
            .query_map([path], |row| row.get::<_, String>(0))
            .map_err(cache)?;
        for language in rows {
            let language = language.map_err(cache)?;
            if languages.insert(language) && languages.len() >= MAX_ROUTING_LANGUAGES {
                break;
            }
        }
    }
    Ok(languages.into_iter().take(MAX_ROUTING_LANGUAGES).collect())
}

/// Symbols defined by the changed paths of the target snapshot, read
/// through the worktree manifest (membership) and the shared
/// content-addressed symbol table, bounded and sorted.
fn changed_symbols(
    paths: &StoragePaths,
    changed: &[String],
) -> Result<Vec<String>, CodeReviewError> {
    let worktree = open_or_recover(&paths.worktree, DbKind::Worktree)
        .map_err(|err| CodeReviewError::Cache(format!("{}: {err}", err)))?;
    let mut content_keys: Vec<(String, String)> = Vec::new();
    {
        let cache = |err: rusqlite::Error| CodeReviewError::Cache(err.to_string());
        let mut stmt = worktree
            .conn
            .prepare(
                "SELECT pc.content_key, pc.language FROM path_content pc \n                 JOIN manifest m ON m.path = pc.path AND m.content_key = pc.content_key \n                 WHERE pc.path = ?1 AND pc.language IS NOT NULL",
            )
            .map_err(cache)?;
        for path in changed.iter().take(MAX_SYMBOL_PATHS) {
            let rows = stmt
                .query_map([path], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(cache)?;
            for row in rows {
                let (key, language) = row.map_err(cache)?;
                if !content_keys.contains(&(key.clone(), language.clone())) {
                    content_keys.push((key, language));
                }
            }
        }
    }

    let shared = open_or_recover(&paths.shared, DbKind::Shared)
        .map_err(|err| CodeReviewError::Cache(format!("{}: {err}", err)))?;
    let cache = |err: rusqlite::Error| CodeReviewError::Cache(err.to_string());
    let mut names: BTreeSet<String> = BTreeSet::new();
    let mut stmt = shared
        .conn
        .prepare(
            "SELECT DISTINCT s.name FROM symbol s \n             JOIN parsed_artifact pa ON pa.id = s.parsed_artifact_id \n             WHERE pa.content_key = ?1 AND pa.language = ?2 AND pa.parser_version = ?3",
        )
        .map_err(cache)?;
    for (content_key, language) in &content_keys {
        let rows = stmt
            .query_map(
                rusqlite::params![content_key, language, PARSER_VERSION],
                |row| row.get::<_, String>(0),
            )
            .map_err(cache)?;
        for name in rows {
            names.insert(name.map_err(cache)?);
            if names.len() >= MAX_CHANGED_SYMBOLS {
                return Ok(names.into_iter().take(MAX_CHANGED_SYMBOLS).collect());
            }
        }
    }
    Ok(names.into_iter().collect())
}

/// Assemble the diff portion of the reviewer initial context: one
/// bounded [`ContextKind::Diff`] entry per changed path, path order,
/// within the aggregate diff budget. The diff is direct review input;
/// it can never be crowded out by retrieved context.
fn diff_context_entries(patches: &[SnapshotPatch]) -> ContextBundle {
    let mut entries: Vec<ContextEntry> = Vec::new();
    let mut total = 0usize;
    let mut truncated = false;
    for patch in patches {
        if total >= DIFF_TOTAL_MAX_BYTES {
            truncated = true;
            break;
        }
        let remaining = DIFF_TOTAL_MAX_BYTES - total;
        let body = if patch.binary {
            "binary content changed; no textual diff available".to_string()
        } else if let Some(text) = patch.patch.as_deref() {
            if text.is_empty() {
                // Headers only (for example a mode-only change).
                "changed with no textual diff (mode change only)".to_string()
            } else {
                text.to_owned()
            }
        } else {
            "changed; no textual diff available".to_string()
        };
        let text = if body.len() > remaining {
            truncated = true;
            if remaining <= 64 {
                break;
            }
            format!(
                "{}\n\u{2026} [diff truncated]",
                truncate_text(&body, remaining - 24)
            )
        } else {
            body
        };
        total += text.len();
        entries.push(ContextEntry {
            kind: ContextKind::Diff,
            title: format!("diff: {} ({})", patch.path, diff_status_label(patch.status)),
            path: Some(patch.path.clone()),
            symbol: None,
            line_start: None,
            line_end: None,
            record_id: None,
            practice_id: None,
            text,
            score: 1.0,
        });
    }
    ContextBundle {
        entries,
        truncated,
        text_bytes: total,
    }
}

/// Stable wire label of a diff status.
fn diff_status_label(status: DiffStatus) -> &'static str {
    match status {
        DiffStatus::Added => "added",
        DiffStatus::Deleted => "deleted",
        DiffStatus::Modified => "modified",
        DiffStatus::Renamed => "renamed",
        DiffStatus::Copied => "copied",
        DiffStatus::TypeChanged => "type-changed",
    }
}

/// Assemble one code reviewer's bounded initial context: the diff
/// entries first (direct review input), then the shared retrieved
/// context and the reviewer's practices fitted into the remaining
/// budget of the shared initial-context bound.
fn code_initial_context(
    diff_context: &ContextBundle,
    shared: &ContextBundle,
    catalog: &Catalog,
    practice_tags: &[String],
) -> ContextBundle {
    let budget = ContextBudget {
        max_entries: INITIAL_CONTEXT_BUDGET
            .max_entries
            .saturating_sub(diff_context.entries.len()),
        max_entry_text_bytes: ContextBudget::default().max_entry_text_bytes,
        max_total_text_bytes: INITIAL_CONTEXT_BUDGET
            .max_total_text_bytes
            .saturating_sub(diff_context.text_bytes),
    };
    let rest = initial_context(&[], shared, catalog, practice_tags, &budget);
    let mut entries = diff_context.entries.clone();
    let text_bytes = diff_context.text_bytes + rest.text_bytes;
    entries.extend(rest.entries);
    ContextBundle {
        entries,
        truncated: diff_context.truncated || rest.truncated,
        text_bytes,
    }
}

/// Normalize a host-provided dynamic domain description against the
/// static catalog (plan section 19): bound every field, drop empty and
/// oversized entries, and keep only practice tags the bundled catalog
/// actually provides. An unusable description (empty label) becomes
/// `None` so the caller falls back to the static template.
fn normalize_domain_description(
    description: Option<DynamicDomainDescription>,
    catalog: &Catalog,
) -> Option<DynamicDomainDescription> {
    let mut description = description?;
    let label = description.domain_label.trim();
    if label.is_empty() {
        return None;
    }
    description.domain_label = bounded(label, DOMAIN_LABEL_MAX_BYTES);

    let known_tags = catalog.practice_tags();
    let normalize_list = |list: &[String], max_bytes: usize| -> Vec<String> {
        let mut seen = BTreeSet::new();
        list.iter()
            .map(|entry| entry.trim())
            .filter(|entry| !entry.is_empty() && entry.len() <= max_bytes)
            .filter(|entry| seen.insert((*entry).to_string()))
            .take(DOMAIN_LIST_MAX_ENTRIES)
            .map(str::to_string)
            .collect()
    };
    description.focus_terms = normalize_list(&description.focus_terms, DOMAIN_TERM_MAX_BYTES);
    description.risks_to_check = normalize_list(&description.risks_to_check, DOMAIN_TERM_MAX_BYTES);
    let known: Vec<String> = description
        .relevant_practice_tags
        .iter()
        .filter(|tag| known_tags.contains(tag.as_str()))
        .cloned()
        .collect();
    description.relevant_practice_tags = normalize_list(&known, DOMAIN_TERM_MAX_BYTES);
    Some(description)
}

/// Everything recorded in the local review event (plan section 57).
struct ReviewEvent<'a> {
    result: &'a ReviewResult,
    routing_input: &'a crate::domain::routing::RoutingInput,
    decision: &'a crate::domain::routing::RoutingDecision,
    jury: &'a crate::domain::jury::JuryPlan,
    domain_description: Option<&'a DynamicDomainDescription>,
    specs: &'a [ReviewerExecutionSpec],
    outcomes: &'a [(String, ReviewerOutcome)],
    validated: &'a [ValidatedFinding],
    rejected: &'a [RejectedFinding],
    synthesis_diagnostics: &'a [crate::domain::finding::SynthesisDiagnostic],
    diagnostics: &'a [String],
    /// Review-input provenance (`plan_source`/`code_source` payload).
    source: &'a serde_json::Value,
}

/// Persist the local review event in the shared cache (regenerable
/// local state, never durable project knowledge).
fn persist_review_event(paths: &StoragePaths, event: &ReviewEvent<'_>) -> Result<(), String> {
    let cache = |err: rusqlite::Error| err.to_string();
    let db =
        open_or_recover(&paths.shared, DbKind::Shared).map_err(|err| format!("{}: {err}", err))?;
    let payload = json!({
        "schema": 1,
        "source": event.source,
        "routing_input": {
            "descriptors": event.routing_input.descriptors,
            "languages": event.routing_input.languages,
            "deterministic_tags": event.routing_input.deterministic_tags,
            "lesson_hints": event.routing_input.lessons.len(),
        },
        "decision": event.decision,
        "jury": event.jury,
        "domain_description": event.domain_description,
        "specs": event
            .specs
            .iter()
            .map(|spec| {
                json!({
                    "reviewer_id": spec.reviewer_id,
                    "specialization": spec.specialization,
                    "model_profile": spec.model_profile,
                    "thinking_level": spec.thinking_level,
                    "checklist_items": spec.checklist.len(),
                    "context_entries": spec.initial_context.len(),
                    "capabilities": spec
                        .allowed_capabilities
                        .iter()
                        .map(|capability| capability.name())
                        .collect::<Vec<_>>(),
                    "max_lookup_calls": spec.max_lookup_calls,
                    "max_returned_context_bytes": spec.max_returned_context_bytes,
                    "timeout_ms": spec.timeout_ms,
                })
            })
            .collect::<Vec<_>>(),
        "outcomes": event
            .outcomes
            .iter()
            .map(|(reviewer_id, outcome)| match outcome {
                ReviewerOutcome::Completed(report) => json!({
                    "reviewer_id": reviewer_id,
                    "outcome": "completed",
                    "summary": report.summary,
                    "finding_count": report.findings.len(),
                    "candidate_fact_count": report.candidate_project_facts.len(),
                    "duration_ms": report.duration_ms,
                    "report": report,
                }),
                ReviewerOutcome::Failed(error) => json!({
                    "reviewer_id": reviewer_id,
                    "outcome": "failed",
                    "error": { "code": error.code, "message": error.message },
                }),
            })
            .collect::<Vec<_>>(),
        "validated": event
            .validated
            .iter()
            .map(|finding| json!({
                "reviewer_id": finding.reviewer_id,
                "finding_id": finding.finding.id,
                "fingerprint": finding.fingerprint,
                "severity": finding.severity(),
            }))
            .collect::<Vec<_>>(),
        "rejected": event
            .rejected
            .iter()
            .map(|rejected| json!({
                "reviewer_id": rejected.reviewer_id,
                "finding_id": rejected.finding_id,
                "reason": rejected.reason,
            }))
            .collect::<Vec<_>>(),
        "synthesis_diagnostics": event.synthesis_diagnostics,
        "diagnostics": event.diagnostics,
        "result": event.result,
    });
    db.conn
        .execute(
            "INSERT INTO review_event (review_id, kind, snapshot, payload, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                event.result.review_id,
                kind_label(event.result.kind),
                serde_json::to_string(&event.result.snapshot).expect("descriptor serializes"),
                serde_json::to_string(&payload).expect("event payload serializes"),
                event.result.created_at,
            ],
        )
        .map(|_| ())
        .map_err(cache)
}

/// Render the human-readable form of a review result.
pub fn render_review_text(result: &ReviewResult) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "nisse {} review {}\n",
        kind_label(result.kind),
        result.review_id
    ));
    out.push_str(&format!("snapshot: {}\n", result.snapshot.mode_label()));
    out.push_str(&format!("created: {}\n", result.created_at));
    out.push('\n');
    out.push_str(&format!("summary: {}\n", result.summary));
    out.push('\n');
    if result.findings.is_empty() {
        out.push_str("findings: none\n");
        return out;
    }
    out.push_str(&format!("findings ({}):\n", result.findings.len()));
    for finding in &result.findings {
        let marker = match finding.severity {
            Severity::Blocker => "[blocker]",
            Severity::High => "[high]",
            Severity::Medium => "[medium]",
            Severity::Low => "[low]",
            Severity::Info => "[info]",
        };
        out.push_str(&format!(
            "  {marker} {} ({}, {}){}\n",
            finding.title,
            serde_json::to_string(&finding.impact_class)
                .expect("impact class serializes")
                .trim_matches('"'),
            finding.category,
            if finding.cross_validated {
                format!(" — cross-validated by {}", finding.reviewer_ids.join(", "))
            } else {
                String::new()
            },
        ));
        out.push_str(&format!("    claim: {}\n", finding.claim));
        if !finding.recommendation.is_empty() {
            out.push_str(&format!("    recommendation: {}\n", finding.recommendation));
        }
        for evidence in &finding.evidence {
            let location: Vec<String> = evidence
                .path
                .as_ref()
                .map(|path| match (evidence.line_start, evidence.line_end) {
                    (Some(start), Some(end)) => format!("{path}:{start}-{end}"),
                    (Some(start), None) => format!("{path}:{start}"),
                    _ => path.clone(),
                })
                .into_iter()
                .chain(evidence.plan_section.clone())
                .chain(evidence.record_id.clone())
                .collect();
            let location = if location.is_empty() {
                String::new()
            } else {
                format!(" at {}", location.join(" "))
            };
            let excerpt = truncate_text(&evidence.excerpt, 160);
            out.push_str(&format!(
                "    evidence: {}{location}: \"{excerpt}\"\n",
                serde_json::to_string(&evidence.kind)
                    .expect("evidence kind serializes")
                    .trim_matches('"'),
            ));
        }
    }
    out
}

/// Wire label of a review kind.
fn kind_label(kind: ReviewKind) -> &'static str {
    match kind {
        ReviewKind::Plan => "plan",
        ReviewKind::Code => "code",
    }
}

/// The host launch used by the `nisse` binary: runtime, interpreter,
/// and host script are selected by explicit environment variables
/// (tests select the deterministic fake/scripted runtimes; production
/// defaults to real Pi sessions). Invalid values are typed errors,
/// never guesses.
pub fn host_launch_from_env() -> Result<PiHostLaunch, String> {
    let runtime = std::env::var("NISSE_PI_HOST_RUNTIME").unwrap_or_else(|_| "pi".to_string());
    let runtime = match runtime.as_str() {
        "fake" => HostRuntime::Fake,
        "pi" => HostRuntime::Pi,
        other => {
            return Err(format!(
                "NISSE_PI_HOST_RUNTIME must be `fake` or `pi`, got `{other}`"
            ));
        }
    };
    let program = std::env::var("NISSE_PI_HOST_PROGRAM").unwrap_or_else(|_| "node".to_string());
    let script = std::env::var("NISSE_PI_HOST_SCRIPT").unwrap_or_else(|_| {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("runtime/pi-host/src/main.ts")
            .to_string_lossy()
            .into_owned()
    });
    Ok(PiHostLaunch::new(
        &program,
        std::path::PathBuf::from(script),
        runtime,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_descriptors_extract_bounded_headings() {
        let plan =
            "# Fix the audio pipeline\n\nbody\n\n## Queue changes\n\ntext\n### Deep heading\n";
        assert_eq!(
            plan_descriptors(plan),
            vec![
                "Fix the audio pipeline".to_string(),
                "Queue changes".to_string(),
                "Deep heading".to_string(),
            ]
        );

        // No headings: the first non-empty line is the descriptor.
        assert_eq!(
            plan_descriptors("\n\nfirst line\nsecond line\n"),
            vec!["first line".to_string()]
        );

        // Descriptor list and entries are bounded.
        let many: String = (0..64)
            .map(|index| format!("# heading {index} with some padding text\n"))
            .collect();
        let descriptors = plan_descriptors(&many);
        assert_eq!(descriptors.len(), MAX_PLAN_DESCRIPTORS);
        assert!(descriptors[0].len() <= MAX_DESCRIPTOR_BYTES);

        let long_heading = format!("# {}\n", "x".repeat(MAX_DESCRIPTOR_BYTES + 50));
        assert!(plan_descriptors(&long_heading)[0].len() <= MAX_DESCRIPTOR_BYTES);

        // Empty and heading-only-noise plans produce no descriptors.
        assert!(plan_descriptors("   \n\n").is_empty());
        assert!(plan_descriptors("#\n##\n").is_empty());
    }

    #[test]
    fn plan_context_entry_truncates_deterministically() {
        let small = PlanInput {
            text: "short plan".to_string(),
            path: Some("plan.md".to_string()),
        };
        let entry = plan_input_entry(&small);
        assert_eq!(entry.kind, ContextKind::Plan);
        assert_eq!(entry.path.as_deref(), Some("plan.md"));
        assert_eq!(entry.text, "short plan");

        let big = PlanInput {
            text: "p\n".repeat(PLAN_CONTEXT_MAX_BYTES),
            path: None,
        };
        let entry = plan_input_entry(&big);
        assert!(entry.text.len() < big.text.len());
        assert!(entry.text.contains("plan truncated"));
        assert!(entry.text.len() < PLAN_CONTEXT_MAX_BYTES + 200);
    }

    #[test]
    fn normalization_bounds_and_filters_domain_descriptions() {
        let catalog = Catalog::bundled().unwrap();
        assert!(catalog.practice_tags().contains("realtime"));

        let mut description = DynamicDomainDescription {
            domain_label: "  audio dsp  ".to_string(),
            focus_terms: vec![
                "sampling rate".to_string(),
                String::new(),
                " ".to_string(),
                "x".repeat(DOMAIN_TERM_MAX_BYTES + 1),
                "sampling rate".to_string(),
            ],
            risks_to_check: (0..20).map(|index| format!("risk {index}")).collect(),
            relevant_practice_tags: vec![
                "realtime".to_string(),
                "no-such-tag".to_string(),
                "realtime".to_string(),
            ],
        };
        let normalized = normalize_domain_description(Some(description.clone()), &catalog);
        let normalized = normalized.expect("description normalizes");
        assert_eq!(normalized.domain_label, "audio dsp");
        assert_eq!(normalized.focus_terms, vec!["sampling rate".to_string()]);
        assert_eq!(normalized.risks_to_check.len(), DOMAIN_LIST_MAX_ENTRIES);
        assert_eq!(
            normalized.relevant_practice_tags,
            vec!["realtime".to_string()]
        );

        // Empty labels and absent descriptions fall back to None.
        description.domain_label = "   ".to_string();
        assert!(normalize_domain_description(Some(description), &catalog).is_none());
        assert!(normalize_domain_description(None, &catalog).is_none());
    }

    #[test]
    fn render_review_text_covers_severities_and_evidence() {
        fn snapshot() -> crate::domain::snapshot::SnapshotDescriptor {
            crate::domain::snapshot::SnapshotDescriptor {
                mode: crate::domain::snapshot::SnapshotMode::Working,
                base_commit: Some("0123456789abcdef0123456789abcdef01234567".to_string()),
                base_tree: Some("fedcba9876543210fedcba9876543210fedcba98".to_string()),
                head_commit: None,
                head_tree: None,
            }
        }
        let result = ReviewResult {
            review_id: "01TEST".to_string(),
            kind: ReviewKind::Plan,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            snapshot: snapshot(),
            summary: "One finding.".to_string(),
            findings: vec![crate::domain::finding::FinalFinding {
                id: "g1".to_string(),
                impact_class: crate::domain::review::ImpactClass::Correctness,
                severity: Severity::High,
                category: "logic".to_string(),
                title: "Queue race".to_string(),
                claim: "Two contexts mutate the queue.".to_string(),
                explanation: "ISR writes during reads.".to_string(),
                recommendation: "Guard access.".to_string(),
                evidence: vec![crate::domain::evidence::EvidenceRef {
                    kind: crate::domain::evidence::EvidenceKind::Plan,
                    path: None,
                    symbol: None,
                    line_start: None,
                    line_end: None,
                    content_key: None,
                    plan_section: Some("Concurrency".to_string()),
                    record_id: None,
                    excerpt: "The ISR enqueues directly.".to_string(),
                }],
                source_finding_ids: vec!["quality/f-1".to_string()],
                reviewer_ids: vec!["quality".to_string()],
                cross_validated: false,
            }],
        };
        let text = render_review_text(&result);
        assert!(text.contains("nisse plan review 01TEST"));
        assert!(text.contains("[high] Queue race (correctness, logic)"));
        assert!(text.contains("claim: Two contexts mutate the queue."));
        assert!(text.contains("evidence: plan at Concurrency"));

        let empty = ReviewResult {
            review_id: "02TEST".to_string(),
            kind: ReviewKind::Plan,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            snapshot: snapshot(),
            summary: "Nothing to report.".to_string(),
            findings: Vec::new(),
        };
        assert!(render_review_text(&empty).contains("findings: none"));
    }

    #[test]
    fn code_descriptors_lead_with_subjects_then_path_tokens() {
        let subjects = vec![
            "Fix the race on the shared queue".to_string(),
            String::new(),
            "  \n".to_string(),
            format!("{}", "x".repeat(MAX_DESCRIPTOR_BYTES + 10)),
        ];
        let descriptors = code_descriptors(
            subjects,
            &[
                "db/migrations/0010_users.sql".to_string(),
                "src/queue/mod.rs".to_string(),
            ],
        );
        assert_eq!(
            descriptors[0],
            "Fix the race on the shared queue".to_string()
        );
        // Empty subjects are skipped; oversized ones are bounded.
        assert_eq!(descriptors[1].len(), MAX_DESCRIPTOR_BYTES);
        assert!(descriptors.contains(&"db migrations 0010_users.sql".to_string()));
        assert!(descriptors.contains(&"src queue mod.rs".to_string()));

        // The descriptor list is bounded.
        let many: Vec<String> = (0..100).map(|i| format!("dir{i}/file{i}.rs")).collect();
        assert_eq!(
            code_descriptors(Vec::new(), &many).len(),
            MAX_CODE_DESCRIPTORS
        );
    }

    #[test]
    fn retrieval_text_joins_and_bounds_descriptors_and_symbols() {
        let text = retrieval_text(&["alpha beta".to_string()], &["enqueue".to_string()]);
        assert_eq!(text, "alpha beta enqueue");

        let big: Vec<String> = (0..1000)
            .map(|index| format!("descriptor{index}"))
            .collect();
        assert!(retrieval_text(&big, &[]).len() <= MAX_QUERY_BYTES);
    }

    fn patch(path: &str, status: DiffStatus, text: Option<&str>, binary: bool) -> SnapshotPatch {
        SnapshotPatch {
            path: path.to_string(),
            status,
            binary,
            patch: text.map(str::to_owned),
            truncated: false,
        }
    }

    #[test]
    fn diff_context_entries_lead_with_real_diffs_and_mark_binary() {
        let patches = vec![
            patch(
                "src/queue.rs",
                DiffStatus::Modified,
                Some("--- a/src/queue.rs\n+++ b/src/queue.rs\n@@ -1,3 +1,4 @@\n- old\n+ new"),
                false,
            ),
            patch("assets/blob.bin", DiffStatus::Modified, None, true),
            patch("mode.sh", DiffStatus::Modified, Some(""), false),
        ];
        let bundle = diff_context_entries(&patches);
        assert_eq!(bundle.entries.len(), 3);
        assert!(!bundle.truncated);
        let first = &bundle.entries[0];
        assert_eq!(first.kind, ContextKind::Diff);
        assert_eq!(first.path.as_deref(), Some("src/queue.rs"));
        assert_eq!(first.title, "diff: src/queue.rs (modified)");
        assert!(first.text.contains("+ new"));
        assert_eq!(first.score, 1.0);
        assert_eq!(
            bundle.entries[1].text,
            "binary content changed; no textual diff available"
        );
        assert_eq!(
            bundle.entries[2].text,
            "changed with no textual diff (mode change only)"
        );
        assert!(bundle.text_bytes > 0);
    }

    #[test]
    fn diff_context_entries_honor_the_aggregate_budget() {
        // Uneven sizes force the last admitted entry to overflow the
        // aggregate budget and be truncated to fit.
        let filler = "x".repeat(15_000);
        let patches: Vec<SnapshotPatch> = (0..8)
            .map(|index| {
                patch(
                    &format!("src/file{index:02}.rs"),
                    DiffStatus::Modified,
                    Some(&filler),
                    false,
                )
            })
            .collect();
        let bundle = diff_context_entries(&patches);
        assert!(bundle.text_bytes <= DIFF_TOTAL_MAX_BYTES);
        assert!(bundle.truncated, "entries beyond the budget are dropped");
        assert_eq!(
            bundle.entries.len(),
            5,
            "four full entries plus one truncated"
        );
        // The last admitted entry is truncated to fit exactly.
        let last = bundle.entries.last().unwrap();
        assert!(
            last.text.contains("[diff truncated]"),
            "{}",
            last.text.len()
        );

        // Exact fits do not count as truncation.
        let exact = "x".repeat(DIFF_ENTRY_MAX_BYTES);
        let patches: Vec<SnapshotPatch> = (0..4)
            .map(|index| {
                patch(
                    &format!("src/fit{index}.rs"),
                    DiffStatus::Modified,
                    Some(&exact),
                    false,
                )
            })
            .collect();
        let bundle = diff_context_entries(&patches);
        assert_eq!(bundle.entries.len(), 4);
        assert!(!bundle.truncated);
        assert_eq!(bundle.text_bytes, DIFF_TOTAL_MAX_BYTES);
    }

    #[test]
    fn code_initial_context_pins_diffs_and_assembles_the_rest() {
        let catalog = Catalog::bundled().unwrap();
        let diff_entry = ContextEntry {
            kind: ContextKind::Diff,
            title: "diff: src/queue.rs (modified)".to_string(),
            path: Some("src/queue.rs".to_string()),
            symbol: None,
            line_start: None,
            line_end: None,
            record_id: None,
            practice_id: None,
            text: "diff body".to_string(),
            score: 1.0,
        };
        let diff_context = ContextBundle {
            entries: vec![diff_entry.clone()],
            truncated: false,
            text_bytes: 9,
        };
        let shared = ContextBundle {
            entries: vec![ContextEntry {
                kind: ContextKind::SymbolDefinition,
                title: "function enqueue".to_string(),
                path: Some("src/queue.rs".to_string()),
                symbol: Some("enqueue".to_string()),
                line_start: Some(1),
                line_end: Some(3),
                record_id: None,
                practice_id: None,
                text: "fn enqueue()".to_string(),
                score: 0.9,
            }],
            truncated: false,
            text_bytes: 12,
        };

        let bundle = code_initial_context(
            &diff_context,
            &shared,
            &catalog,
            &["concurrency".to_string()],
        );
        // The diff is the leading entry and is never crowded out.
        assert_eq!(bundle.entries.first(), Some(&diff_entry));
        assert_eq!(bundle.entries[1].kind, ContextKind::SymbolDefinition);
        assert!(bundle.entries.len() >= 3, "diff + symbol + practices");
        assert!(
            bundle.entries[2..]
                .iter()
                .all(|entry| entry.kind == ContextKind::Practice)
        );
        assert!(bundle.text_bytes > 9);
    }

    #[test]
    fn code_review_instructions_frame_the_diff_as_primary_input() {
        let instructions = code_review_instructions("Base instructions.");
        assert!(instructions.starts_with("Base instructions."));
        assert!(instructions.contains("## Review kind: code"));
        assert!(instructions.contains("unified diff entries"));
    }
}
