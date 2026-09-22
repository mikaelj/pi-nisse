//! Finding consolidation and synthesis (plan section 38).
//!
//! This module turns validated reviewer findings into final findings:
//!
//! 1. obvious duplicates are grouped deterministically by fingerprint
//!    ([`group_findings`](super::findings::group_findings));
//! 2. ambiguous cross-reviewer candidates go through the narrow Pi
//!    equivalence check (`start_finding_equivalence_check`); only `same`
//!    verdicts merge groups;
//! 3. one non-juror synthesis session restates the surviving groups;
//! 4. its output is validated against the groups — an invented finding
//!    id, a wrong impact class, or unbounded text is rejected and the
//!    group falls back to deterministic wording from its top source
//!    finding.
//!
//! Synthesis can therefore never invent review issues: every
//! [`FinalFinding`] field except the reworded text is computed by Rust
//! from the validated source findings, severity may only be lowered, no
//! valid finding is silently deleted, and every conservative fallback
//! records a diagnostic.
//!
//! The synthesis session itself runs through the frozen
//! `start_review`/`reviewer_completed` exchange with the stable
//! non-juror identity [`SYNTHESIZER_ID`] (the synthesizer is not a
//! juror, plan section 17; the protocol vocabulary has no dedicated
//! finding-synthesis message). Its only granted inputs are the finding
//! groups rendered as initial context; it receives no tools.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::domain::context::{ContextEntry, ContextKind};
use crate::domain::evidence::EvidenceRef;
use crate::domain::finding::{FinalFinding, SynthesisDiagnostic, SynthesisResult, SynthesisStage};
use crate::domain::ids::new_ulid;
use crate::domain::model::ThinkingLevel;
use crate::domain::report::ReviewerReport;
use crate::domain::review::{ReviewKind, Severity};
use crate::domain::reviewer::ReviewerExecutionSpec;

use super::execution::execute_review;
use super::findings::{
    FindingGroup, MAX_CLAIM_BYTES, MAX_EXPLANATION_BYTES, MAX_RECOMMENDATION_BYTES,
    MAX_TITLE_BYTES, ValidatedFinding, apply_equivalence, equivalence_candidates, group_findings,
};
use super::host::{HostError, HostProcess};
use super::protocol::{
    EquivalenceFinding, FindingEquivalenceCheckPayload, FindingEquivalenceOutcome,
    StartReviewPayload, ToolRequestPayload, ToolResponsePayload,
};

/// Stable identity of the non-juror synthesis session. It is a Nisse
/// controlled session identity, not a reviewer: the synthesizer never
/// participates in jury composition and never invents permanent
/// reviewer identities (plan sections 13 and 17).
pub const SYNTHESIZER_ID: &str = "synthesizer";

/// Maximum evidence entries combined into one final finding.
pub const MAX_EVIDENCE_PER_FINAL_FINDING: usize = 32;

/// Maximum bytes of one rendered finding-group context entry.
pub const MAX_GROUP_TEXT_BYTES: usize = 16_384;

/// Maximum bytes of the synthesized review summary.
pub const MAX_SUMMARY_BYTES: usize = 2_000;

/// Bounds and model selection of the consolidation pipeline.
#[derive(Debug, Clone)]
pub struct ConsolidationOptions {
    /// Explicitly configured model profile for the narrow semantic jobs
    /// (equivalence checks and synthesis). Reviewer sessions never
    /// inherit a model; neither do these.
    pub model_profile: String,
    /// Explicit thinking level for the narrow semantic jobs.
    pub thinking_level: ThinkingLevel,
    /// Maximum number of equivalence checks one consolidation performs;
    /// candidate pairs beyond the cap stay ungrouped.
    pub max_equivalence_checks: usize,
    /// Per-check session timeout (milliseconds).
    pub equivalence_timeout_ms: u64,
    /// Synthesis session timeout (milliseconds).
    pub synthesis_timeout_ms: u64,
}

impl Default for ConsolidationOptions {
    fn default() -> Self {
        ConsolidationOptions {
            model_profile: "standard".to_string(),
            thinking_level: ThinkingLevel::Medium,
            max_equivalence_checks: 32,
            equivalence_timeout_ms: 60_000,
            synthesis_timeout_ms: 180_000,
        }
    }
}

