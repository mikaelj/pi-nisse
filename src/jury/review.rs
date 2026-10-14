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
//! 7. route specialists with the mandatory rules classifier and,
//!    when configured, the optional provider blend (plan sections
//!    33–34) — a provider may adjust scores but never bypass the
//!    deterministic jury constraints, and provider failures keep the
//!    rules decision;
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
//!     section 57);
//! 19. validate/verify fact candidates through independent Pi verifier
//!     sessions and deterministic checks (plan section 47);
//! 20. persist supported facts — only with a contributor identity — or
//!     retain the candidates as pending local state.
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
use std::sync::Arc;
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
use crate::providers::routing::classify_with_optional_provider;
use crate::providers::{EmbeddingProvider, RoutingClassifierProvider};
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
use super::synthesis::{ConsolidationOptions, consolidate, consolidate_with_dispositions};
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
    max_entries: 96,
    max_entry_text_bytes: PLAN_CONTEXT_MAX_BYTES,
    max_total_text_bytes: 192 * 1024,
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

/// The one-round driver budget for a configured reviewer-session
/// budget: the session timeout plus the frozen slack.
pub(crate) fn review_round_timeout(reviewer_timeout_ms: u64) -> Duration {
    Duration::from_millis(reviewer_timeout_ms.saturating_add(REVIEW_ROUND_SLACK_MS))
}

/// Slack the round driver adds on top of the reviewer-session budget.
const REVIEW_ROUND_SLACK_MS: u64 = 10_000;

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

    /// A session's (model profile, thinking level) pair could not be
    /// made valid (unresolved profile or model).
    #[error("thinking policy: {0}")]
    ThinkingPolicy(String),

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
/// (Not `Debug`: the optional provider handles are opaque trait
/// objects.)
#[derive(Clone)]
pub struct PlanReviewOptions {
    /// How to launch the Pi review host child process.
    pub launch: PiHostLaunch,
    /// Bounds of the consolidation pipeline.
    pub consolidation: ConsolidationOptions,
    /// Resolved model profiles forwarded to the host through
    /// `host_config` (from trusted local configuration; reviewer
    /// sessions never inherit a model).
    pub model_profiles: BTreeMap<String, crate::domain::model::ModelProfile>,
    /// Optional routing provider blended into the rules decision
    /// (plan sections 33–34). Failures fall back to rules-only routing.
    pub routing_provider: Option<Arc<dyn RoutingClassifierProvider>>,
    /// Optional embedding provider reranking retrieval results (plan
    /// sections 30 and 34). Failures degrade to the baseline ranking.
    pub embedding_provider: Option<Arc<dyn EmbeddingProvider>>,
    /// Contributor identity for durable knowledge created by the
    /// review (plan section 44). Reviews are noninteractive: when no
    /// identity is configured, learning remains pending locally.
    pub contributor_id: Option<String>,
    /// Bounds of the fact-candidate pipeline (plan section 47).
    pub fact_processing: crate::learning::facts::FactProcessingOptions,
    /// Bound on concurrently running reviewer sessions in the Pi host
    /// (plan milestone 29: reviewer concurrency obeys config).
    pub max_concurrent_reviewers: u32,
    /// Whole-session budget of one reviewer session (all turns; the
    /// frozen single retry uses the same budget). Defaults to
    /// [`crate::config::DEFAULT_REVIEWER_TIMEOUT_MS`].
    pub reviewer_timeout_ms: u64,
}

