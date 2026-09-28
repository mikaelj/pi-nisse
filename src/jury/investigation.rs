//! Investigation review pipeline (PLAN_INVESTIGATION.md).
//!
//! Runs the frozen four-seat jury (quality, claim-skeptic,
//! coverage-auditor, one domain reviewer) over the resolved task/result
//! inputs, validates reports against the inputs and the working-tree
//! snapshot, merges claim verdicts deterministically (most skeptical
//! wins), and assembles the [`InvestigationResult`]. Synthesis is a
//! deterministic assembly: it never invents claims, changes verdicts, or
//! exceeds the source ceilings (plan section 38 discipline).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;

use crate::domain::context::{ContextEntry, ContextKind};
use crate::domain::ids::new_ulid;
use crate::domain::investigation::{
    self, ClaimAssessment, ConfidenceTier, CoverageGap, GapImportance, InputValue,
    InvestigationReport, InvestigationResult, InvestigationVerdict, TaskOrResult,
    clamp_verdict_confidence, validate_claim_shape, validate_gap_shape,
};
use crate::domain::review::ReviewKind;
use crate::domain::routing::RoutingInput;
use crate::domain::time::now_utc_rfc3339;
use crate::jury::catalog::{Catalog, DYNAMIC_DOMAIN_EXPERT_ID};
use crate::jury::execution::execute_investigation;
use crate::jury::findings::ValidationContext;
use crate::jury::host::{HostProcess, InvestigationReviewerOutcome};
use crate::jury::plan::build_investigation_jury_plan;
use crate::jury::protocol::{
    HostConfigPayload, StartInvestigationPayload, ToolRequestPayload, ToolResponsePayload,
};
use crate::jury::review::{
    active_lesson_hints, execution_spec, initial_context, record_tags, seat_plan,
    snapshot_languages, stale_fact_ids,
};
use crate::jury::{ReviewerToolServer, review as review_pipeline};
use crate::retrieval::Retriever;
use crate::retrieval::query::RetrievalQuery;
use crate::retrieval::query::{MAX_QUERY_BYTES, truncate_text};
use crate::routing::rules::RulesClassifier;
use crate::storage::StoragePaths;
use crate::storage::records::RecordStore;
use crate::storage::records::{StoredRecord, active_records};

use crate::git::{RepoContext, ResolvedSnapshot};

/// Re-export the round timeout so callers share one budget.
pub use crate::jury::review::REVIEW_ROUND_TIMEOUT as INVESTIGATION_ROUND_TIMEOUT;

/// How the review was invoked (provenance in the review event).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvestigationInputs {
    /// The resolved task input.
    pub task: InputValue,
    /// The resolved result input.
    pub result: InputValue,
}

/// Everything the investigation pipeline needs besides the repository.
#[derive(Clone)]
pub struct InvestigationReviewOptions {
    /// How to launch the Pi review host child process.
    pub launch: crate::jury::host::PiHostLaunch,
    /// Resolved model profiles (trusted local configuration).
    pub model_profiles: BTreeMap<String, crate::domain::model::ModelProfile>,
    /// Optional routing provider blended into the rules decision.
    pub routing_provider: Option<Arc<dyn crate::providers::RoutingClassifierProvider>>,
    /// Optional embedding provider reranking retrieval.
    pub embedding_provider: Option<Arc<dyn crate::providers::EmbeddingProvider>>,
    /// Contributor identity for durable knowledge (plan section 44).
    pub contributor_id: Option<String>,
    /// Reviewer-concurrency bound (plan milestone 29).
    pub max_concurrent_reviewers: u32,
}

/// One reviewer report's validated assessments (ids assigned by Rust).
struct ValidatedReport {
    claims: Vec<(
        String,
        crate::domain::investigation::ReviewerClaimAssessment,
    )>,
    gaps: Vec<(String, crate::domain::investigation::ReviewerCoverageGap)>,
}