/// Consolidate validated findings into a [`SynthesisResult`] (plan
/// section 38). See the module documentation for the pipeline; every
/// step is bounded and deterministic in outcome order.
pub fn consolidate(
    host: &mut HostProcess,
    kind: ReviewKind,
    findings: &[ValidatedFinding],
    options: &ConsolidationOptions,
) -> Result<SynthesisResult, HostError> {
    let mut diagnostics: Vec<SynthesisDiagnostic> = Vec::new();

    // 1. Deterministic obvious-duplicate grouping.
    let mut groups = group_findings(findings);

    // 2. Narrow equivalence checks for ambiguous cross-reviewer pairs.
    let mut pairs = equivalence_candidates(&groups);
    if pairs.len() > options.max_equivalence_checks {
        diagnostics.push(SynthesisDiagnostic::new(
            SynthesisStage::Equivalence,
            format!(
                "{} candidate pairs exceeded the cap of {}; excess pairs left ungrouped",
                pairs.len(),
                options.max_equivalence_checks
            ),
        ));
        pairs.truncate(options.max_equivalence_checks);
    }
    if !pairs.is_empty() {
        let outcomes = run_equivalence_checks(host, &groups, &pairs, options)?;
        groups = apply_equivalence(groups, &pairs, &outcomes, &mut diagnostics);
    }

    // 3. Synthesis session over the surviving groups.
    let mut synthesized = BTreeMap::new();
    let mut session_summary = None;
    if !groups.is_empty() {
        match run_synthesis_session(host, kind, &groups, options)? {
            Some(report) => {
                let (map, summary) = validate_synthesis_report(&report, &groups, &mut diagnostics);
                synthesized = map;
                session_summary = summary;
            }
            None => diagnostics.push(SynthesisDiagnostic::new(
                SynthesisStage::Synthesis,
                "synthesis session failed after its retry; deterministic wording used",
            )),
        }
    }

    // 4. Deterministic final construction.
    let final_findings = build_final_findings(&groups, &synthesized, &mut diagnostics);
    let summary = session_summary.unwrap_or_else(|| deterministic_summary(&final_findings));

    Ok(SynthesisResult {
        summary,
        findings: final_findings,
        diagnostics,
    })
}

/// Send every equivalence check and collect the outcomes, in `pairs`
/// order. Protocol-level failures propagate: infrastructure failure is
/// an explicit review failure, not uncertainty.
fn run_equivalence_checks(
    host: &mut HostProcess,
    groups: &[FindingGroup],
    pairs: &[(usize, usize)],
    options: &ConsolidationOptions,
) -> Result<Vec<FindingEquivalenceOutcome>, HostError> {
    let mut check_ids = Vec::with_capacity(pairs.len());
    for &(a, b) in pairs {
        let payload = FindingEquivalenceCheckPayload {
            check_id: new_ulid(),
            timeout_ms: options.equivalence_timeout_ms,
            model_profile: options.model_profile.clone(),
            thinking_level: options.thinking_level,
            finding_a: equivalence_finding(&groups[a]),
            finding_b: equivalence_finding(&groups[b]),
        };
        check_ids.push(payload.check_id.clone());
        host.start_finding_equivalence_check(&payload)?;
    }
    // The host runs checks concurrently; the batch budget is one check
    // timeout plus slack for serialization.
    let budget = Duration::from_millis(options.equivalence_timeout_ms.saturating_add(10_000));
    host.complete_equivalence_checks(&check_ids, budget)
}

/// The representative of a group in an equivalence check: its first
/// member (deterministic).
fn equivalence_finding(group: &FindingGroup) -> EquivalenceFinding {
    let member = &group.members[0];
    EquivalenceFinding {
        reviewer_id: member.reviewer_id.clone(),
        finding: member.finding.clone(),
    }
}