impl PlanReviewOptions {
    /// Options for `launch` with default consolidation bounds, the
    /// default reviewer-concurrency bound, and no configured model
    /// profiles or optional providers.
    pub fn new(launch: PiHostLaunch) -> Self {
        PlanReviewOptions {
            launch,
            consolidation: ConsolidationOptions::default(),
            model_profiles: BTreeMap::new(),
            routing_provider: None,
            embedding_provider: None,
            contributor_id: None,
            fact_processing: crate::learning::facts::FactProcessingOptions::default(),
            max_concurrent_reviewers: crate::config::DEFAULT_MAX_CONCURRENT_REVIEWERS as u32,
            reviewer_timeout_ms: crate::config::DEFAULT_REVIEWER_TIMEOUT_MS,
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

    // 5. Durable records + preliminary facts/lessons retrieval. An
    // optional embedding provider may rerank the bounded candidate
    // set; every provider failure degrades to the baseline ranking.
    let catalog = Catalog::bundled()?;
    let records = RecordStore::new(&ctx.worktree_root).load()?;
    let retriever = match &options.embedding_provider {
        Some(provider) => {
            Retriever::open(resolved, paths)?.with_embedding_provider(provider.clone())
        }
        None => Retriever::open(resolved, paths)?,
    };
    // Potentially stale facts (plan section 48) are marked and
    // downranked in the shared context: current source evidence
    // outranks stale memory (permanent invariant 22).
    let stale_facts = stale_fact_ids(ctx, resolved, &records)?;
    retriever.sync_records(&records, &stale_facts)?;
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

    // 7. Deterministic routing with the optional provider (plan
    //    sections 33–34); 8. jury rules. The provider may only blend
    //    scores — never bypass the jury constraints — and every
    //    provider failure keeps the rules decision.
    let rules_decision = RulesClassifier::new(catalog.clone()).classify(&routing_input);
    let (decision, mut diagnostics) = classify_with_optional_provider(
        rules_decision,
        options.routing_provider.as_deref(),
        &routing_input,
        &catalog,
    );
    let jury = build_jury_plan(&decision, &catalog)?;

    // 11. Start the Pi host early so every semantic job shares it. The
    // configured reviewer-concurrency bound travels with the host
    // config (plan milestone 29).
    let mut host = HostProcess::spawn(&options.launch)?;
    host.wait_ready(HOST_READY_TIMEOUT)?;
    let host_config = HostConfigPayload::from_config_profiles(&options.model_profiles)
        .with_max_concurrent_reviewers(options.max_concurrent_reviewers);
    host.send_host_config(&host_config)?;

    // 11b. Thinking policy (protocol version 2): the host reports each
    // profile's supported thinking levels; every session level below
    // is routed through it, so an invalid (model, level) pair can
    // never be constructed.
    let capabilities = host.wait_model_capabilities(HOST_READY_TIMEOUT)?;
    let thinking = super::thinking::ThinkingPolicy::build(&capabilities, &options.model_profiles);
    diagnostics.extend(thinking.warnings().iter().cloned());

    // 12. Constrained dynamic-domain description for the fallback seat.
    // The session level is routed through the thinking policy; an
    // unusable profile skips the session and falls back to the static
    // template.
    let mut domain_description: Option<DynamicDomainDescription> = None;
    let lookup_budget = REVIEWER_MAX_LOOKUP_CALLS;
    let dynamic_level = if jury.uses_dynamic_fallback() {
        let expert = catalog.dynamic_domain_expert();
        match super::thinking::ThinkingPolicy::resolve_job_level(
            Some(&thinking),
            &expert.default_model_profile,
            expert.default_thinking_level,
        ) {
            Ok(level) => Some(level),
            Err(reason) => {
                diagnostics.push(format!(
                    "dynamic domain description fell back to the static template: {reason}"
                ));
                None
            }
        }
    } else {
        None
    };
    if let Some(thinking_level) = dynamic_level {
        let expert = catalog.dynamic_domain_expert();
        let payload = DomainDescriptionPayload {
            description_id: new_ulid(),
            timeout_ms: DOMAIN_DESCRIPTION_TIMEOUT_MS,
            model_profile: expert.default_model_profile.clone(),
            thinking_level,
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
    let mut specs: Vec<ReviewerExecutionSpec> = Vec::with_capacity(jury.seats.len());
    for seat in &jury.seats {
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
        let (level, note) = thinking
            .resolve(
                &profile.default_model_profile,
                None,
                profile.default_thinking_level,
            )
            .map_err(PlanReviewError::ThinkingPolicy)?;
        if let Some(note) = note {
            diagnostics.push(note);
        }
        specs.push(execution_spec(
            profile,
            seat,
            context,
            level,
            options.reviewer_timeout_ms,
            lookup_budget,
            REVIEWER_MAX_CONTEXT_BYTES,
        ));
    }

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
    let outcomes = super::execution::execute_review(
        &mut host,
        &payload,
        &mut serve,
        review_round_timeout(options.reviewer_timeout_ms),
    )?;

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
        &thinking,
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

    // 19./20. Validate/verify fact candidates; persist supported facts
    // or pending candidates (plan sections 47 and 58). Reviews are
    // noninteractive: without a contributor identity, learning remains
    // pending locally (plan section 44), and every verifier failure
    // degrades to pending state — never a failed review.
    let fact_summary = crate::learning::facts::process_fact_candidates(
        &mut host,
        ctx,
        paths,
        &validation_context,
        &review_id,
        &reports,
        &records,
        options.contributor_id.as_deref(),
        &options.fact_processing,
        &thinking,
    );
    diagnostics.extend(fact_summary.diagnostics);

    // 21. Deterministic learning maintenance (plan section 58 step
    //     21): repeated review outcomes become routing lessons without
    //     any LLM statistics (plan section 51). Maintenance is
    //     idempotent and conservative — it can never fail the review.
    let lesson_summary = crate::learning::lessons::run_learning_maintenance(
        ctx,
        Some(paths),
        &records,
        options.contributor_id.as_deref(),
    );
    diagnostics.extend(lesson_summary.diagnostics);

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

/// The potentially stale fact IDs of `records` against the exact
/// target snapshot (plan section 48). Assessment is deterministic —
/// content-key provenance, path presence, excerpt containment — and
/// never consults an LLM.
pub(crate) fn stale_fact_ids(
    ctx: &RepoContext,
    resolved: &ResolvedSnapshot,
    records: &[StoredRecord],
) -> Result<BTreeSet<String>, crate::git::GitError> {
    let reader = SnapshotReader::new(ctx, resolved)?;
    let format = ctx.object_format()?;
    Ok(crate::learning::staleness::assess_records(records, &reader, format).ids())
}

/// Distinct index languages of the freshly refreshed target snapshot,
/// read from the worktree manifest (the same refresh that bound the
/// snapshot; `Retriever::open` has already verified the binding).
pub(crate) fn snapshot_languages(paths: &StoragePaths) -> Result<Vec<String>, PlanReviewError> {
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
pub(crate) fn record_tags(context: &ContextBundle, active: &[&StoredRecord]) -> Vec<String> {
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
pub(crate) fn active_lesson_hints(active: &[&StoredRecord]) -> Vec<RoutingHint> {
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
pub(crate) struct SeatPlan {
    /// Seat reviewer id.
    pub(crate) reviewer: String,
    /// Seat specialization label.
    pub(crate) specialization: String,
    /// Full reviewer instructions.
    pub(crate) instructions: String,
    /// Checklist items (static first, domain risks appended).
    pub(crate) checklist: Vec<String>,
    /// Practice tags whose packs may enter the reviewer's context.
    pub(crate) practice_tags: Vec<String>,
}

/// Combine a reviewer profile with review-kind instructions and an
/// optional constrained domain description (plan section 19: the
/// description can never add tools, providers, or capabilities — those
/// come only from the static profile; the static checklist items always
/// come first).
pub(crate) fn seat_plan(
    profile: &ReviewerProfile,
    kind_instructions: String,
    description: Option<&DynamicDomainDescription>,
) -> SeatPlan {
    let reviewer = profile.id.clone();
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
        reviewer,
        specialization: bounded(&specialization, MAX_DESCRIPTOR_BYTES * 2),
        instructions,
        checklist,
        practice_tags,
    }
}

/// Assemble one juror's final execution spec from its seat plan and
/// assembled initial context (plan section 18).
pub(crate) fn execution_spec(
    profile: &ReviewerProfile,
    seat: SeatPlan,
    initial_context: ContextBundle,
    thinking_level: crate::domain::model::ThinkingLevel,
    timeout_ms: u64,
    max_lookup_calls: u32,
    max_returned_context_bytes: u64,
) -> ReviewerExecutionSpec {
    ReviewerExecutionSpec {
        reviewer_id: profile.id.clone(),
        specialization: seat.specialization,
        instructions: seat.instructions,
        checklist: seat.checklist,
        model_profile: profile.default_model_profile.clone(),
        thinking_level,
        initial_context: initial_context.entries,
        allowed_capabilities: profile.allowed_capabilities.clone(),
        max_lookup_calls,
        max_returned_context_bytes,
        timeout_ms,
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
pub(crate) fn initial_context(
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

/// Per-file byte budget of one unified diff in reviewer context: the
/// largest patch the review layer will materialize and chunk. A file
/// whose diff exceeds it is cut with an explicit retrieval hint.
pub const DIFF_ENTRY_MAX_BYTES: usize = 128 * 1024;

/// Aggregate byte budget of the diff portion of reviewer context.
pub const DIFF_TOTAL_MAX_BYTES: usize = 160 * 1024;

/// Byte headroom reserved for the trailing coverage note inside
/// [`DIFF_TOTAL_MAX_BYTES`], and the number of omissions listed in it
/// before abbreviating.
pub const DIFF_COVERAGE_NOTE_RESERVE: usize = 2048;
pub const DIFF_COVERAGE_NOTE_LISTED: usize = 12;

/// Maximum prior-disposition entries in reviewer context, and the byte
/// bound of each entry's text (a dedicated budget — never charged
/// against the diff totals).
pub const DISPOSITION_CONTEXT_MAX: usize = 8;
pub const DISPOSITION_TEXT_MAX_BYTES: usize = 512;

/// Smallest per-file share of [`DIFF_TOTAL_MAX_BYTES`] when many files
/// changed: every changed file keeps at least this much diff context
/// before any file gets a second chunk.
pub const DIFF_TOTAL_MIN_SHARE: usize = 16 * 1024;

/// Ceiling of the reviewer lookup budget after omission and
/// changed-line scaling.
pub const REVIEWER_MAX_LOOKUP_CALLS_CAP: u32 = 40;

/// Base returned-context budget of one reviewer session.
pub const REVIEWER_MAX_CONTEXT_BYTES_BASE: u64 = 64 * 1024;

/// Ceiling of the scaled returned-context budget.
pub const REVIEWER_MAX_CONTEXT_BYTES_CAP: u64 = 512 * 1024;

/// Scale the configured reviewer-session budget for thinking level and
/// change size: `xhigh`/`max` sessions on large changes multiply the
/// configured budget by `clamp(changed_lines / 1_000, 1, 3)` (integer
/// thresholds: 999 lines 1x, 1 000-1 999 1x..2x at 2_000, capped at 3x
/// from 3 000 lines). The configured value is clamped to
/// [`MAX_REVIEWER_TIMEOUT_MS`] uniformly for every level. Levels
/// `off`..`high` keep the configured value.
pub(crate) fn effective_session_timeout(
    configured_ms: u64,
    level: crate::domain::model::ThinkingLevel,
    changed_lines: usize,
) -> u64 {
    let configured = configured_ms.min(crate::config::MAX_REVIEWER_TIMEOUT_MS);
    use crate::domain::model::ThinkingLevel;
    match level {
        ThinkingLevel::Xhigh | ThinkingLevel::Max => {
            let factor = (changed_lines / 1_000).clamp(1, 3) as u64;
            configured
                .saturating_mul(factor)
                .min(crate::config::MAX_REVIEWER_TIMEOUT_MS)
        }
        _ => configured,
    }
}

/// The reviewer lookup budget for a change: the base budget plus two
/// retrieval calls per omitted diff tail plus one call per 150 changed
/// lines beyond a 1 000-line allowance, clamped by
/// [`REVIEWER_MAX_LOOKUP_CALLS_CAP`].
pub(crate) fn lookup_budget(omissions: usize, changed_lines: usize) -> u32 {
    let line_calls = changed_lines.saturating_sub(1_000) as u32 / 150;
    (REVIEWER_MAX_LOOKUP_CALLS + 2 * omissions as u32 + line_calls)
        .min(REVIEWER_MAX_LOOKUP_CALLS_CAP)
}

/// The returned-context budget for a change: the base budget grows by
/// 48 bytes per changed body line beyond a 500-line allowance, never
/// falls below the largest single changed file plus 32 KiB of headroom
/// (a reviewer must be able to read any one changed file in full; files
/// beyond the cap are covered by ranged reads instead), and is clamped
/// by [`REVIEWER_MAX_CONTEXT_BYTES_CAP`].
pub(crate) fn returned_context_budget(changed_lines: usize, largest_file_bytes: u64) -> u64 {
    let scaled = REVIEWER_MAX_CONTEXT_BYTES_BASE + 48 * changed_lines.saturating_sub(500) as u64;
    scaled
        .max(largest_file_bytes.saturating_add(32 * 1024))
        .min(REVIEWER_MAX_CONTEXT_BYTES_CAP)
}

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

    /// A session's (model profile, thinking level) pair could not be
    /// made valid (unresolved profile or model).
    #[error("thinking policy: {0}")]
    ThinkingPolicy(String),

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
/// (Not `Debug`: the optional provider handles are opaque trait
/// objects.)
#[derive(Clone)]
pub struct CodeReviewOptions {
    /// How to launch the Pi review host child process.
    pub launch: PiHostLaunch,
    /// Bounds of the consolidation pipeline.
    pub consolidation: ConsolidationOptions,
    /// Resolved model profiles forwarded to the host through
    /// `host_config` (from trusted local configuration; reviewer
    /// sessions never inherit a model).
    pub model_profiles: BTreeMap<String, crate::domain::model::ModelProfile>,
    /// Optional routing provider blended into the rules decision
    /// (plan sections 33–34). Failures fall back to rules-only routing.
    pub routing_provider: Option<Arc<dyn RoutingClassifierProvider>>,
    /// Optional embedding provider reranking retrieval results (plan
    /// sections 30 and 34). Failures degrade to the baseline ranking.
    pub embedding_provider: Option<Arc<dyn EmbeddingProvider>>,
    /// Contributor identity for durable knowledge created by the
    /// review (plan section 44). Reviews are noninteractive: when no
    /// identity is configured, learning remains pending locally.
    pub contributor_id: Option<String>,
    /// Bounds of the fact-candidate pipeline (plan section 47).
    pub fact_processing: crate::learning::facts::FactProcessingOptions,
    /// Bound on concurrently running reviewer sessions in the Pi host
    /// (plan milestone 29: reviewer concurrency obeys config).
    pub max_concurrent_reviewers: u32,
    /// Whole-session budget of one reviewer session (all turns; the
    /// frozen single retry uses the same budget). Defaults to
    /// [`crate::config::DEFAULT_REVIEWER_TIMEOUT_MS`].
    pub reviewer_timeout_ms: u64,
}

impl CodeReviewOptions {
    /// Options for `launch` with default consolidation bounds, the
    /// default reviewer-concurrency bound, and no configured model
    /// profiles or optional providers.
    pub fn new(launch: PiHostLaunch) -> Self {
        CodeReviewOptions {
            launch,
            consolidation: ConsolidationOptions::default(),
            model_profiles: BTreeMap::new(),
            routing_provider: None,
            embedding_provider: None,
            contributor_id: None,
            fact_processing: crate::learning::facts::FactProcessingOptions::default(),
            max_concurrent_reviewers: crate::config::DEFAULT_MAX_CONCURRENT_REVIEWERS as u32,
            reviewer_timeout_ms: crate::config::DEFAULT_REVIEWER_TIMEOUT_MS,
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

    // 5. Durable records + preliminary facts/lessons retrieval. An
    // optional embedding provider may rerank the bounded candidate
    // set; every provider failure degrades to the baseline ranking.
    let catalog = Catalog::bundled()?;
    let records = RecordStore::new(&ctx.worktree_root).load()?;
    let retriever = match &options.embedding_provider {
        Some(provider) => {
            Retriever::open(resolved, paths)?.with_embedding_provider(provider.clone())
        }
        None => Retriever::open(resolved, paths)?,
    };
    // Potentially stale facts (plan section 48) are marked and
    // downranked in the shared context: current source evidence
    // outranks stale memory (permanent invariant 22).
    let stale_facts = stale_fact_ids(ctx, resolved, &records)?;
    retriever.sync_records(&records, &stale_facts)?;
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

    // 7. Deterministic routing with the optional provider (plan
    //    sections 33–34); 8. jury rules. The provider may only blend
    //    scores — never bypass the jury constraints — and every
    //    provider failure keeps the rules decision.
    let rules_decision = RulesClassifier::new(catalog.clone()).classify(&routing_input);
    let (decision, mut diagnostics) = classify_with_optional_provider(
        rules_decision,
        options.routing_provider.as_deref(),
        &routing_input,
        &catalog,
    );
    let jury = build_jury_plan(&decision, &catalog)?;

    // 11. Start the Pi host early so every semantic job shares it. The
    // configured reviewer-concurrency bound travels with the host
    // config (plan milestone 29).
    let mut host = HostProcess::spawn(&options.launch)?;
    host.wait_ready(HOST_READY_TIMEOUT)?;
    let host_config = HostConfigPayload::from_config_profiles(&options.model_profiles)
        .with_max_concurrent_reviewers(options.max_concurrent_reviewers);
    host.send_host_config(&host_config)?;

    // 11b. Thinking policy (protocol version 2): the host reports each
    // profile's supported thinking levels; every session level below
    // is routed through it, so an invalid (model, level) pair can
    // never be constructed.
    let capabilities = host.wait_model_capabilities(HOST_READY_TIMEOUT)?;
    let thinking = super::thinking::ThinkingPolicy::build(&capabilities, &options.model_profiles);
    diagnostics.extend(thinking.warnings().iter().cloned());

    // 12. Constrained dynamic-domain description for the fallback seat.
    // The session level is routed through the thinking policy; an
    // unusable profile skips the session and falls back to the static
    // template.
    let mut domain_description: Option<DynamicDomainDescription> = None;
    let dynamic_level = if jury.uses_dynamic_fallback() {
        let expert = catalog.dynamic_domain_expert();
        match super::thinking::ThinkingPolicy::resolve_job_level(
            Some(&thinking),
            &expert.default_model_profile,
            expert.default_thinking_level,
        ) {
            Ok(level) => Some(level),
            Err(reason) => {
                diagnostics.push(format!(
                    "dynamic domain description fell back to the static template: {reason}"
                ));
                None
            }
        }
    } else {
        None
    };
    if let Some(thinking_level) = dynamic_level {
        let expert = catalog.dynamic_domain_expert();
        let payload = DomainDescriptionPayload {
            description_id: new_ulid(),
            timeout_ms: DOMAIN_DESCRIPTION_TIMEOUT_MS,
            model_profile: expert.default_model_profile.clone(),
            thinking_level,
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
    // Changed files beyond the context file cap are named in the
    // coverage note instead of vanishing silently.
    let overflow_paths: Vec<String> = diff
        .iter()
        .skip(MAX_DIFF_FILES_IN_CONTEXT)
        .map(|entry| entry.path.clone())
        .collect();
    let diff_context = diff_context_entries(&patches, &overflow_paths);
    // Scale both retrieval budgets with the changed content: reviewers
    // must be able to finish reading the files they are asked about.
    let changed_lines: usize = patches
        .iter()
        .filter_map(|patch| patch.patch.as_deref())
        .map(|text| {
            text.lines()
                // Diff body lines only: added/removed lines, excluding
                // file (+++/---) and hunk (@@) headers.
                .filter(|line| {
                    (line.starts_with('+') && !line.starts_with("+++"))
                        || (line.starts_with('-') && !line.starts_with("---"))
                })
                .count()
        })
        .sum();
    // The floor honors real file sizes (a reviewer must be able to
    // read any one changed file in full), read from the snapshot side.
    let size_reader = SnapshotReader::new(ctx, resolved)?;
    let largest_file_bytes = patches
        .iter()
        .filter_map(|patch| size_reader.read(&patch.path).ok().flatten())
        .map(|file| file.bytes.len() as u64)
        .max()
        .unwrap_or(0);
    let lookup_budget = lookup_budget(diff_context.omitted.len(), changed_lines);
    let context_budget = returned_context_budget(changed_lines, largest_file_bytes);

    // 12b. Prior dispositions (accepted/dismissed feedback outcomes)
    // are verifiable context, never verdicts: reviewers confirm
    // unchanged ones or re-verify when the rationale no longer holds.
    // Bounded to a dedicated 4 KiB budget so no diff entry is evicted.
    let disposition_entries: Vec<ContextEntry> =
        crate::learning::feedback::prior_dispositions(ctx, paths, DISPOSITION_CONTEXT_MAX)
            .unwrap_or_default()
            .into_iter()
            .map(|disposition| ContextEntry {
                kind: ContextKind::Practice,
                title: bounded(
                    &format!(
                        "prior disposition ({}): {}",
                        disposition.outcome.as_str(),
                        disposition.title
                    ),
                    crate::jury::findings::MAX_TITLE_BYTES,
                )
                .to_string(),
                path: None,
                symbol: None,
                line_start: None,
                line_end: None,
                record_id: None,
                practice_id: None,
                text: bounded(
                    &format!(
                        "outcome {} (review {}): {} — re-verify against the code before \
                         confirming; never suppress a finding your evidence supports.",
                        disposition.outcome.as_str(),
                        &disposition.review_id[..12.min(disposition.review_id.len())],
                        disposition.claim
                    ),
                    DISPOSITION_TEXT_MAX_BYTES,
                )
                .to_string(),
                score: 0.9,
            })
            .collect();
    let mut specs: Vec<ReviewerExecutionSpec> = Vec::with_capacity(jury.seats.len());
    let mut seat_timeouts: Vec<u64> = Vec::with_capacity(jury.seats.len());
    for seat in &jury.seats {
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
            &diff_context.bundle,
            &disposition_entries,
            &shared_context,
            &catalog,
            &seat.practice_tags,
        );
        let (level, note) = thinking
            .resolve(
                &profile.default_model_profile,
                None,
                profile.default_thinking_level,
            )
            .map_err(CodeReviewError::ThinkingPolicy)?;
        if let Some(note) = note {
            diagnostics.push(note);
        }
        // Per-seat session budget: xhigh/max thinking on large changes
        // scales the configured budget (see effective_session_timeout).
        let seat_timeout =
            effective_session_timeout(options.reviewer_timeout_ms, level, changed_lines);
        seat_timeouts.push(seat_timeout);
        specs.push(execution_spec(
            profile,
            seat,
            context,
            level,
            seat_timeout,
            lookup_budget,
            context_budget,
        ));
    }

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
    let outcomes = super::execution::execute_review(
        &mut host,
        &payload,
        &mut serve,
        review_round_timeout(
            seat_timeouts
                .iter()
                .copied()
                .max()
                .unwrap_or(options.reviewer_timeout_ms),
        ),
    )?;

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
    // Timeout-caused shortfalls degrade to a clearly marked PARTIAL
    // result (one valid report still carries validated findings);
    // disagreement, invalid output, or zero reports keep the frozen
    // hard failure.
    let mut partial = None;
    if reports.len() < 2 {
        let all_session_timeouts = !outcomes.is_empty()
            && outcomes.iter().all(|(_, outcome)| match outcome {
                ReviewerOutcome::Completed(_) => true,
                ReviewerOutcome::Failed(error) => error.code == "session_timeout",
            });
        if reports.is_empty() || !all_session_timeouts {
            return Err(CodeReviewError::TooFewValidReports {
                review_id,
                valid: reports.len(),
                failures,
            });
        }
        partial = Some(failures.join("; "));
        diagnostics.push(format!(
            "PARTIAL REVIEW: {} of the 2 required independent reports; the missing reviewer(s) exhausted their session budgets",
            reports.len()
        ));
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

    // 15b. Label exact recurrences of previously dispositioned
    // findings (never suppress: a dismissed-once real bug must be able
    // to resurface — the label carries the prior outcome for the
    // synthesis and the reader).
    let prior_outcomes =
        crate::learning::feedback::prior_fingerprint_outcomes(ctx, paths).unwrap_or_default();
    for finding in &mut validated {
        let Some(outcome) = prior_outcomes.get(&finding.fingerprint) else {
            continue;
        };
        let prefix = format!("recurrence (previously {}): ", outcome.as_str());
        if !finding.finding.title.starts_with(&prefix) {
            let prefixed = format!("{}{}", prefix, finding.finding.title);
            finding.finding.title =
                bounded(&prefixed, crate::jury::findings::MAX_TITLE_BYTES).to_string();
        }
    }

    // 16. Consolidation (equivalence checks + synthesis, bounded).
    let synthesis = consolidate_with_dispositions(
        &mut host,
        ReviewKind::Code,
        &validated,
        &options.consolidation,
        &thinking,
        &disposition_entries,
    )?;
    diagnostics.extend(synthesis.diagnostics.iter().map(|d| d.message.clone()));

    // 17. ReviewResult.
    let result = ReviewResult {
        review_id: review_id.clone(),
        kind: ReviewKind::Code,
        created_at: crate::domain::time::now_utc_rfc3339(),
        snapshot: resolved.descriptor.clone(),
        summary: match &partial {
            Some(why) => format!("PARTIAL REVIEW ({why}): {}", synthesis.summary),
            None => synthesis.summary.clone(),
        },
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

    // 19./20. Validate/verify fact candidates; persist supported facts
    // or pending candidates (plan sections 47 and 58). Reviews are
    // noninteractive: without a contributor identity, learning remains
    // pending locally (plan section 44), and every verifier failure
    // degrades to pending state — never a failed review.
    let fact_summary = crate::learning::facts::process_fact_candidates(
        &mut host,
        ctx,
        paths,
        &validation_context,
        &review_id,
        &reports,
        &records,
        options.contributor_id.as_deref(),
        &options.fact_processing,
        &thinking,
    );
    diagnostics.extend(fact_summary.diagnostics);

    // 21. Deterministic learning maintenance (plan section 58 step
    //     21): repeated review outcomes become routing lessons without
    //     any LLM statistics (plan section 51). Maintenance is
    //     idempotent and conservative — it can never fail the review.
    let lesson_summary = crate::learning::lessons::run_learning_maintenance(
        ctx,
        Some(paths),
        &records,
        options.contributor_id.as_deref(),
    );
    diagnostics.extend(lesson_summary.diagnostics);

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
/// One chunk of a file's unified diff: the repeated file header block
/// plus whole hunks (or a line-aligned slice of one oversized hunk),
/// with the new-file line the chunk starts at.
struct DiffChunk {
    text: String,
    /// New-file line this chunk starts at (1-based), when derivable.
    start_line: Option<u32>,
}

/// Split a unified diff into chunks of at most `cap` bytes, hunk-aligned
/// whenever possible. Every chunk repeats the `diff --git` / `---` / `+++`
/// header block so later parts stay self-contained; a single hunk larger
/// than the cap is split on line boundaries with a synthetic header line
/// carrying the continuation's new-file start.
fn chunk_patch(patch: &str, cap: usize) -> Vec<DiffChunk> {
    if patch.len() <= cap {
        return vec![DiffChunk {
            text: patch.to_owned(),
            start_line: first_hunk_start(patch),
        }];
    }
    let mut lines = patch.split_inclusive('\n').peekable();
    let mut header = String::new();
    // The file header block: everything before the first hunk header.
    while let Some(line) = lines.peek() {
        if line.starts_with("@@") {
            break;
        }
        header.push_str(line);
        lines.next();
    }
    if lines.peek().is_none() && !header.is_empty() {
        // No hunk structure at all (header-only or synthesized text
        // without hunks): fall back to line-aligned byte chunks.
        let mut chunks = Vec::new();
        let mut current = String::new();
        for line in header.split_inclusive('\n') {
            if !current.is_empty() && current.len() + line.len() > cap {
                chunks.push(DiffChunk {
                    text: std::mem::take(&mut current),
                    start_line: None,
                });
            }
            current.push_str(line);
        }
        if !current.is_empty() {
            chunks.push(DiffChunk {
                text: current,
                start_line: None,
            });
        }
        return chunks;
    }
    // Group the remaining lines into hunks.
    let mut hunks: Vec<(String, Option<u32>, Vec<String>)> = Vec::new(); // (header, start, body)
    for line in lines {
        if line.starts_with("@@") {
            hunks.push((line.to_owned(), hunk_new_start(line), Vec::new()));
        } else if let Some(last) = hunks.last_mut() {
            last.2.push(line.to_owned());
        }
    }
    let header_len = header.len();
    let mut chunks: Vec<DiffChunk> = Vec::new();
    let mut current = String::new();
    let mut current_start: Option<u32> = None;
    let flush = |current: &mut String, current_start: Option<u32>, chunks: &mut Vec<DiffChunk>| {
        if current.is_empty() {
            return;
        }
        chunks.push(DiffChunk {
            text: std::mem::take(current),
            start_line: current_start,
        });
    };
    for (hunk_header, hunk_start, body) in hunks {
        let hunk_len = hunk_header.len() + body.iter().map(String::len).sum::<usize>();
        // A hunk joins the current chunk only if the result fits the
        // cap; the first hunk of a chunk must fit together with the
        // repeated file header block.
        let fits_whole = if current.is_empty() {
            header_len + hunk_len <= cap
        } else {
            current.len() + hunk_len <= cap
        };
        if fits_whole {
            if current.is_empty() {
                current.push_str(&header);
                current_start = hunk_start;
            }
            current.push_str(&hunk_header);
            current.extend(body.iter().cloned());
            continue;
        }
        // Flush what we have, then place this hunk (splitting it when
        // it alone exceeds the cap).
        flush(&mut current, current_start, &mut chunks);
        current_start = hunk_start;
        if header_len + hunk_len <= cap {
            current.push_str(&header);
            current.push_str(&hunk_header);
            current.extend(body.iter().cloned());
            continue;
        }
        // Oversized single hunk: line-aligned slices, each under its
        // own synthetic header naming the continuation's start line.
        let mut slice = String::new();
        let mut slice_start = hunk_start;
        let mut new_line = hunk_start.unwrap_or(1);
        for line in body {
            let line_new_len = if line.starts_with('+') || line.starts_with(' ') {
                1
            } else {
                0
            };
            let synthetic_len = header_len
                + synthetic_hunk_header(Some(new_line)).len()
                + 1 // separator newline
                + slice.len()
                + line.len();
            if !slice.is_empty() && synthetic_len > cap {
                let text = format!(
                    "{header}{}\n{synthetic}",
                    synthetic_hunk_header(slice_start),
                    synthetic = std::mem::take(&mut slice)
                );
                chunks.push(DiffChunk {
                    text,
                    start_line: slice_start,
                });
                slice_start = Some(new_line);
            }
            if line_new_len == 1 {
                new_line += 1;
            }
            if line.len() > cap.saturating_sub(header_len + 64) {
                // A single line longer than a whole chunk (for example
                // a minified file): split it on char boundaries with an
                // explicit marker instead of overflowing the cap.
                let budget = cap.saturating_sub(header_len + 128);
                let mut rest = line.as_str();
                while !rest.is_empty() {
                    let mut end = budget.min(rest.len());
                    while end > 0 && !rest.is_char_boundary(end) {
                        end -= 1;
                    }
                    end = end.max(1);
                    let (head, tail) = rest.split_at(end);
                    chunks.push(DiffChunk {
                        text: format!(
                            "{header}{}\n{head}\n[long diff line split across chunks]",
                            synthetic_hunk_header(slice_start)
                        ),
                        start_line: slice_start,
                    });
                    slice_start = None;
                    rest = tail;
                }
                continue;
            }
            slice.push_str(&line);
        }
        if !slice.is_empty() {
            let text = format!(
                "{header}{}\n{synthetic}",
                synthetic_hunk_header(slice_start),
                synthetic = slice
            );
            chunks.push(DiffChunk {
                text,
                start_line: slice_start,
            });
        }
        current_start = None;
    }
    flush(&mut current, current_start, &mut chunks);
    chunks
}

/// The `+start` new-file line of a `@@ -a,b +c,d @@` header line.
fn hunk_new_start(header: &str) -> Option<u32> {
    let plus = header.split_once('+')?.1;
    let start = plus.split([',', ' ']).next()?.parse::<u32>().ok()?;
    Some(start.max(1))
}

/// The new-file start of the first hunk of a patch, when parseable.
fn first_hunk_start(patch: &str) -> Option<u32> {
    patch
        .split('\n')
        .find(|line| line.starts_with("@@"))
        .and_then(hunk_new_start)
}

/// Synthetic header for a mid-hunk continuation chunk.
fn synthetic_hunk_header(start: Option<u32>) -> String {
    match start {
        Some(start) => format!("@@ +{start},? @@ (continued from the previous part)"),
        None => "@@ +?,? @@ (continued from the previous part)".to_string(),
    }
}

/// The diff portion of reviewer context plus the omissions the reviewer
/// must be told about: every changed file whose diff is partially or
/// entirely absent, with a retrieval pointer.
pub(crate) struct DiffContext {
    /// The assembled diff context bundle (entries in display order,
    /// coverage note last when omissions exist).
    pub bundle: ContextBundle,
    /// One descriptor per changed file with an absent diff tail:
    /// `path (from line N)` or `path (entirely omitted)`.
    pub omitted: Vec<String>,
}

/// Assemble the diff entries of reviewer context: every changed file's
/// diff, chunked to fair per-file shares of the total budget, with
/// hunk-aligned parts that never silently cut — every omission ends
/// with an explicit note, all omissions are summarized once, and files
/// beyond the context file cap are named instead of vanishing.
fn diff_context_entries(patches: &[SnapshotPatch], overflow_paths: &[String]) -> DiffContext {
    let changed_files = patches.len() + overflow_paths.len();
    let share = if changed_files == 0 {
        DIFF_TOTAL_MAX_BYTES
    } else {
        (DIFF_TOTAL_MAX_BYTES / changed_files).clamp(DIFF_TOTAL_MIN_SHARE, DIFF_ENTRY_MAX_BYTES)
    };
    // A chunk never exceeds the single-entry text bound, with headroom
    // for an appended omission note (the bundle assembler truncates
    // per entry at PLAN_CONTEXT_MAX_BYTES).
    let chunk_cap = share.min(PLAN_CONTEXT_MAX_BYTES.saturating_sub(512));

    let mut entries: Vec<ContextEntry> = Vec::new();
    let mut total = 0usize;
    let mut omitted: Vec<String> = Vec::new();

    for patch in patches {
        let status = diff_status_label(patch.status);
        let body: Option<String> = if patch.binary {
            Some("binary content changed; no textual diff available".to_string())
        } else if let Some(text) = patch.patch.as_deref() {
            if text.is_empty() {
                // Headers only (for example a mode-only change).
                Some("changed with no textual diff (mode change only)".to_string())
            } else {
                Some(text.to_owned())
            }
        } else {
            Some("changed; no textual diff available".to_string())
        };
        let Some(body) = body else {
            continue;
        };

        let chunks = chunk_patch(&body, chunk_cap);
        let n_parts = chunks.len();
        let mut placed_any = false;
        // Deletions have no new-file lines to point at; their omission
        // descriptors say so instead of fabricating anchors.
        let deleted = patch.status == DiffStatus::Deleted;
        let tail_descriptor = |from_line: Option<u32>| -> String {
            if deleted {
                format!("{} (old-file tail omitted; file deleted)", patch.path)
            } else {
                match from_line {
                    Some(line) => format!("{} (from line {line})", patch.path),
                    None => format!("{} (beyond this point)", patch.path),
                }
            }
        };
        for (index, chunk) in chunks.iter().enumerate() {
            let remaining = DIFF_TOTAL_MAX_BYTES
                .saturating_sub(total)
                .saturating_sub(DIFF_COVERAGE_NOTE_RESERVE);
            if chunk.text.len() > remaining {
                // The budget is exhausted mid-file: name exactly where
                // the visible diff stops. A starved file (nothing
                // placed) gets exactly one descriptor below.
                if placed_any {
                    omitted.push(tail_descriptor(chunk.start_line));
                }
                break;
            }
            let text = chunk.text.clone();
            total += text.len();
            let title = if n_parts == 1 {
                format!("diff: {} ({})", patch.path, status)
            } else {
                format!(
                    "diff: {} ({}, part {}/{})",
                    patch.path,
                    status,
                    index + 1,
                    n_parts
                )
            };
            entries.push(ContextEntry {
                kind: ContextKind::Diff,
                title,
                path: Some(patch.path.clone()),
                symbol: None,
                line_start: chunk.start_line,
                line_end: None,
                record_id: None,
                practice_id: None,
                text,
                score: 1.0,
            });
            placed_any = true;
        }
        if !placed_any && n_parts > 0 {
            omitted.push(format!("{} (entirely omitted)", patch.path));
        } else if placed_any && patch.truncated {
            // Every chunk fits, yet the git layer cut the patch at its
            // materialization cap (its marker rides in the final
            // chunk): record the tail so the coverage note and lookup
            // budget account for it.
            omitted.push(tail_descriptor(
                chunks.last().and_then(|chunk| chunk.start_line),
            ));
        }
    }

    for path in overflow_paths {
        omitted.push(format!(
            "{path} (entirely omitted; beyond the diff file cap)"
        ));
    }

    if !omitted.is_empty() {
        let mut text = String::from(
            "Diff coverage note: regions of the following changed files are omitted from this context: ",
        );
        let listed = omitted.len().min(DIFF_COVERAGE_NOTE_LISTED);
        text.push_str(&omitted[..listed].join(", "));
        if omitted.len() > listed {
            text.push_str(&format!(", +{} more", omitted.len() - listed));
        }
        text.push_str(
            ". Before reporting an issue about an omitted region, verify it with the \
             source_lookup tool (path=<path>, line_start=<line>); absence from the \
             visible diff is not evidence of absence in the file.",
        );
        total += text.len();
        entries.push(ContextEntry {
            kind: ContextKind::Diff,
            title: "coverage note: diff context omissions".to_string(),
            path: None,
            symbol: None,
            line_start: None,
            line_end: None,
            record_id: None,
            practice_id: None,
            text,
            score: 1.0,
        });
    }

    DiffContext {
        bundle: ContextBundle {
            entries,
            truncated: !omitted.is_empty(),
            text_bytes: total,
        },
        omitted,
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
    dispositions: &[ContextEntry],
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
    let disposition_bytes: usize = dispositions.iter().map(|entry| entry.text.len()).sum();
    let text_bytes = diff_context.text_bytes + disposition_bytes + rest.text_bytes;
    // Dispositions sit directly after the diff entries; they are not
    // charged against the diff budget.
    entries.extend(dispositions.iter().cloned());
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
pub(crate) fn normalize_domain_description(
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
                let context_titles: serde_json::Value = {
                    let titles: Vec<&str> = spec
                        .initial_context
                        .iter()
                        .map(|entry| entry.title.as_str())
                        .collect();
                    let cap = 32;
                    if titles.len() <= cap {
                        json!(titles)
                    } else {
                        let mut shown: Vec<String> = titles[..cap].iter().map(|t| t.to_string()).collect();
                        shown.push(format!("+{} more", titles.len() - cap));
                        json!(shown)
                    }
                };
                json!({
                    "reviewer_id": spec.reviewer_id,
                    "specialization": spec.specialization,
                    "model_profile": spec.model_profile,
                    "thinking_level": spec.thinking_level,
                    "checklist_items": spec.checklist.len(),
                    "context_entries": spec.initial_context.len(),
                    "context_titles": context_titles,
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
        // The finding identifier makes `nisse feedback <review-id>
        // <finding-id>` usable from the text output.
        out.push_str(&format!("    id: {}\n", finding.id));
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
        ReviewKind::Investigation => "investigation",
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
                    input_part: None,
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
        // Finding identifiers are rendered so feedback can target them.
        assert!(text.contains("id: g1"), "{text}");

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
        let bundle = diff_context_entries(&patches, &[]).bundle;
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
    fn diff_context_entries_chunk_large_diffs_hunk_aligned() {
        // A multi-hunk patch larger than one chunk splits on hunk
        // boundaries; every part repeats the file header and carries
        // its new-file start line.
        let hunk = |start: u32| {
            format!("@@ -{start},3 +{start},3 @@\n context\n-old{start}\n+new{start}\n")
        };
        let mut big = String::from("diff --git a/big.rs b/big.rs\n--- a/big.rs\n+++ b/big.rs\n");
        for start in 1..800 {
            big.push_str(&hunk(start * 10));
        }
        assert!(big.len() > DIFF_TOTAL_MIN_SHARE);
        let patches = vec![patch("src/big.rs", DiffStatus::Modified, Some(&big), false)];
        let context = diff_context_entries(&patches, &[]);
        assert!(context.omitted.is_empty(), "{:?}", context.omitted);
        let titles: Vec<&str> = context
            .bundle
            .entries
            .iter()
            .map(|entry| entry.title.as_str())
            .collect();
        assert!(titles.len() >= 2, "large patch splits: {titles:?}");
        assert!(
            titles
                .iter()
                .all(|title| title.starts_with("diff: src/big.rs (modified, part ")),
            "{titles:?}"
        );
        // Every part is header-complete and hunk-anchored.
        for entry in &context.bundle.entries {
            assert!(entry.text.starts_with("diff --git a/big.rs b/big.rs"));
            assert!(entry.text.contains("@@"));
            assert!(entry.text.len() <= PLAN_CONTEXT_MAX_BYTES);
        }
        // Parts cover strictly increasing new-file lines.
        let starts: Vec<u32> = context
            .bundle
            .entries
            .iter()
            .map(|entry| entry.line_start.unwrap_or(0))
            .collect();
        let mut sorted = starts.clone();
        sorted.sort();
        assert_eq!(starts, sorted, "{starts:?}");
    }

    #[test]
    fn diff_context_entries_split_oversized_single_hunks_with_synthetic_headers() {
        // One hunk far larger than the chunk cap: continuations get
        // synthetic headers naming the continuation's start line.
        let mut body = String::new();
        for line in 0..6_000 {
            body.push_str(&format!("+line {line}\n"));
        }
        let huge = format!(
            "diff --git a/huge.gs b/huge.gs\n--- /dev/null\n+++ b/huge.gs\n@@ -0,0 +1,6000 @@\n{body}"
        );
        let patches = vec![patch("src/huge.gs", DiffStatus::Added, Some(&huge), false)];
        let context = diff_context_entries(&patches, &[]);
        assert!(context.omitted.is_empty(), "{:?}", context.omitted);
        assert!(
            context.bundle.entries.len() >= 2,
            "oversized hunk splits: {:?}",
            context.bundle.entries.len()
        );
        for entry in &context.bundle.entries {
            assert!(entry.text.len() <= PLAN_CONTEXT_MAX_BYTES);
            assert!(entry.text.contains("@@"), "synthetic header present");
        }
        // Continuation chunks state their start line.
        assert!(
            context.bundle.entries[1]
                .text
                .contains("(continued from the previous part)")
        );
    }

    #[test]
    fn diff_context_entries_name_omissions_and_summarize_once() {
        // More data than the total budget: every cut is named with a
        // line anchor, files beyond the budget are named entirely, and
        // exactly one coverage note is appended.
        let patches: Vec<SnapshotPatch> = (0..9)
            .map(|i| {
                let mut p =
                    format!("diff --git a/f{i}.rs b/f{i}.rs\n--- a/f{i}.rs\n+++ b/f{i}.rs\n");
                for h in 0..600 {
                    p.push_str(&format!("@@ +{} @@\n+{}\n", h * 10 + 1, "x".repeat(40)));
                }
                patch(
                    &format!("src/f{i}.rs"),
                    DiffStatus::Modified,
                    Some(&p),
                    false,
                )
            })
            .collect();
        let overflow = vec!["src/overflow.rs".to_string()];
        let context = diff_context_entries(&patches, &overflow);
        assert!(!context.omitted.is_empty());
        assert!(context.bundle.truncated);
        assert!(context.bundle.text_bytes <= DIFF_TOTAL_MAX_BYTES);
        // Exactly one coverage note, listing omissions and the overflow.
        let notes: Vec<&ContextEntry> = context
            .bundle
            .entries
            .iter()
            .filter(|entry| entry.title.contains("coverage note"))
            .collect();
        assert_eq!(notes.len(), 1, "one summary note");
        let note = notes[0];
        assert!(
            note.text
                .contains("src/overflow.rs (entirely omitted; beyond the diff file cap)")
        );
        assert!(note.text.contains("verify"));
        assert!(note.text.contains("source_lookup"));
        // Omitted tails carry line anchors.
        assert!(
            context
                .omitted
                .iter()
                .any(|omission| omission.contains("from line ")),
            "{:?}",
            context.omitted
        );
        // The mid-file cut leaves a retrieval hint on the last placed
        // part only when the git layer cut it; budget cuts are named in
        // the note. Either way nothing disappears silently.
        for omission in &context.omitted {
            assert!(
                omission.contains("from line ") || omission.contains("entirely omitted"),
                "{omission}"
            );
        }
    }

    #[test]
    fn diff_context_entries_exact_fit_is_not_truncated() {
        // A single file whose patch exactly fills the total budget:
        // chunked, complete, no omissions.
        let mut p = String::from("diff --git a/fit.rs b/fit.rs\n--- a/fit.rs\n+++ b/fit.rs\n");
        let mut line = 1;
        // Headroom for the per-part repeated file headers.
        while p.len() < DIFF_TOTAL_MAX_BYTES - 4096 {
            p.push_str(&format!("@@ +{} @@\n+{}\n", line, "y".repeat(30)));
            line += 1;
        }
        let patches = vec![patch("src/fit.rs", DiffStatus::Modified, Some(&p), false)];
        let context = diff_context_entries(&patches, &[]);
        assert!(context.omitted.is_empty(), "{:?}", context.omitted);
        assert!(!context.bundle.truncated);
        assert!(context.bundle.text_bytes <= DIFF_TOTAL_MAX_BYTES);
        for entry in &context.bundle.entries {
            assert!(entry.text.len() <= PLAN_CONTEXT_MAX_BYTES);
        }
    }

    #[test]
    fn diff_context_entries_small_diffs_stay_single_entries() {
        let patches = vec![
            patch(
                "src/a.rs",
                DiffStatus::Modified,
                Some("--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,2 +1,2 @@\n-a\n+b"),
                false,
            ),
            patch(
                "src/b.rs",
                DiffStatus::Added,
                Some("--- /dev/null\n+++ b/src/b.rs\n@@ -0,0 +1 @@\n+new"),
                false,
            ),
        ];
        let context = diff_context_entries(&patches, &[]);
        assert_eq!(context.bundle.entries.len(), 2);
        assert!(context.omitted.is_empty());
        assert!(!context.bundle.truncated);
        assert_eq!(context.bundle.entries[0].title, "diff: src/a.rs (modified)");
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
            &[],
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

    #[test]
    fn lookup_budget_scales_with_omissions_and_changed_lines() {
        assert_eq!(lookup_budget(0, 0), REVIEWER_MAX_LOOKUP_CALLS);
        assert_eq!(lookup_budget(1, 0), REVIEWER_MAX_LOOKUP_CALLS + 2);
        // Lines beyond the 1 000-line allowance add one call per 150.
        assert_eq!(lookup_budget(0, 1_000), REVIEWER_MAX_LOOKUP_CALLS);
        assert_eq!(lookup_budget(0, 1_150), REVIEWER_MAX_LOOKUP_CALLS + 1);
        assert_eq!(lookup_budget(0, 3_600), REVIEWER_MAX_LOOKUP_CALLS + 17);
        // The clamp binds regardless of the inputs.
        assert_eq!(lookup_budget(20, 100_000), REVIEWER_MAX_LOOKUP_CALLS_CAP);
    }

    #[test]
    fn returned_context_budget_covers_the_largest_changed_file() {
        // Small changes keep the base budget...
        assert_eq!(
            returned_context_budget(0, 0),
            REVIEWER_MAX_CONTEXT_BYTES_BASE
        );
        // ...scaling 48 bytes per changed line beyond 500...
        assert_eq!(
            returned_context_budget(1_500, 0),
            REVIEWER_MAX_CONTEXT_BYTES_BASE + 48 * 1_000
        );
        // ...never below any single changed file plus headroom...
        assert_eq!(
            returned_context_budget(10, 200 * 1024),
            200 * 1024 + 32 * 1024
        );
        // ...and clamped by the ceiling.
        assert_eq!(
            returned_context_budget(1_000_000, 4 * 1024 * 1024),
            REVIEWER_MAX_CONTEXT_BYTES_CAP
        );
    }

    #[test]
    fn budget_starved_files_get_exactly_one_omission_descriptor() {
        // Enough files to exhaust the total budget: the starved file
        // must not appear twice in the omission list.
        let patches: Vec<SnapshotPatch> = (0..12)
            .map(|i| {
                let mut p =
                    format!("diff --git a/s{i}.rs b/s{i}.rs\n--- a/s{i}.rs\n+++ b/s{i}.rs\n");
                for h in 0..900 {
                    p.push_str(&format!("@@ +{} @@\n+{}\n", h * 10 + 1, "x".repeat(30)));
                }
                patch(
                    &format!("src/s{i}.rs"),
                    DiffStatus::Modified,
                    Some(&p),
                    false,
                )
            })
            .collect();
        let context = diff_context_entries(&patches, &[]);
        assert!(!context.omitted.is_empty());
        let mut paths: Vec<&str> = context
            .omitted
            .iter()
            .map(|omission| omission.split(" (").next().unwrap())
            .collect();
        paths.sort_unstable();
        let duplicates = paths.windows(2).any(|pair| pair[0] == pair[1]);
        assert!(!duplicates, "{:?}", context.omitted);
        // The note stays inside its reserve: the total respects the
        // aggregate budget even with the appended note.
        assert!(context.bundle.text_bytes <= DIFF_TOTAL_MAX_BYTES);
        let note_count = context
            .bundle
            .entries
            .iter()
            .filter(|entry| entry.title.contains("coverage note"))
            .count();
        assert_eq!(note_count, 1);
    }

    #[test]
    fn materialization_capped_patches_report_their_tail_when_fully_placed() {
        // A single file whose patch was cut at the git-layer
        // materialization cap (128 KiB) fits the total budget (160 KiB):
        // every chunk is placed and the tail is still recorded.
        let mut p = String::from("diff --git a/cap.rs b/cap.rs\n--- a/cap.rs\n+++ b/cap.rs\n");
        let mut line = 1;
        while p.len() < DIFF_ENTRY_MAX_BYTES + 4096 {
            p.push_str(&format!("@@ +{} @@\n+{}\n", line, "z".repeat(40)));
            line += 1;
        }
        // Emulate the git layer: cut at the cap with the marker and the
        // truncated flag (the real marker text rides in the last chunk).
        let (cut, _) = {
            let mut text = p[..p.len().saturating_sub(120)].to_string();
            text.push_str(&format!(
                "\n\u{2026} [diff truncated after {DIFF_ENTRY_MAX_BYTES} bytes; verify the omitted region via source_lookup (path=src/cap.rs)]"
            ));
            (text, ())
        };
        let patches = vec![SnapshotPatch {
            path: "src/cap.rs".to_string(),
            status: DiffStatus::Modified,
            binary: false,
            patch: Some(cut),
            truncated: true,
        }];
        let context = diff_context_entries(&patches, &[]);
        assert!(
            context.omitted.len() == 1,
            "the materialization cap tail is recorded: {:?}",
            context.omitted
        );
        assert!(context.omitted[0].contains("from line "));
        assert!(context.bundle.text_bytes <= DIFF_TOTAL_MAX_BYTES);
    }

    #[test]
    fn chunk_patch_fallbacks_split_headerless_and_malformed_bodies() {
        // A large body with no hunk headers: line-aligned byte chunks.
        let body = "diff --git a/plain.txt b/plain.txt\n+++ b/plain.txt\n".to_string();
        let headerless = format!("{body}{}", "raw content line\n".repeat(4_000));
        let chunks = chunk_patch(&headerless, 2048);
        assert!(chunks.len() >= 2, "{}", chunks.len());
        for chunk in &chunks {
            assert!(chunk.text.len() <= 2048, "{}", chunk.text.len());
        }
        // A hunk whose start cannot be parsed: continuations still
        // carry a synthetic (line-less) header.
        let malformed = format!("{body}@@ garbage @@\n{}", "+minified data\n".repeat(4_000));
        let chunks = chunk_patch(&malformed, 2048);
        assert!(chunks.len() >= 2);
        assert!(
            chunks
                .iter()
                .any(|chunk| chunk.text.contains("(continued from the previous part)"))
        );
        // A single line longer than a chunk splits on char boundaries
        // with an explicit marker.
        let minified = format!("{body}@@ +1 @@\n+{}\n", "m".repeat(9_000));
        let chunks = chunk_patch(&minified, 2048);
        assert!(chunks.len() >= 3, "{}", chunks.len());
        for chunk in &chunks {
            assert!(chunk.text.len() <= 2048 + 256, "{}", chunk.text.len());
        }
        assert!(
            chunks
                .iter()
                .any(|chunk| chunk.text.contains("[long diff line split across chunks]"))
        );
    }

    #[test]
    fn deleted_file_omissions_do_not_fabricate_line_anchors() {
        let mut p = String::from("diff --git a/gone.rs b/gone.rs\n--- a/gone.rs\n+++ /dev/null\n");
        let mut line = 1;
        while p.len() < DIFF_TOTAL_MAX_BYTES {
            p.push_str(&format!("@@ -{} +0,0 @@\n-gone {line}\n", line));
            line += 1;
        }
        let patches = vec![patch("src/gone.rs", DiffStatus::Deleted, Some(&p), false)];
        let context = diff_context_entries(&patches, &[]);
        assert!(
            context
                .omitted
                .iter()
                .all(|omission| omission.contains("file deleted")),
            "{:?}",
            context.omitted
        );
    }

    #[test]
    fn effective_session_timeout_scales_only_extreme_thinking() {
        use crate::domain::model::ThinkingLevel;
        // Low levels: identity, uniformly clamped to the 1 h maximum.
        assert_eq!(
            effective_session_timeout(600_000, ThinkingLevel::High, 5_000),
            600_000
        );
        assert_eq!(
            effective_session_timeout(9_000_000, ThinkingLevel::Low, 0),
            crate::config::MAX_REVIEWER_TIMEOUT_MS
        );
        // Max thinking: integer thresholds 1x / 2x / 3x with cliffs at
        // 2 000 and 3 000 changed lines, clamped at 1 h.
        assert_eq!(
            effective_session_timeout(600_000, ThinkingLevel::Max, 1_999),
            600_000
        );
        assert_eq!(
            effective_session_timeout(600_000, ThinkingLevel::Max, 2_000),
            1_200_000
        );
        assert_eq!(
            effective_session_timeout(600_000, ThinkingLevel::Xhigh, 3_000),
            1_800_000
        );
        assert_eq!(
            effective_session_timeout(1_800_000, ThinkingLevel::Max, 5_000),
            crate::config::MAX_REVIEWER_TIMEOUT_MS
        );
    }
}