/// Typed investigation-review failures.
#[derive(Debug, Error)]
pub enum InvestigationReviewError {
    #[error("configuration: {0}")]
    Config(String),
    #[error("git: {0}")]
    Git(#[from] crate::git::GitError),
    #[error("cache: {0}")]
    Cache(String),
    #[error("retrieval: {0}")]
    Retrieval(#[from] crate::retrieval::RetrievalError),
    #[error("host: {0}")]
    Host(#[from] crate::jury::host::HostError),
    #[error("no valid investigation reports: {failures:?}")]
    TooFewValidReports { failures: Vec<String> },
    #[error("routing: {0}")]
    Routing(String),
}

/// Span-match an investigation evidence excerpt against one input:
/// whitespace-normalized substring containment.
fn input_contains(input: &str, excerpt: &str) -> bool {
    let normalize = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    let (haystack, needle) = (normalize(input), normalize(excerpt));
    haystack.contains(&needle)
}

/// Validate one investigation report against the resolved inputs and the
/// working-tree snapshot. Returns the accepted raw assessments (ids
/// assigned by Rust) plus diagnostics; unanchored or evidence-free claim
/// assessments are clamped to `InsufficientlySupported` instead of being
/// dropped (the failure itself is signal), and structurally invalid
/// shapes are rejected outright.
#[allow(clippy::too_many_lines)]
fn validate_report(
    validation: &ValidationContext<'_>,
    reviewer_id: &str,
    report: &InvestigationReport,
    inputs: &InvestigationInputs,
    diagnostics: &mut Vec<String>,
) -> ValidatedReport {
    let resolve_input = |part: TaskOrResult| match part {
        TaskOrResult::Task => inputs.task.text.as_str(),
        TaskOrResult::Result => inputs.result.text.as_str(),
    };
    let mut claims_out = Vec::new();
    let mut gaps_out = Vec::new();
    for (index, assessment) in report.claim_assessments.iter().enumerate() {
        let raw_id = new_ulid();
        // Per-evidence validation: input spans and snapshot membership.
        let mut evidence = Vec::new();
        let mut span_failures = 0usize;
        for reference in &assessment.evidence {
            match reference.kind {
                crate::domain::evidence::EvidenceKind::Investigation => {
                    let Some(part) = reference.input_part else {
                        span_failures += 1;
                        continue;
                    };
                    if input_contains(resolve_input(part), &reference.excerpt) {
                        evidence.push(reference.clone());
                    } else {
                        span_failures += 1;
                    }
                }
                crate::domain::evidence::EvidenceKind::Source => {
                    // Snapshot membership via the existing evidence
                    // validator: a minimal synthetic finding wrapper.
                    if crate::jury::findings::validate_evidence(validation, reference).is_ok() {
                        evidence.push(reference.clone());
                    } else {
                        span_failures += 1;
                    }
                }
                _ => {
                    // Diff/plan/project-fact kinds do not occur in
                    // investigation reviews; drop them.
                    span_failures += 1;
                }
            }
        }
        if span_failures > 0 {
            diagnostics.push(format!(
                "reviewer `{reviewer_id}` claim {index}: {span_failures} evidence ref(s) failed \
                 input-span or snapshot validation"
            ));
        }
        // Claim anchor rule (reviewed plan): every claim must anchor to
        // the result input; unanchored or evidence-free assessments clamp
        // to InsufficientlySupported with a visible diagnostic.
        let mut verdict = assessment.verdict;
        let mut confidence = clamp_verdict_confidence(verdict, assessment.confidence);
        let mut overstated = assessment.overstated.clone();
        let anchored_to_result = evidence
            .iter()
            .any(|reference| reference.input_part == Some(TaskOrResult::Result));
        if !anchored_to_result {
            diagnostics.push(format!(
                "reviewer `{reviewer_id}` claim {index}: no result-anchored evidence survived \
                 validation; verdict clamped to insufficiently_supported"
            ));
            verdict = InvestigationVerdict::InsufficientlySupported;
            confidence = ConfidenceTier::Unsupported;
            overstated = Some(
                overstated
                    .unwrap_or_else(|| "no anchored evidence survived validation".to_string()),
            );
        }
        let validated_assessment = crate::domain::investigation::ReviewerClaimAssessment {
            claim: assessment.claim.clone(),
            verdict,
            confidence,
            overstated,
            task_relevance: assessment.task_relevance.clone(),
            reasoning: assessment.reasoning.clone(),
            evidence,
        };
        claims_out.push((raw_id, validated_assessment));
    }

    // Coverage gaps: structural shape + task-span evidence.
    for gap in &report.coverage_gaps {
        let mut evidence = Vec::new();
        for reference in &gap.evidence {
            if reference.kind == crate::domain::evidence::EvidenceKind::Investigation
                && let Some(part) = reference.input_part
                && input_contains(resolve_input(part), &reference.excerpt)
            {
                evidence.push(reference.clone());
            }
        }
        gaps_out.push((
            new_ulid(),
            crate::domain::investigation::ReviewerCoverageGap {
                area: gap.area.clone(),
                why_expected: gap.why_expected.clone(),
                importance: gap.importance,
                evidence,
            },
        ));
    }
    ValidatedReport {
        claims: claims_out,
        gaps: gaps_out,
    }
}

/// Deterministic most-skeptical merge of one equivalence group.
#[allow(clippy::too_many_lines)]
fn merge_group(
    members: &[(
        String,
        crate::domain::investigation::ReviewerClaimAssessment,
    )],
) -> ClaimAssessment {
    let mut verdicts: Vec<InvestigationVerdict> = members
        .iter()
        .map(|(_, assessment)| assessment.verdict)
        .collect();
    verdicts.sort_by_key(|verdict| std::cmp::Reverse(verdict.skepticism()));

    let substantive: Vec<InvestigationVerdict> = verdicts
        .iter()
        .copied()
        .filter(|v| !v.is_relevance())
        .collect();
    let all_relevance = substantive.is_empty();
    let unanimous = verdicts.iter().all(|v| *v == verdicts[0]);

    let verdict = if all_relevance {
        // Relevance verdicts stand only when unanimous; disagreement on
        // relevance degrades to a substantive failure verdict with an
        // explicit note.
        if unanimous {
            verdicts[0]
        } else {
            InvestigationVerdict::InsufficientlySupported
        }
    } else {
        // Most skeptical substantive verdict wins.
        substantive[0]
    };

    let mut confidence = members
        .iter()
        .map(|(_, assessment)| assessment.confidence)
        .min()
        .unwrap_or(ConfidenceTier::Unsupported);
    confidence = clamp_verdict_confidence(verdict, confidence);

    let reviewers: Vec<String> = members
        .iter()
        .map(|(reviewer_id, _)| reviewer_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let cross_validated = reviewers.len() >= 2;

    let mut disagreement = String::new();
    let mut distinct: Vec<InvestigationVerdict> = verdicts.clone();
    distinct.sort_by_key(|verdict| verdict.skepticism());
    distinct.dedup();
    if distinct.len() > 1 || (!unanimous && all_relevance) {
        disagreement = format!(
            "reviewers disagreed: {:?}",
            verdicts
                .iter()
                .map(|v| serde_json::to_value(v).unwrap_or_default())
                .collect::<Vec<_>>()
        );
    }

    // Evidence union (deduplicated by normalized excerpt + part).
    let mut evidence: Vec<crate::domain::evidence::EvidenceRef> = Vec::new();
    let mut seen = BTreeSet::new();
    for (_, assessment) in members {
        for reference in &assessment.evidence {
            let key = format!(
                "{:?}|{:?}|{}",
                reference.kind,
                reference.input_part,
                reference
                    .excerpt
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            if seen.insert(key) {
                evidence.push(reference.clone());
            }
        }
    }

    // Overstated notes are unioned (deduplicated).
    let mut overstated_notes: Vec<String> = Vec::new();
    for (_, assessment) in members {
        if let Some(note) = &assessment.overstated
            && !overstated_notes.contains(note)
        {
            overstated_notes.push(note.clone());
        }
    }

    let reasoning = if members.len() == 1 {
        members[0].1.reasoning.clone()
    } else {
        members
            .iter()
            .map(|(reviewer_id, assessment)| {
                format!(
                    "{}: {}",
                    reviewer_id,
                    truncate_text(&assessment.reasoning, 600)
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    };

    let mut overstated = if overstated_notes.is_empty() {
        None
    } else {
        Some(overstated_notes.join("; "))
    };
    if !disagreement.is_empty() {
        overstated = Some(match overstated {
            Some(notes) => format!("{notes}; {disagreement}"),
            None => disagreement,
        });
    }
    ClaimAssessment {
        schema_version: investigation::INVESTIGATION_SCHEMA_VERSION,
        id: new_ulid(),
        claim: members[0].1.claim.clone(),
        verdict,
        confidence,
        overstated,
        task_relevance: members[0].1.task_relevance.clone(),
        evidence,
        reasoning,
        source_claim_ids: members.iter().map(|(id, _)| id.clone()).collect(),
        reviewer_ids: reviewers,
        cross_validated,
    }
}

/// Normalized grouping key for claim assessments (deterministic
/// equivalence: normalized claim text).
fn claim_key(claim: &str) -> String {
    claim
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Investigation review summary (deterministic assembly).
fn build_summary(claims: &[ClaimAssessment], gaps: &[CoverageGap]) -> String {
    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    for claim in claims {
        let label = match claim.verdict {
            InvestigationVerdict::Supported => "supported",
            InvestigationVerdict::PartiallySupported => "partially supported",
            InvestigationVerdict::Contradicted => "contradicted",
            InvestigationVerdict::InsufficientlySupported => "insufficiently supported",
            InvestigationVerdict::NotApplicable => "not applicable",
            InvestigationVerdict::OutsideScope => "outside scope",
        };
        *counts.entry(label).or_default() += 1;
    }
    let breakdown = counts
        .iter()
        .map(|(label, count)| format!("{count} {label}"))
        .collect::<Vec<_>>()
        .join(", ");
    let cross = claims.iter().filter(|claim| claim.cross_validated).count();
    let overstated = claims
        .iter()
        .filter(|claim| claim.overstated.is_some())
        .count();
    format!(
        "Assessed {} claim(s) ({breakdown}); {cross} cross-validated by independent reviewers; \
         {overstated} flagged as overstated. {} important coverage gap(s) identified. \
         Verdicts are most-skeptical merges: confidence never exceeds what the surviving \
         evidence supports, and coverage gaps indicate incompleteness rather than proven \
         findings.",
        claims.len(),
        gaps.len(),
    )
}

/// The full investigation review pipeline (frozen order, mirroring the
/// plan/code pipelines).
#[allow(clippy::too_many_lines)]
pub fn run_investigation_review(
    ctx: &RepoContext,
    resolved: &ResolvedSnapshot,
    paths: &StoragePaths,
    inputs: &InvestigationInputs,
    options: &InvestigationReviewOptions,
) -> Result<InvestigationResult, InvestigationReviewError> {
    // 1-4. Snapshot is the working tree (resolved by the caller); the
    // source index is refreshed lazily for exactly that snapshot.
    crate::index::refresh_index(ctx, resolved, paths)
        .map_err(|err| InvestigationReviewError::Cache(err.to_string()))?;
    let catalog =
        Catalog::bundled().map_err(|err| InvestigationReviewError::Config(err.to_string()))?;
    let records = RecordStore::new(&ctx.worktree_root)
        .load()
        .map_err(|err| InvestigationReviewError::Cache(err.to_string()))?;
    let retriever = match &options.embedding_provider {
        Some(provider) => {
            Retriever::open(resolved, paths)?.with_embedding_provider(provider.clone())
        }
        None => Retriever::open(resolved, paths)?,
    };
    let stale_facts = stale_fact_ids(ctx, resolved, &records)?;
    retriever.sync_records(&records, &stale_facts)?;
    retriever.sync_practices(&catalog)?;

    // 4. Descriptors from BOTH inputs.
    let descriptors: Vec<String> = review_pipeline::plan_descriptors(&inputs.task.text)
        .into_iter()
        .chain(review_pipeline::plan_descriptors(&inputs.result.text))
        .take(crate::jury::review::MAX_PLAN_DESCRIPTORS * 2)
        .collect();
    let query_text = format!(
        "{}\n{}",
        truncate_text(&inputs.task.text, MAX_QUERY_BYTES),
        truncate_text(&inputs.result.text, MAX_QUERY_BYTES)
    );
    let query = RetrievalQuery::from_text(truncate_text(&query_text, MAX_QUERY_BYTES));
    let shared_context = retriever.lookup(&query)?;
    let active = active_records(&records);

    // 6. Routing input from both inputs.
    let routing_input = RoutingInput {
        descriptors: descriptors.clone(),
        changed_paths: Vec::new(),
        languages: snapshot_languages(paths)
            .map_err(|err| InvestigationReviewError::Cache(err.to_string()))?,
        changed_symbols: Vec::new(),
        deterministic_tags: record_tags(&shared_context, &active),
        lessons: active_lesson_hints(&active),
    };

    // 7./8. Deterministic routing (+ optional provider blend).
    let rules_decision = RulesClassifier::new(catalog.clone()).classify(&routing_input);
    let (decision, mut diagnostics) = crate::providers::routing::classify_with_optional_provider(
        rules_decision,
        options.routing_provider.as_deref(),
        &routing_input,
        &catalog,
    );
    let jury = build_investigation_jury_plan(&decision, &catalog)
        .map_err(|err| InvestigationReviewError::Routing(err.to_string()))?;

    // 11. Host.
    let mut host = HostProcess::spawn(&options.launch)?;
    host.wait_ready(crate::jury::review::HOST_READY_TIMEOUT)?;
    let host_config = HostConfigPayload::from_config_profiles(&options.model_profiles)
        .with_max_concurrent_reviewers(options.max_concurrent_reviewers);
    host.send_host_config(&host_config)?;

    // 12. Dynamic-domain description for the fallback seat.
    let mut domain_description: Option<crate::domain::reviewer::DynamicDomainDescription> = None;
    if jury.uses_dynamic_fallback() {
        let expert = catalog.dynamic_domain_expert();
        let payload = crate::jury::protocol::DomainDescriptionPayload {
            description_id: new_ulid(),
            timeout_ms: 120_000,
            model_profile: expert.default_model_profile.clone(),
            thinking_level: expert.default_thinking_level,
            descriptors: descriptors.clone(),
            changed_paths: Vec::new(),
            languages: routing_input.languages.clone(),
        };
        host.start_domain_description(&payload)?;
        let budget = Duration::from_millis(130_000);
        let outcome = host.complete_domain_description(&payload.description_id, budget)?;
        match crate::jury::review::normalize_domain_description(outcome.description, &catalog) {
            Some(normalized) => domain_description = Some(normalized),
            None => diagnostics
                .push("dynamic domain description fell back to the static template".to_string()),
        }
    }

    // 9./10. Execution specs: claim-skeptic is result-first,
    // coverage-auditor task-first; both get both inputs.
    let task_entry = input_entry(TaskOrResult::Task, &inputs.task);
    let result_entry = input_entry(TaskOrResult::Result, &inputs.result);
    let specs: Vec<crate::domain::reviewer::ReviewerExecutionSpec> = jury
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
            let framing = investigation_instructions(&seat.reviewer_id, &profile.base_instructions);
            let seat = seat_plan(profile, framing, description);
            let primary = match seat.reviewer.as_str() {
                "coverage-auditor" => &task_entry,
                _ => &result_entry,
            };
            let context = initial_context(
                &[primary.clone(), task_entry.clone(), result_entry.clone()],
                &shared_context,
                &catalog,
                &seat.practice_tags,
                &crate::jury::review::INITIAL_CONTEXT_BUDGET,
            );
            execution_spec(profile, seat, context)
        })
        .collect();

    // 13. Execute reviewers independently (one-retry policy).
    let review_id = new_ulid();
    let payload = StartInvestigationPayload {
        review_id: review_id.clone(),
        task: inputs.task.text.clone(),
        result: inputs.result.text.clone(),
        reviewers: specs.clone(),
    };
    let tool_reader = crate::git::SnapshotReader::new(ctx, resolved)?;
    let mut tool_server =
        ReviewerToolServer::new(review_id.clone(), &specs, retriever, tool_reader);
    let mut serve =
        |request: &ToolRequestPayload| -> ToolResponsePayload { tool_server.handle("", request) };
    let outcomes =
        execute_investigation(&mut host, &payload, &mut serve, INVESTIGATION_ROUND_TIMEOUT)?;

    // 14. At least two valid independent reports.
    let mut reports: Vec<(String, InvestigationReport)> = Vec::new();
    let mut failures = Vec::new();
    for (reviewer_id, outcome) in &outcomes {
        match outcome {
            InvestigationReviewerOutcome::Completed(report) => {
                reports.push((reviewer_id.clone(), report.clone()));
            }
            InvestigationReviewerOutcome::Failed(error) => failures.push(format!(
                "reviewer `{reviewer_id}` failed after its retry ({}: {})",
                error.code, error.message
            )),
        }
    }
    if reports.len() < 2 {
        return Err(InvestigationReviewError::TooFewValidReports { failures });
    }

    // 15. Validate reports against the inputs and the snapshot.
    let known_record_ids: BTreeSet<String> = records
        .iter()
        .map(StoredRecord::id)
        .map(str::to_string)
        .collect();
    let empty_changed: BTreeSet<String> = BTreeSet::new();
    let validation_context = ValidationContext {
        reader: &crate::git::SnapshotReader::new(ctx, resolved)?,
        object_format: ctx.object_format()?,
        changed_paths: Some(&empty_changed),
        known_record_ids: &known_record_ids,
    };
    // Raw accepted assessments and coverage gaps (ids assigned by Rust).
    let mut accepted_claims: Vec<(
        String,
        String,
        String,
        crate::domain::investigation::ReviewerClaimAssessment,
    )> = Vec::new();
    let mut accepted_gaps: Vec<(
        String,
        String,
        crate::domain::investigation::ReviewerCoverageGap,
    )> = Vec::new();
    for (reviewer_id, report) in &reports {
        let validated = validate_report(
            &validation_context,
            reviewer_id,
            report,
            inputs,
            &mut diagnostics,
        );
        for (raw_id, assessment) in validated.claims {
            accepted_claims.push((review_id.clone(), reviewer_id.clone(), raw_id, assessment));
        }
        for (gap_id, gap) in validated.gaps {
            accepted_gaps.push((review_id.clone(), gap_id, gap));
        }
    }

    // 16. Deterministic grouping + most-skeptical merge. Group members
    // are (reviewer_id, assessment); source_claim_ids (the raw ids) flow
    // through the merged provenance below.
    let mut groups: BTreeMap<
        String,
        Vec<(
            String,
            crate::domain::investigation::ReviewerClaimAssessment,
        )>,
    > = BTreeMap::new();
    for (_review_id, reviewer_id, raw_id, assessment) in &accepted_claims {
        let _ = raw_id;
        groups
            .entry(claim_key(&assessment.claim))
            .or_default()
            .push((reviewer_id.clone(), assessment.clone()));
    }
    let claims: Vec<ClaimAssessment> = groups
        .into_values()
        .filter(|members| !members.is_empty())
        .map(|members| merge_group(&members))
        .collect();
    // Coverage gaps: merge by normalized area; importance = strongest;
    // reviewer provenance from each contributing report.
    let mut gap_groups: BTreeMap<
        String,
        Vec<(
            String,
            String,
            crate::domain::investigation::ReviewerCoverageGap,
        )>,
    > = BTreeMap::new();
    for (review_id, gap_id, gap) in &accepted_gaps {
        gap_groups.entry(claim_key(&gap.area)).or_default().push((
            review_id.clone(),
            gap_id.clone(),
            gap.clone(),
        ));
    }
    let coverage_gaps: Vec<CoverageGap> = gap_groups
        .into_values()
        .map(|members| {
            let reviewers: Vec<String> = members
                .iter()
                .map(|(reviewer_id, _, _)| reviewer_id.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            let importance = members
                .iter()
                .map(|(_, _, gap)| gap.importance)
                .max()
                .unwrap_or(GapImportance::Low);
            let mut evidence: Vec<crate::domain::evidence::EvidenceRef> = Vec::new();
            let mut seen = BTreeSet::new();
            for (_, _, gap) in &members {
                for reference in &gap.evidence {
                    let key = format!(
                        "{:?}|{}",
                        reference.input_part,
                        reference
                            .excerpt
                            .split_whitespace()
                            .collect::<Vec<_>>()
                            .join(" ")
                    );
                    if seen.insert(key) {
                        evidence.push(reference.clone());
                    }
                }
            }
            CoverageGap {
                schema_version: investigation::INVESTIGATION_SCHEMA_VERSION,
                id: new_ulid(),
                area: members[0].2.area.clone(),
                why_expected: members[0].2.why_expected.clone(),
                importance,
                evidence,
                reviewer_ids: reviewers,
            }
        })
        .collect();
    let mut coverage_gaps = coverage_gaps;
    coverage_gaps.sort_by(|a, b| b.importance.cmp(&a.importance));

    for claim in &claims {
        if let Err(err) = validate_claim_shape(claim) {
            return Err(InvestigationReviewError::Config(err));
        }
    }
    for gap in &coverage_gaps {
        if let Err(err) = validate_gap_shape(gap) {
            return Err(InvestigationReviewError::Config(err));
        }
    }

    // 17. InvestigationResult (deterministic assembly).
    let result = InvestigationResult {
        schema_version: investigation::INVESTIGATION_SCHEMA_VERSION,
        review_id: review_id.clone(),
        kind: ReviewKind::Investigation,
        created_at: now_utc_rfc3339(),
        snapshot: resolved.descriptor.clone(),
        summary: build_summary(&claims, &coverage_gaps),
        claims,
        coverage_gaps,
    };

    // 18. Persist the local review event.
    persist_investigation_event(
        paths,
        &result,
        &routing_input,
        &decision,
        &jury,
        domain_description.as_ref(),
        &specs,
        &outcomes,
        &diagnostics,
        inputs,
    )?;

    // 19./20. Fact candidates through the existing verification path:
    // adapt each investigation report into the findings-shaped input the
    // fact pipeline expects (facts carried; findings empty).
    let adapted: Vec<(String, crate::domain::report::ReviewerReport)> = reports
        .iter()
        .map(|(reviewer_id, report)| {
            (
                reviewer_id.clone(),
                crate::domain::report::ReviewerReport {
                    reviewer_id: report.reviewer_id.clone(),
                    summary: report.summary.clone(),
                    findings: Vec::new(),
                    candidate_project_facts: report.candidate_project_facts.clone(),
                    model_metadata: report.model_metadata.clone(),
                    duration_ms: report.duration_ms,
                },
            )
        })
        .collect();
    let fact_summary = crate::learning::facts::process_fact_candidates(
        &mut host,
        ctx,
        paths,
        &validation_context,
        &review_id,
        &adapted,
        &records,
        options.contributor_id.as_deref(),
        &crate::learning::facts::FactProcessingOptions::default(),
    );
    diagnostics.extend(fact_summary.diagnostics);

    // 21. Deterministic learning maintenance is feedback-driven and
    // intentionally not part of investigation reviews (non-goal).

    Ok(result)
}

/// Context entry for one resolved investigation input.
fn input_entry(part: TaskOrResult, value: &InputValue) -> ContextEntry {
    let truncated = truncate_text(&value.text, crate::jury::review::PLAN_CONTEXT_MAX_BYTES);
    let label = match part {
        TaskOrResult::Task => "investigation input: task",
        TaskOrResult::Result => "investigation input: result",
    };
    ContextEntry {
        kind: ContextKind::Plan,
        title: label.to_string(),
        path: match &value.origin {
            investigation::InputOrigin::File { path } => Some(path.display().to_string()),
            investigation::InputOrigin::Literal => None,
        },
        symbol: None,
        line_start: None,
        line_end: None,
        record_id: None,
        practice_id: None,
        text: truncated.to_string(),
        score: 1.0,
    }
}

/// Reviewer instructions for an investigation review.
fn investigation_instructions(reviewer_id: &str, base: &str) -> String {
    let kind_note = match reviewer_id {
        "claim-skeptic" => {
            "This is an investigation CLAIM review: attempt falsification before acceptance \
             and leave `coverage_gaps` empty."
        }
        "coverage-auditor" => {
            "This is an investigation COVERAGE review: compare the result against the task \
             and leave `claim_assessments` empty."
        }
        _ => {
            "This is an investigation review: assess claims in the result from your domain \
             perspective and leave `coverage_gaps` empty."
        }
    };
    format!(
        "{base}\n\n## Review kind: investigation\n\nThe task and result inputs in your initial \
context are the primary review inputs. {kind_note} Cite task/result text or project source as \
evidence."
    )
}

/// Persist the local investigation review event (regenerable cache).
#[allow(clippy::too_many_arguments)]
fn persist_investigation_event(
    paths: &StoragePaths,
    result: &InvestigationResult,
    routing_input: &RoutingInput,
    decision: &crate::domain::routing::RoutingDecision,
    jury: &crate::domain::jury::JuryPlan,
    domain_description: Option<&crate::domain::reviewer::DynamicDomainDescription>,
    specs: &[crate::domain::reviewer::ReviewerExecutionSpec],
    outcomes: &[(String, InvestigationReviewerOutcome)],
    diagnostics: &[String],
    inputs: &InvestigationInputs,
) -> Result<(), InvestigationReviewError> {
    use crate::storage::open_or_recover;
    use crate::storage::schema::DbKind;
    let db = open_or_recover(&paths.shared, DbKind::Shared)
        .map_err(|err| InvestigationReviewError::Cache(err.to_string()))?;
    let payload = serde_json::json!({
        "schema": 1,
        "source": {
            "task": inputs.task.text.len(),
            "result": inputs.result.text.len(),
            "task_origin": format!("{:?}", inputs.task.origin),
            "result_origin": format!("{:?}", inputs.result.origin),
        },
        "routing_input": {
            "descriptors": routing_input.descriptors,
            "languages": routing_input.languages,
        },
        "decision": decision,
        "jury": jury,
        "domain_description": domain_description,
        "specs": specs
            .iter()
            .map(|spec| serde_json::json!({
                "reviewer_id": spec.reviewer_id,
                "model_profile": spec.model_profile,
                "thinking_level": spec.thinking_level,
                "timeout_ms": spec.timeout_ms,
            }))
            .collect::<Vec<_>>(),
        "outcomes": outcomes
            .iter()
            .map(|(reviewer_id, outcome)| match outcome {
                InvestigationReviewerOutcome::Completed(report) => serde_json::json!({
                    "reviewer_id": reviewer_id,
                    "outcome": "completed",
                    "claim_count": report.claim_assessments.len(),
                    "coverage_gap_count": report.coverage_gaps.len(),
                    "duration_ms": report.duration_ms,
                }),
                InvestigationReviewerOutcome::Failed(error) => serde_json::json!({
                    "reviewer_id": reviewer_id,
                    "outcome": "failed",
                    "error": error,
                }),
            })
            .collect::<Vec<_>>(),
        "diagnostics": diagnostics,
        "result": result,
    });
    db.conn
        .execute(
            "INSERT INTO review_event (review_id, kind, snapshot, payload, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                result.review_id,
                "investigation",
                serde_json::to_string(&result.snapshot).expect("descriptor serializes"),
                serde_json::to_string(&payload).expect("event payload serializes"),
                result.created_at,
            ],
        )
        .map(|_| ())
        .map_err(|err| InvestigationReviewError::Cache(err.to_string()))
}

/// Render the human-readable investigation review.
pub fn render_investigation_text(result: &InvestigationResult) -> String {
    let mode = format!("{:?}", result.snapshot.mode);
    let mut out = format!(
        "nisse investigation review {}\nsnapshot: {mode}\ncreated: {}\n\n",
        result.review_id, result.created_at
    );
    out.push_str(&result.summary);
    out.push_str("\n\n");
    let section = |title: &str, predicate: &dyn Fn(&ClaimAssessment) -> bool| -> String {
        let matching: Vec<&ClaimAssessment> =
            result.claims.iter().filter(|c| predicate(c)).collect();
        if matching.is_empty() {
            return String::new();
        }
        let mut section = format!("{title}:\n");
        for claim in matching {
            section.push_str(&format!(
                "  [{}] {}\n    id: {}\n    confidence: {}\n    relevance: {}\n    reasoning: {}\n",
                serde_json::to_value(claim.verdict)
                    .unwrap_or_default()
                    .as_str()
                    .unwrap_or("?"),
                claim.claim,
                claim.id,
                serde_json::to_value(claim.confidence)
                    .unwrap_or_default()
                    .as_str()
                    .unwrap_or("?"),
                claim.task_relevance,
                claim.reasoning,
            ));
            if let Some(note) = &claim.overstated {
                section.push_str(&format!("    overstated: {note}\n"));
            }
            for evidence in &claim.evidence {
                section.push_str(&format!(
                    "    evidence: [{}] {}\n",
                    serde_json::to_value(evidence.input_part)
                        .unwrap_or_default()
                        .as_str()
                        .unwrap_or("?"),
                    evidence.excerpt,
                ));
            }
        }
        section
    };
    out.push_str(&section("Supported", &|claim| {
        claim.verdict == InvestigationVerdict::Supported
    }));
    out.push_str(&section("Partially supported", &|claim| {
        claim.verdict == InvestigationVerdict::PartiallySupported
    }));
    out.push_str(&section("Contradicted", &|claim| {
        claim.verdict == InvestigationVerdict::Contradicted
    }));
    out.push_str(&section("Insufficiently supported", &|claim| {
        claim.verdict == InvestigationVerdict::InsufficientlySupported
    }));
    let relevance: Vec<&ClaimAssessment> = result
        .claims
        .iter()
        .filter(|claim| claim.verdict.is_relevance())
        .collect();
    if !relevance.is_empty() {
        out.push_str("Not applicable / out of scope:\n");
        for claim in relevance {
            out.push_str(&format!(
                "  [{}] {}\n    relevance: {}\n",
                serde_json::to_value(claim.verdict)
                    .unwrap_or_default()
                    .as_str()
                    .unwrap_or("?"),
                claim.claim,
                claim.task_relevance,
            ));
        }
    }
    if !result.coverage_gaps.is_empty() {
        out.push_str("Coverage gaps (incompleteness, not proven findings):\n");
        for gap in &result.coverage_gaps {
            out.push_str(&format!(
                "  [{}] {}\n    why expected: {}\n",
                serde_json::to_value(gap.importance)
                    .unwrap_or_default()
                    .as_str()
                    .unwrap_or("?"),
                gap.area,
                gap.why_expected,
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_span_matching_normalizes_whitespace() {
        assert!(input_contains(
            "the   firewall\nrule  is applied",
            "firewall rule is applied"
        ));
        assert!(!input_contains(
            "the firewall rule is applied",
            "firewall rule denied"
        ));
    }

    #[test]
    fn merge_group_takes_the_most_skeptical_verdict_and_notes_disagreement() {
        use crate::domain::evidence::{EvidenceKind, EvidenceRef};
        let evidence = || {
            vec![EvidenceRef {
                kind: EvidenceKind::Investigation,
                path: None,
                symbol: None,
                line_start: None,
                line_end: None,
                content_key: None,
                plan_section: None,
                record_id: None,
                input_part: Some(TaskOrResult::Result),
                excerpt: "a long enough verbatim excerpt from the result input".to_string(),
            }]
        };
        let make = |verdict, confidence| crate::domain::investigation::ReviewerClaimAssessment {
            claim: "the firewall rule is applied".to_string(),
            verdict,
            confidence,
            overstated: None,
            task_relevance: "the task asks about the firewall".to_string(),
            reasoning: format!("reasoning for {verdict:?}"),
            evidence: evidence(),
        };
        let members = vec![
            (
                "reviewer-a-raw".to_string(),
                make(
                    InvestigationVerdict::Supported,
                    ConfidenceTier::StronglySupported,
                ),
            ),
            (
                "reviewer-b-raw".to_string(),
                make(
                    InvestigationVerdict::Contradicted,
                    ConfidenceTier::MerelyPossible,
                ),
            ),
        ];
        let merged = merge_group(&members);
        assert_eq!(merged.verdict, InvestigationVerdict::Contradicted);
        assert_eq!(merged.confidence, ConfidenceTier::MerelyPossible);
        assert!(merged.cross_validated);
        assert!(merged.overstated.as_deref().unwrap().contains("disagreed"));
        assert_eq!(merged.source_claim_ids.len(), 2);
        assert_eq!(merged.reviewer_ids.len(), 2);
    }
}