/// Run the non-juror synthesis session. Returns `None` when the session
/// failed even after the frozen single retry.
fn run_synthesis_session(
    host: &mut HostProcess,
    kind: ReviewKind,
    groups: &[FindingGroup],
    options: &ConsolidationOptions,
) -> Result<Option<ReviewerReport>, HostError> {
    let payload = StartReviewPayload {
        review_id: format!("synthesis-{}", new_ulid()),
        kind,
        reviewers: vec![synthesizer_spec(groups, options)],
    };
    let mut no_tools = |_request: &ToolRequestPayload| -> ToolResponsePayload {
        ToolResponsePayload::failure(
            "unused",
            "capability_denied",
            "the synthesis session has no tools",
        )
    };
    let timeout = Duration::from_millis(options.synthesis_timeout_ms);
    let outcomes = execute_review(host, &payload, &mut no_tools, timeout)?;
    Ok(outcomes
        .into_iter()
        .next()
        .and_then(|(_, outcome)| outcome.as_report().cloned()))
}

/// Build the execution spec of the non-juror synthesis session.
fn synthesizer_spec(
    groups: &[FindingGroup],
    options: &ConsolidationOptions,
) -> ReviewerExecutionSpec {
    ReviewerExecutionSpec {
        reviewer_id: SYNTHESIZER_ID.to_string(),
        specialization: "final finding synthesis (non-juror)".to_string(),
        instructions: SYNTHESIZER_INSTRUCTIONS.to_string(),
        checklist: vec![
            "Restate every group you can honestly restate.".to_string(),
            "Use each group key exactly as the finding `id`.".to_string(),
            "Never raise a severity above the group maximum.".to_string(),
            "Never introduce issues, ids, or evidence that are not in the input.".to_string(),
        ],
        model_profile: options.model_profile.clone(),
        thinking_level: options.thinking_level,
        initial_context: groups.iter().map(render_group_entry).collect(),
        allowed_capabilities: Vec::new(),
        max_lookup_calls: 0,
        max_returned_context_bytes: 0,
        timeout_ms: options.synthesis_timeout_ms,
    }
}

/// Instructions of the synthesis session. The host appends the strict
/// output contract automatically; these instructions carry the
/// synthesis-specific constraints.
const SYNTHESIZER_INSTRUCTIONS: &str = "\
You are the Nisse finding synthesizer, a constrained non-juror session.
You receive validated finding groups produced by independent reviewers
and restate them clearly for the final review result.

Hard rules:
- Emit exactly one finding for every group you restate, and use the
  group key as the finding `id`, character for character.
- You may shorten, clarify, and combine compatible recommendations, and
  you may describe disagreement between the source reviewers.
- You may propose a severity lower than the group maximum; never higher.
- Keep each group's impact class and category.
- Never invent a new issue, a new finding id, new evidence, or a merge
  of groups. Only the groups supplied in the context exist.
- Emit an empty `evidence` array for every finding; Nisse attaches the
  combined source evidence itself.
- If you cannot honestly restate a group, omit it; Nisse will fall back
  to the original reviewer wording.";

/// Render one finding group as the initial-context entry the synthesis
/// session sees. The text is bounded.
fn render_group_entry(group: &FindingGroup) -> ContextEntry {
    let mut text = format!(
        "group key: {}\nimpact class: {}\ncategory: {}\ngroup severity ceiling: {}\nmembers:\n",
        group.key,
        serde_json::to_string(&group.impact_class()).expect("impact class serializes"),
        group.top_member().finding.category,
        severity_name(group.max_severity()),
    );
    for member in &group.members {
        text.push_str(&format!(
            "- reviewer `{}` (finding id `{}`, severity {}): {}\n  claim: {}\n  explanation: {}\n  recommendation: {}\n  evidence: {}\n",
            member.reviewer_id,
            member.finding.id,
            severity_name(member.severity()),
            member.finding.title,
            member.finding.claim,
            member.finding.explanation,
            member.finding.recommendation,
            render_evidence(&member.finding.evidence),
        ));
    }
    ContextEntry {
        kind: ContextKind::FindingGroup,
        title: format!("finding group {}", &group.key[..12.min(group.key.len())]),
        path: None,
        symbol: None,
        line_start: None,
        line_end: None,
        record_id: None,
        practice_id: None,
        text: truncate_char_bound(&text, MAX_GROUP_TEXT_BYTES),
        score: 1.0,
    }
}

/// Compact one-line rendering of a finding's evidence list.
fn render_evidence(evidence: &[EvidenceRef]) -> String {
    if evidence.is_empty() {
        return "(none)".to_string();
    }
    let parts: Vec<String> = evidence
        .iter()
        .map(|e| {
            let kind = serde_json::to_string(&e.kind).expect("evidence kind serializes");
            match &e.path {
                Some(path) => format!("{kind} at {path}"),
                None => kind,
            }
        })
        .collect();
    parts.join("; ")
}

/// Wire name of a severity (`"high"`, `"blocker"`, …), for diagnostics
/// and rendered context.
fn severity_name(severity: Severity) -> String {
    serde_json::to_string(&severity)
        .expect("severity serializes")
        .trim_matches('"')
        .to_string()
}

/// Truncate to at most `limit` bytes without splitting a character.
fn truncate_char_bound(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut boundary = limit;
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    format!("{}…", &text[..boundary])
}

/// One validated synthesized wording of a group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SynthesizedGroup {
    /// Severity proposed by the synthesizer; never above the group
    /// maximum (raising attempts are clamped with a diagnostic).
    pub severity: Severity,
    /// Synthesized title.
    pub title: String,
    /// Synthesized claim.
    pub claim: String,
    /// Synthesized explanation.
    pub explanation: String,
    /// Synthesized recommendation.
    pub recommendation: String,
}

/// Validate a synthesis session report against the groups it was given
/// (plan section 38: synthesis may only reference source finding ids).
///
/// Every synthesized finding must reference a known group key, carry the
/// group's impact class, and respect the text bounds; a severity above
/// the group maximum is clamped (lowering is allowed). Rejected entries
/// record diagnostics; affected groups fall back to deterministic
/// wording. The report summary is accepted when non-empty and bounded.
pub fn validate_synthesis_report(
    report: &ReviewerReport,
    groups: &[FindingGroup],
    diagnostics: &mut Vec<SynthesisDiagnostic>,
) -> (BTreeMap<String, SynthesizedGroup>, Option<String>) {
    let mut accepted: BTreeMap<String, SynthesizedGroup> = BTreeMap::new();
    let group_by_key: BTreeMap<&str, &FindingGroup> =
        groups.iter().map(|g| (g.key.as_str(), g)).collect();

    if report.reviewer_id != SYNTHESIZER_ID {
        diagnostics.push(SynthesisDiagnostic::new(
            SynthesisStage::Synthesis,
            format!(
                "synthesis report identity `{}` is not the expected non-juror `{SYNTHESIZER_ID}`; all synthesized wording rejected",
                report.reviewer_id
            ),
        ));
        return (accepted, None);
    }

    let bounded = |value: &str, bound: usize, field: &str, key: &str| -> Option<String> {
        (value.is_empty() || value.len() > bound).then(|| {
            format!(
                "synthesized finding `{key}` was rejected: `{field}` must be non-empty and at most {bound} bytes"
            )
        })
    };

    for finding in &report.findings {
        let Some(group) = group_by_key.get(finding.id.as_str()) else {
            diagnostics.push(SynthesisDiagnostic::new(
                SynthesisStage::Synthesis,
                format!(
                    "synthesis invented finding id `{}` which is no known group key; entry rejected",
                    finding.id
                ),
            ));
            continue;
        };
        if accepted.contains_key(&finding.id) {
            diagnostics.push(SynthesisDiagnostic::new(
                SynthesisStage::Synthesis,
                format!(
                    "synthesis emitted group `{}` more than once; later entry rejected",
                    finding.id
                ),
            ));
            continue;
        }
        if finding.impact_class != group.impact_class() {
            diagnostics.push(SynthesisDiagnostic::new(
                SynthesisStage::Synthesis,
                format!(
                    "synthesized finding `{}` changes the impact class; entry rejected",
                    finding.id
                ),
            ));
            continue;
        }
        if let Some(reason) = bounded(&finding.title, MAX_TITLE_BYTES, "title", &finding.id)
            .or_else(|| bounded(&finding.claim, MAX_CLAIM_BYTES, "claim", &finding.id))
            .or_else(|| {
                bounded(
                    &finding.explanation,
                    MAX_EXPLANATION_BYTES,
                    "explanation",
                    &finding.id,
                )
            })
            .or_else(|| {
                bounded(
                    &finding.recommendation,
                    MAX_RECOMMENDATION_BYTES,
                    "recommendation",
                    &finding.id,
                )
            })
        {
            diagnostics.push(SynthesisDiagnostic::new(SynthesisStage::Synthesis, reason));
            continue;
        }

        let mut severity = finding.proposed_severity;
        if severity > group.max_severity() {
            diagnostics.push(SynthesisDiagnostic::new(
                SynthesisStage::Synthesis,
                format!(
                    "synthesis tried to raise group `{}` above its severity ceiling; clamped to {}",
                    finding.id,
                    severity_name(group.max_severity())
                ),
            ));
            severity = group.max_severity();
        }
        accepted.insert(
            finding.id.clone(),
            SynthesizedGroup {
                severity,
                title: finding.title.clone(),
                claim: finding.claim.clone(),
                explanation: finding.explanation.clone(),
                recommendation: finding.recommendation.clone(),
            },
        );
    }

    let summary = match &report.summary {
        summary if !summary.is_empty() && summary.len() <= MAX_SUMMARY_BYTES => {
            Some(summary.clone())
        }
        _ => None,
    };
    (accepted, summary)
}

/// Build the final findings of every group: synthesized wording where
/// validly supplied, deterministic wording from the top source finding
/// otherwise. Every group becomes exactly one final finding — synthesis
/// may never silently delete a valid finding.
fn build_final_findings(
    groups: &[FindingGroup],
    synthesized: &BTreeMap<String, SynthesizedGroup>,
    diagnostics: &mut Vec<SynthesisDiagnostic>,
) -> Vec<FinalFinding> {
    let mut final_findings = Vec::with_capacity(groups.len());
    for group in groups {
        let top = group.top_member();
        let entry = synthesized.get(&group.key);
        if entry.is_none() {
            diagnostics.push(SynthesisDiagnostic::new(
                SynthesisStage::Synthesis,
                format!(
                    "group `{}` was not covered by the synthesis session; deterministic wording from `{}` used",
                    group.key,
                    top.qualified_id()
                ),
            ));
        }

        let (title, claim, explanation, recommendation, severity) = match entry {
            Some(entry) => (
                entry.title.clone(),
                entry.claim.clone(),
                entry.explanation.clone(),
                entry.recommendation.clone(),
                // Synthesis may only lower severity: the ceiling is
                // enforced again here so no code path can raise it.
                entry.severity.min(group.max_severity()),
            ),
            None => (
                top.finding.title.clone(),
                top.finding.claim.clone(),
                top.finding.explanation.clone(),
                top.finding.recommendation.clone(),
                group.max_severity(),
            ),
        };

        let source_finding_ids: Vec<String> = group
            .members
            .iter()
            .map(|member| member.qualified_id())
            .collect();
        let reviewer_ids = group.reviewer_ids();

        final_findings.push(FinalFinding {
            id: new_ulid(),
            impact_class: group.impact_class(),
            severity,
            category: top.finding.category.clone(),
            title,
            claim,
            explanation,
            recommendation,
            evidence: combine_evidence(group),
            source_finding_ids,
            reviewer_ids,
            cross_validated: group.cross_validated(),
        });
    }
    final_findings
}

/// Combine and deduplicate the evidence of a group's members, in member
/// order, bounded.
fn combine_evidence(group: &FindingGroup) -> Vec<EvidenceRef> {
    let mut combined: Vec<EvidenceRef> = Vec::new();
    'member: for member in &group.members {
        for evidence in &member.finding.evidence {
            if combined.len() >= MAX_EVIDENCE_PER_FINAL_FINDING {
                break 'member;
            }
            if !combined.contains(evidence) {
                combined.push(evidence.clone());
            }
        }
    }
    combined
}

/// The deterministic fallback summary.
fn deterministic_summary(findings: &[FinalFinding]) -> String {
    if findings.is_empty() {
        return "No validated findings.".to_string();
    }
    let reviewers: std::collections::BTreeSet<&str> = findings
        .iter()
        .flat_map(|finding| finding.reviewer_ids.iter().map(String::as_str))
        .collect();
    let cross_validated = findings
        .iter()
        .filter(|finding| finding.cross_validated)
        .count();
    format!(
        "{} finding(s) consolidated from {} reviewer report(s); {cross_validated} cross-validated by independent reviewers.",
        findings.len(),
        reviewers.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::evidence::{EvidenceKind, EvidenceRef};
    use crate::domain::finding::ReviewerFinding;
    use crate::domain::review::ImpactClass;
    use crate::jury::protocol::EquivalenceVerdict;

    fn evidence(path: &str) -> EvidenceRef {
        EvidenceRef {
            kind: EvidenceKind::Source,
            path: Some(path.to_string()),
            symbol: None,
            line_start: None,
            line_end: None,
            content_key: None,
            plan_section: None,
            record_id: None,
            input_part: None,
            excerpt: "fn".to_string(),
        }
    }

    fn validated(reviewer: &str, id: &str, severity: Severity, claim: &str) -> ValidatedFinding {
        let finding = ReviewerFinding {
            id: id.to_string(),
            impact_class: ImpactClass::Correctness,
            proposed_severity: severity,
            category: "logic".to_string(),
            title: format!("Title {id}"),
            claim: claim.to_string(),
            explanation: format!("Explanation {id}"),
            recommendation: format!("Recommendation {id}"),
            evidence: vec![evidence("src/a.rs")],
        };
        let fingerprint = super::super::findings::finding_fingerprint(&finding);
        ValidatedFinding {
            reviewer_id: reviewer.to_string(),
            fingerprint,
            finding,
        }
    }

    fn group_of(members: Vec<ValidatedFinding>) -> FindingGroup {
        FindingGroup {
            key: members[0].fingerprint.clone(),
            members,
        }
    }

    fn synthesis_report(findings: Vec<ReviewerFinding>, summary: &str) -> ReviewerReport {
        ReviewerReport {
            reviewer_id: SYNTHESIZER_ID.to_string(),
            summary: summary.to_string(),
            findings,
            candidate_project_facts: Vec::new(),
            model_metadata: serde_json::Value::Null,
            duration_ms: 0,
        }
    }

    #[test]
    fn invented_synthesis_findings_are_rejected() {
        // Same normalized claim and evidence location: one obvious group
        // of two members from two reviewers.
        let members = vec![
            validated("quality", "q-1", Severity::Medium, "same claim"),
            validated("concurrency", "c-1", Severity::High, "same claim"),
        ];
        assert_eq!(members[0].fingerprint, members[1].fingerprint);
        let groups = vec![group_of(members)];

        let invented = ReviewerFinding {
            id: "not-a-group-key".to_string(),
            impact_class: ImpactClass::Correctness,
            proposed_severity: Severity::Low,
            category: "logic".to_string(),
            title: "Invented".to_string(),
            claim: "I invented this.".to_string(),
            explanation: "No such group.".to_string(),
            recommendation: "Reject me.".to_string(),
            evidence: vec![],
        };
        let report = synthesis_report(vec![invented], "summary");
        let mut diagnostics = Vec::new();
        let (map, summary) = validate_synthesis_report(&report, &groups, &mut diagnostics);
        assert!(map.is_empty());
        assert_eq!(summary.as_deref(), Some("summary"));
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("no known group key"))
        );

        // The final construction still emits the group with fallback
        // wording (from the top-severity member) and the group's
        // severity ceiling.
        let findings = build_final_findings(&groups, &map, &mut diagnostics);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::High);
        assert_eq!(findings[0].title, "Title c-1");
        assert!(findings[0].cross_validated);
        assert_eq!(
            findings[0].source_finding_ids,
            vec!["quality/q-1".to_string(), "concurrency/c-1".to_string()]
        );
        assert_eq!(findings[0].reviewer_ids, vec!["concurrency", "quality"]);
    }

    #[test]
    fn synthesis_severity_may_only_lower() {
        let members = vec![
            validated("quality", "q-1", Severity::Medium, "same claim"),
            validated("concurrency", "c-1", Severity::High, "same claim"),
        ];
        let groups = vec![group_of(members)];
        let key = groups[0].key.clone();

        let raised = ReviewerFinding {
            id: key.clone(),
            impact_class: ImpactClass::Correctness,
            proposed_severity: Severity::Blocker,
            category: "logic".to_string(),
            title: "Reworded".to_string(),
            claim: "Clearer claim.".to_string(),
            explanation: "Clearer explanation.".to_string(),
            recommendation: "Clearer recommendation.".to_string(),
            evidence: vec![],
        };
        let mut diagnostics = Vec::new();
        let (map, _) = validate_synthesis_report(
            &synthesis_report(vec![raised], ""),
            &groups,
            &mut diagnostics,
        );
        assert_eq!(map[&key].severity, Severity::High);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("severity ceiling"))
        );

        let lowered = ReviewerFinding {
            id: key.clone(),
            impact_class: ImpactClass::Correctness,
            proposed_severity: Severity::Low,
            category: "logic".to_string(),
            title: "Reworded".to_string(),
            claim: "Clearer claim.".to_string(),
            explanation: "Clearer explanation.".to_string(),
            recommendation: "Clearer recommendation.".to_string(),
            evidence: vec![],
        };
        let mut diagnostics = Vec::new();
        let (map, summary) = validate_synthesis_report(
            &synthesis_report(vec![lowered], "summarized"),
            &groups,
            &mut diagnostics,
        );
        assert_eq!(map[&key].severity, Severity::Low);
        assert!(diagnostics.is_empty());
        assert_eq!(summary.as_deref(), Some("summarized"));

        let findings = build_final_findings(&groups, &map, &mut diagnostics);
        assert_eq!(findings[0].severity, Severity::Low);
        assert_eq!(findings[0].title, "Reworded");
    }

    #[test]
    fn synthesis_identity_and_impact_are_enforced() {
        let groups = vec![group_of(vec![validated(
            "quality",
            "q-1",
            Severity::Low,
            "claim",
        )])];
        let key = groups[0].key.clone();

        // Wrong session identity: everything rejected.
        let mut report = synthesis_report(vec![], "s");
        report.reviewer_id = "impostor".to_string();
        let mut diagnostics = Vec::new();
        let (map, summary) = validate_synthesis_report(&report, &groups, &mut diagnostics);
        assert!(map.is_empty());
        assert!(summary.is_none());
        assert!(diagnostics.iter().any(|d| d.message.contains("non-juror")));

        // Wrong impact class: entry rejected.
        let wrong_impact = ReviewerFinding {
            impact_class: ImpactClass::Security,
            id: key,
            proposed_severity: Severity::Low,
            ..synthesis_finding_shape()
        };
        let mut diagnostics = Vec::new();
        let (map, _) = validate_synthesis_report(
            &synthesis_report(vec![wrong_impact], "s"),
            &groups,
            &mut diagnostics,
        );
        assert!(map.is_empty());
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("changes the impact class"))
        );

        // Duplicate group emission: later entry rejected.
        let groups = vec![group_of(vec![validated(
            "quality",
            "q-2",
            Severity::Low,
            "other claim",
        )])];
        let key = groups[0].key.clone();
        let first = ReviewerFinding {
            id: key.clone(),
            ..synthesis_finding_shape()
        };
        let second = ReviewerFinding {
            id: key.clone(),
            ..synthesis_finding_shape()
        };
        let mut diagnostics = Vec::new();
        let (map, _) = validate_synthesis_report(
            &synthesis_report(vec![first, second], "s"),
            &groups,
            &mut diagnostics,
        );
        assert_eq!(map.len(), 1);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("more than once"))
        );
    }

    fn synthesis_finding_shape() -> ReviewerFinding {
        ReviewerFinding {
            id: String::new(),
            impact_class: ImpactClass::Correctness,
            proposed_severity: Severity::Low,
            category: "logic".to_string(),
            title: "T".to_string(),
            claim: "C".to_string(),
            explanation: "E".to_string(),
            recommendation: "R".to_string(),
            evidence: vec![],
        }
    }

    #[test]
    fn evidence_is_combined_bounded_and_deduplicated() {
        // Two members with identical claims but distinct evidence lists
        // (built manually so fingerprints match on claim while evidence
        // differs is impossible via finding_fingerprint — evidence is
        // part of it — so exercise combine_evidence through a manual
        // group instead).
        let mut a = validated("quality", "q-1", Severity::Low, "claim a");
        a.finding.evidence = vec![evidence("src/a.rs"), evidence("src/b.rs")];
        let mut b = validated("concurrency", "c-1", Severity::Low, "claim b");
        b.finding.evidence = vec![evidence("src/a.rs"), evidence("src/c.rs")];
        let group = FindingGroup {
            key: a.fingerprint.clone(),
            members: vec![a, b],
        };
        let combined = combine_evidence(&group);
        assert_eq!(combined.len(), 3);
        assert_eq!(combined[0].path.as_deref(), Some("src/a.rs"));
        assert_eq!(combined[1].path.as_deref(), Some("src/b.rs"));
        assert_eq!(combined[2].path.as_deref(), Some("src/c.rs"));

        // Bounded: many distinct evidence entries collapse to the cap.
        let mut wide = validated("quality", "q-2", Severity::Low, "wide");
        wide.finding.evidence = (0..64)
            .map(|i| evidence(&format!("src/{i}.rs")))
            .collect::<Vec<_>>();
        let group = FindingGroup {
            key: wide.fingerprint.clone(),
            members: vec![wide],
        };
        assert_eq!(
            combine_evidence(&group).len(),
            MAX_EVIDENCE_PER_FINAL_FINDING
        );
    }

    #[test]
    fn deterministic_summary_and_empty_result() {
        assert_eq!(deterministic_summary(&[]), "No validated findings.");
        let finding = FinalFinding {
            id: new_ulid(),
            impact_class: ImpactClass::Correctness,
            severity: Severity::Low,
            category: "logic".to_string(),
            title: "T".to_string(),
            claim: "C".to_string(),
            explanation: "E".to_string(),
            recommendation: "R".to_string(),
            evidence: vec![],
            source_finding_ids: vec!["quality/q-1".to_string()],
            reviewer_ids: vec!["quality".to_string()],
            cross_validated: false,
        };
        let summary = deterministic_summary(&[finding]);
        assert!(summary.contains("1 finding(s)"));
        assert!(summary.contains("1 reviewer report(s)"));
    }

    #[test]
    fn synthesizer_spec_is_fully_constrained() {
        let members = vec![validated("quality", "q-1", Severity::Low, "claim")];
        let groups = vec![group_of(members)];
        let options = ConsolidationOptions::default();
        let spec = synthesizer_spec(&groups, &options);
        assert_eq!(spec.reviewer_id, SYNTHESIZER_ID);
        assert!(spec.allowed_capabilities.is_empty());
        assert_eq!(spec.max_lookup_calls, 0);
        assert_eq!(spec.timeout_ms, options.synthesis_timeout_ms);
        assert_eq!(spec.model_profile, options.model_profile);
        assert_eq!(spec.initial_context.len(), 1);
        assert_eq!(spec.initial_context[0].kind, ContextKind::FindingGroup);
        assert!(spec.initial_context[0].text.contains("group key: "));
        assert!(
            spec.initial_context[0]
                .text
                .contains("reviewer `quality` (finding id `q-1`")
        );
        assert!(!spec.instructions.contains('{'));
    }

    #[test]
    fn verdicts_map_to_equivalence_finding_representatives() {
        let members = vec![
            validated("quality", "q-1", Severity::Low, "claim a"),
            validated("concurrency", "c-1", Severity::Low, "claim b"),
        ];
        let group = group_of(members);
        let check = equivalence_finding(&group);
        assert_eq!(check.reviewer_id, "quality");
        assert_eq!(check.finding.id, "q-1");
        // Verdicts stay untouched by the mapping (compile-time coverage
        // of the enum used across the boundary).
        assert_eq!(
            serde_json::to_string(&EquivalenceVerdict::Same).unwrap(),
            "\"same\""
        );
        assert_eq!(
            serde_json::to_string(&EquivalenceVerdict::Different).unwrap(),
            "\"different\""
        );
        assert_eq!(
            serde_json::to_string(&EquivalenceVerdict::Uncertain).unwrap(),
            "\"uncertain\""
        );
    }
}
